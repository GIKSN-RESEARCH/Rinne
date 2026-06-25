//! OpenCode adapter (`CONTEXT.md` §16).
//!
//! Drives `opencode run --format json`, honoring the provider config the user
//! set up in OpenCode. Parses the JSON result defensively, falling back to raw
//! stdout on any schema surprise (`CONTEXT.md` §21).

use std::time::Duration;

use rinne_core::worker::{
    AuthMode, Capability, LatencyProfile, QuotaModel, Transport, Usage, WorkerDescriptor,
    WorkerEvent, WorkerFamily,
};

use super::claude_code::last_json_object;
use super::common::{HarnessAdapter, ParsedHarness};
use crate::transport::subprocess::SubprocessOutput;

pub fn worker() -> HarnessAdapter {
    HarnessAdapter {
        descriptor: descriptor(),
        program: "opencode".to_string(),
        build_args,
        plan_args: None,
        parse,
        line_mapper,
        prompt_via_stdin: false,
        default_timeout: Duration::from_secs(600),
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
    let mut args = vec!["run".into(), prompt.into(), "--format".into(), "json".into()];
    if let Some(m) = model {
        // OpenCode takes `provider/model`; pass through as given.
        args.push("--model".into());
        args.push(m.into());
    }
    args
}

fn parse(out: &SubprocessOutput) -> ParsedHarness {
    let Some(value) = last_json_object(&out.stdout) else {
        return ParsedHarness::raw(&out.stdout);
    };

    // OpenCode JSON varies by version; probe a few likely fields, else raw.
    let result = value
        .get("result")
        .or_else(|| value.get("text"))
        .or_else(|| value.get("message"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| out.stdout.trim().to_string());

    let session_id = value
        .get("session_id")
        .or_else(|| value.get("sessionID"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    ParsedHarness {
        result,
        session_id,
        usage: Usage::default(),
        is_error: false,
    }
}

fn line_mapper(line: &str) -> Vec<WorkerEvent> {
    let t = line.trim();
    if t.is_empty() {
        Vec::new()
    } else {
        vec![WorkerEvent::Raw(t.to_string())]
    }
}
