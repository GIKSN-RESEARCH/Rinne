# Harness Stage — Rail + Fidelity Pane

**Date:** 2026-07-21
**Status:** Design approved, ready for planning
**Scope:** Rinne TUI Harness Stage. Shared context between harnesses (task brief,
cross-harness findings) is explicitly **out of scope** and gets its own spec.

---

## 1. Problem

Rinne ships two incompatible ways to watch concurrent harness sessions:

- `crates/rinne-cli/src/tui/stage.rs` — an in-TUI board that splits the Stage area
  into `Constraint::Ratio(1, n)` panes (`ui.rs:468`). At three sessions each pane is
  ~33 columns, too narrow to read real harness output.
- `crates/rinne-workers/src/transport/external_terminal.rs` — 1373 lines that open a
  **real system Terminal window per harness** so the user sees the actual product UI.
  Its own header documents four failed prior approaches (nested PTY mouse garbage,
  `tee` stealing the TTY, `kill -9` leaving mouse tracking on, banner/alt-screen
  fights).

The second exists because the first cannot render harness UIs. `StageSession` stores
output as `lines: Vec<String>` fed by `push_line`/`push_token`, so ANSI cursor
movement, SGR colour, `\r` progress rewrites and line erases arrive as literal
garbage. Escaping to a real Terminal was the workaround, and it produced the window
sprawl this spec removes.

Three user-stated pains:

1. **Window/pane sprawl** — N harnesses means N windows or N unreadable slivers.
   No single place to look.
2. **No sense of collective state** — answering "who is running, who is blocked,
   who is done" requires visiting each session.
3. *(Deferred)* **No shared context between harnesses.** Separate spec.

## 2. Goals

- One terminal, one place to look, at any session count.
- Selected session readable at near-full width; all sessions visible as status.
- Harness output renders correctly (colour, progress bars, redrawn lines).
- A blocked harness is unmistakable without stealing focus from what you are reading.
- Stays responsive with three sessions streaming concurrently.

## 3. Non-goals

- Shared task brief / cross-harness findings (separate spec).
- Removing `external_terminal.rs` in this change (deprecated behind a flag; see §9).
- Faithful alt-screen emulation. See §4 for the accepted tradeoff.
- Mouse support on the rail. Keyboard only for v1.

---

## 4. Prior art: grok-build

xAI's grok-build (`github.com/xai-org/grok-build`, Rust + ratatui 0.29 — the same
version Rinne pins) solved this exact problem in `crates/codegen/xai-grok-pager/src/
views/dashboard/`: a session roster plus a "peek" fidelity pane.

**(a) grok-build deliberately does not use `vt100` or `tui-term`.** It renders child
PTY output through a custom line-model VTE sink built on the `vte` crate, keeping an
unbounded styled transcript that maps onto its line model rather than emulating a
screen grid — and yielding a plain-text channel for copy and search as a side effect.
`portable-pty` is present in grok-build but only for ssh wrapping, tests and
benchmarks, not rendering.

This suits Rinne better than a grid emulator: it preserves the existing line/scroll
model, needs no fixed per-session viewport, and removes the need for a
`Screen`/`Lines` split between harness and API workers — both become styled lines.

**Accepted tradeoff:** a harness that drives a full alt-screen TUI will have its
output rendered as a transcript, not as a faithful screen. This is the deliberate
choice; see §9 for the escape hatch and §11 for the verification gate.

**(b) Background sessions signal, they never steal focus.** When a permission request
fires on a non-focused agent, grok-build's dashboard does *not* open a modal — the row
flips to a blinking marker and the user presses a key to attend to it. For a
multi-harness Stage this is the single most important behavioural rule, and Rinne
currently has no answer for it.

### 4.1 Provenance and licensing

grok-build is **Apache-2.0**; Rinne is **MIT**. This design takes ideas, not code.

**What was taken** — none of it copyrightable expression:

- Numeric constants (rail width, breakpoints, tick divisors, caps)
- Glyph choices (`▏` U+258F for selection; braille vs dot spinners)
- Behavioural rules ("wave = running, frozen = blocked, dim = done";
  "background sessions signal, never steal focus"; "Esc cascades")
- The architectural conclusion that a line-model VTE sink beats a grid emulator for a
  line/scroll UI — most usefully, the knowledge that they tried the alternative
- Layout *policy* (floor for the list, fraction cap, shrink-to-content)

**What was not taken:** any source, function body, doc comment, or close paraphrase.

