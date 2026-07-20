# Execute-Phase Parallel Routing — Design Notes

> Status: design, not yet built. Captures decisions from the grok-build analysis
> session (2026-07). Rinne is a **multi-agent orchestrator**, not a single agent;
> this doc is filtered entirely through that lens.

## The scenario that motivates this

```
BIG TASK
  ▼
[BRAINSTORM]  worker: claude · opus   · superpowers:brainstorming  → spec.md   (approve)
[PLAN]        worker: claude · sonnet · superpowers:writing-plans  → plan.md   (approve)
[EXECUTE]  ← the pain lives here
   today: subagent-driven-development, one harness, all Anthropic, ~sequential → SLOW
   want:  fan plan.md's INDEPENDENT tasks across the WHOLE model/harness pool,
          each task routed to the best-fit worker on {capability, speed, cost}
```

Brainstorm and plan are serial + single-agent — fine as-is. The bottleneck is
the **execute phase**: the plan already decomposed the work into independent
tasks, but they get fed back into one agent instead of fanned across the pool
the user actually has.

## Locked decisions

1. **Core methodology unchanged.** Still a prompted conductor emitting a DAG,
   still brainstorm→plan→execute with approval gates. We add scalable layers,
   we do not rewrite the core.
2. **Pipeline stays conductor-emergent + soft "recipe" hints.** Do NOT hard-code
   `brainstorm=opus+skill` as a node type (fossilizes the methodology). Instead:
   recipe *data files* (`.rinne/recipes/*.toml`) carry phase-shape + skill +
   model-tier **hints** the conductor reads as soft priors — same machinery as
   the existing `priors.rs` / `tier_exemplars.rs`. New pipeline shapes = new data
   files, zero code. Mirrors the existing soft/overridable `prefer` field, one
   level up (phases instead of workers).
3. **Routing = explicit scored triad.** `score(worker, task) = w_cap·capability_fit
   + w_spd·speed + w_cost·(1/cost)`; pick argmax per task. Weights in config,
   tunable without recompile.
