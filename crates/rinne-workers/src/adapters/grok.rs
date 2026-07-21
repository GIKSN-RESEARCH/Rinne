//! Grok Build adapter (`CONTEXT.md` §16).
//!
//! Drives `grok -p --output-format streaming-json`, which honors the
//! `grok login` subscription (or `XAI_API_KEY`). Grok streams token-level
//! NDJSON events: `{"type":"thought",...}`, `{"type":"text","data":"..."}`,
//! tool events, and a terminal `{"type":"end",...}`. We stream text tokens and
//! tool uses live, and accumulate the text into the result.

use std::time::Duration;

use rinne_core::worker::{
    AuthMode, Capability, LatencyProfile, QuotaModel, Transport, Usage, WorkerDescriptor,
    WorkerEvent, WorkerFamily,
};

use super::common::{approvals_are_auto, HarnessAdapter, ParsedHarness};
use crate::transport::subprocess::SubprocessOutput;

pub fn worker() -> HarnessAdapter {
    HarnessAdapter {
        descriptor: descriptor(),
        program: "grok".to_string(),
        build_args,
        plan_args: Some(plan_args),
        // Interactive Grok Build TUI: positional prompt, no -p / streaming-json.
        interactive_args: Some(interactive_args),
        parse,
        line_mapper,
        prompt_via_stdin: false,
        default_timeout: Duration::from_secs(600),
        // Grok manages MCP via `grok mcp`; no portable headless provision flag yet.
        provisioner: None,
    }
}

fn descriptor() -> WorkerDescriptor {
    WorkerDescriptor {
        name: "grok".to_string(),
        family: WorkerFamily::Harness,
        capabilities: vec![
            Capability::CodeEdit,
            Capability::RepoAware,
            Capability::Reasoning,
            Capability::Writing,
            Capability::ToolRun,
            Capability::CodeReview,
            Capability::WebSearch,
            Capability::LongContext,
        ],
        auth_mode: AuthMode::Subscription,
        quota: QuotaModel {
            capacity: 150_000.0,
            refill_per_minute: 15_000.0,
        },
        latency: LatencyProfile::Medium,
        transport: Transport::SubprocessJson,
        // Fallback ladder only — at registry build time `discover_cli_models`
        // replaces this with whatever `grok models` currently lists (cheap→strong).
        // Do not hard-code retired ids like `grok-build`; they block the DAG.
        models: vec!["grok-composer-2.5-fast".into(), "grok-4.5".into()],
    }
}

fn build_args(prompt: &str, model: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "-p".into(),
        prompt.into(),
        "--output-format".into(),
        "streaming-json".into(),
        // Skip Grok's plan-and-stop preamble so a single `-p` turn actually does
        // the work, and auto-approve tools so nothing blocks headlessly (matching
        // how `claude -p` executes). Headless worker role (`CONTEXT.md` §8).
        "--no-plan".into(),
        "--always-approve".into(),
    ];
    if let Some(m) = model {
        args.push("-m".into());
        args.push(m.into());
    }
    args
}

/// Lean planner: plain single-turn without streaming-json (more robust for short plans).
fn plan_args(prompt: &str, model: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "-p".into(),
        prompt.into(),
        "--output-format".into(),
        "plain".into(),
        "--no-plan".into(),
        "--always-approve".into(),
    ];
    if let Some(m) = model {
        args.push("-m".into());
        args.push(m.into());
    }
    args
}

/// Full Grok Build interactive UI (`grok "prompt"`), not headless `-p` JSON dump.
///
/// Keep the argv close to what a human would type so the product TUI behaves
/// normally. Auto-approve (unless approvals are `human`) + no-plan so Stage
/// sessions are not stuck on prompts.
/// Prefer `--fullscreen` for the alt-screen product chrome.
fn interactive_args(prompt: &str, model: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = vec!["--fullscreen".into()];
    // `[harness_stage].approvals = "human"` must reach every adapter, not just
    // the two that had a flag wired: an unattended Stage node auto-approves,
    // but a user who asked to be consulted still gets grok's own prompts.
    if approvals_are_auto() {
        args.push("--always-approve".into());
    }
    args.push("--no-plan".into());
    if let Some(m) = model {
        args.push("-m".into());
        args.push(m.into());
    }
    // Positional PROMPT → interactive session with initial message (see grok --help).
    args.push(prompt.into());
    args
}

