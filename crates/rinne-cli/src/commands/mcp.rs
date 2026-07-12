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
        "login" | "auth" => login(cwd, rest).await,
        "remove" | "rm" => remove(scope, cwd, rest),
        other => vec![format!("unknown mcp subcommand `{other}`"), usage()],
    }
}

fn usage() -> String {
    [
        "usage: rinne mcp add <link> [--name <name>] [auth] [--host-only]",
        "  <link>            an http(s) URL (remote server) or a launch command (local stdio)",
        "  auth (remote):    --bearer <token>            Authorization: Bearer <token>",
        "                    --api-key <token> [--auth-header <NAME>]   custom header (default X-API-Key)",
        "                    --auth bearer|apikey        use token already in keychain (mcp:<name>)",
        "                    --oauth [--client-id <id>]  browser login (OAuth 2.1 + PKCE)",
        "  auth (local):     --secret-env <VAR>=<token>  token as a server env var",
        "  extra:            --header <k=v> (remote)   --env <k=v> (local)   (repeatable, non-secret)",
        "  rinne mcp list · tools <name> · test <name> · login <name> · remove <name>   (--project to scope to this repo)",
    ]
    .join("\n")
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
    // Parse: <link> [--name <name>] [auth flags] [--header k=v] [--env k=v] [--host-only]
    let mut link: Option<&str> = None;
    let mut name: Option<String> = None;
    let mut headers: Vec<(String, String)> = Vec::new(); // non-secret http headers
    let mut env_vars: Vec<(String, String)> = Vec::new(); // non-secret stdio env
    let mut bearer: Option<String> = None;
    let mut api_key: Option<String> = None;
    let mut auth_header_name: Option<String> = None;
    let mut secret_env: Option<(String, String)> = None;
    let mut oauth = false;
    let mut client_id: Option<String> = None;
    let mut host_only = false;
    let mut i = 0;
    let kv = |s: &str| -> Option<(String, String)> {
        s.split_once('=')
            .map(|(k, v)| (k.trim().to_string(), v.to_string()))
    };
    while i < rest.len() {
        let next = rest.get(i + 1).map(|s| s.to_string());
        match rest[i] {
            "--name" => {
                name = next;
                i += 1;
            }
            // Remote auth: a bearer token or an API key in a (custom) header.
            // GUI clients should prefer `--auth bearer|apikey` after writing the
            // token to the keychain (never put secrets on argv / process lists).
            "--bearer" | "--key" => {
                bearer = next;
                i += 1;
            }
            "--api-key" => {
                api_key = next;
                i += 1;
            }
            // Auth kind only — token already in keychain as `mcp:<name>`.
            "--auth" => {
                // next is "bearer" | "apikey" | "env"
                // Handled after name is known (see from_keychain_auth below).
                // Store in next via a local we parse into auth kind with no token.
                if let Some(ref kind) = next {
                    match kind.as_str() {
                        "bearer" => bearer = Some(String::new()), // empty → keychain-only
                        "apikey" => api_key = Some(String::new()),
                        _ => {}
                    }
                }
                i += 1;
            }
            "--auth-header" => {
                auth_header_name = next;
                i += 1;
            }
            // OAuth 2.1 login flow (remote servers that require it).
            "--oauth" => oauth = true,
            "--client-id" => {
                client_id = next;
                i += 1;
            }
            // Local (stdio) auth: a token set as a server environment variable.
            "--secret-env" => {
                secret_env = next.as_deref().and_then(kv);
                i += 1;
            }
            "--env" => {
                if let Some(pair) = next.as_deref().and_then(kv) {
                    env_vars.push(pair);
                }
                i += 1;
            }
            "--header" => {
                if let Some(pair) = next.as_deref().and_then(kv) {
                    headers.push(pair);
                }
                i += 1;
            }
            "--host-only" => host_only = true,
            t if t.starts_with("--") => {} // unknown flag: ignore
            t if link.is_none() => link = Some(t),
            _ => {}
        }
        i += 1;
    }

    let Some(link) = link else {
        return vec![usage()];
    };

    // Infer transport from the link: an http(s) URL is a remote server, anything
    // else is a stdio launch command.
    let is_url = link.starts_with("http://") || link.starts_with("https://");
    let name = match name {
        Some(n) => n,
        None => derive_name(link, is_url),
    };
    if name.is_empty() {
        return vec!["could not derive a name from the link — pass one with --name <name>".to_string()];
    }

    // Resolve the single auth secret + how it's presented. Bearer/api-key/oauth
    // are for remote (http) servers; secret-env is for local (stdio) servers.
    if is_url && secret_env.is_some() {
        return vec!["--secret-env is for local (stdio) servers; use --bearer/--api-key/--oauth for a URL".to_string()];
    }
    if !is_url && (bearer.is_some() || api_key.is_some() || oauth) {
        return vec!["--bearer/--api-key/--oauth are for remote (http) servers; use --secret-env VAR=<token> for a local server".to_string()];
    }
    // The token here is a static secret set at add time; for OAuth the token is
    // obtained interactively below and lives in the keychain as a session blob.
    // Empty bearer/api_key (from `--auth bearer|apikey`) means: use keychain only,
    // do not re-store, and never require the secret on argv.
    let (token, auth, auth_hdr): (Option<String>, Option<String>, Option<String>) = if oauth {
        (None, Some("oauth".to_string()), None)
    } else if let Some(t) = bearer {
        if t.is_empty() {
            // Keychain-only: require an existing keychain entry.
            if rinne_config::secrets::keychain_key(&keychain_provider(&name)).is_none() {
                return vec![format!(
                    "no keychain token for mcp:{name} — store it first, or pass --bearer <token> once"
                )];
            }
            (None, Some("bearer".to_string()), None)
        } else {
            (Some(t), Some("bearer".to_string()), None)
        }
    } else if let Some(t) = api_key {
        let hdr = auth_header_name.unwrap_or_else(|| "X-API-Key".to_string());
        if t.is_empty() {
            if rinne_config::secrets::keychain_key(&keychain_provider(&name)).is_none() {
                return vec![format!(
                    "no keychain token for mcp:{name} — store it first, or pass --api-key <token> once"
                )];
            }
            (None, Some("apikey".to_string()), Some(hdr))
        } else {
            (Some(t), Some("apikey".to_string()), Some(hdr))
        }
    } else if let Some((var, t)) = secret_env {
        if t.is_empty() {
            if rinne_config::secrets::keychain_key(&keychain_provider(&name)).is_none() {
                return vec![format!(
                    "no keychain token for mcp:{name} — store it first, or pass --secret-env {var}=<token> once"
                )];
            }
            (None, Some("env".to_string()), Some(var))
        } else {
            (Some(t), Some("env".to_string()), Some(var))
        }
    } else {
        (None, None, None)
    };
    // key_env is required whenever auth uses a static token (including keychain-only).
    let key_env = if auth.as_deref() == Some("bearer")
        || auth.as_deref() == Some("apikey")
        || auth.as_deref() == Some("env")
    {
        Some(default_key_env(&name))
    } else {
        None
    };

    let server = if is_url {
        McpServer {
            transport: McpTransport::Http,
            command: None,
            args: Vec::new(),
            env: Default::default(),
            url: Some(link.to_string()),
            headers: headers.into_iter().collect(),
            key_env,
            enabled: true,
            tools_allow: vec!["*".to_string()],
            host_only,
            auth,
            auth_header: auth_hdr,
        }
    } else {
        let mut parts = link.split_whitespace();
        let Some(command) = parts.next() else {
            return vec!["the link is empty".to_string()];
        };
        McpServer {
            transport: McpTransport::Stdio,
            command: Some(command.to_string()),
            args: parts.map(String::from).collect(),
            env: env_vars.into_iter().collect(),
            url: None,
            headers: Default::default(),
            key_env,
            enabled: true,
            tools_allow: vec!["*".to_string()],
            host_only,
            auth,
            auth_header: auth_hdr,
        }
    };

    // Guard against duplicates across the merged config (global + project): one
    // endpoint, one name. Adding the same link again — under any name — is
    // reported against the existing server; a name already in use is refused.
    if let Ok(config) = rinne_config::load(cwd) {
        let new_key = link_key(&server);
        if let Some((existing, _)) = config.mcp.servers.iter().find(|(_, s)| link_key(s) == new_key) {
            return vec![format!(
                "that server is already added as `{existing}` — remove it first with `rinne mcp remove {existing}`"
            )];
        }
        if config.mcp.servers.contains_key(&name) {
            return vec![format!(
                "the name `{name}` is already in use — choose another with `--name`, or remove it first with `rinne mcp remove {name}`"
            )];
        }
    }

    let mut out = vec![format!("Adding MCP server `{name}`.")];

    // OAuth: run the interactive login (browser + local callback) and stash the
    // resulting session in the keychain before saving the server. Do this after
    // the dedup guard so a redundant add can't open a browser.
    if oauth {
        out.push(format!("Authorizing `{name}` via OAuth — opening your browser…"));
        match rinne_mcp::login(link, None, client_id, now_secs()).await {
            Ok(session) => match serde_json::to_string(&session) {
                Ok(json) => {
                    if let Err(e) =
                        rinne_config::secrets::store_secret(&oauth_provider(&name), &json)
                    {
                        return vec![format!("✗ could not store the OAuth session: {e}")];
                    }
                    out.push("✔ authorized; tokens stored in your OS keychain.".to_string());
                }
                Err(e) => return vec![format!("✗ could not serialize the OAuth session: {e}")],
            },
            Err(e) => return vec![format!("✗ OAuth login failed: {e}")],
        }
    }

    // Store the auth token in the keychain (never in the config file).
    if let Some(token) = &token {
        match rinne_config::secrets::store_api_key(&keychain_provider(&name), token) {
            Ok(()) => out.push("✔ token stored in your OS keychain (set once).".to_string()),
            Err(e) => out.push(format!("⚠ could not store the token in the keychain ({e}).")),
        }
    }

    // Write the config table.
    let path = match write::target_path(scope, cwd) {
        Ok(p) => p,
        Err(e) => return vec![format!("✗ {e}")],
    };
    if let Err(e) = write::write_mcp_server_to(&path, &name, &server) {
        return vec![format!("✗ could not write config: {e}")];
    }
    out.push(format!("Wrote [mcp.servers.{name}] to {} ({})", path.display(), scope.label()));

    // Connect-test so problems surface now, not mid-run.
    out.push("testing the connection…".to_string());
    match connect_client(&name, &server).await {
        Ok(mut client) => out.push(connected_line(&mut client, &name).await),
        // A 401 on a plain remote add almost always means the server wants a
        // login — auto-fall back to the OAuth flow rather than just reporting it.
        Err(e) if is_url && server.auth.is_none() && e.contains("401") => {
            out.push("this server requires authorization — starting OAuth login, opening your browser…".to_string());
            match rinne_mcp::login(link, None, None, now_secs()).await {
                Ok(session) => {
                    let stored = serde_json::to_string(&session).ok().and_then(|json| {
                        rinne_config::secrets::store_secret(&oauth_provider(&name), &json).ok()
                    });
                    if stored.is_none() {
                        out.push("✗ could not store the OAuth session.".to_string());
                        return out;
                    }
                    // Upgrade the saved server to OAuth auth and re-test.
                    let mut authed = server.clone();
                    authed.auth = Some("oauth".to_string());
                    authed.key_env = None;
                    let _ = write::write_mcp_server_to(&path, &name, &authed);
                    out.push("✔ authorized via OAuth; tokens stored in your keychain.".to_string());
                    match connect_client(&name, &authed).await {
                        Ok(mut client) => out.push(connected_line(&mut client, &name).await),
                        Err(e2) => out.push(format!("✗ still could not connect after login: {e2}")),
                    }
                }
                Err(le) => {
                    out.push(format!("✗ could not connect: {e}"));
                    out.push(format!(
                        "  OAuth login also failed ({le}) — if this server uses a token instead, re-add with --bearer or --api-key."
                    ));
                }
            }
        }
        Err(e) => {
            out.push(format!("✗ could not connect: {e}"));
            out.push("  the server is saved; fix the link/token and re-run `rinne mcp test`.".to_string());
        }
    }
    out
}

