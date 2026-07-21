//! OpenCode adapter (`CONTEXT.md` §16, `plan.md` Phase 3).
//!
//! Drives `opencode run --format json`, honoring the provider config the user
//! set up in OpenCode. Parses the JSON result defensively (usage, errors,
//! session id), streams NDJSON progress when present, and best-effort MCP via
//! env-pointed config (`mcp_util`).

use std::path::Path;
use std::time::Duration;

use rinne_core::worker::{
    AuthMode, Capability, LatencyProfile, McpServerSpec, QuotaModel, Transport, Usage,
    WorkerDescriptor, WorkerEvent, WorkerFamily,
};
use rinne_core::Result;

use super::claude_code::last_json_object;
use super::common::{HarnessAdapter, ParsedHarness, Provision};
use super::mcp_util;
use crate::transport::subprocess::SubprocessOutput;

pub fn worker() -> HarnessAdapter {
    HarnessAdapter {
        descriptor: descriptor(),
        program: "opencode".to_string(),
        build_args,
        plan_args: Some(plan_args),
        // Interactive OpenCode UI (not `run --format json`).
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
        name: "opencode".to_string(),
        family: WorkerFamily::Harness,
        capabilities: vec![
            Capability::CodeEdit,
            Capability::RepoAware,
            Capability::Reasoning,
            Capability::Writing,
            Capability::ToolRun,
            Capability::CodeReview,
        ],
        // Auth depends on the provider config; treated as subscription unless a
        // metered provider is set. `doctor` reports the effective mode.
        auth_mode: AuthMode::Subscription,
        quota: QuotaModel {
            capacity: 150_000.0,
            refill_per_minute: 15_000.0,
        },
        latency: LatencyProfile::Medium,
        transport: Transport::SubprocessJson,
        models: Vec::new(),
    }
}

fn build_args(prompt: &str, model: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "run".into(),
        prompt.into(),
        "--format".into(),
        "json".into(),
    ];
    if let Some(m) = model {
        // OpenCode takes `provider/model`; pass through as given.
        args.push("--model".into());
        args.push(m.into());
    }
    args
}

/// Lean planner: plain run without requiring JSON (more tolerant for short answers).
fn plan_args(prompt: &str, model: Option<&str>) -> Vec<String> {
    let mut args = vec!["run".into(), prompt.into()];
    if let Some(m) = model {
        args.push("--model".into());
        args.push(m.into());
    }
    args
}

/// Interactive OpenCode session (no `run --format json`).
fn interactive_args(prompt: &str, model: Option<&str>) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(m) = model {
        args.push("--model".into());
        args.push(m.into());
    }
    args.push(prompt.into());
    args
}

fn parse(out: &SubprocessOutput) -> ParsedHarness {
    // Prefer last full JSON object (final result), then scan NDJSON for parts.
    if let Some(value) = last_json_object(&out.stdout) {
        return parse_value(&value, out);
    }

    // NDJSON: accumulate text tokens / last message.
    let mut result = String::new();
    let mut session_id = None;
    let mut usage = Usage::default();
    let mut is_error = false;
    let mut saw = false;

    for line in out.stdout.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        saw = true;
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match ty {
            "text" | "message" | "assistant" | "result" => {
                let t = text_from(&v).or_else(|| {
                    v.get("data")
                        .and_then(|d| d.as_str())
                        .map(|s| s.to_string())
                });
                if let Some(t) = t {
                    if ty == "text" {
                        result.push_str(&t);
                    } else {
                        result = t;
                    }
                }
            }
            "error" => {
                is_error = true;
                if let Some(t) = text_from(&v) {
                    result = t;
                }
            }
            "done" | "end" | "complete" => {
                session_id = v
                    .get("sessionID")
                    .or_else(|| v.get("session_id"))
                    .or_else(|| v.get("sessionId"))
                    .and_then(|s| s.as_str())
                    .map(String::from)
                    .or(session_id);
                if let Some(u) = v.get("usage") {
                    fill_usage(&mut usage, u);
                }
            }
            _ => {
                if let Some(t) = text_from(&v) {
                    if t.len() > result.len() {
                        result = t;
                    }
                }
            }
        }
        if let Some(u) = v.get("usage") {
            fill_usage(&mut usage, u);
        }
        session_id = v
            .get("session_id")
            .or_else(|| v.get("sessionID"))
            .and_then(|s| s.as_str())
            .map(String::from)
            .or(session_id);
    }

    if !result.trim().is_empty() {
        return ParsedHarness {
            result: result.trim().to_string(),
            session_id,
            usage,
            is_error,
        };
    }
    if saw {
        return ParsedHarness::raw(&out.stdout);
    }
    ParsedHarness::raw(&out.stdout)
}

