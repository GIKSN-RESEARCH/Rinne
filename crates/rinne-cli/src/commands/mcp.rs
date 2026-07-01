//! `rinne mcp <subcommand>` — connect and manage MCP servers (`MCP_SKILLS.md`
//! §10). Adds/lists/removes `[mcp.servers.*]` and connects live to test or list
//! a server's tools. The same logic backs the CLI and the TUI `/mcp`.
//!
//! Secrets are never written to config: a remote server's bearer token goes to
//! the OS keychain (§12), keyed by `mcp:<name>`.

use std::path::Path;

use anyhow::Result;

use rinne_config::model::{McpServer, McpTransport};
use rinne_config::write::{self, Scope};
use rinne_mcp::McpClient;

/// CLI entry: run a subcommand and print its report.
pub async fn run(args: &[String]) -> Result<()> {
    let cwd = std::env::current_dir()?;
    for line in run_lines(args, &cwd).await {
        println!("{line}");
    }
    Ok(())
}

/// Dispatch an `mcp` subcommand, returning report lines.
pub async fn run_lines(args: &[String], cwd: &Path) -> Vec<String> {
    // Pull scope flags out of anywhere in the args.
    let mut scope = Scope::Global;
    let toks: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|t| match *t {
            "--project" | "--proj" => {
                scope = Scope::Project;
                false
            }
            "--global" => {
                scope = Scope::Global;
                false
            }
            _ => true,
        })
        .collect();

    let Some((head, rest)) = toks.split_first() else {
        return vec![usage()];
    };
    match *head {
        "add" => add(scope, cwd, rest).await,
        "list" | "ls" => list(cwd),
        "tools" => tools(cwd, rest).await,
        "test" => test(cwd, rest).await,
        "remove" | "rm" => remove(scope, cwd, rest),
        other => vec![format!("unknown mcp subcommand `{other}`"), usage()],
    }
}

fn usage() -> String {
    "usage: rinne mcp add <name> --stdio \"<cmd args>\" | --http <url> [--header k=v] [--key <token>] [--host-only]\n       rinne mcp list · tools <name> · test <name> · remove <name>   (add --project to scope to this repo)".to_string()
}

/// The keychain provider name for an MCP server's token (kept distinct from API
/// providers of the same name).
fn keychain_provider(name: &str) -> String {
    format!("mcp:{name}")
}

fn default_key_env(name: &str) -> String {
    let up: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() { c.to_ascii_uppercase() } else { '_' })
        .collect();
    format!("{up}_MCP_TOKEN")
}

