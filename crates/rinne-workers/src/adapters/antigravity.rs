//! Antigravity adapter (`CONTEXT.md` §16).
//!
//! Drives `agy --print "<prompt>"` (replaces the Gemini CLI for free users),
//! honoring the Google OAuth login. Output is parsed defensively.

use std::time::Duration;

use rinne_core::worker::{
    AuthMode, Capability, LatencyProfile, QuotaModel, Transport, WorkerDescriptor, WorkerFamily,
};

use super::common::{parse_generic_json, HarnessAdapter};
use crate::transport::subprocess::raw_lines;

pub fn worker() -> HarnessAdapter {
    HarnessAdapter {
        descriptor: descriptor(),
        program: "agy".to_string(),
        build_args,
        plan_args: None,
        parse: parse_generic_json,
        line_mapper: raw_lines,
        prompt_via_stdin: false,
        default_timeout: Duration::from_secs(600),
    }
}

fn descriptor() -> WorkerDescriptor {
    WorkerDescriptor {
        name: "antigravity".to_string(),
        family: WorkerFamily::Harness,
        capabilities: vec![
            Capability::CodeEdit,
            Capability::RepoAware,
            Capability::Reasoning,
            Capability::Writing,
            Capability::LongContext,
        ],
        auth_mode: AuthMode::Subscription,
        quota: QuotaModel {
            capacity: 100_000.0,
            refill_per_minute: 10_000.0,
        },
        latency: LatencyProfile::Medium,
        transport: Transport::SubprocessJson,
        models: Vec::new(),
    }
}

fn build_args(prompt: &str, model: Option<&str>) -> Vec<String> {
    let mut args = vec!["--print".into(), prompt.into()];
    if let Some(m) = model {
        args.push("--model".into());
        args.push(m.into());
    }
    args
}
