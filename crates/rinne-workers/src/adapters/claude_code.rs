//! Claude Code adapter (`CONTEXT.md` §9, §16).
//!
//! Drives the native `claude -p --output-format json` call, which honors the
//! Pro/Max subscription login — never the ACP adapter, which would force an
//! Anthropic API key (`CONTEXT.md` §9). The footgun guard for a stray
//! `ANTHROPIC_API_KEY` lives in `doctor` (Phase 1).

use std::time::Duration;

use rinne_core::worker::{
    AuthMode, Capability, LatencyProfile, QuotaModel, Transport, Usage, WorkerDescriptor,
    WorkerEvent, WorkerFamily,
};

use super::common::{HarnessAdapter, ParsedHarness};
use crate::transport::subprocess::SubprocessOutput;

/// Construct a Claude Code harness worker.
pub fn worker() -> HarnessAdapter {
    HarnessAdapter {
        descriptor: descriptor(),
        program: "claude".to_string(),
        build_args,
        parse,
        line_mapper,
        prompt_via_stdin: false,
        default_timeout: Duration::from_secs(600),
    }
}

fn descriptor() -> WorkerDescriptor {
    WorkerDescriptor {
        name: "claude-code".to_string(),
        family: WorkerFamily::Harness,
        capabilities: vec![
            Capability::CodeEdit,
            Capability::RepoAware,
            Capability::CodeReview,
            Capability::Reasoning,
            Capability::LongContext,
            Capability::Writing,
            Capability::ToolRun,
        ],
        auth_mode: AuthMode::Subscription,
        // Conservative subscription rate-limit window; tuned by live quota state
        // in the scheduler (Phase 3).
        quota: QuotaModel {
            capacity: 200_000.0,
            refill_per_minute: 20_000.0,
        },
        latency: LatencyProfile::Medium,
        transport: Transport::SubprocessJson,
    }
}

fn build_args(prompt: &str) -> Vec<String> {
    vec![
        "-p".into(),
        prompt.into(),
        "--output-format".into(),
        "json".into(),
    ]
}

/// Parse Claude Code's `--output-format json` result, defensively.
fn parse(out: &SubprocessOutput) -> ParsedHarness {
    let Some(value) = last_json_object(&out.stdout) else {
        return ParsedHarness::raw(&out.stdout);
    };

    let result = value
        .get("result")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| out.stdout.trim().to_string());

    let session_id = value
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let is_error = value
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let usage = value
        .get("usage")
        .map(|u| Usage {
            prompt_tokens: u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
            completion_tokens: u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
            wall_ms: 0,
        })
        .unwrap_or_default();

    ParsedHarness {
        result,
        session_id,
        usage,
        is_error,
    }
}

/// Best-effort: surface progress lines as messages. Claude's `-p` json mode
/// emits a single final object, so most lines are the result; keep it raw.
fn line_mapper(line: &str) -> Option<WorkerEvent> {
    let t = line.trim();
    if t.is_empty() {
        None
    } else {
        Some(WorkerEvent::Raw(t.to_string()))
    }
}

/// Find the last top-level JSON object in mixed output (some CLIs prepend logs).
pub(crate) fn last_json_object(s: &str) -> Option<serde_json::Value> {
    // Try whole-string parse first (the common case).
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(s.trim()) {
        if v.is_object() {
            return Some(v);
        }
    }
    // Otherwise scan lines from the end for a parseable object.
    for line in s.lines().rev() {
        let line = line.trim();
        if line.starts_with('{') {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                return Some(v);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(stdout: &str) -> SubprocessOutput {
        SubprocessOutput {
            stdout: stdout.to_string(),
            stderr: String::new(),
            exit_code: Some(0),
            status: rinne_core::worker::ExecStatus::Success,
            wall_ms: 5,
        }
    }

    #[test]
    fn parses_claude_json_result() {
        let json = r#"{"type":"result","is_error":false,"result":"pong","session_id":"abc","usage":{"input_tokens":12,"output_tokens":3}}"#;
        let p = parse(&out(json));
        assert_eq!(p.result, "pong");
        assert_eq!(p.session_id.as_deref(), Some("abc"));
        assert_eq!(p.usage.prompt_tokens, 12);
        assert_eq!(p.usage.completion_tokens, 3);
        assert!(!p.is_error);
    }

    #[test]
    fn flags_is_error_true() {
        let json = r#"{"is_error":true,"result":"boom"}"#;
        let p = parse(&out(json));
        assert!(p.is_error);
    }

    #[test]
    fn falls_back_to_raw_on_non_json() {
        let p = parse(&out("not json at all"));
        assert_eq!(p.result, "not json at all");
        assert!(!p.is_error);
    }

    #[test]
    fn finds_json_after_log_lines() {
        let mixed = "INFO starting\nWARN something\n{\"result\":\"hi\"}";
        let p = parse(&out(mixed));
        assert_eq!(p.result, "hi");
    }
}