async fn add(scope: Scope, cwd: &Path, rest: &[&str]) -> Vec<String> {
    // Parse: <name> [--stdio "<cmd args>"] [--http <url>] [--header k=v]…
    //        [--key <token>] [--host-only]
    let mut name: Option<&str> = None;
    let mut stdio: Option<String> = None;
    let mut http: Option<String> = None;
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut key: Option<String> = None;
    let mut host_only = false;
    let mut i = 0;
    while i < rest.len() {
        match rest[i] {
            "--stdio" => {
                stdio = rest.get(i + 1).map(|s| s.to_string());
                i += 1;
            }
            "--http" => {
                http = rest.get(i + 1).map(|s| s.to_string());
                i += 1;
            }
            "--header" => {
                if let Some(h) = rest.get(i + 1) {
                    if let Some((k, v)) = h.split_once('=') {
                        headers.push((k.trim().to_string(), v.trim().to_string()));
                    }
                }
                i += 1;
            }
            "--key" => {
                key = rest.get(i + 1).map(|s| s.to_string());
                i += 1;
            }
            "--host-only" => host_only = true,
            t if t.starts_with("--") => {} // unknown flag: ignore
            t if name.is_none() => name = Some(t),
            _ => {}
        }
        i += 1;
    }

    let Some(name) = name else {
        return vec!["usage: rinne mcp add <name> --stdio \"<cmd args>\" | --http <url>".to_string()];
    };

    let mut out = vec![format!("Adding MCP server `{name}`.")];

    let server = match (stdio, http) {
        (Some(cmdline), None) => {
            let mut parts = cmdline.split_whitespace();
            let Some(command) = parts.next() else {
                return vec!["--stdio needs a command".to_string()];
            };
            McpServer {
                transport: McpTransport::Stdio,
                command: Some(command.to_string()),
                args: parts.map(String::from).collect(),
                env: Default::default(),
                url: None,
                headers: Default::default(),
                key_env: None,
                enabled: true,
                tools_allow: vec!["*".to_string()],
                host_only,
            }
        }
        (None, Some(url)) => {
            let key_env = key.as_ref().map(|_| default_key_env(name));
            McpServer {
                transport: McpTransport::Http,
                command: None,
                args: Vec::new(),
                env: Default::default(),
                url: Some(url),
                headers: headers.into_iter().collect(),
                key_env,
                enabled: true,
                tools_allow: vec!["*".to_string()],
                host_only,
            }
        }
        (Some(_), Some(_)) => {
            return vec!["choose one transport: --stdio OR --http, not both".to_string()];
        }
        (None, None) => {
            return vec!["specify a transport: --stdio \"<cmd args>\" or --http <url>".to_string()];
        }
    };

    // Store a bearer token in the keychain (never in the config file).
    if let Some(token) = &key {
        match rinne_config::secrets::store_api_key(&keychain_provider(name), token) {
            Ok(()) => out.push("✔ token stored in your OS keychain (set once).".to_string()),
            Err(e) => out.push(format!("⚠ could not store the token in the keychain ({e}).")),
        }
    }

    // Write the config table.
    let path = match write::target_path(scope, cwd) {
        Ok(p) => p,
        Err(e) => return vec![format!("✗ {e}")],
    };
    if let Err(e) = write::write_mcp_server_to(&path, name, &server) {
        return vec![format!("✗ could not write config: {e}")];
    }
    out.push(format!("Wrote [mcp.servers.{name}] to {} ({})", path.display(), scope.label()));

    // Connect-test so problems surface now, not mid-run.
    out.push("testing the connection…".to_string());
    match connect_client(name, &server).await {
        Ok(mut client) => {
            let server_label = client.server_name().unwrap_or(name).to_string();
            match client.list_tools().await {
                Ok(tools) => out.push(format!(
                    "✔ connected to `{server_label}` — {} tool{} available.",
                    tools.len(),
                    if tools.len() == 1 { "" } else { "s" }
                )),
                Err(e) => out.push(format!("✔ connected, but listing tools failed: {e}")),
            }
        }
        Err(e) => {
            out.push(format!("✗ could not connect: {e}"));
            out.push("  the server is saved; fix the command/url/token and re-run `rinne mcp test`.".to_string());
        }
    }
    out
}

fn list(cwd: &Path) -> Vec<String> {
    let config = match rinne_config::load(cwd) {
        Ok(c) => c,
        Err(e) => return vec![format!("could not read config: {e}")],
    };
    if config.mcp.servers.is_empty() {
        return vec![
            "No MCP servers connected.".to_string(),
            "Add one: rinne mcp add <name> --stdio \"npx -y @modelcontextprotocol/server-filesystem .\"".to_string(),
        ];
    }
    let mut out = vec!["MCP SERVERS:".to_string()];
    for (name, s) in &config.mcp.servers {
        let endpoint = match s.transport {
            McpTransport::Stdio => format!(
                "stdio: {} {}",
                s.command.as_deref().unwrap_or("?"),
                s.args.join(" ")
            ),
            McpTransport::Http => format!("http: {}", s.url.as_deref().unwrap_or("?")),
        };
        let mut flags = Vec::new();
        if !s.enabled {
            flags.push("disabled".to_string());
        }
        if s.host_only {
            flags.push("host-only".to_string());
        }
        if let Some(key_env) = &s.key_env {
            let has = rinne_config::secrets::key_source(&keychain_provider(name), key_env).is_some();
            flags.push(if has { "key ✔".into() } else { "key missing".into() });
        }
        let suffix = if flags.is_empty() { String::new() } else { format!("  [{}]", flags.join(", ")) };
        out.push(format!("  {:<16} {}{}", name, endpoint.trim(), suffix));
    }
    out
}