4. **Quota is a HARD FILTER applied before scoring.** Drop workers that are
   rate-limited / unavailable *right now*, then score the survivors. A worker you
   can't call can't win on cost. (Reconciles with CONTEXT.md §13 "quota, not
   dollars": quota gates eligibility; cap/speed/cost rank the eligible.)
5. **Scoring data is measured from runs, with a static cold-start prior.**
   Pure-measured has a cold-start problem (can't route without data, can't get
   data without routing). So: static prior seeds the first pick; measurements
   override it the moment a sample exists (even N=1); confidence grows with N.
   Synergy: Rinne's evaluator gate already produces pass/fail per node — that IS
   the pass-rate signal. Latency + token-usage + verdict → written to the existing
   `state.db` → feeds the next run's scoring. No new instrumentation pathway.
6. **Unit of parallelism = independent plan tasks.** plan.md's tasks with no
   shared deps run concurrently, each on its best-fit worker, isolated in
   worktrees, merged back, evaluator-gated. (Race-the-models mode deferred.)

## The layered plan (dependency order)

```
FLOOR    token-estimation primitive
   │       → per-worker packet sizing; assembler has ZERO token budgeting today
   │
DOORWAY  scored-triad routing  (cap/speed/cost · measured · hard-filter-first)
   │       → net-new; extends rinne-conductor/routing.rs
   │
UNLOCK   worktree-isolated parallel write-nodes  (+ snapshot-to-ref for resume)
   │       → lifts engine.rs:304 serial-write ceiling
   │
NEW      cross-worker reconciliation  (competing diffs: pick-best / merge / replan)
           → net-new; unavoidable once UNLOCK ships
```

## What changes in the codebase (scalable; only ONE real core change)

| Layer | Change | Core? |
|---|---|---|
| Worker registry | add speed/cost profile fields + measured-stats view | No (data on existing descriptor) |
| `state.db` | new table: per-node `(worker, task-class, latency, tokens, eval-verdict)` | No (new table) |
| `rinne-conductor/routing.rs` | hard-filter + scoring fn; weights from config | Extends, not replaces |
| `priors.rs` / recipes | recipe files as phase-shape soft priors | No (reuses prior machinery) |
| `rinne-loop/engine.rs` | lift serial-write ceiling: worktree-isolated parallel writes + merge-back | **YES — the one core change** |
| `rinne-worktree` (new) | small cross-platform CoW worktree crate | No (new leaf crate) |
| `rinne-token` (new) | bytes/4 + image-token primitive | No (new leaf crate) |

## Verified facts (checked against the code, not assumed)

- `engine.rs:304` — parallelism restricted to read-only evaluators; "everything
  that writes the workspace stays serial." DAG can express parallel generators;
  engine refuses to run them.
- `assembler.rs` — caps a *single* inlined file (`MAX_INLINE_BYTES`, `read_inlined`)
  but has **no aggregate/per-worker token budget**. Inline N files → unbounded.
- No token estimation anywhere in the repo (only string-parse "tokens" + CSS).
- Conductor `digest` is an unbounded `String` (`prompt.rs`).
- Rinne already has: conductor (classifier, ladder, tier_exemplars, routing_report),
  loop engine (stuck-detector, checkpoints, human parking, replanner, resume via
  state.db), many worker adapters, `rinne-graph` (tree-sitter, ≈ grok's codebase-graph).

## grok-build leverage ledger (orchestrator lens)

grok-build is a **single-agent execution substrate** and is itself one of Rinne's
workers (`adapters/grok.rs`). So it informs the *substrate* layers and is useless
for the *orchestration* layers (which only exist with many agents).

### ✅ LIFT (ideas, not crates)
- **`xai-fast-worktree`** → reference design, NOT a dependency. Reimplement small
  (~300-500 lines): `git worktree add` + `reflink-copy` (cross-platform CoW —
  `clonefile` macOS / `FICLONE` Linux) + the **mode taxonomy** (`WorkingTreeMode`:
  preserve-dirty / clean-tracked / clean-all; `IgnoredFilesMode`: skip / copy /
  copy-only — the real insight is populating a *usable* worktree incl. gitignored
  `.env`/`node_modules`, not an empty checkout) + **`snapshot_worktree_to_ref` /
  `rehydrate_worktree_from_ref`** (fits Rinne's resume model exactly) + the **pool**
  concept + `count_tracked_files` (O(1) index read to decide if pooling is worth it).
  DROP: the heavy stack (`gix`, `crossbeam`, `dashmap`, btrfs O(1) snapshots,
  overlayfs, `BtrfsDelegate` IPC) — built for grok's sandboxed no-`CAP_SYS_ADMIN`
  Linux-server constraint, which Rinne does not share. 90% of value at ~15% surface.
- **`xai-token-estimation`** → the bytes/4 + image-token primitive as a single
  source of truth. Tiny leaf crate. This is the FLOOR layer.

### 🟡 PARTIAL
- **`xai-prompt-queue`** → take the *wire semantics* (versioned in-place edits,
  stale-edit = no-op, attribution, `queue/changed` broadcast); drop its
  single-agent turn-loop coupling. Rinne's `/steer` should queue over a DAG.
  (Verify current `/steer` isn't fire-and-forget first.)
- **`xai-hunk-tracker`** → hunk-granularity diff attribution would sharpen the
  `test_ratchet` + evaluator loop-back (know *which* hunk regressed). Second-wave.

### ❌ DON'T LIFT (agent-internal — wrong layer)
- `xai-grok-subagent-resolution` — grok managing *its own* children; Rinne's
  children are whole workers. Conductor's role→capability→worker is the equivalent.
- `xai-agent-lifecycle` (contributor hooks) — hardens one agent's turn loop.
- `xai-grok-sandbox` — each worker sandboxes itself; genuine safety topic but not
  orchestration and not this layer.
- `xai-grok-memory`, `xai-grok-hooks` — single-agent session features.
- `xai-codebase-graph` — NOT "at parity" (earlier claim was wrong). grok's is a
  ~8909-LOC IDE-navigation engine (per-file scope-graph, go-to-def by cursor
  position); rinne-graph is a ~1565-LOC retrieval index for the context assembler
  (name→neighborhood). Different jobs. Grok's scope-graph resolves by cursor
  position, which the assembler doesn't have — wrong fix. See the native-fix note below.

### Empirically-found rinne-graph defect (do NOT solve with grok)

Indexed Rinne on itself (136 files / 1620 symbols / 4903 edges) and probed
ambiguous names. `graph neighborhood <name>` returns ONE arbitrary definition and
silently drops all other same-name symbols: `execute` (5 defs) → a TEST file;
`build` → a TUI helper; `run` (~20 defs) → one command. This is worse than
merged-blob: the assembler can anchor a worker's context packet on the WRONG
symbol with no signal anything was dropped.

Root cause (both native, both small):
- `store.rs:141` — `SELECT ... WHERE name = ?1 LIMIT 1` with **no ORDER BY**:
  undefined single-winner, roughly file-walk order.
- `store.rs:92,98` — edge resolver is `name→id` HashMap (last-write-wins per file)
  and unresolved names resolve to `unwrap_or(0)` → aliased to a non-existent symbol
  id 0. Real correctness bug in edge attribution.

Native fix (inside `rinne-graph`, ~an afternoon; no grok borrow):
1. `neighborhood_all(name) -> Vec<Neighborhood>` — drop `LIMIT 1`, add stable ORDER BY.
2. `neighborhood_in_file(name, file)` — `resolve_in_file` (store.rs:263) already
   does the filtering; the assembler has the node's mentioned files — wire the hint.
3. Drop (don't alias-to-0) unresolved edge targets.
4. Rank when forced to pick one: prefer non-test, prefer mentioned/nearby files.

Borrow from grok ONLY as ideas if the graph later becomes a bottleneck: rayon
parallel initial indexing; `notify`-driven incrementality (Rinne already depends
on `notify` for the @-picker) to replace mtime polling.

#### Staged native fix (grounded in extract.rs + assembler.rs, not just store.rs)

Key discovery reading the CONSUMER: `assembler.rs:16` hardcodes a `COMMON_NAMES`
denylist (`new,run,get,set,build,main,init,from,into`) + `MIN_IDENT_LEN=4` — a
BAND-AID over exactly this ambiguity bug. And `resolve_symbols` takes `graph` +
`mentioned` but THROWS THEM AWAY (`let _ = graph;`, line 50) — the file hint it
needs to disambiguate already flows in and is ignored. A real fix RETIRES the
denylist; that's the tell.

- **Stage 1 (small, no schema) — stops the silent wrong-anchor, the actual harm:**
  (a) `store.rs`: `neighborhood_all(name) -> Vec<Neighborhood>` — drop `LIMIT 1`
  (line 141), add `ORDER BY file, start_line` (deterministic; no more test-file
  winning by luck). (b) `assembler.rs`: use the `mentioned`/input file hint already
  in scope to prefer the neighborhood in a mentioned file; attach all when still
  ambiguous instead of dropping (`neighborhood_in_file` already exists as
  `resolve_in_file`, store.rs:263). THEN delete `COMMON_NAMES`. Flips behavior from
  "confidently wrong + silent" to "right when context disambiguates, complete otherwise".
- **Stage 2 (tiny, independent) — within-file duplicate-name collapse:** CORRECTED
  scope after reading resolve.rs — resolve.rs is actually clean (unresolved dst →
  `None`, "never dropped or guessed", tested). The real defect is `store.rs:92-93`:
  `name_to_id` HashMap is last-write-wins, so two same-name symbols in ONE file
  collapse to one id and edges mis-attribute. The `store.rs:98` `unwrap_or(0)` is a
  latent footgun (symbols are inserted before the closure runs, so it's ~unreachable
  today) not an active bug. Fix: key symbols by `(name, start_line)` / real rowid so
  same-name-same-file defs stay distinct.
- **Stage 3 (medium, schema+extract) — qualified identity, the STRUCTURAL fix:**
  add `container TEXT` to `graph_symbols`; extend `enclosing_symbol_name`'s
  parent-walk (extract.rs:49, ALREADY walks parents for call-edge src) to capture
  enclosing type/class/trait per language. CAVEAT: Rust `impl Type`/`impl Trait for
  Type` container is `impl_item.child_by_field_name("type")` not `"name"` — real
  per-lang work, medium not small. Then `neighborhood(name, container)` is exact and
  edges resolve `Type::method`. **Do Stage 3 ONLY if Stage 1's file-hint proves
  insufficient in practice — measure first** (index Rinne, probe, same loop that
  disproved the grok-scope-graph borrow). Ship 1+2, measure, then decide on 3.

#### MEASURED validation of all three stages (2026-07, on Rinne itself)

- **Stage 1 — DONE + validated.** Implemented (`neighborhood_all -> Vec` in trait/
  store/Graph; assembler uses mentioned-file hint via `neighborhoods_for`;
  `COMMON_NAMES` deleted). Measured on the real indexed DB: `execute` went from 1
  result (a TEST file) to 8 defs across 5 files incl. all real Worker impls; `run`
  1→20; `build` 1→2. Full workspace suite green, clippy clean.
- **Stage 2 — REAL bug, low frequency. Twice mis-scoped before landing on truth:**
  NOT cross-file `unwrap_or(0)` (resolve.rs is clean), NOT symbol collapse
  (graph_symbols.id is AUTOINCREMENT → distinct rows, proven). It is EDGE
  ATTRIBUTION: `name_to_id` last-write-wins routes both callers of two same-file
  same-name defs to ONE. Measured red spec fails with caller counts `[0, 2]`. Fix:
  key `name_to_id` by `(name, start_line)`. Low priority — only bites when a name is
  defined twice in ONE file AND called from that file (rare in Rust).
- **Stage 2 & 3 — IMPLEMENTED + validated on the real repo (2026-07).**
  Stage 3: `container TEXT` column added to `graph_symbols` (with idempotent
  `ALTER TABLE` migration for existing DBs); `container_of` in extract.rs walks to
  the enclosing `impl_item` (Rust `type` field) / `mod_item` / class-like node;
  threaded through RawSymbol→Symbol→store. Verified: `new` now resolves to
  `OpenAiBackend` vs `HarnessBackend`; `grade` to Tool/Ai/HumanEvaluator.
  Stage 2: `resolve_file` rewritten to assign each symbol a DISTINCT id (by
  position, no name-keyed collapse) and resolve call edges CONTAINER-AWARE
  (same-container def → file-scope def → unique match → None). `RawEdge` carries
  `src_container` (the call site's scope). Edge test green: two same-file `build`s
  each own exactly their caller.
  OPERATIONAL CAVEAT (cost real debugging): `graph index` is incremental
  (`ensure_current` skips unchanged files by content hash), so a schema/extractor
  change does NOT backfill existing rows — the first re-index after this change
  showed empty containers on unchanged files. A full re-index (`rm .rinne/state.db`)
  is required. Production rollout should bump an index/schema version to force
  re-extraction automatically.
- **Stage 3 — premise VALIDATED as REAL (reversed an earlier "probably gold-plating"
  lean).** Grepped Rinne: same-file / same-name / DIFFERENT-container collisions are
  common in production code — `OpenAiBackend::new` vs `HarnessBackend::new` in
  backend.rs; 3× `grade` impls in evaluator.rs; inherent-vs-trait `status` in
  blackboard.rs; `as_str`/`from_str` on two enums in model.rs. Stage 1's file hint
  CANNOT resolve these (file-granular; the file has two). The `container` column is
  the right-sized fix. CAVEAT bounding impact: the assembler skips names < 4 chars,
  so `new` (3) — the most common collision — is already filtered; real blast radius
  is 4+ char collisions (`grade`, `status`, `query`, `translate`). Justified, not
  urgent.

**Dividing line:** grok-build helps build the two *substrate* layers (token math,
worktree mechanics) and gives NOTHING for the two *orchestration* layers (scored
routing, cross-worker reconciliation) — because those problems only exist with
many agents, which grok-build never has.

#### Library question: `stack-graphs` (evaluated, deferred — not for stages 1-3)

`stack-graphs` + `tree-sitter-stack-graphs` (GitHub's name-resolution engine that
powers Precise Code Navigation; incremental; Visser scope-graphs research,
arxiv 2211.01224) is the PROVEN version of what grok hand-rolled. Verdict: do NOT
adopt for any of the three stages. Stage 1 = a `LIMIT 1`/`ORDER BY` fix; Stage 2 =
a HashMap keying fix — neither is library-shaped. Stage 3 needs only an enclosing
`container` string (the extract.rs parent-walk already exists), NOT cross-scope
binding; adopting stack-graphs there means writing per-language TSG rules and
REPLACING rinne-graph's extract/resolve/store core — the same "import an IDE engine"
trap rejected for grok's scope-graph, just a more legitimate engine. Reach for
stack-graphs ONLY as a future CAPABILITY UPGRADE if Rinne needs true cross-file,
cross-scope call-site→definition binding (imports, shadowing) — a separate decision
with its own justification, not a fix to the bugs found here. Match the tool to the
proven size of the problem.
