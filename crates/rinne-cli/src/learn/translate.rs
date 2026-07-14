//! Translation of code and documentation into narrative.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use rinne_loop::worker::{
    format_token_count, ContextPacket, Constraints, ExecuteRequest, ExecuteResult, InlinedFile,
    Role, Usage, Worker, WorkerEvent,
};
use rinne_loop::WorkerRegistry;

use super::{LearnDoc, Narration};

/// Live progress callback shared by the CLI (stderr) and TUI (feed notes).
///
/// Held behind [`Arc`] so async workers can forward events without fighting
/// `async_trait`'s `'static` future bound.
pub type ProgressSink = std::sync::Arc<dyn Fn(String) + Send + Sync>;

/// Narration wall-clock budget. Harness workers often tool-call for a while;
/// the old 120s cap made long topics feel "stuck" and then silently fail.
const NARRATION_TIMEOUT_SECS: u64 = 300;
/// Symbol-pick is a short structured call; still allow headroom for cold starts.
const PICK_TIMEOUT_SECS: u64 = 90;
/// Heartbeat cadence while a worker is silent.
const HEARTBEAT_SECS: u64 = 15;

/// The worker + model a learn AI call used (or will use).
///
/// Surfaced so the CLI/TUI can tell the user *which* cheap tier is burning
/// quota, instead of a silent harness default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerModel {
    pub worker: String,
    /// Cheapest ladder rung pinned for this call; `None` only when the worker
    /// declares no models (CLI default, last resort).
    pub model: Option<String>,
}

impl WorkerModel {
    /// Human label: `claude-code / haiku`, or just the worker name when no model.
    pub fn label(&self) -> String {
        match &self.model {
            Some(m) => format!("{} / {}", self.worker, m),
            None => self.worker.clone(),
        }
    }
}

/// Successful narration plus the worker/model that produced it and the tokens it burned.
pub struct Translation {
    pub narration: Narration,
    pub used: WorkerModel,
    pub usage: Usage,
}

/// Converts a [`LearnDoc`] into an optional [`Translation`].
///
/// Returns `None` on degradation (no AI, no worker) or any execution failure.
#[async_trait]
pub trait Translator: Send + Sync {
    async fn translate(&self, doc: &LearnDoc) -> Option<Translation>;
}

/// Always returns `None`. Used when `--no-ai` is passed or the registry is empty.
pub struct NullTranslator;

#[async_trait]
impl Translator for NullTranslator {
    async fn translate(&self, _doc: &LearnDoc) -> Option<Translation> {
        None
    }
}

/// Runs real workers from the pool to produce a [`Narration`].
///
/// Learning is an optional enhancement, so it must not make a document fail
/// merely because the user's preferred worker is temporarily unavailable. Keep
/// the registry ordering, but try its whole fallback chain before degrading to
/// the graph-derived document.
///
/// Model selection is intentionally **cheap-first**: each invocation pins the
/// lowest rung of that worker's cascade ladder (`descriptor.models[0]`). Leaving
/// the model unset would let the harness CLI pick its own default (often the
/// frontier / highest-quota tier — e.g. Claude's Fable), which burns the user's
/// subscription for an optional teaching pass. Learn is not a production
/// cascade: we never escalate.
pub struct WorkerTranslator {
    workers: Vec<Arc<dyn Worker>>,
    workspace: PathBuf,
    on_progress: ProgressSink,
}

/// Cheapest model on a worker's ladder, or `None` when the worker has no
/// declared models (leave the CLI default alone only in that case).
fn cheapest_model(worker: &dyn Worker) -> Option<String> {
    worker.descriptor().models.first().cloned()
}

fn worker_model(worker: &dyn Worker) -> WorkerModel {
    WorkerModel {
        worker: worker.descriptor().name.clone(),
        model: cheapest_model(worker),
    }
}