/// The "connected — N tools" summary line for a freshly connected client.
async fn connected_line(client: &mut McpClient, name: &str) -> String {
    let label = client.server_name().unwrap_or(name).to_string();
    match client.list_tools().await {
        Ok(tools) => format!(
            "✔ connected to `{label}` — {} tool{} available.",
            tools.len(),
            if tools.len() == 1 { "" } else { "s" }
        ),
        Err(e) => format!("✔ connected, but listing tools failed: {e}"),
    }
}

/// A stable identity for a server's endpoint, so the same link can't be added
/// twice under different names.
fn link_key(s: &McpServer) -> String {
    match s.transport {
        McpTransport::Http => format!(
            "http:{}",
            s.url.as_deref().unwrap_or("").trim_end_matches('/')
        ),
        McpTransport::Stdio => format!(
            "stdio:{} {}",
            s.command.as_deref().unwrap_or(""),
            s.args.join(" ")
        )
        .trim()
        .to_string(),
    }
}

/// Derive a friendly server name from the link when `--name` isn't given: the
/// domain's second-level label for a URL, or the package/command name for stdio.
fn derive_name(link: &str, is_url: bool) -> String {
    let raw = if is_url {
        let host = link
            .split("://")
            .nth(1)
            .unwrap_or(link)
            .split('/')
            .next()
            .unwrap_or(link)
            .rsplit('@')
            .next()
            .unwrap_or(link)
            .split(':')
            .next()
            .unwrap_or(link);
        let labels: Vec<&str> = host.split('.').filter(|l| !l.is_empty()).collect();
        match labels.len() {
            0 => host.to_string(),
            1 => labels[0].to_string(),
            n => labels[n - 2].to_string(),
        }
    } else {
        let tokens: Vec<&str> = link.split_whitespace().collect();
        tokens
            .iter()
            .rev()
            .find(|t| t.contains('/') || t.contains("server"))
            .map(|t| t.rsplit('/').next().unwrap_or(t))
            .or_else(|| tokens.first().copied())
            .unwrap_or("mcp")
            .to_string()
    };
    sanitize_name(&raw)
}

