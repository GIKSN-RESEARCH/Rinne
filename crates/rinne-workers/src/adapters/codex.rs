//! Codex CLI adapter (`CONTEXT.md` §16, `plan.md` Phase 3).
//!
//! Drives `codex exec --json`, which honors the user's ChatGPT login or API key.
//! `--json` emits a JSONL event stream (when supported); we parse terminal
//! agent messages into the result and map progress events for the Stage UI.
//! Falls back to plain stdout when the CLI does not speak JSONL.

use std::path::Path;
use std::time::Duration;

use rinne_core::worker::{
    AuthMode, Capability, LatencyProfile, McpServerSpec, QuotaModel, Transport, Usage,
    WorkerDescriptor, WorkerEvent, WorkerFamily,
};
use rinne_core::Result;

use super::common::{HarnessAdapter, ParsedHarness, Provision};
use super::mcp_util;
use crate::transport::subprocess::SubprocessOutput;

pub fn worker() -> HarnessAdapter {
    HarnessAdapter {
        descriptor: descriptor(),
        program: "codex".to_string(),
        build_args,
        plan_args: Some(plan_args),
        // Interactive Codex TUI (not `codex exec --json`).
        interactive_args: Some(interactive_args),
        parse,
        line_mapper,
        prompt_via_stdin: false,
        default_timeout: Duration::from_secs(600),
        provisioner: Some(provision),
    }
}

fn descriptor() -> WorkerDescriptor {
    WorkerDescriptor {
        name: "codex".to_string(),
        family: WorkerFamily::Harness,
        capabilities: vec![
            Capability::CodeEdit,
            Capability::RepoAware,
            Capability::Reasoning,
            Capability::ToolRun,
            Capability::Writing,
            Capability::CodeReview,
        ],
        auth_mode: AuthMode::Subscription,
        quota: QuotaModel {
            capacity: 150_000.0,
            refill_per_minute: 15_000.0,
        },
        latency: LatencyProfile::Medium,
        transport: Transport::SubprocessJson,
        // Discover fills live ladder; cheap→strong defaults for ChatGPT/Codex.
        // Keep this list to models a ChatGPT-account login can actually run —
        // Codex rejects o-series with HTTP 400 there, which fails the node
        // rather than degrading, so an unusable rung is worse than a short ladder.
        models: vec!["gpt-5-mini".into(), "gpt-5".into(), "o4-mini".into()],
    }
}

/// Work invocation: JSONL stream + non-interactive approval when the CLI allows.
fn build_args(prompt: &str, model: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "exec".into(),
        // Structured event stream for Stage + parser (ignored by older CLIs →
        // they still run; we fall back to raw parse).
        "--json".into(),
        // Auto-approve tool use for headless/PTY orchestration (harness power
        // without blocking on interactive prompts).
        "--full-auto".into(),
    ];
    if let Some(m) = model {
        args.push("--model".into());
        args.push(m.into());
    }
    args.push(prompt.into());
    args
}

/// Lean planner path: plain exec without JSON flags (more robust for short plans).
fn plan_args(prompt: &str, model: Option<&str>) -> Vec<String> {
    let mut args = vec!["exec".into(), "--full-auto".into()];
    if let Some(m) = model {
        args.push("--model".into());
        args.push(m.into());
    }
    args.push(prompt.into());
    args
}

/// Full interactive Codex UI (no `exec` / `--json`).
fn interactive_args(prompt: &str, model: Option<&str>) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(m) = model {
        args.push("--model".into());
        args.push(m.into());
    }
    args.push(prompt.into());
    args
}