/// First worker/model learn would try from the registry (for pre-flight progress).
///
/// Matches the order [`build_translator`] / [`ai_pick_symbols`] walk, so the
/// label shown before the call is the one that will actually run first.
pub fn planned_worker_model(registry: &WorkerRegistry) -> Option<WorkerModel> {
    registry
        .resolve_candidates(&[], None, false)
        .into_iter()
        .next()
        .map(|(w, _)| worker_model(w.as_ref()))
}

/// Compact usage line for the CLI/TUI: `1.2k in + 340 out = 1.5k · 87s`.
pub fn format_usage(usage: &Usage) -> String {
    let secs = usage.wall_ms.div_ceil(1000);
    format!(
        "{} in + {} out = {} · {}s",
        format_token_count(usage.prompt_tokens),
        format_token_count(usage.completion_tokens),
        usage.format_total(),
        secs,
    )
}

pub fn add_usage(a: Usage, b: Usage) -> Usage {
    Usage {
        prompt_tokens: a.prompt_tokens + b.prompt_tokens,
        completion_tokens: a.completion_tokens + b.completion_tokens,
        wall_ms: a.wall_ms + b.wall_ms,
    }
}

/// Scannable progress line: fixed kind column + detail.
///
/// ```text
/// resolve · 40 symbols · 22 files
/// narrate · claude-code / haiku
/// read    · enums/index.ts
/// tool    · find KAM backend modules
/// wait    · 30s
/// ```
pub fn progress(kind: &str, detail: impl AsRef<str>) -> String {
    format!("{kind:<7} · {}", detail.as_ref())
}

/// Friendly tier name from a full model id (`haiku-4-5-20251001` → `haiku`).
fn short_model_label(raw: &str) -> String {
    let m = raw
        .trim()
        .strip_prefix("model:")
        .unwrap_or(raw)
        .trim()
        .to_lowercase();
    let m = m.strip_prefix("claude-").unwrap_or(&m);
    for tier in ["haiku", "sonnet", "opus", "fable"] {
        if m == tier || m.starts_with(&format!("{tier}-")) || m.contains(&format!("-{tier}")) {
            return tier.to_string();
        }
    }
    // Fall back to first two hyphen segments, capped.
    let short: String = m.split('-').take(2).collect::<Vec<_>>().join("-");
    if short.is_empty() {
        m.chars().take(24).collect()
    } else {
        short
    }
}

/// True when text looks like free-form assistant chatter, not a status line.
fn looks_like_prose(t: &str) -> bool {
    let lower = t.to_lowercase();
    // Multi-sentence or clearly conversational.
    if t.contains(". ") || t.ends_with('.') || t.contains('?') || t.contains('!') {
        return true;
    }
    const LEADS: &[&str] = &[
        "let me",
        "i'll",
        "i will",
        "i'm",
        "i am",
        "now ",
        "looking ",
        "searching ",
        "gathering ",
        "checking ",
        "here ",
        "okay",
        "ok,",
        "sure",
        "first,",
        "next,",
        "then ",
        "based on",
        "from the",
        "this ",
        "that ",
        "the ",
        "a ",
        "an ",
        "we ",
        "to ",
    ];
    LEADS.iter().any(|p| lower.starts_with(p))
}