/// Reduce a raw label to a config-key-friendly server name, trimming noise
/// prefixes/suffixes common in MCP package names (`server-`, `-mcp`, …).
fn sanitize_name(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let mut s = cleaned.trim_matches('-').to_lowercase();
    for p in ["mcp-server-", "server-", "mcp-"] {
        if let Some(rest) = s.strip_prefix(p) {
            s = rest.to_string();
            break;
        }
    }
    for suf in ["-mcp-server", "-server", "-mcp"] {
        if let Some(rest) = s.strip_suffix(suf) {
            s = rest.to_string();
            break;
        }
    }
    s.trim_matches('-').to_string()
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
            let kind = s.auth.as_deref().unwrap_or("bearer");
            flags.push(if has {
                format!("{kind} ✔")
            } else {
                format!("{kind} missing")
            });
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

/// (Re)authorize an already-added remote server via OAuth — for a server added
/// with `--oauth` whose tokens were revoked, or one that turned out to need it.
async fn login(cwd: &Path, rest: &[&str]) -> Vec<String> {
    let Some(name) = rest.first().copied() else {
        return vec!["usage: rinne mcp login <name> [--client-id <id>]".to_string()];
    };
    let client_id = rest
        .iter()
        .position(|a| *a == "--client-id")
        .and_then(|i| rest.get(i + 1))
        .map(|s| s.to_string());
    let server = match server_config(cwd, name) {
        Ok(s) => s,
        Err(e) => return vec![e],
    };
    let Some(url) = server.url.clone() else {
        return vec![format!("`{name}` is not a remote server — OAuth applies to http servers")];
    };

    let mut out = vec![format!("Authorizing `{name}` via OAuth — opening your browser…")];
    match rinne_mcp::login(&url, None, client_id, now_secs()).await {
        Ok(session) => match serde_json::to_string(&session) {
            Ok(json) => {
                if let Err(e) = rinne_config::secrets::store_secret(&oauth_provider(name), &json) {
                    return vec![format!("✗ could not store the OAuth session: {e}")];
                }
                // Ensure the server records oauth auth even if it was added plain.
                if server.auth.as_deref() != Some("oauth") {
                    let mut s = server.clone();
                    s.auth = Some("oauth".to_string());
                    s.key_env = None;
                    if let Ok(path) = write::target_path(scope_of(cwd, name), cwd) {
                        let _ = write::write_mcp_server_to(&path, name, &s);
                    }
                }
                out.push("✔ authorized; tokens stored in your OS keychain.".to_string());
                out
            }
            Err(e) => vec![format!("✗ could not serialize the OAuth session: {e}")],
        },
        Err(e) => vec![format!("✗ OAuth login failed: {e}")],
    }
}

/// Which scope a server currently lives in (project shadows global), so a
/// re-auth writes it back to the same place.
fn scope_of(cwd: &Path, name: &str) -> Scope {
    let project = write::target_path(Scope::Project, cwd)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| t.contains(&format!("[mcp.servers.{name}]")))
        .unwrap_or(false);
    if project {
        Scope::Project
    } else {
        Scope::Global
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
            let _ = rinne_config::secrets::delete_api_key(&oauth_provider(name));
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
            let mut env: Vec<(String, String)> =
                server.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            // Inject a token as the server's auth environment variable.
            if let Some(var) = server.stdio_auth_env() {
                if let Some(token) = resolve_token(name, server).await {
                    env.push((var, token));
                }
            }
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
            // Inject a token in the configured auth header (bearer by default).
            if let Some(token) = resolve_token(name, server).await {
                let (header, prefix) = server.http_auth();
                headers.push((header, format!("{prefix}{token}")));
            }
            McpClient::connect_http(url, headers)
                .await
                .map_err(|e| e.to_string())
        }
    }
}