**Process rule, binding on implementation.** Implement from this document and from
upstream crate documentation (`vte`, `ratatui`). Do **not** work with grok-build
source open. The risk is not deliberate copying — it is structural paraphrase, where
an open reference produces the same decomposition and ordering with renamed
identifiers. This document exists as the intermediary precisely so that cannot happen.
Any local clone of grok-build used for research should be deleted before
implementation begins.

**No attribution in source.** Prior-art discussion belongs here, in the design
document, where it is honest context for a reader. Rinne source comments should
describe what the code does, not cite grok-build — a citation trail implies derivation
while providing none of the compliance that actual derivation would require.

Reviewed 2026-07-21: assessed as low risk and not requiring legal review, on the basis
that Apache-2.0 is permissive (worst case is an attribution obligation, not
relicensing), that Rinne is public and MIT, and that what was taken is factual rather
than expressive. **Revisit if Rinne ever vendors, bundles or redistributes grok-build
code** — that is a materially different question from reading it for design research.

---

## 5. Architecture

Three changes, each independently landable.

### 5.1 Terminal output: bytes to styled lines

New module `crates/rinne-cli/src/tui/terminal_output.rs`, modelled on grok-build's
approach, implemented fresh:

```rust
pub struct RenderedLine {
    pub line: Line<'static>,  // styled, for rendering
    pub plain: String,        // de-escaped, for copy/search/logs
}

/// Long-lived per session: partial escape sequences buffer across chunks.
pub struct TermSink { /* vte::Parser + current style + rows */ }

impl TermSink {
    pub fn feed(&mut self, bytes: &[u8]);
    pub fn lines(&self) -> &[RenderedLine];
}
```

Handles SGR (colour/style), carriage-return line rewrites (progress bars), line
erase (`\x1b[K`), and cursor-column moves within the current line. Full cursor
addressing and alt-screen switches are consumed and ignored rather than emulated.

`vte = "0.15"` is the only new dependency.

### 5.2 StageSession: one representation

`lines: Vec<String>` + `push_line`/`push_token` are replaced by a `TermSink`. Headless
and PTY sessions both produce styled lines; they differ only in source. No enum split.

```rust
pub struct StageSession {
    pub node_id: String,
    pub worker: String,
    pub model: Option<String>,
    pub backend: String,
    pub status: StageStatus,
    sink: TermSink,
    pub scroll: usize,
    last_output_at: Instant,
}

pub enum StageStatus {
    Running,
    NeedsInput,   // NEW
    Succeeded,
    Failed,
    Cancelled,
}
```

`StageBoard` keeps its `sessions` / `focus` / `visible` shape and its existing
methods (`open_session`, `finish`, `cycle_focus`, `prune_finished`). `append` /
`append_token` become `feed(&node_id, &[u8])`.

Live session count is bounded by the existing `max_sessions` cap (default 3) in
`session_gate.rs`, unchanged by this design. Finished sessions linger until
`prune_finished` reclaims them, so the rail may briefly hold more rows than the cap —
`stage_layout` is therefore specified for counts beyond 3 (§10) rather than assuming
the cap bounds rendering.

**Rail contents:** live harness sessions only. Queued nodes and API workers stay in
the PLAN tree; the Stage is exclusively for sessions with real output to watch. This
keeps `StageSession` free of "not started" and "structured events" variants.

**NeedsInput detection.** A harness awaiting input goes quiet with output pending and
no trailing newline. Rather than pattern-matching each harness's question prose
(brittle, per-adapter), the heuristic is transport-level and harness-agnostic:

> `Running` → `NeedsInput` when no bytes have arrived for `needs_input_after`
> (default 4s) **and** the last line is non-empty and unterminated.

Any subsequent byte returns the session to `Running`. Adapters may later override
with a precise signal; the heuristic is the floor, not the ceiling.

### 5.3 Layout: rail + pane

New pure module `crates/rinne-cli/src/tui/stage_layout.rs`. Geometry is computed once
per frame into a struct consumed by both rendering and (future) hit-testing so the two
cannot drift. Recomputing geometry separately in each path is how rail UIs develop
click targets that no longer match what is drawn.

```rust
pub struct StageLayout {
    pub rail: Option<Rect>,      // None when zoomed or too narrow
    pub pane: Rect,
    pub rows: Vec<Rect>,         // one per visible rail row, for hit-testing
    pub mode: RailMode,          // Full | Collapsed | Hidden
}

pub fn compute(area: Rect, session_count: usize, zoomed: bool) -> StageLayout;
```