/// Accumulate `type:text` token data into the result; read the session id from
/// the terminal `end` event. Falls back to a non-streaming `{"text":...}` shape.
///
/// Grok's streaming-json currently has **no usage field** on `end` (only
/// `stopReason` / `sessionId` / `requestId`). We therefore estimate completion
/// tokens from streamed thought+text payload length so the ledger is not 0.
fn parse(out: &SubprocessOutput) -> ParsedHarness {
    let mut result = String::new();
    let mut session_id = None;
    let mut thought = String::new();
    let mut reported = Usage::default();

    for line in out.stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(d) = v.get("data").and_then(|d| d.as_str()) {
                    result.push_str(d);
                }
            }
            Some("thought") => {
                if let Some(d) = v.get("data").and_then(|d| d.as_str()) {
                    thought.push_str(d);
                }
            }
            Some("end") => {
                session_id = v
                    .get("sessionId")
                    .or_else(|| v.get("session_id"))
                    .and_then(|s| s.as_str())
                    .map(String::from);
                // Future-proof: accept usage if Grok starts emitting it.
                if let Some(u) = v.get("usage") {
                    reported.prompt_tokens = u
                        .get("input_tokens")
                        .or_else(|| u.get("prompt_tokens"))
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0);
                    reported.completion_tokens = u
                        .get("output_tokens")
                        .or_else(|| u.get("completion_tokens"))
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0);
                }
            }
            _ => {
                // Non-streaming json: a single object with a `text` field.
                if result.is_empty() {
                    if let Some(t) = v.get("text").and_then(|t| t.as_str()) {
                        result = t.to_string();
                    }
                }
            }
        }
    }

    if result.is_empty() {
        return ParsedHarness::raw(&out.stdout);
    }

    let mut usage = reported;
    if usage.completion_tokens == 0 {
        // Estimate output from visible text + reasoning stream.
        let out_text = format!("{thought}{result}");
        usage.completion_tokens = Usage::estimate_tokens(&out_text);
    }

    ParsedHarness {
        result: result.trim().to_string(),
        session_id,
        usage,
        is_error: false,
    }
}

/// Stream text tokens, thinking, and tool uses live for Stage / TUI.
fn line_mapper(line: &str) -> Vec<WorkerEvent> {
    let line = line.trim();
    if line.is_empty() {
        return Vec::new();
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return Vec::new();
    };
    match v.get("type").and_then(|t| t.as_str()) {
        Some("text") => v
            .get("data")
            .and_then(|d| d.as_str())
            .map(|d| vec![WorkerEvent::Token(d.to_string())])
            .unwrap_or_default(),
        Some("thought") | Some("thinking") | Some("reasoning") => v
            .get("data")
            .and_then(|d| d.as_str())
            .map(|d| vec![WorkerEvent::Thinking(d.to_string())])
            .unwrap_or_default(),
        Some("tool_use") | Some("tool") | Some("tool_call") => {
            let name = v
                .get("name")
                .or_else(|| v.get("tool"))
                .and_then(|n| n.as_str())
                .unwrap_or("tool");
            let input = v
                .get("input")
                .or_else(|| v.get("arguments"))
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            vec![tool_event(name, &input)]
        }
        Some("error") => v
            .get("data")
            .or_else(|| v.get("message"))
            .and_then(|d| d.as_str())
            .map(|d| vec![WorkerEvent::Message(format!("error: {d}"))])
            .unwrap_or_default(),
        // end, system: not shown.
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::subprocess::SubprocessOutput;
    use rinne_core::worker::ExecStatus;

    #[test]
    fn parse_estimates_tokens_when_grok_omits_usage() {
        let stdout = r#"
{"type":"thought","data":"thinking hard about this"}
{"type":"text","data":"Hello world from Grok"}
{"type":"end","stopReason":"EndTurn","sessionId":"abc"}
"#;
        let out = SubprocessOutput {
            stdout: stdout.into(),
            stderr: String::new(),
            status: ExecStatus::Success,
            wall_ms: 100,
            exit_code: Some(0),
        };
        let p = parse(&out);
        assert!(p.result.contains("Hello world"));
        assert_eq!(p.session_id.as_deref(), Some("abc"));
        assert!(
            p.usage.completion_tokens > 0,
            "expected estimated completion tokens, got {}",
            p.usage.completion_tokens
        );
    }
}

fn tool_event(name: &str, input: &serde_json::Value) -> WorkerEvent {
    let s = |k: &str| {
        input
            .get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    match name {
        "Read" => WorkerEvent::Reading(s("file_path")),
        "Write" => WorkerEvent::Editing(format!("writing {}", s("file_path"))),
        "Edit" | "MultiEdit" => WorkerEvent::Editing(format!("editing {}", s("file_path"))),
        "Bash" => {
            let cmd = s("command");
            let desc = s("description");
            WorkerEvent::ToolUse(if desc.is_empty() { cmd } else { desc })
        }
        other => WorkerEvent::ToolUse(other.to_string()),
    }
}
