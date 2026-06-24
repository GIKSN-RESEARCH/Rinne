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

use super::common::{HarnessAdapter, ParsedHarness};
use crate::transport::subprocess::SubprocessOutput;

pub fn worker() -> HarnessAdapter {
    HarnessAdapter {
        descriptor: descriptor(),
        program: "grok".to_string(),
        build_args,
        parse,
        line_mapper,
        prompt_via_stdin: false,
        default_timeout: Duration::from_secs(600),
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
        models: vec!["grok-composer-2.5-fast".into(), "grok-build".into()],
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

/// Accumulate `type:text` token data into the result; read the session id from
/// the terminal `end` event. Falls back to a non-streaming `{"text":...}` shape.
fn parse(out: &SubprocessOutput) -> ParsedHarness {
    let mut result = String::new();
    let mut session_id = None;

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
            Some("end") => {
                session_id = v.get("sessionId").and_then(|s| s.as_str()).map(String::from);
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
    ParsedHarness {
        result: result.trim().to_string(),
        session_id,
        usage: Usage::default(),
        is_error: false,
    }
}

/// Stream text tokens and tool uses live; suppress reasoning tokens and the end
/// marker. Text tokens are coalesced into one growing line by the interface.
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
            .map(|d| vec![WorkerEvent::Message(d.to_string())])
            .unwrap_or_default(),
        Some("tool_use") | Some("tool") => {
            let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
            let input = v.get("input").cloned().unwrap_or(serde_json::Value::Null);
            vec![tool_event(name, &input)]
        }
        // thought, end, system: not shown.
        _ => Vec::new(),
    }
}

fn tool_event(name: &str, input: &serde_json::Value) -> WorkerEvent {
    let s = |k: &str| input.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
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