fn parse_value(value: &serde_json::Value, out: &SubprocessOutput) -> ParsedHarness {
    let result = text_from(value).unwrap_or_else(|| out.stdout.trim().to_string());

    let session_id = value
        .get("session_id")
        .or_else(|| value.get("sessionID"))
        .or_else(|| value.get("sessionId"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let mut usage = Usage::default();
    if let Some(u) = value.get("usage") {
        fill_usage(&mut usage, u);
    }

    let is_error = value
        .get("error")
        .map(|e| !e.is_null())
        .or_else(|| value.get("is_error").and_then(|b| b.as_bool()))
        .or_else(|| value.get("ok").and_then(|b| b.as_bool().map(|ok| !ok)))
        .unwrap_or(false);

    ParsedHarness {
        result,
        session_id,
        usage,
        is_error,
    }
}

fn text_from(v: &serde_json::Value) -> Option<String> {
    const KEYS: &[&str] = &[
        "result", "text", "message", "content", "output", "response", "data",
    ];
    for k in KEYS {
        if let Some(s) = v.get(*k).and_then(|x| x.as_str()) {
            let t = s.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    if let Some(arr) = v.get("content").and_then(|c| c.as_array()) {
        let mut acc = String::new();
        for block in arr {
            if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                acc.push_str(t);
            } else if let Some(t) = block.as_str() {
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
        .or_else(|| u.get("input"))
        .and_then(|x| x.as_u64())
    {
        usage.prompt_tokens = n;
    }
    if let Some(n) = u
        .get("output_tokens")
        .or_else(|| u.get("completion_tokens"))
        .or_else(|| u.get("output"))
        .and_then(|x| x.as_u64())
    {
        usage.completion_tokens = n;
    }
}

fn line_mapper(line: &str) -> Vec<WorkerEvent> {
    let t = line.trim();
    if t.is_empty() {
        return Vec::new();
    }
    if t.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(t) {
            return map_json(&v);
        }
    }
    // Non-JSON progress
    let lower = t.to_ascii_lowercase();
    if lower.contains("read ") || lower.contains("reading ") {
        return vec![WorkerEvent::Reading(t.to_string())];
    }
    if lower.contains("write ") || lower.contains("edit ") || lower.contains("wrote ") {
        return vec![WorkerEvent::Editing(t.to_string())];
    }
    vec![WorkerEvent::Raw(t.to_string())]
}

fn map_json(v: &serde_json::Value) -> Vec<WorkerEvent> {
    let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        "text" | "token" => text_from(v)
            .or_else(|| v.get("data").and_then(|d| d.as_str()).map(String::from))
            .map(|t| vec![WorkerEvent::Token(t)])
            .unwrap_or_default(),
        "message" | "assistant" | "result" => text_from(v)
            .map(|t| vec![WorkerEvent::Message(t)])
            .unwrap_or_default(),
        "thinking" | "thought" | "reasoning" => text_from(v)
            .or_else(|| v.get("data").and_then(|d| d.as_str()).map(String::from))
            .map(|t| vec![WorkerEvent::Thinking(t)])
            .unwrap_or_default(),
        "tool" | "tool_use" | "tool_call" => {
            let name = v
                .get("name")
                .or_else(|| v.get("tool"))
                .and_then(|n| n.as_str())
                .unwrap_or("tool");
            let input = v.get("input").cloned().unwrap_or(serde_json::Value::Null);
            vec![tool_event(name, &input)]
        }
        "file" | "read" => {
            let p = v
                .get("path")
                .or_else(|| v.get("file"))
                .and_then(|p| p.as_str())
                .unwrap_or("file");
            vec![WorkerEvent::Reading(p.into())]
        }
        "write" | "edit" => {
            let p = v
                .get("path")
                .or_else(|| v.get("file"))
                .and_then(|p| p.as_str())
                .unwrap_or("file");
            vec![WorkerEvent::Editing(format!("editing {p}"))]
        }
        "error" => text_from(v)
            .map(|t| vec![WorkerEvent::Message(format!("error: {t}"))])
            .unwrap_or_default(),
        _ => Vec::new(),
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
    let path = {
        let p = s("path");
        if p.is_empty() {
            s("file_path")
        } else {
            p
        }
    };
    match name {
        "Read" | "read" => WorkerEvent::Reading(path),
        "Write" | "write" => WorkerEvent::Editing(format!("writing {path}")),
        "Edit" | "edit" => WorkerEvent::Editing(format!("editing {path}")),
        "Bash" | "bash" | "shell" => {
            let cmd = s("command");
            WorkerEvent::ToolUse(if cmd.is_empty() { name.into() } else { cmd })
        }
        other => WorkerEvent::ToolUse(other.to_string()),
    }
}

fn provision(servers: &[McpServerSpec], scratch: &Path) -> Result<Provision> {
    mcp_util::provision_opencode_style(servers, scratch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rinne_core::worker::ExecStatus;

    fn out(stdout: &str) -> SubprocessOutput {
        SubprocessOutput {
            stdout: stdout.into(),
            stderr: String::new(),
            exit_code: Some(0),
            status: ExecStatus::Success,
            wall_ms: 5,
        }
    }

    #[test]
    fn parses_result_object() {
        let json = r#"{"result":"all green","sessionID":"oc1","usage":{"input_tokens":3,"output_tokens":2}}"#;
        let p = parse(&out(json));
        assert_eq!(p.result, "all green");
        assert_eq!(p.session_id.as_deref(), Some("oc1"));
        assert_eq!(p.usage.prompt_tokens, 3);
        assert_eq!(p.usage.completion_tokens, 2);
    }

    #[test]
    fn parses_ndjson_text_stream() {
        let stdout = r#"
{"type":"text","data":"Hello "}
{"type":"text","data":"world"}
{"type":"done","session_id":"s2"}
"#;
        let p = parse(&out(stdout));
        assert!(p.result.contains("Hello"), "got {:?}", p.result);
        assert!(p.result.contains("world"), "got {:?}", p.result);
        assert_eq!(p.session_id.as_deref(), Some("s2"));
    }

    #[test]
    fn line_mapper_tool() {
        let evs = line_mapper(r#"{"type":"tool","name":"Read","input":{"path":"a.rs"}}"#);
        assert!(matches!(evs.first(), Some(WorkerEvent::Reading(_))));
    }
}