Constants (starting values, tuned from grok-build's equivalents):

| Constant | Value | Rationale |
|---|---|---|
| `RAIL_WIDTH` | 26 | Fits `glyph + worker(12) + node(4) + elapsed(5)` |
| `MIN_STAGE_WIDTH` | 40 | Below this, nothing meaningful renders |
| `RAIL_FULL_MIN_WIDTH` | 60 | Below this the rail collapses to a glyph strip above the pane |
| `PANE_MIN_WIDTH` | 34 | Pane floor; rail yields first |
| `MAX_TRANSCRIPT_ROWS` | 10_000 | Per session; drop from the front |
| `MAX_TRANSCRIPT_COLS` | 8_192 | Guard against pathological single lines |

Responsive behaviour:

- `width >= 60` — vertical rail left (26 cols), pane right.
- `40 <= width < 60` — rail collapses to a one-line glyph strip above the pane
  (`▏⋅n2  ⸬n3  ◆n4`); pane keeps full width.
- `width < 40` — pane only; `Tab` cycles sessions.
- Zoomed (`z`) — rail hidden at any width, pane takes the full area.

The Stage **shares** the TUI middle region with the PLAN tree (plan on top,
compressed; Stage below), rather than replacing it.

**Wide (≥60 cols), normal state** — rail left, selected session right:

```
 rinne · add Redis rate limiting to the Express API and prove it works
 ──────────────────────────────────────────────────────────────────────────────
 PLAN                                          budget 11m left · 2/5 nodes
 ├─ ✔ n1 design limiter          api:claude-sonnet              1.2k tok
 ├─ ⠋ n2 implement               hns:claude-code    ··· editing 4 files
 ├─ ⠹ n3 write tests             hns:codex          ··· running npm test
 └─ ○ n4 adversarial review      api:gpt-5.5
 ──────────────────────────────────────────────────────────────────────────────
 STAGE                                                                  3 live
 ┌────────────────────────┬───────────────────────────────────────────────────┐
 │▏⋅ claude-code  n2 1m12s│ ⠋ claude-code · n2 · implement                    │
 │ ⸬ codex        n3   48s│                                                   │
 │ ◆ grok-build   n4 2m03s│   Reading src/middleware/, package.json           │
 │ ◇ opencode     n5    --│   ● Edit  src/middleware/rateLimit.ts             │
 │                        │     42  export const limiter = rateLimit({        │
 │                        │     43    windowMs: 60_000,                       │
 │                        │     44    max: 100,                               │
 │                        │     45  })                                        │
 │                        │   ● Bash  npm test -- rateLimit                   │
 │                        │     PASS  test/rateLimit.spec.ts (4 passed)       │
 └────────────────────────┴───────────────────────────────────────────────────┘
 ──────────────────────────────────────────────────────────────────────────────
 conductor: implement → claude-code (repo-aware, subscription). tests + review
            split across two models on purpose.
 ──────────────────────────────────────────────────────────────────────────────
 rinne› ▏                                        ↑↓ select · tab focus · z zoom
```

Rail columns: selection marker, status glyph, worker, node, elapsed. The indented
`● Edit` / `42 export const…` block is only renderable because of the vte sink —
today those bytes arrive as raw ANSI and render as litter.

**NeedsInput** — `grok-build n4` is asking while the user reads `n2`:

```
 ┌────────────────────────┬───────────────────────────────────────────────────┐
 │ ⋅ claude-code  n2 1m12s│ ⠋ claude-code · n2 · implement                    │
 │ ⸬ codex        n3   48s│                                                   │
 │▏◆ grok-build   n4 2m03s│   Reading src/middleware/…                        │
 │   └ needs input        │                                                   │
```

`◆` blinks at ~1.5 Hz and its accent column freezes solid. Nothing opens over the
pane; the user presses `↓` when ready.

**Narrow (<60 cols)** — rail collapses to a glyph strip, pane keeps full width:

```
 STAGE                                   3 live
 ▏⋅n2  ⸬n3  ◆n4  ◇n5
 ┌────────────────────────────────────────────┐
 │ ⠋ claude-code · n2 · implement             │
 │   ● Edit  src/middleware/rateLimit.ts      │
 │     42  export const limiter = rateLimit({ │
 │   ● Bash  npm test -- rateLimit            │
 │     PASS  test/rateLimit.spec.ts           │
 └────────────────────────────────────────────┘
```

### 5.4 Visual language

Lifted as design, implemented against Rinne's existing theme. Rinne's colours are
kept; only the structural vocabulary is adopted.

**Selection marker** `▏` U+258F (left one-eighth block), not `│` — a lighter rail.
Fallback `|` where the terminal lacks the glyph.

**Two spinners, both exactly one column** so layout never shifts:

- Focused/pane header: braille `⠋⠙⠹⠸⠼⠴⠦⠧`
- Rail rows: dot pulse `⋅ : ⸬ ⁙`

**Status glyphs:** `◆` filled (needs input / attention), `◇` hollow (idle), `✔`
success, `✗` failure. Rail and header use the same vocabulary.

**Three accent states in one column** — the core idiom:

| State | Rendering | Reads as |
|---|---|---|
| Running | wave-animated brightness down the column | working |
| NeedsInput | **frozen solid at full colour** | paused on you |
| Finished | dimmed, blended ~50% toward background | done |

Motion means working; stillness means waiting. This distinction is the reason a user
can read the rail at a glance.

**Timing** (three separate clocks, per grok-build):

| Clock | Rate | Constant |
|---|---|---|
| Draw cadence | ~60 Hz (16 ms floor) | `MIN_DRAW_INTERVAL` |
| Animation tick | 30 Hz | `TICK_INTERVAL` |
| Spinner frame | ~7.5 Hz (every 4 ticks) | `SPINNER_DIVISOR = 4` |
| NeedsInput blink | ~1.5 Hz (every 10 ticks) | `BLINK_DIVISOR = 10` |

**Focus model:** `Tab` toggles rail ↔ prompt focus, signalled by border colour alone
(bright when focused, dim when not) with the caret suppressed on the unfocused side.

### 5.5 Keybindings

| Key | Action |
|---|---|
| `↑` / `↓` | Select previous / next session (rail focused) |
| `Tab` | Toggle focus: rail ↔ prompt |
| `z` | Toggle zoom (hide rail, fullscreen pane) |
| `Ctrl+Y` | Toggle Stage visibility (existing binding, preserved) |
| `PgUp` / `PgDn` | Scroll selected session's transcript |
| `Esc` | Cascade: unzoom, then clear selection, then exit Stage focus |

`Esc` as a cascade rather than a single action avoids the common failure of one key
doing too much — the user should never lose a zoomed view when they meant to clear a
selection.

### 5.6 Performance

Three concurrent streaming sessions will melt the terminal without back-pressure.
Rinne has none today. New module `crates/rinne-cli/src/tui/present.rs`:

```rust
pub struct Presenter {
    dirty: bool,
    in_flight: bool,          // writer back-pressure
    last_draw_at: Instant,
    draw_scheduled_at: Option<Instant>,
}
```

Three stacked mechanisms, each necessary — dropping any one reintroduces a distinct
failure (redundant frames, tty back-pressure stalls, unbounded redraw under streaming):

1. **Dirty coalescing** — N state changes collapse into one draw.
2. **In-flight gating** — never issue a frame while the previous is still queued.
3. **Throttled request** — enforce `MIN_DRAW_INTERVAL`; when it is too soon, schedule
   a catch-up draw rather than dropping the update.

Applied specifically to the high-volume paths: `feed()` from streaming PTY bytes,
and scroll floods.

**Tick demand** so an idle Stage costs nothing:

```rust
pub enum TickDemand { Fast, Slow, None }
```

`Fast` when any session is `Running`; `Slow` when any is `NeedsInput` (blink only);
`None` when all are finished — the Stage parks and burns zero CPU.

---

## 6. Data flow

```
harness CLI (PTY)
   │ raw bytes
   ▼
pty.rs ─── existing: split on \n → line events → transcript/capture  (UNCHANGED)
   │
   └─ NEW: WorkerEvent::ScreenBytes(Vec<u8>)          ← purely additive
         ▼
     StageBoard::feed(node_id, &bytes)
         ▼
     TermSink (vte::Parser) → Vec<RenderedLine>
         ▼
     Presenter::request_throttled()
         ▼
     draw_stage → rail (all sessions) + pane (selected only)
```

`ScreenBytes` is an **additional consumer** of the existing byte stream. The capture
path, result files, evaluators and logs are untouched, so this cannot regress the
loop engine.

---

## 7. Modules and boundaries

| Unit | Location | Responsibility | Depends on |
|---|---|---|---|
| `terminal_output` | `tui/terminal_output.rs` (new) | bytes → styled lines; no state beyond the sink | `vte`, `ratatui` |
| `stage` | `tui/stage.rs` (rewrite) | session state, selection, status transitions | `terminal_output` |
| `stage_layout` | `tui/stage_layout.rs` (new) | pure geometry from a `Rect` | `ratatui::layout` |
| `present` | `tui/present.rs` (new) | draw coalescing, back-pressure, throttle | `std::time` |
| `ui::draw_stage` | `tui/ui.rs` (modified) | paint rail + pane from the above | all four |

Each of the four new/rewritten units is testable without a terminal. Keeping layout
as a separate pure module matters: geometry is where these UIs rot, and `ui.rs` is
already 1298 lines.

---

## 8. Error handling and edge cases

| Case | Behaviour |
|---|---|
| Malformed UTF-8 / split escape sequence | `vte` is a byte parser and the `TermSink` is long-lived per session, so partial sequences buffer across chunks naturally |
| Transcript growth | Cap at `MAX_TRANSCRIPT_ROWS`, drop from the front; adjust `scroll` by the dropped count so the viewport does not jump |
| Pathological single line | Cap at `MAX_TRANSCRIPT_COLS` |
| Terminal narrower than `MIN_STAGE_WIDTH` | Render nothing rather than garbage |
| Session finishes while selected | Stays selected and scrollable; `prune_finished` keeps the last N so results do not vanish mid-read |
| All sessions finish | Stage stays visible until dismissed; `TickDemand::None` |
| `external_terminal` enabled | Rail row renders a placeholder noting the session is in an external window; no transcript. Paths never interleave |
| Stage hidden (`Ctrl+Y`) while sessions run | `feed()` continues into the sink; no drawing. Re-showing displays full history |
| Harness emits alt-screen sequences | Consumed and ignored; transcript continues. Documented, with the flag as escape hatch |

---

## 9. `external_terminal.rs` deprecation

Kept, unreferenced by default, behind existing config:

```toml
[harness_stage]
external_terminal = false   # NEW; deprecated, removal targeted next release
needs_input_after = "4s"    # NEW
```

`HarnessStageConfig` in `crates/rinne-config/src/model.rs` gains both fields. The
existing `mode` / `approvals` / `max_sessions` fields are unchanged, as is
`session_gate.rs` (the `max_sessions` cap still applies to concurrent PTY sessions).

Removal is gated on the §11 verification passing against all three target harnesses.

---

## 10. Testing

**Unit** — one suite per module:

- `terminal_output`: feed known ANSI (SGR colour, `\r` rewrite, `\x1b[K` erase,
  split-across-chunks escape); assert both styled spans and `plain`. Assert row/col
  caps drop from the front.
- `stage`: status transitions, especially `Running → NeedsInput → Running`; selection
  bounds; `prune_finished` keeping focus valid.
- `stage_layout`: table-driven across widths 20..200 and session counts 0..8;
  assert rail/pane never overlap, never exceed `area`, and mode breakpoints are exact.
- `present`: simulated tick sequences; assert N rapid requests produce one draw, that
  no draw is issued while in-flight, and that a throttled request is not dropped.

**Snapshot** — the real risk here is *visual* regression, which unit tests miss.
ratatui `TestBackend` golden-buffer tests rendering a 3-session board at three widths
(100 / 60 / 38 cols), plus one `NeedsInput` state and one zoomed state. Committed
goldens.

**Existing tests** in `stage.rs` are adapted, not deleted — `open_append_finish_and_scroll`,
`scrollback_cap` and `cycle_focus_and_toggle` all have direct equivalents.

---

## 11. Verification gate (manual, blocking)

Before `external_terminal.rs` may be removed in a later change, the vte path must be
confirmed readable against each target harness, run interactively through Rinne:

- [ ] Claude Code
- [ ] Codex
- [ ] Grok Build

"Readable" means: output legible, colours sane, no escape-sequence litter, progress
bars not duplicated per redraw. If any harness fails, it keeps the external-terminal
flag and the failure is documented here rather than worked around.

---

## 12. Out of scope — next spec

Shared context between harnesses, which was the third pain raised:

- **Task brief** — conductor writes one brief per run (goal, constraints, acceptance
  criteria); every harness dispatch injects it, so the task is stated once.
- **Cross-harness findings** — append-only findings log on the blackboard, written via
  `rinne-mcp` (a `record_finding` tool), folded into peer prompts by the context
  assembler so harness B starts knowing what A ruled out.

Deliberately separate: it is a blackboard and context-assembly concern with no TUI
dependency, and it can land in either order relative to this work.
