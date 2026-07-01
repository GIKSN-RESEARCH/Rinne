//! Claude Code adapter (`CONTEXT.md` §9, §16).
//!
//! Drives the native `claude -p --output-format json` call, which honors the
//! Pro/Max subscription login — never the ACP adapter, which would force an
//! Anthropic API key (`CONTEXT.md` §9). The footgun guard for a stray
//! `ANTHROPIC_API_KEY` lives in `doctor` (Phase 1).

use std::path::Path;
use std::time::Duration;

use serde_json::{json, Map, Value};

use rinne_core::worker::{
    AuthMode, Capability, LatencyProfile, McpServerSpec, McpTransportKind, QuotaModel, Transport,
    Usage, WorkerDescriptor, WorkerEvent, WorkerFamily,
};
use rinne_core::{Result, RinneError};

use super::common::{HarnessAdapter, ParsedHarness, Provision};
use crate::transport::subprocess::SubprocessOutput;

/// Construct a Claude Code harness worker.
pub fn worker() -> HarnessAdapter {
    HarnessAdapter {
        descriptor: descriptor(),
        program: "claude".to_string(),
        build_args,
        plan_args: Some(plan_args),
        parse,
        line_mapper,
        prompt_via_stdin: false,
        default_timeout: Duration::from_secs(600),
        provisioner: Some(provision),
    }
}

/// Lean argv for the planner role: a plain `claude -p "<prompt>"` print, with no
/// `stream-json`/`--verbose` (which some CLI versions reject, and which we don't
/// need just to read a JSON plan). stdout is the answer text.
fn plan_args(prompt: &str, model: Option<&str>) -> Vec<String> {
    let mut args = vec!["-p".into(), prompt.into()];
    if let Some(m) = model {
        args.push("--model".into());
        args.push(m.into());
    }
    args
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
        // Claude `--model` aliases, listed cheap→strong (the cascade ladder).
        models: vec!["haiku".into(), "sonnet".into(), "opus".into()],
    }
}

fn build_args(prompt: &str, model: Option<&str>) -> Vec<String> {
    // `stream-json` emits one NDJSON event per line as work happens, so the user
    // sees reads/edits/commands live instead of one blob at the end. `--verbose`
    // is required to stream events under `-p`.
    let mut args = vec![
        "-p".into(),
        prompt.into(),
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
    ];
    if let Some(m) = model {
        args.push("--model".into());
        args.push(m.into());
    }
    args
}

/// Parse Claude Code's stream-json output: find the terminal `result` event
/// (a line with `"type":"result"` or a `result` field) and extract its fields.
fn parse(out: &SubprocessOutput) -> ParsedHarness {
    for line in out.stdout.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let is_result = value.get("type").and_then(|t| t.as_str()) == Some("result")
            || value.get("result").is_some();
        if !is_result {
            continue;
        }

        let result = value
            .get("result")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_default();
        let session_id = value
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let is_error = value.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
        let usage = value
            .get("usage")
            .map(|u| Usage {
                prompt_tokens: u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                completion_tokens: u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                wall_ms: 0,
            })
            .unwrap_or_default();
        return ParsedHarness {
            result,
            session_id,
            usage,
            is_error,
        };
    }
    ParsedHarness::raw(&out.stdout)
}

/// Map one NDJSON stream event into live worker events. An assistant message can
/// carry several content blocks (text + tool uses), so this returns a vector.
fn line_mapper(line: &str) -> Vec<WorkerEvent> {
    let line = line.trim();
    if line.is_empty() {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return Vec::new();
    };

    match value.get("type").and_then(|t| t.as_str()) {
        // The init event names the model the harness is actually running.
        Some("system") => {
            if value.get("subtype").and_then(|s| s.as_str()) == Some("init") {
                if let Some(model) = value.get("model").and_then(|m| m.as_str()) {
                    return vec![WorkerEvent::Message(format!("model: {}", short_model(model)))];
                }
            }
            return Vec::new();
        }
        // Assistant messages carry text + tool uses (handled below).
        Some("assistant") => {}
        // Suppress tool_result and the final result event (parsed separately).
        _ => return Vec::new(),
    }

    let Some(content) = value.pointer("/message/content").and_then(|c| c.as_array()) else {
        return Vec::new();
    };

    let mut events = Vec::new();
    for block in content {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    let text = text.trim();
                    if !text.is_empty() {
                        events.push(WorkerEvent::Message(text.to_string()));
                    }
                }
            }
            Some("tool_use") => {
                let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                let input = block.get("input").cloned().unwrap_or(serde_json::Value::Null);
                events.push(tool_event(name, &input));
            }
            _ => {}
        }
    }
    events
}

