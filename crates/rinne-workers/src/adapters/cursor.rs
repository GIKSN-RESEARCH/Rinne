//! Cursor CLI adapter (`CONTEXT.md` §16).
//!
//! Drives `cursor-agent -p --output-format json --force`, honoring the Cursor
//! subscription. Cursor's `-p` is known to hang, so the transport's timeout is
//! the guard. Output is parsed defensively (`CONTEXT.md` §21).

use std::time::Duration;

use rinne_core::worker::{
    AuthMode, Capability, LatencyProfile, QuotaModel, Transport, WorkerDescriptor, WorkerFamily,
};

use rinne_core::worker::WorkerEvent;

use super::common::{parse_generic_json, HarnessAdapter};

pub fn worker() -> HarnessAdapter {
    HarnessAdapter {
        descriptor: descriptor(),
        program: "cursor-agent".to_string(),
        build_args,
        plan_args: Some(plan_args),
        interactive_args: Some(interactive_args),
        parse: parse_generic_json,
        line_mapper,
        prompt_via_stdin: false,
        default_timeout: Duration::from_secs(600),
        provisioner: None,
    }
}

fn descriptor() -> WorkerDescriptor {
    WorkerDescriptor {
        name: "cursor-agent".to_string(),
        family: WorkerFamily::Harness,
        capabilities: vec![
            Capability::CodeEdit,
            Capability::RepoAware,
            Capability::Reasoning,
            Capability::Writing,
            Capability::ToolRun,
            Capability::CodeReview,
            Capability::LongContext,
        ],
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
        "-p".into(),
        prompt.into(),
        "--output-format".into(),
        "json".into(),
        "--force".into(),
    ];
    if let Some(m) = model {
        args.push("--model".into());
        args.push(m.into());
    }
    args
}

/// Lean planner: still force non-interactive, JSON optional.
fn plan_args(prompt: &str, model: Option<&str>) -> Vec<String> {
    let mut args = vec!["-p".into(), prompt.into(), "--force".into()];
    if let Some(m) = model {
        args.push("--model".into());
        args.push(m.into());
    }
    args
}

fn interactive_args(prompt: &str, model: Option<&str>) -> Vec<String> {
    // No -p / --output-format json — interactive agent UI.
    let mut args = Vec::new();
    if let Some(m) = model {
        args.push("--model".into());
        args.push(m.into());
    }
    args.push(prompt.into());
    args
}

fn line_mapper(line: &str) -> Vec<WorkerEvent> {
    let t = line.trim();
    if t.is_empty() {
        return Vec::new();
    }
    if t.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(t) {
            if let Some(ty) = v.get("type").and_then(|t| t.as_str()) {
                match ty {
                    "tool_use" | "tool" => {
                        let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                        return vec![WorkerEvent::ToolUse(name.into())];
                    }
                    "text" | "message" => {
                        if let Some(text) = v
                            .get("text")
                            .or_else(|| v.get("content"))
                            .and_then(|x| x.as_str())
                        {
                            return vec![WorkerEvent::Message(text.into())];
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    let lower = t.to_ascii_lowercase();
    if lower.contains("read") && (lower.contains('/') || lower.contains('.')) {
        return vec![WorkerEvent::Reading(t.into())];
    }
    if lower.contains("edit") || lower.contains("write") {
        return vec![WorkerEvent::Editing(t.into())];
    }
    vec![WorkerEvent::Raw(t.into())]
}