/// Resolve a server's token to inject. For OAuth, load the session and refresh
/// it if the access token has expired (persisting the new tokens). Otherwise,
/// env-first (via `key_env`) then the OS keychain.
pub(crate) async fn resolve_token(name: &str, server: &McpServer) -> Option<String> {
    if server.auth.as_deref() == Some("oauth") {
        return current_oauth_token(name).await;
    }
    let key_env = server.key_env.as_ref()?;
    rinne_config::secrets::resolve_api_key(&keychain_provider(name), key_env)
}

/// The keychain provider name for a server's OAuth session blob.
fn oauth_provider(name: &str) -> String {
    format!("mcp-oauth:{name}")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Load a server's OAuth session, refresh it if expired (persisting the result),
/// and return the current access token.
async fn current_oauth_token(name: &str) -> Option<String> {
    let provider = oauth_provider(name);
    let raw = rinne_config::secrets::load_secret(&provider)?;
    let session: rinne_mcp::OAuthSession = serde_json::from_str(&raw).ok()?;
    if session.is_expired(now_secs()) {
        match rinne_mcp::refresh(&session, now_secs()).await {
            Ok(fresh) => {
                if let Ok(json) = serde_json::to_string(&fresh) {
                    let _ = rinne_config::secrets::store_secret(&provider, &json);
                }
                return Some(fresh.access_token);
            }
            Err(e) => tracing::warn!("oauth refresh for `{name}` failed: {e}"),
        }
    }
    Some(session.access_token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_name_from_url_second_level_label() {
        assert_eq!(derive_name("https://mcp.notion.so/mcp", true), "notion");
        assert_eq!(derive_name("https://api.github.com/x", true), "github");
        assert_eq!(derive_name("http://localhost:3000/mcp", true), "localhost");
    }

    #[test]
    fn derives_name_from_stdio_package() {
        // `server-` prefix is trimmed; the package's final segment wins.
        assert_eq!(
            derive_name("npx -y @modelcontextprotocol/server-filesystem .", false),
            "filesystem"
        );
        assert_eq!(derive_name("/usr/local/bin/my-mcp-server", false), "my");
    }

    #[test]
    fn link_key_matches_same_endpoint_regardless_of_name() {
        let a = McpServer {
            transport: McpTransport::Stdio,
            command: Some("python3".into()),
            args: vec!["srv.py".into()],
            env: Default::default(),
            url: None,
            headers: Default::default(),
            key_env: None,
            enabled: true,
            tools_allow: vec!["*".into()],
            host_only: false,
            auth: None,
            auth_header: None,
        };
        let b = McpServer { ..a.clone() };
        assert_eq!(link_key(&a), link_key(&b));

        let http1 = McpServer {
            transport: McpTransport::Http,
            command: None,
            args: vec![],
            env: Default::default(),
            url: Some("https://x/mcp/".into()),
            headers: Default::default(),
            key_env: None,
            enabled: true,
            tools_allow: vec!["*".into()],
            host_only: false,
            auth: None,
            auth_header: None,
        };
        let http2 = McpServer {
            url: Some("https://x/mcp".into()),
            ..http1.clone()
        };
        // Trailing slash is normalized, so these are the same endpoint.
        assert_eq!(link_key(&http1), link_key(&http2));
        assert_ne!(link_key(&a), link_key(&http1));
    }

    #[test]
    fn sanitize_trims_noise_and_lowercases() {
        assert_eq!(sanitize_name("Server-Foo.Bar"), "foo-bar");
        assert_eq!(sanitize_name("Foo-server"), "foo");
        assert_eq!(sanitize_name("@scope/thing"), "scope-thing");
    }
}