/// Parse Codex JSONL (when present) or fall back to the last non-empty text block.
fn parse(out: &SubprocessOutput) -> ParsedHarness {
    let mut result = String::new();
    let mut session_id = None;
    let mut usage = Usage::default();
    let mut saw_json = false;
    let mut is_error = false;

    for line in out.stdout.lines() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        saw_json = true;

        // Event type field varies: `type`, `msg.type`, `item.type`.
        let ty = v
            .get("type")
            .or_else(|| v.pointer("/msg/type"))
            .or_else(|| v.pointer("/item/type"))
            .and_then(|t| t.as_str())
            .unwrap_or("");

        match ty {
            "agent_message" | "message" | "assistant_message" | "agent_message_delta" => {
                if let Some(text) = pick_text(&v) {
                    if ty.ends_with("_delta") {
                        result.push_str(&text);
                    } else if text.len() >= result.len() {
                        result = text;
                    }
                }
            }
            "item.completed" | "turn.completed" | "task_complete" | "completed" => {
                if let Some(text) = pick_text(&v) {
                    if !text.is_empty() {
                        result = text;
                    }
                }
                if let Some(u) = v.get("usage").or_else(|| v.pointer("/msg/usage")) {
                    fill_usage(&mut usage, u);
                }
            }
            "error" | "turn.failed" | "task_failed" => {
                is_error = true;
                if let Some(text) = pick_text(&v) {
                    result = text;
                }
            }
            "session_created" | "thread.started" => {
                session_id = v
                    .get("session_id")
                    .or_else(|| v.get("thread_id"))
                    .or_else(|| v.pointer("/msg/session_id"))
                    .and_then(|s| s.as_str())
                    .map(String::from)
                    .or(session_id);
            }
            _ => {
                // Generic: last substantial text field wins.
                if let Some(text) = pick_text(&v) {
                    if text.len() > result.len() {
                        result = text;
                    }
                }
                if let Some(u) = v.get("usage") {
                    fill_usage(&mut usage, u);
                }
            }
        }
    }

    if !result.trim().is_empty() {
        return ParsedHarness {
            result: result.trim().to_string(),
            session_id,
            usage,
            is_error,
        };
    }

    if saw_json {
        // JSONL present but no text — still return raw for debugging.
        return ParsedHarness::raw(&out.stdout);
    }

    // Plain text path: take the last non-empty paragraph as the answer.
    let plain = last_paragraph(&out.stdout);
    if plain.is_empty() {
        ParsedHarness::raw(&out.stdout)
    } else {
        ParsedHarness {
            result: plain,
            session_id: None,
            usage: Usage::default(),
            is_error: !matches!(out.status, rinne_core::worker::ExecStatus::Success),
        }
    }
}