/// Render a Claude tool call as a friendly, harness-style line.
fn tool_event(name: &str, input: &serde_json::Value) -> WorkerEvent {
    let s = |k: &str| input.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    match name {
        "Read" => WorkerEvent::Reading(short_path(&s("file_path"))),
        "Write" => WorkerEvent::Editing(format!("writing {}", short_path(&s("file_path")))),
        "Edit" | "MultiEdit" | "NotebookEdit" => {
            WorkerEvent::Editing(format!("editing {}", short_path(&s("file_path"))))
        }
        "Bash" => {
            let desc = s("description");
            let cmd = s("command");
            WorkerEvent::ToolUse(if desc.is_empty() { truncate(&cmd, 80) } else { desc })
        }
        "Glob" => WorkerEvent::ToolUse(format!("glob {}", s("pattern"))),
        "Grep" => WorkerEvent::ToolUse(format!("grep {}", s("pattern"))),
        "Task" => WorkerEvent::ToolUse(format!("subagent: {}", s("description"))),
        "TodoWrite" => WorkerEvent::Message("updating plan".into()),
        "WebSearch" => WorkerEvent::ToolUse(format!("web search {}", s("query"))),
        "WebFetch" => WorkerEvent::ToolUse(format!("fetch {}", s("url"))),
        other => WorkerEvent::ToolUse(other.to_string()),
    }
}

/// Friendly model label: `claude-opus-4-8[1m]` → `opus-4-8`.
fn short_model(m: &str) -> String {
    m.split_once("[1m]")
        .map(|(a, _)| a)
        .unwrap_or(m)
        .strip_prefix("claude-")
        .unwrap_or(m)
        .to_string()
}

/// Show the last two path segments so lines stay readable.
fn short_path(p: &str) -> String {
    let parts: Vec<&str> = p.rsplit('/').take(2).collect();
    parts.into_iter().rev().collect::<Vec<_>>().join("/")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}

/// Provision a node's MCP servers into a `claude` invocation (`MCP_SKILLS.md`
/// §6). Writes a `.mcp.json`-shaped config and returns `--mcp-config <path>`
/// `--strict-mcp-config` (only our servers) and `--allowedTools mcp__<server>…`
/// (pre-approve the tools for the non-interactive `-p` run).
///
/// Secrets stay off disk: a token is written as a `${VAR}` reference and the
/// real value is set on the subprocess environment, which Claude expands.
fn provision(servers: &[McpServerSpec], scratch: &Path) -> Result<Provision> {
    let mut mcp_servers = Map::new();
    let mut env = Vec::new();
    let mut allowed = Vec::new();

    for (i, s) in servers.iter().enumerate() {
        allowed.push(format!("mcp__{}", s.name));
        let mut entry = Map::new();
        match s.transport {
            McpTransportKind::Stdio => {
                entry.insert("command".into(), json!(s.command.clone().unwrap_or_default()));
                entry.insert("args".into(), json!(s.args));
                let mut env_map = Map::new();
                for (k, v) in &s.env {
                    env_map.insert(k.clone(), json!(v));
                }
                if let (Some(token), Some(var)) = (&s.token, &s.token_env) {
                    // The server reads its token from `var`; reference it through
                    // a host env var so the value never lands in the file.
                    let host_var = host_env_var(i, &s.name);
                    env_map.insert(var.clone(), json!(format!("${{{host_var}}}")));
                    env.push((host_var, token.clone()));
                }
                if !env_map.is_empty() {
                    entry.insert("env".into(), Value::Object(env_map));
                }
            }
            McpTransportKind::Http => {
                entry.insert("type".into(), json!("http"));
                entry.insert("url".into(), json!(s.url.clone().unwrap_or_default()));
                let mut headers = Map::new();
                for (k, v) in &s.headers {
                    headers.insert(k.clone(), json!(v));
                }
                if let Some(token) = &s.token {
                    let host_var = host_env_var(i, &s.name);
                    headers.insert("Authorization".into(), json!(format!("Bearer ${{{host_var}}}")));
                    env.push((host_var, token.clone()));
                }
                if !headers.is_empty() {
                    entry.insert("headers".into(), Value::Object(headers));
                }
            }
        }
        mcp_servers.insert(s.name.clone(), Value::Object(entry));
    }

    let config = json!({ "mcpServers": Value::Object(mcp_servers) });
    std::fs::create_dir_all(scratch)
        .map_err(|e| RinneError::Worker(format!("could not create MCP scratch dir: {e}")))?;
    let path = scratch.join(format!("mcp-{}-{}.json", std::process::id(), unique_suffix()));
    std::fs::write(&path, serde_json::to_vec_pretty(&config).unwrap_or_default())
        .map_err(|e| RinneError::Worker(format!("could not write MCP config: {e}")))?;

    let args = vec![
        "--mcp-config".to_string(),
        path.display().to_string(),
        "--strict-mcp-config".to_string(),
        "--allowedTools".to_string(),
        allowed.join(","),
    ];
    Ok(Provision {
        args,
        env,
        cleanup: Some(path),
    })
}

