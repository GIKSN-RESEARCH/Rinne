//! Codex CLI adapter (`CONTEXT.md` §16).
//!
//! Drives `codex exec`, which honors the user's ChatGPT login or API key per
//! their setup. `codex exec` streams progress to stdout and prints the final
//! message; we capture stdout as the result and stream lines as events.

use std::time::Duration;

use rinne_core::worker::{
    AuthMode, Capability, LatencyProfile, QuotaModel, Transport, WorkerDescriptor, WorkerEvent,
    WorkerFamily,
};

use super::common::{HarnessAdapter, ParsedHarness};
use crate::transport::subprocess::SubprocessOutput;

pub fn worker() -> HarnessAdapter {
    HarnessAdapter {
        descriptor: descriptor(),
        program: "codex".to_string(),
        build_args,
        plan_args: None,
        parse,
        line_mapper,
        prompt_via_stdin: false,
        default_timeout: Duration::from_secs(600),
        provisioner: None,
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
    let mut args = vec!["exec".into()];
    if let Some(m) = model {
        args.push("--model".into());
        args.push(m.into());
    }
    args.push(prompt.into());
    args
}

/// `codex exec` is line/stream oriented rather than a single JSON object, so we
/// take the captured stdout as the result text.
fn parse(out: &SubprocessOutput) -> ParsedHarness {
    ParsedHarness::raw(&out.stdout)
}

fn line_mapper(line: &str) -> Vec<WorkerEvent> {
    let t = line.trim();
    if t.is_empty() {
        Vec::new()
    } else {
        vec![WorkerEvent::Message(t.to_string())]
    }
}
