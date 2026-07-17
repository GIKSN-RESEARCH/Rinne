//! Shared MCP provisioning helpers for harness adapters (`MCP_SKILLS.md` §6).
//!
//! Claude Code is the reference implementation (`claude_code::provision`). Other
//! harnesses that accept a Claude-compatible `.mcp.json` (or can load one via a
//! CLI flag) reuse [`write_mcp_json`] so secrets stay off disk via `${VAR}`
//! expansion + subprocess env.

use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use rinne_core::worker::{McpServerSpec, McpTransportKind};
use rinne_core::{Result, RinneError};

use super::common::Provision;

/// Write a Claude-shaped `{ "mcpServers": … }` config under `scratch`.
///
/// Returns the config path, subprocess env carrying secrets, and the list of
/// `mcp__<server>` allowlist names (for CLIs that pre-approve tools).
pub fn write_mcp_json(
    servers: &[McpServerSpec],
    scratch: &Path,
    file_stem: &str,
) -> Result<(PathBuf, Vec<(String, String)>, Vec<String>)> {
    let mut mcp_servers = Map::new();
    let mut env = Vec::new();
    let mut allowed = Vec::new();

    for (i, s) in servers.iter().enumerate() {
        allowed.push(format!("mcp__{}", s.name));
        let mut entry = Map::new();
        match s.transport {
            McpTransportKind::Stdio => {
                entry.insert(
                    "command".into(),
                    json!(s.command.clone().unwrap_or_default()),
                );
                entry.insert("args".into(), json!(s.args));
                let mut env_map = Map::new();
                for (k, v) in &s.env {
                    env_map.insert(k.clone(), json!(v));
                }
                if let Some(token) = &s.token {
                    if let Some(var) = s.stdio_auth_env().or_else(|| s.token_env.clone()) {
                        let host_var = host_env_var(i, &s.name);
                        env_map.insert(var, json!(format!("${{{host_var}}}")));
                        env.push((host_var, token.clone()));
                    }
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
                    let (header, prefix) = s.http_auth();
                    let host_var = host_env_var(i, &s.name);
                    headers.insert(header, json!(format!("{prefix}${{{host_var}}}")));
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
    let path = scratch.join(format!(
        "{file_stem}-{}-{}.json",
        std::process::id(),
        unique_suffix()
    ));
    std::fs::write(&path, serde_json::to_vec_pretty(&config).unwrap_or_default())
        .map_err(|e| RinneError::Worker(format!("could not write MCP config: {e}")))?;

    Ok((path, env, allowed))
}

/// Claude Code provision path — shared implementation.
pub fn provision_claude_style(
    servers: &[McpServerSpec],
    scratch: &Path,
) -> Result<Provision> {
    let (path, env, allowed) = write_mcp_json(servers, scratch, "mcp")?;
    Ok(Provision {
        args: vec![
            "--mcp-config".into(),
            path.display().to_string(),
            "--strict-mcp-config".into(),
            "--allowedTools".into(),
            allowed.join(","),
        ],
        env,
        cleanup: Some(path),
    })
}

/// OpenCode: point at a Claude-compatible mcp config via env when supported.
/// OpenCode reads MCP from its own config; many builds also honor
/// `OPENCODE_MCP_CONFIG` or a project `.mcp.json`. We write the file and set
/// both a well-known env and pass nothing extra on argv (safe no-op if ignored).
pub fn provision_opencode_style(
    servers: &[McpServerSpec],
    scratch: &Path,
) -> Result<Provision> {
    let (path, mut env, _allowed) = write_mcp_json(servers, scratch, "opencode-mcp")?;
    env.push((
        "OPENCODE_MCP_CONFIG".into(),
        path.display().to_string(),
    ));
    // Some builds look for MCP_CONFIG / CLAUDE-style path.
    env.push(("MCP_CONFIG".into(), path.display().to_string()));
    Ok(Provision {
        args: Vec::new(),
        env,
        cleanup: Some(path),
    })
}

/// Codex: write mcp.json and set `CODEX_MCP_CONFIG` / pass through if the CLI
/// grows a flag. Codex primarily uses `~/.codex/config.toml`; env is best-effort.
pub fn provision_codex_style(
    servers: &[McpServerSpec],
    scratch: &Path,
) -> Result<Provision> {
    let (path, mut env, _allowed) = write_mcp_json(servers, scratch, "codex-mcp")?;
    env.push(("CODEX_MCP_CONFIG".into(), path.display().to_string()));
    env.push(("MCP_CONFIG".into(), path.display().to_string()));
    Ok(Provision {
        args: Vec::new(),
        env,
        cleanup: Some(path),
    })
}

fn host_env_var(index: usize, server: &str) -> String {
    let up: String = server
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("RINNE_MCP_{index}_{up}_TOKEN")
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_mcp_json_keeps_secrets_out_of_file() {
        let dir = std::env::temp_dir().join(format!("rinne-mcp-util-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let servers = vec![McpServerSpec {
            name: "fs".into(),
            transport: McpTransportKind::Stdio,
            command: Some("npx".into()),
            args: vec!["-y".into(), "server".into()],
            env: vec![],
            url: None,
            headers: vec![],
            token_env: Some("GITHUB_TOKEN".into()),
            token: Some("secret123".into()),
            auth: Some("env".into()),
            auth_header: Some("GITHUB_TOKEN".into()),
        }];
        let (path, env, allowed) = write_mcp_json(&servers, &dir, "t").unwrap();
        assert!(allowed.iter().any(|a| a == "mcp__fs"));
        assert!(env.iter().any(|(k, v)| k.contains("FS_TOKEN") && v == "secret123"));
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("secret123"));
        assert!(content.contains("${RINNE_MCP_0_FS_TOKEN}"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