fn pick_text(v: &serde_json::Value) -> Option<String> {
    const KEYS: &[&str] = &[
        "text",
        "message",
        "content",
        "result",
        "output",
        "agent_message",
        "final_message",
    ];
    for k in KEYS {
        if let Some(s) = v.get(*k).and_then(|x| x.as_str()) {
            let t = s.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
        if let Some(s) = v.pointer(&format!("/msg/{k}")).and_then(|x| x.as_str()) {
            let t = s.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
        if let Some(s) = v.pointer(&format!("/item/{k}")).and_then(|x| x.as_str()) {
            let t = s.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    // content array (OpenAI-style)
    if let Some(arr) = v
        .get("content")
        .or_else(|| v.pointer("/msg/content"))
        .and_then(|c| c.as_array())
    {
        let mut acc = String::new();
        for block in arr {
            if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                acc.push_str(t);
            }
        }
        if !acc.trim().is_empty() {
            return Some(acc);
        }
    }
    None
}

fn fill_usage(usage: &mut Usage, u: &serde_json::Value) {
    if let Some(n) = u
        .get("input_tokens")
        .or_else(|| u.get("prompt_tokens"))
        .and_then(|x| x.as_u64())
    {
        usage.prompt_tokens = n;
    }
    if let Some(n) = u
        .get("output_tokens")
        .or_else(|| u.get("completion_tokens"))
        .and_then(|x| x.as_u64())
    {
        usage.completion_tokens = n;
    }
}

fn last_paragraph(stdout: &str) -> String {
    let mut blocks: Vec<&str> = stdout
        .split("\n\n")
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .collect();
    // Prefer the last block that doesn't look like a log line.
    while let Some(b) = blocks.last() {
        if b.starts_with("INFO") || b.starts_with("DEBUG") || b.starts_with("WARN") {
            blocks.pop();
            continue;
        }
        break;
    }
    blocks.last().copied().unwrap_or("").to_string()
}

/// Map Codex JSONL or plain progress lines into Stage-friendly events.
fn line_mapper(line: &str) -> Vec<WorkerEvent> {
    let t = line.trim();
    if t.is_empty() {
        return Vec::new();
    }

    if t.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(t) {
            return map_json_event(&v);
        }
    }

    // Plain-text heuristics for older non-JSON codex builds.
    let lower = t.to_ascii_lowercase();
    if lower.contains("reading ") || lower.starts_with("read ") {
        return vec![WorkerEvent::Reading(t.to_string())];
    }
    if lower.contains("writing ")
        || lower.contains("edited ")
        || lower.contains("applying patch")
        || lower.contains("applied edit")
    {
        return vec![WorkerEvent::Editing(t.to_string())];
    }
    if lower.contains("running ") || lower.starts_with("$ ") || lower.contains("command:") {
        return vec![WorkerEvent::ToolUse(t.to_string())];
    }
    vec![WorkerEvent::Message(t.to_string())]
}

fn map_json_event(v: &serde_json::Value) -> Vec<WorkerEvent> {
    let ty = v
        .get("type")
        .or_else(|| v.pointer("/msg/type"))
        .or_else(|| v.pointer("/item/type"))
        .and_then(|t| t.as_str())
        .unwrap_or("");

    match ty {
        "agent_message" | "message" | "assistant_message" => pick_text(v)
            .map(|t| vec![WorkerEvent::Message(t)])
            .unwrap_or_default(),
        "agent_message_delta" | "message_delta" => pick_text(v)
            .map(|t| vec![WorkerEvent::Token(t)])
            .unwrap_or_default(),
        "reasoning" | "thought" | "thinking" => pick_text(v)
            .map(|t| vec![WorkerEvent::Thinking(t)])
            .unwrap_or_default(),
        "command_execution" | "exec_command" | "shell" | "command" => {
            let cmd = v
                .get("command")
                .or_else(|| v.pointer("/item/command"))
                .or_else(|| v.pointer("/msg/command"))
                .and_then(|c| c.as_str())
                .unwrap_or("command");
            vec![WorkerEvent::ToolUse(truncate(cmd, 100))]
        }
        "file_change" | "patch" | "apply_patch" | "file_edit" => {
            let path = v
                .get("path")
                .or_else(|| v.get("file"))
                .or_else(|| v.pointer("/item/path"))
                .and_then(|p| p.as_str())
                .unwrap_or("file");
            vec![WorkerEvent::Editing(format!("editing {path}"))]
        }
        "file_read" | "read" => {
            let path = v
                .get("path")
                .or_else(|| v.get("file"))
                .and_then(|p| p.as_str())
                .unwrap_or("file");
            vec![WorkerEvent::Reading(path.to_string())]
        }
        "tool" | "tool_call" | "mcp_tool_call" => {
            let name = v
                .get("name")
                .or_else(|| v.get("tool"))
                .and_then(|n| n.as_str())
                .unwrap_or("tool");
            vec![WorkerEvent::ToolUse(name.to_string())]
        }
        "error" | "turn.failed" => pick_text(v)
            .map(|t| vec![WorkerEvent::Message(format!("error: {t}"))])
            .unwrap_or_default(),
        // session / completed / noise
        _ => Vec::new(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}

fn provision(servers: &[McpServerSpec], scratch: &Path) -> Result<Provision> {
    mcp_util::provision_codex_style(servers, scratch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rinne_core::worker::ExecStatus;

    #[test]
    fn default_ladder_only_holds_models_a_chatgpt_account_can_run() {
        // Codex signed in with a ChatGPT account rejects o-series models with
        // "The 'o3' model is not supported when using Codex with a ChatGPT
        // account" (HTTP 400), so routing to that rung fails the whole node.
        let models = descriptor().models;
        assert!(
            !models.iter().any(|m| m == "o3"),
            "o3 is not runnable on a ChatGPT account: {models:?}"
        );
        assert!(models.iter().any(|m| m == "gpt-5"), "{models:?}");
    }

    fn out(stdout: &str) -> SubprocessOutput {
        SubprocessOutput {
            stdout: stdout.into(),
            stderr: String::new(),
            exit_code: Some(0),
            status: ExecStatus::Success,
            wall_ms: 10,
        }
    }

    #[test]
    fn parses_jsonl_agent_message() {
        let stdout = r#"
{"type":"session_created","session_id":"s1"}
{"type":"agent_message","text":"Done: fixed the bug."}
{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":5}}
"#;
        let p = parse(&out(stdout));
        assert!(p.result.contains("fixed the bug"));
        assert_eq!(p.session_id.as_deref(), Some("s1"));
        assert_eq!(p.usage.prompt_tokens, 10);
        assert_eq!(p.usage.completion_tokens, 5);
    }

    #[test]
    fn plain_text_fallback() {
        let p = parse(&out("INFO start\n\nAll tests passed.\n"));
        assert!(p.result.contains("All tests passed"));
    }

    #[test]
    fn line_mapper_maps_tool_events() {
        let evs = line_mapper(r#"{"type":"file_change","path":"src/main.rs"}"#);
        assert!(matches!(evs.first(), Some(WorkerEvent::Editing(_))));
        let evs = line_mapper("Applying patch to foo.rs");
        assert!(matches!(evs.first(), Some(WorkerEvent::Editing(_))));
    }
}
