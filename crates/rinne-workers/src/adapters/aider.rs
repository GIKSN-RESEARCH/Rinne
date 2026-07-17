//! Aider adapter (`CONTEXT.md` §16).
//!
//! Drives `aider --message "<prompt>" --yes-always`, which runs non-interactively,
//! edits files, and commits. Aider uses provider keys (metered) and prints plain
//! text rather than JSON, so the captured stdout is the result.

use std::time::Duration;

use rinne_core::worker::{
    AuthMode, Capability, LatencyProfile, QuotaModel, Transport, WorkerDescriptor, WorkerEvent,
    WorkerFamily,
};

use super::common::{parse_raw, HarnessAdapter};

pub fn worker() -> HarnessAdapter {
    HarnessAdapter {
        descriptor: descriptor(),
        program: "aider".to_string(),
        build_args,
        plan_args: Some(plan_args),
        interactive_args: Some(interactive_args),
        parse: parse_raw,
        line_mapper,
        prompt_via_stdin: false,
        default_timeout: Duration::from_secs(600),
        provisioner: None,
    }
}

fn descriptor() -> WorkerDescriptor {
    WorkerDescriptor {
        name: "aider".to_string(),
        family: WorkerFamily::Harness,
        capabilities: vec![
            Capability::CodeEdit,
            Capability::RepoAware,
            Capability::Writing,
        ],
        // Aider uses the user's provider keys — metered.
        auth_mode: AuthMode::ApiKey,
        quota: QuotaModel::unlimited(),
        latency: LatencyProfile::Medium,
        transport: Transport::SubprocessJson,
        models: Vec::new(),
    }
}

fn build_args(prompt: &str, model: Option<&str>) -> Vec<String> {
    let mut args = vec!["--message".into(), prompt.into(), "--yes-always".into()];
    if let Some(m) = model {
        args.push("--model".into());
        args.push(m.into());
    }
    args
}

fn plan_args(prompt: &str, model: Option<&str>) -> Vec<String> {
    // Same non-interactive surface; planner doesn't need extra flags.
    build_args(prompt, model)
}

fn interactive_args(prompt: &str, model: Option<&str>) -> Vec<String> {
    // Interactive aider chat (no --yes-always / --message one-shot).
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
    let lower = t.to_ascii_lowercase();
    if lower.contains("applied edit")
        || lower.contains("wrote ")
        || lower.starts_with("edit ")
        || lower.contains("committing ")
    {
        return vec![WorkerEvent::Editing(t.into())];
    }
    if lower.contains("reading ") || lower.starts_with("read ") {
        return vec![WorkerEvent::Reading(t.into())];
    }
    if lower.starts_with("run ") || lower.contains("running ") {
        return vec![WorkerEvent::ToolUse(t.into())];
    }
    vec![WorkerEvent::Message(t.into())]
}
