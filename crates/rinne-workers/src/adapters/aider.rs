//! Aider adapter (`CONTEXT.md` §16).
//!
//! Drives `aider --message "<prompt>" --yes-always`, which runs non-interactively,
//! edits files, and commits. Aider uses provider keys (metered) and prints plain
//! text rather than JSON, so the captured stdout is the result.

use std::time::Duration;

use rinne_core::worker::{
    AuthMode, Capability, LatencyProfile, QuotaModel, Transport, WorkerDescriptor, WorkerFamily,
};

use super::common::{parse_raw, HarnessAdapter};
use crate::transport::subprocess::raw_lines;

pub fn worker() -> HarnessAdapter {
    HarnessAdapter {
        descriptor: descriptor(),
        program: "aider".to_string(),
        build_args,
        plan_args: None,
        parse: parse_raw,
        line_mapper: raw_lines,
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