/// A host environment variable name to carry a server's token to the subprocess.
/// The index keeps the name unique even when two server names differ only by
/// punctuation (e.g. `fs.x` and `fs-x` both sanitize to `FS_X`).
fn host_env_var(index: usize, server: &str) -> String {
    let up: String = server
        .chars()
        .map(|c| if c.is_alphanumeric() { c.to_ascii_uppercase() } else { '_' })
        .collect();
    format!("RINNE_MCP_{index}_{up}_TOKEN")
}

/// A process-unique suffix for the scratch config filename (no `rand` dep).
fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
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

    #[test]
    fn provision_writes_config_and_keeps_tokens_off_disk() {
        let dir = std::env::temp_dir().join(format!("rinne-prov-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let servers = vec![
            McpServerSpec {
                name: "fs".into(),
                transport: McpTransportKind::Stdio,
                command: Some("npx".into()),
                args: vec!["-y".into(), "server".into()],
                env: vec![],
                url: None,
                headers: vec![],
                token_env: Some("GITHUB_TOKEN".into()),
                token: Some("secret123".into()),
            },
            McpServerSpec {
                name: "remote".into(),
                transport: McpTransportKind::Http,
                command: None,
                args: vec![],
                env: vec![],
                url: Some("https://x/mcp".into()),
                headers: vec![],
                token_env: Some("REMOTE_TOKEN".into()),
                token: Some("bearer456".into()),
            },
        ];

        let p = provision(&servers, &dir).unwrap();

        // The flags pre-approve each server's tools and pin to our config only.
        assert!(p.args.contains(&"--mcp-config".to_string()));
        assert!(p.args.contains(&"--strict-mcp-config".to_string()));
        let i = p.args.iter().position(|a| a == "--allowedTools").unwrap();
        assert!(p.args[i + 1].contains("mcp__fs"));
        assert!(p.args[i + 1].contains("mcp__remote"));

        // Tokens travel via the subprocess environment, never the file. The
        // var names are index-prefixed to stay unique across servers.
        assert!(p.env.iter().any(|(k, v)| k == "RINNE_MCP_0_FS_TOKEN" && v == "secret123"));
        assert!(p.env.iter().any(|(k, v)| k == "RINNE_MCP_1_REMOTE_TOKEN" && v == "bearer456"));

        let content = std::fs::read_to_string(p.cleanup.as_ref().unwrap()).unwrap();
        assert!(!content.contains("secret123"), "stdio token must not be on disk");
        assert!(!content.contains("bearer456"), "http token must not be on disk");
        assert!(content.contains("${RINNE_MCP_0_FS_TOKEN}"));
        assert!(content.contains("Bearer ${RINNE_MCP_1_REMOTE_TOKEN}"));

        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["mcpServers"]["fs"]["command"], "npx");
        assert_eq!(v["mcpServers"]["fs"]["env"]["GITHUB_TOKEN"], "${RINNE_MCP_0_FS_TOKEN}");
        assert_eq!(v["mcpServers"]["remote"]["type"], "http");
        assert_eq!(v["mcpServers"]["remote"]["url"], "https://x/mcp");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn provision_errors_gracefully_when_scratch_cannot_be_created() {
        // A regular file standing where the scratch directory's parent should be:
        // `create_dir_all` under it fails, and provisioning must return an error
        // (the adapter then runs the node without tools) rather than panic.
        let blocker = std::env::temp_dir().join(format!("rinne-prov-blocker-{}", std::process::id()));
        let _ = std::fs::remove_file(&blocker);
        std::fs::write(&blocker, b"x").unwrap();
        let scratch = blocker.join("mcp");

        let servers = vec![McpServerSpec {
            name: "fs".into(),
            transport: McpTransportKind::Stdio,
            command: Some("npx".into()),
            args: vec![],
            env: vec![],
            url: None,
            headers: vec![],
            token_env: None,
            token: None,
        }];
        assert!(provision(&servers, &scratch).is_err());

        let _ = std::fs::remove_file(&blocker);
    }
}