/// Bare harness tool ids (PascalCase, no spaces) — noise, not progress.
fn is_raw_tool_id(t: &str) -> bool {
    !t.contains(' ')
        && t.chars().any(|c| c.is_ascii_uppercase())
        && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Turn `mcp__codegraph__codegraph_context` into `codegraph context`.
fn humanize_tool_label(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    // Internal / noisy tool names with nothing human-readable.
    if matches!(
        t,
        "ToolSearch" | "TodoWrite" | "ListMcpResourcesTool" | "ReadMcpResourceTool"
    ) {
        return None;
    }
    if t.starts_with("mcp__") || t.contains("__") {
        let parts: Vec<&str> = t
            .trim_start_matches("mcp__")
            .split("__")
            .filter(|p| !p.is_empty())
            .collect();
        if parts.is_empty() {
            return None;
        }
        // Drop repeated package prefix: codegraph / codegraph_context → context-ish
        let last = parts.last().unwrap_or(&"");
        let label = last.replace('_', " ");
        let label = label.trim();
        if label.is_empty() {
            return None;
        }
        // Prefer "codegraph · context" when we have server + tool.
        if parts.len() >= 2 {
            let server = parts[0].replace('_', " ");
            if label.starts_with(&server) {
                return Some(label.to_string());
            }
            return Some(format!("{server} · {label}"));
        }
        return Some(label.to_string());
    }
    if is_raw_tool_id(t) {
        // Unknown CamelCase tool with no description — skip rather than spam.
        return None;
    }
    // Descriptions from Bash/Task ("Find KAM-related source files") — keep, trim.
    let cleaned = t.trim();
    if cleaned.len() > 72 {
        let mut cut: String = cleaned.chars().take(69).collect();
        cut.push('…');
        return Some(cut);
    }
    Some(cleaned.to_string())
}

/// Map a streamed worker event to a designed progress line. Drops prose,
/// raw MCP ids, and token/thinking deltas so the feed stays scannable.
fn format_worker_event(ev: &WorkerEvent) -> Option<String> {
    match ev {
        WorkerEvent::Message(m) => {
            let t = m.trim();
            if t.is_empty() || t.contains('\n') {
                return None;
            }
            if let Some(rest) = t.strip_prefix("model:") {
                return Some(progress("model", short_model_label(rest)));
            }
            if t.eq_ignore_ascii_case("updating plan") {
                return Some(progress("plan", "updated"));
            }
            // Assistant narration is not progress — even short lines.
            if looks_like_prose(t) || is_raw_tool_id(t) {
                return None;
            }
            // Allow rare short machine status, otherwise drop.
            if t.len() <= 48 && !t.contains(' ') {
                return Some(progress("note", t));
            }
            None
        }
        WorkerEvent::Reading(p) => {
            let p = p.trim();
            if p.is_empty() {
                None
            } else {
                Some(progress("read", p))
            }
        }
        WorkerEvent::Editing(p) => {
            let p = p.trim();
            if p.is_empty() {
                None
            } else if let Some(rest) = p.strip_prefix("writing ") {
                Some(progress("write", rest))
            } else if let Some(rest) = p.strip_prefix("editing ") {
                Some(progress("edit", rest))
            } else {
                Some(progress("edit", p))
            }
        }
        WorkerEvent::ToolUse(s) => humanize_tool_label(s).map(|d| progress("tool", d)),
        WorkerEvent::Raw(_) | WorkerEvent::Token(_) | WorkerEvent::Thinking(_) | WorkerEvent::Done => {
            None
        }
    }
}

/// Drive one worker invocation while forwarding events + elapsed heartbeats.
/// Consecutive duplicate lines are suppressed so a sticky tool call doesn't
/// spam the feed.
async fn execute_with_progress(
    worker: &dyn Worker,
    req: ExecuteRequest,
    on_progress: &ProgressSink,
) -> Option<ExecuteResult> {
    let (sink, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let start = std::time::Instant::now();
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(HEARTBEAT_SECS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // First tick fires immediately; skip it so we don't heartbeat at t=0.
    interval.tick().await;

    let exec = worker.execute(req, sink, CancellationToken::new());
    tokio::pin!(exec);

    let mut last: Option<String> = None;
    let mut emit = |line: String| {
        if last.as_ref() == Some(&line) {
            return;
        }
        last = Some(line.clone());
        on_progress(line);
    };

    loop {
        tokio::select! {
            res = &mut exec => {
                while let Ok(ev) = rx.try_recv() {
                    if let Some(line) = format_worker_event(&ev) {
                        emit(line);
                    }
                }
                return res.ok();
            }
            ev = rx.recv() => {
                match ev {
                    Some(ev) => {
                        if let Some(line) = format_worker_event(&ev) {
                            emit(line);
                        }
                    }
                    None => {
                        // All senders dropped; wait for the result.
                        return exec.await.ok();
                    }
                }
            }
            _ = interval.tick() => {
                emit(progress(
                    "wait",
                    format!("{}s", start.elapsed().as_secs()),
                ));
            }
        }
    }
}

/// Markers the model wraps each part in, so one response splits deterministically
/// into the [`Narration`] fields. Kept as plain fenced blocks (not headings) so
/// they can't collide with `##` headings the model writes inside a part.
const OVERVIEW_MARK: &str = "===OVERVIEW===";
const DECISIONS_MARK: &str = "===DECISIONS===";
const CONCEPTS_MARK: &str = "===CONCEPTS===";

fn teach_prompt(doc: &LearnDoc) -> String {
    format!(
        "You are onboarding someone into a real product codebase — often years of domain logic \
         across one or more verticals. The project might be AI/agent systems, multi-stage eval \
         pipelines, construction/field ops, healthcare workflows, e-commerce checkout, logistics, \
         fintech, CRM, or a portal that mixes several of those. Your job is NOT a code tour. \
         Your job is to recover the REAL DECISIONS (business, product, safety, quality, money) \
         so a developer — or the original team returning later — can re-enter a complex system \
         quickly.\n\
         Optimize for UNDERSTANDING: domain problem(s), end-to-end journeys/pipelines, rules and \
         exceptions. The reader can open the source separately — do NOT paste large code blocks. \
         Refer to symbols by name inline (`like_this`) and quote at most a line or two only when \
         a specific line IS the rule.\n\
         Infer domain(s) from names, types, and docs — do not force a single industry template. \
         If the codebase spans multiple verticals or pipelines, say so and name each relevant \
         one. Examples of domains you might encounter (not a checklist to invent): AI model \
         routing / tool use / eval harnesses, construction schedules & compliance, clinical \
         pathways & consent, cart → payment → fulfillment, lead stages, inventory allocation.\n\
         Topic: {topic}\n\n\
         Emit THREE parts, each introduced by its exact marker on its own line, in this order:\n\n\
         {ov}\n\
         2–4 short paragraphs: what this subsystem IS in product terms, the problem it solves, \
         who/what it serves, and where it sits in a larger journey or pipeline (whatever that \
         journey is for THIS domain — stages, jobs, evals, orders, encounters, builds…). Then \
         ONE Mermaid diagram in a ```mermaid fenced block — a DOMAIN map of the real journey, \
         not a code class diagram. Follow the Mermaid rules below strictly.\n\
         Mermaid rules (readability > completeness):\n\
         - Prefer `flowchart TD` (top-down). Use `LR` only for 2–4 node pairs; never a long \
           left-to-right sausage of 6+ stages (those become unreadable).\n\
         - Cap at ~8–10 nodes and ~12 edges. Collapse minor steps; omit pure plumbing.\n\
         - Labels: 2–5 words max per node; edge labels ≤3 words. No HTML/`<br/>` in labels \
           (breaks layout). Safe ids: letters/digits only (A, B1, reconcile…).\n\
         - NOT everything is sequential. Real business logic has branches and multiple ends — \
           show them. Use diamond decision nodes `{{}}` for gates (pass/fail, eligible?, \
           over-allocated?, match found?). Parallel inputs can join; one stage can fan out to \
           several outcomes (success / degraded / blocked / manual review).\n\
         - Prefer shape over a single happy path: entry → decisions → 2–3 terminal outcomes \
           beats a 10-step conveyor belt.\n\n\
         {dec}\n\
         The RULES and CONDITIONS — decisions that encode how this product/company works in its \
         industry. Prioritize what actually matters here: stage/state transitions, eligibility, \
         routing across verticals/tenants/pipelines, scoring or ranking thresholds, pricing or \
         allocation, safety/compliance gates, quality bars (e.g. eval pass/fail), ownership \
         handoffs, failure/degradation when a rule can't be met. Technical invariants only when \
         they protect a domain outcome. Use a Markdown list; each item names the rule/condition \
         and how the code enforces it, citing the relevant symbol by name.\n\n\
         {con}\n\
         The CONCEPTS someone must hold to work here — domain vocabulary first, then design \
         ideas that carry those rules. Prefer concepts native to THIS codebase (e.g. eval run, \
         trajectory score, clinical encounter, work order, cart session, multi-tenant vertical, \
         SLA clock) over generic engineering slogans. Engineering patterns (state machine, saga, \
         policy object, circuit breaker) only when they encode a real rule. For each: name it, \
         one line on what it means in this product, one line on where it shows up. Markdown list.\n\n\
         Output Markdown only (it will be rendered to HTML). Do not add text before the first \
         marker or after the last part.",
        topic = doc.topic,
        ov = OVERVIEW_MARK,
        dec = DECISIONS_MARK,
        con = CONCEPTS_MARK,
    )
}

/// Split a marked worker response into (overview, decisions, concepts). A part
/// absent from the response comes back empty; if NO marker is present at all,
/// the whole response is treated as the overview (older/looser models degrade
/// gracefully instead of losing everything).
fn split_narration(raw: &str) -> (String, String, String) {
    let find = |mark: &str| raw.find(mark).map(|i| (i, i + mark.len()));
    let ov = find(OVERVIEW_MARK);
    let dec = find(DECISIONS_MARK);
    let con = find(CONCEPTS_MARK);

    if ov.is_none() && dec.is_none() && con.is_none() {
        return (raw.trim().to_string(), String::new(), String::new());
    }

    // Collect present markers in document order, then each part runs from the end
    // of its marker to the start of the next present marker (or end of string).
    let mut marks: Vec<(usize, usize, u8)> = Vec::new(); // (start, content_start, which)
    if let Some((s, c)) = ov { marks.push((s, c, 0)); }
    if let Some((s, c)) = dec { marks.push((s, c, 1)); }
    if let Some((s, c)) = con { marks.push((s, c, 2)); }
    marks.sort_by_key(|m| m.0);

    let mut parts = [String::new(), String::new(), String::new()];
    for (idx, &(_, content_start, which)) in marks.iter().enumerate() {
        let end = marks.get(idx + 1).map(|m| m.0).unwrap_or(raw.len());
        parts[which as usize] = raw[content_start..end].trim().to_string();
    }
    let [ov, dec, con] = parts;
    (ov, dec, con)
}

#[async_trait]
impl Translator for WorkerTranslator {
    async fn translate(&self, doc: &LearnDoc) -> Option<Translation> {
        let inlined_files: Vec<InlinedFile> = doc
            .snippets
            .iter()
            .map(|s| InlinedFile {
                path: PathBuf::from(&s.file),
                contents: format!("{}\n{}", s.doc, s.code),
            })
            .collect();

        let prior_context: String = doc
            .doc_sections
            .iter()
            .map(|ds| ds.body.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");

        let context = ContextPacket {
            inlined_files,
            prior_context,
            ..Default::default()
        };

        for worker in &self.workers {
            let used = worker_model(worker.as_ref());
            // Only announce fallbacks; the outer phase line already names the first try.
            if self.workers.len() > 1 {
                (self.on_progress)(progress("try", used.label()));
            }
            // Rebuild per worker so each call pins *that* worker's cheapest model.
            let req = ExecuteRequest {
                role: Role::Generator,
                instruction: teach_prompt(doc),
                context: context.clone(),
                workspace: self.workspace.clone(),
                constraints: Constraints {
                    timeout_secs: Some(NARRATION_TIMEOUT_SECS),
                    model: used.model.clone(),
                    ..Default::default()
                },
                tools: vec![],
                mcp_servers: vec![],
            };

            let Some(r) =
                execute_with_progress(worker.as_ref(), req, &self.on_progress).await
            else {
                (self.on_progress)(progress(
                    "fail",
                    format!("{} — next", used.label()),
                ));
                continue;
            };

            if r.status.is_success() {
                let (overview, decisions, concepts) = split_narration(&r.result);
                return Some(Translation {
                    narration: Narration {
                        overview,
                        components: vec![],
                        decisions,
                        concepts,
                    },
                    used,
                    usage: r.usage,
                });
            }
            (self.on_progress)(progress(
                "fail",
                format!("{} — next", used.label()),
            ));
        }

        None
    }
}

/// Prompt asking a worker to map a free-form query to relevant symbol names,
/// choosing only from `known`. Kept small: names in, names out, one per line.
fn pick_prompt(query: &str, known: &[String]) -> String {
    // Cap the candidate list so the prompt stays within a sane token budget on
    // large repos; the model picks from what it sees.
    const MAX_CANDIDATES: usize = 600;
    let candidates = known
        .iter()
        .take(MAX_CANDIDATES)
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "A user wants to learn about: \"{query}\"\n\n\
         Below is a list of symbol names defined in this codebase. Choose the ones \
         most relevant to the user's request. Output ONLY the chosen names, one per \
         line, exactly as written, with no commentary, numbering, or extra text. \
         Choose at most 8. If none are relevant, output nothing.\n\n\
         SYMBOLS:\n{candidates}"
    )
}

/// Result of an AI symbol-pick call (names + usage so the caller can tally tokens).
#[derive(Debug, Default)]
pub struct SymbolPick {
    pub symbols: Vec<String>,
    pub usage: Usage,
}

/// Ask a worker to resolve a free-form `query` to relevant symbol names, chosen
/// from `known`. Returns the picked names (intersected with `known`, deduped,
/// capped). Returns empty symbols on `--no-ai`, no worker, execution failure, or
/// when the model picks nothing — the caller then reports "no code found".
pub async fn ai_pick_symbols(
    no_ai: bool,
    registry: &WorkerRegistry,
    workspace: &Path,
    query: &str,
    known: &[String],
    on_progress: &ProgressSink,
) -> SymbolPick {
    if no_ai || registry.is_empty() || known.is_empty() {
        return SymbolPick::default();
    }
    let instruction = pick_prompt(query, known);
    let mut text = None;
    let mut usage = Usage::default();
    for (worker, _) in registry.resolve_candidates(&[], None, false) {
        // Same cheap-first rule as [`WorkerTranslator`]: symbol pick is a
        // lightweight helper call and must not default to the harness frontier.
        let used = worker_model(worker.as_ref());
        on_progress(progress("pick", used.label()));
        let req = ExecuteRequest {
            role: Role::Generator,
            instruction: instruction.clone(),
            context: ContextPacket::default(),
            workspace: workspace.to_path_buf(),
            constraints: Constraints {
                timeout_secs: Some(PICK_TIMEOUT_SECS),
                model: used.model.clone(),
                ..Default::default()
            },
            tools: vec![],
            mcp_servers: vec![],
        };
        if let Some(r) = execute_with_progress(worker.as_ref(), req, on_progress).await {
            if r.status.is_success() {
                usage = r.usage;
                text = Some(r.result);
                break;
            }
        }
    }
    let Some(text) = text else {
        return SymbolPick::default();
    };

    let known_set: std::collections::HashSet<&str> = known.iter().map(|s| s.as_str()).collect();
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let name = line.trim().trim_matches(|c: char| c == '`' || c == '-' || c == '*').trim();
        if !name.is_empty()
            && known_set.contains(name)
            && !out.iter().any(|o| o == name)
        {
            out.push(name.to_string());
        }
        if out.len() >= 8 {
            break;
        }
    }
    SymbolPick {
        symbols: out,
        usage,
    }
}

/// Build the appropriate [`Translator`] based on flags and registry state.
///
/// Returns a [`NullTranslator`] when `no_ai` is true or the registry has no workers.
/// Otherwise wraps every compatible worker, in preference order, in a
/// [`WorkerTranslator`]. `on_progress` receives worker events and heartbeats.
pub fn build_translator(
    no_ai: bool,
    registry: &WorkerRegistry,
    workspace: &Path,
    on_progress: ProgressSink,
) -> Box<dyn Translator> {
    if no_ai || registry.is_empty() {
        Box::new(NullTranslator)
    } else {
        Box::new(WorkerTranslator {
            workers: registry
                .resolve_candidates(&[], None, false)
                .into_iter()
                .map(|(worker, _)| worker)
                .collect(),
            workspace: workspace.to_path_buf(),
            on_progress,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::learn::LearnDoc;

    #[tokio::test]
    async fn null_translator_returns_none() {
        let doc = LearnDoc {
            topic: "x".into(),
            snippets: vec![],
            flow: vec![],
            flow_seeds: vec![],
            doc_sections: vec![],
        };
        assert!(NullTranslator.translate(&doc).await.is_none());
    }

    #[test]
    fn format_usage_shows_tokens_and_seconds() {
        let u = Usage {
            prompt_tokens: 1200,
            completion_tokens: 340,
            wall_ms: 87_400,
        };
        let s = format_usage(&u);
        assert!(s.contains("in") && s.contains("out"), "token split missing: {s}");
        assert!(s.contains("87s") || s.contains("88s"), "elapsed missing: {s}");
    }

    #[test]
    fn format_worker_event_is_designed_and_quiet() {
        assert_eq!(
            format_worker_event(&WorkerEvent::Message("model: haiku-4-5-20251001".into()))
                .as_deref(),
            Some("model   · haiku")
        );
        assert_eq!(
            format_worker_event(&WorkerEvent::Reading("enums/index.ts".into())).as_deref(),
            Some("read    · enums/index.ts")
        );
        assert_eq!(
            format_worker_event(&WorkerEvent::ToolUse(
                "Find KAM-related source files".into()
            ))
            .as_deref(),
            Some("tool    · Find KAM-related source files")
        );
        assert_eq!(
            format_worker_event(&WorkerEvent::ToolUse(
                "mcp__codegraph__codegraph_context".into()
            ))
            .as_deref(),
            Some("tool    · codegraph context")
        );
        // Raw tool ids + assistant prose must not leak into the feed.
        assert!(format_worker_event(&WorkerEvent::ToolUse("ToolSearch".into())).is_none());
        assert!(format_worker_event(&WorkerEvent::Message(
            "Let me try to load the code-review-graph tools directly.".into()
        ))
        .is_none());
        assert!(format_worker_event(&WorkerEvent::Message(
            "Now let me search for KAM-related code using the codegraph tools.".into()
        ))
        .is_none());
        assert!(format_worker_event(&WorkerEvent::Token("x".into())).is_none());
    }

    #[test]
    fn progress_line_aligns_kind_column() {
        let line = progress("resolve", "40 symbols · 22 files");
        assert_eq!(line, "resolve · 40 symbols · 22 files");
        assert!(progress("read", "a.ts").starts_with("read    · "));
    }

    #[test]
    fn worker_model_label_includes_model_when_present() {
        let with_model = WorkerModel {
            worker: "claude-code".into(),
            model: Some("haiku".into()),
        };
        assert_eq!(with_model.label(), "claude-code / haiku");
        let bare = WorkerModel {
            worker: "custom".into(),
            model: None,
        };
        assert_eq!(bare.label(), "custom");
    }

    #[tokio::test]
    async fn ai_pick_returns_empty_without_worker() {
        let known = vec!["Foo".to_string(), "Bar".to_string()];
        let reg = WorkerRegistry::new();
        let ws = std::path::Path::new(".");
        // no_ai=true short-circuits; empty registry also short-circuits.
        let noop: ProgressSink = std::sync::Arc::new(|_: String| {});
        assert!(ai_pick_symbols(true, &reg, ws, "layer 1", &known, &noop)
            .await
            .symbols
            .is_empty());
        assert!(ai_pick_symbols(false, &reg, ws, "layer 1", &known, &noop)
            .await
            .symbols
            .is_empty());
    }

    #[test]
    fn pick_prompt_lists_candidates_and_query() {
        let known = vec!["Blackboard".to_string(), "Engine".to_string()];
        let p = pick_prompt("what is the loop", &known);
        assert!(p.contains("what is the loop"), "query missing");
        assert!(p.contains("Blackboard") && p.contains("Engine"), "candidates missing");
        assert!(p.to_lowercase().contains("one per line"), "output format missing");
    }

    #[test]
    fn teach_prompt_demands_business_understanding_and_mermaid() {
        let doc = LearnDoc {
            topic: "harness".into(),
            snippets: vec![],
            flow: vec![],
            flow_seeds: vec![],
            doc_sections: vec![],
        };
        let p = teach_prompt(&doc);
        let lower = p.to_lowercase();
        assert!(p.contains("harness"), "topic missing");
        assert!(lower.contains("mermaid"), "no mermaid instruction");
        // Domain-first across verticals — not a single-industry template.
        assert!(lower.contains("vertical") || lower.contains("domain"), "no multi-domain framing");
        assert!(
            lower.contains("healthcare")
                || lower.contains("e-commerce")
                || lower.contains("eval")
                || lower.contains("construction"),
            "should name diverse industry examples, not only one vertical"
        );
        assert!(lower.contains("journey") || lower.contains("pipeline"), "no journey framing");
        assert!(lower.contains("rules") && lower.contains("conditions"), "no rules ask");
        assert!(p.to_uppercase().contains("CONCEPTS"), "no concepts ask");
        assert!(p.contains(OVERVIEW_MARK) && p.contains(DECISIONS_MARK) && p.contains(CONCEPTS_MARK));
        // Diagram shape: prefer TD, branches/multiple ends, not a long LR sausage.
        assert!(lower.contains("flowchart td") || lower.contains("top-down"), "no TD preference");
        assert!(
            lower.contains("branch") || lower.contains("diamond") || lower.contains("multiple"),
            "should allow non-linear / multi-end business flows"
        );
        assert!(lower.contains("8") || lower.contains("10") || lower.contains("cap"), "no node budget");
    }

    #[test]
    fn split_narration_separates_marked_parts() {
        let raw = format!(
            "{ov}\nwhat it is\n{dec}\n- handles empty topic\n{con}\n- trait seam",
            ov = OVERVIEW_MARK, dec = DECISIONS_MARK, con = CONCEPTS_MARK,
        );
        let (ov, dec, con) = split_narration(&raw);
        assert_eq!(ov, "what it is");
        assert_eq!(dec, "- handles empty topic");
        assert_eq!(con, "- trait seam");
    }

    #[test]
    fn split_narration_without_markers_is_all_overview() {
        let (ov, dec, con) = split_narration("just some prose from an older model");
        assert_eq!(ov, "just some prose from an older model");
        assert!(dec.is_empty() && con.is_empty());
    }

    #[test]
    fn split_narration_tolerates_missing_middle_part() {
        // Only overview and concepts present — decisions comes back empty, and
        // overview must not bleed into the concepts region.
        let raw = format!("{ov}\nA\n{con}\nC", ov = OVERVIEW_MARK, con = CONCEPTS_MARK);
        let (ov, dec, con) = split_narration(&raw);
        assert_eq!(ov, "A");
        assert!(dec.is_empty(), "decisions should be empty");
        assert_eq!(con, "C");
    }

    #[test]
    fn cheapest_model_takes_first_ladder_rung() {
        // Mirrors the claude-code adapter's cheap→strong ladder: haiku first.
        // Learn must pin that, never leave the model unset (CLI default = frontier).
        use rinne_workers::mock::{MockScript, MockWorker};

        let mut script = MockScript::success("claude-code", "unused");
        script.descriptor.models = vec!["haiku".into(), "sonnet".into(), "opus".into()];
        let worker = MockWorker::new(script);
        assert_eq!(cheapest_model(&worker).as_deref(), Some("haiku"));
    }

    #[test]
    fn cheapest_model_none_when_ladder_empty() {
        use rinne_workers::mock::{MockScript, MockWorker};

        let mut script = MockScript::success("empty", "unused");
        script.descriptor.models = vec![];
        let worker = MockWorker::new(script);
        assert!(cheapest_model(&worker).is_none());
    }
}