async fn tools(cwd: &Path, rest: &[&str]) -> Vec<String> {
    let Some(name) = rest.first().copied() else {
        return vec!["usage: rinne mcp tools <name>".to_string()];
    };
    let server = match server_config(cwd, name) {
        Ok(s) => s,
        Err(e) => return vec![e],
    };
    match connect_client(name, &server).await {
        Ok(mut client) => match client.list_tools().await {
            Ok(tools) if tools.is_empty() => vec![format!("`{name}` exposes no tools.")],
            Ok(tools) => {
                let mut out = vec![format!("Tools on `{name}`:")];
                for t in tools {
                    let desc = t.description.lines().next().unwrap_or("");
                    out.push(format!("  {:<22} {}", format!("{name}.{}", t.name), desc));
                }
                out
            }
            Err(e) => vec![format!("✗ listing tools failed: {e}")],
        },
        Err(e) => vec![format!("✗ could not connect to `{name}`: {e}")],
    }
}

async fn test(cwd: &Path, rest: &[&str]) -> Vec<String> {
    let Some(name) = rest.first().copied() else {
        return vec!["usage: rinne mcp test <name>".to_string()];
    };
    let server = match server_config(cwd, name) {
        Ok(s) => s,
        Err(e) => return vec![e],
    };
    match connect_client(name, &server).await {
        Ok(mut client) => {
            let label = client.server_name().unwrap_or(name).to_string();
            let n = client.list_tools().await.map(|t| t.len()).unwrap_or(0);
            vec![format!("✔ `{name}` reachable — server `{label}`, {n} tool{}.", if n == 1 { "" } else { "s" })]
        }
        Err(e) => vec![format!("✗ `{name}` not reachable: {e}")],
    }
}

fn remove(scope: Scope, cwd: &Path, rest: &[&str]) -> Vec<String> {
    let Some(name) = rest.first().copied() else {
        return vec!["usage: rinne mcp remove <name>".to_string()];
    };
    let path = match write::target_path(scope, cwd) {
        Ok(p) => p,
        Err(e) => return vec![format!("✗ {e}")],
    };
    match write::remove_mcp_server_from(&path, name) {
        Ok(true) => {
            let _ = rinne_config::secrets::delete_api_key(&keychain_provider(name));
            vec![format!("✔ removed MCP server `{name}` ({})", scope.label())]
        }
        Ok(false) => vec![format!("· `{name}` is not in the {} config", scope.label())],
        Err(e) => vec![format!("✗ {e}")],
    }
}

/// Look up a configured server by name.
fn server_config(cwd: &Path, name: &str) -> std::result::Result<McpServer, String> {
    let config = rinne_config::load(cwd).map_err(|e| format!("could not read config: {e}"))?;
    config
        .mcp
        .servers
        .get(name)
        .cloned()
        .ok_or_else(|| format!("no MCP server named `{name}` — `rinne mcp list` to see them"))
}

/// Build and connect an [`McpClient`] from a server's config. Shared with the
/// planner's catalog builder, which lists tools across all configured servers.
pub(crate) async fn connect_client(
    name: &str,
    server: &McpServer,
) -> std::result::Result<McpClient, String> {
    match server.transport {
        McpTransport::Stdio => {
            let Some(command) = server.command.as_deref() else {
                return Err("stdio server has no `command`".into());
            };
            let env: Vec<(String, String)> =
                server.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            McpClient::connect_stdio(command, &server.args, &env)
                .await
                .map_err(|e| e.to_string())
        }
        McpTransport::Http => {
            let Some(url) = server.url.as_deref() else {
                return Err("http server has no `url`".into());
            };
            let mut headers: Vec<(String, String)> =
                server.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            if let Some(key_env) = &server.key_env {
                if let Some(token) =
                    rinne_config::secrets::resolve_api_key(&keychain_provider(name), key_env)
                {
                    headers.push(("authorization".to_string(), format!("Bearer {token}")));
                }
            }
            McpClient::connect_http(url, headers)
                .await
                .map_err(|e| e.to_string())
        }
    }
}
