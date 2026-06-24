//! `rinne connect <backend>` — set up a worker (`CONTEXT.md` §9, §17).
//!
//! Harnesses authenticate natively (Rinne just surfaces the login). API
//! providers are configured from a built-in catalog so the user doesn't
//! hand-edit TOML. The work returns lines of output so the same logic backs both
//! the CLI command and the TUI `/connect`. Rinne stores only the env-var name —
//! never the key.

use anyhow::Result;

use rinne_config::known::{known_api_provider, KnownApiProvider, KNOWN_API_PROVIDERS};
use rinne_config::model::ApiProvider;
use rinne_config::probe::{harness_by_name, KnownHarness, WorkerFamily, WorkerStatus};

/// CLI entry: print the connect report.
pub async fn run(
    backend: &str,
    key: Option<String>,
    models: Vec<String>,
    base_url: Option<String>,
    add: bool,
) -> Result<()> {
    for line in connect_lines(backend, key.as_deref(), &models, base_url.as_deref(), add).await? {
        println!("{line}");
    }
    Ok(())
}

/// Connect a backend and return the user-facing report lines. An optional `key`
/// (for API providers) is stored securely in the OS keychain; `models` sets the
/// provider's model ladder; `base_url` overrides the endpoint; `add` appends the
/// key to the rotation pool instead of replacing it.
pub async fn connect_lines(
    backend: &str,
    key: Option<&str>,
    models: &[String],
    base_url: Option<&str>,
    add: bool,
) -> Result<Vec<String>> {
    let config = rinne_config::load_cwd()?;
    let mut out = Vec::new();

    if let Some(harness) = harness_by_name(backend) {
        connect_harness(harness, &mut out);
    } else {
        let known = known_api_provider(backend);
        let configured = config.backends.api.providers.get(backend).cloned();
        // A base_url override (or being known/configured) makes this an API
        // provider we can set up even if it's not in the catalog.
        if known.is_some() || configured.is_some() || base_url.is_some() {
            connect_api(backend, known, configured.as_ref(), key, models, base_url, add, &mut out).await;
        } else {
            unknown(backend, &mut out);
            return Ok(out);
        }
    }

    // Re-probe from a freshly-reloaded config (a write may have just landed).
    let fresh = rinne_config::load_cwd()?;
    let report = rinne_config::doctor(&fresh, true).await?;
    if let Some(w) = report.workers.iter().find(|w| w.name == backend) {
        let state = match &w.status {
            WorkerStatus::Available => "available ✔",
            WorkerStatus::NotInstalled => "not detected yet ·",
            WorkerStatus::SmokeTestFailed(_) => "error ✗",
        };
        out.push(format!("Re-probed `{backend}`: {state} (auth: {}).", w.auth_mode.label()));
    }
    Ok(out)
}

/// List every worker and connection status — harnesses, configured API
/// providers, and known-but-unconfigured providers. Backs `/workers`.
pub async fn list_lines() -> Result<Vec<String>> {
    let config = rinne_config::load_cwd()?;
    let report = rinne_config::doctor(&config, false).await?;
    let mut out = Vec::new();

    out.push("HARNESSES (auto-detected once installed + logged in):".to_string());
    for w in report.workers.iter().filter(|w| w.family == WorkerFamily::Harness) {
        let mark = match w.status {
            WorkerStatus::Available => "✔",
            WorkerStatus::NotInstalled => "·",
            WorkerStatus::SmokeTestFailed(_) => "✗",
        };
        let enabled = if w.enabled { "" } else { " (disabled)" };
        out.push(format!("  {mark} {:<14} {}{}", w.name, status_word(&w.status), enabled));
    }

    out.push("API PROVIDERS (configured):".to_string());
    let mut any_api = false;
    for w in report.workers.iter().filter(|w| w.family == WorkerFamily::Api) {
        any_api = true;
        let mark = if w.status.is_available() { "✔ key set" } else { "· key missing" };
        out.push(format!("  {:<14} {}", w.name, mark));
    }
    if !any_api {
        out.push("  (none configured)".to_string());
    }

    // Known providers the user hasn't set up yet.
    let unconfigured: Vec<&str> = KNOWN_API_PROVIDERS
        .iter()
        .map(|p| p.name)
        .filter(|n| !config.backends.api.providers.contains_key(*n))
        .collect();
    if !unconfigured.is_empty() {
        out.push(format!(
            "AVAILABLE TO ADD: {}  (e.g. `rinne connect {}`)",
            unconfigured.join(", "),
            unconfigured[0]
        ));
    }
    Ok(out)
}

fn connect_harness(harness: &KnownHarness, out: &mut Vec<String>) {
    out.push(format!("Connecting harness worker `{}`.", harness.name));
    out.push("Rinne holds no credentials — authenticate it natively:".to_string());
    out.push(format!("    {}", harness.login_hint));
    if let Some(env) = harness.override_env {
        if harness.footgun {
            out.push(format!(
                "⚠ Note: setting {env} OVERRIDES the subscription and bills the metered API account."
            ));
        }
    }
}

async fn connect_api(
    name: &str,
    known: Option<&'static KnownApiProvider>,
    configured: Option<&ApiProvider>,
    key: Option<&str>,
    models: &[String],
    base_url_override: Option<&str>,
    add: bool,
    out: &mut Vec<String>,
) {
    out.push(format!("Connecting API worker `{name}`."));

    let key_env = configured
        .map(|p| p.key_env.clone())
        .or_else(|| known.map(|k| k.key_env.to_string()))
        .unwrap_or_else(|| format!("{}_API_KEY", name.to_uppercase()));

    // Base URL: explicit override wins, then existing config, then catalog.
    // Normalize so a pasted full endpoint (…/chat/completions) becomes the base.
    let base_url = base_url_override
        .map(String::from)
        .or_else(|| configured.and_then(|p| p.base_url.clone()))
        .or_else(|| known.map(|k| k.base_url.to_string()))
        .map(|b| rinne_workers::transport::normalize_base_url(&b));

    // Models to persist: explicit `--model` wins, else keep existing, else the
    // catalog default.
    let final_models: Vec<String> = if !models.is_empty() {
        models.to_vec()
    } else if let Some(p) = configured {
        p.models.clone()
    } else {
        known.map(|k| k.models.iter().map(|m| m.to_string()).collect()).unwrap_or_default()
    };

    // Write config when first configuring, or whenever models/base_url change.
    if configured.is_none() || !models.is_empty() || base_url_override.is_some() {
        match base_url.as_deref() {
            Some(base) => {
                let refs: Vec<&str> = final_models.iter().map(|s| s.as_str()).collect();
                match rinne_config::write::add_api_provider(name, &key_env, base, &refs) {
                    Ok(path) => {
                        out.push(format!("Wrote [backends.api.{name}] to {}", path.display()));
                        out.push(format!("  base_url = {base}"));
                        if final_models.is_empty() {
                            out.push("  models = []  ← set the model: `rinne connect {name} <key> --model <id>`".to_string());
                        } else {
                            out.push(format!("  models = [{}]", final_models.join(", ")));
                        }
                    }
                    Err(e) => out.push(format!("Could not write config: {e}")),
                }
            }
            None => out.push(format!(
                "Need a base URL for `{name}` — add `base_url = \"…\"` under [backends.api.{name}]."
            )),
        }
    } else {
        out.push("Already in your config.".to_string());
    }

    // If a key was supplied, store it in the OS keychain (encrypted, persistent
    // — set once and forget). Never written to config in plaintext.
    if let Some(k) = key {
        let stored = if add {
            rinne_config::secrets::add_api_key(name, k).map(Some)
        } else {
            rinne_config::secrets::store_api_key(name, k).map(|_| None)
        };
        match stored {
            Ok(Some(n)) => out.push(format!("✔ key added to the keychain pool ({n} key{} for rotation).", if n == 1 { "" } else { "s" })),
            Ok(None) => out.push("✔ key stored securely in your OS keychain.".to_string()),
            Err(e) => {
                out.push(format!("⚠ could not use the keychain ({e})."));
                out.push(format!("  fall back to: export {key_env}=<your-key>"));
            }
        }
    } else {
        match rinne_config::secrets::key_source(name, &key_env) {
            Some(src) => out.push(format!("✔ key found ({src}).")),
            None => {
                out.push("Provide your key once — either of:".to_string());
                out.push(format!("  rinne connect {name} <your-key>   (stored securely in the OS keychain)"));
                out.push(format!("  export {key_env}=<your-key>        (per-shell env var)"));
                return;
            }
        }
    }

    // Verify the connection end-to-end so problems surface NOW, not mid-run.
    let resolved_key = key
        .map(String::from)
        .or_else(|| rinne_config::secrets::resolve_api_key(name, &key_env));
    match (base_url.as_deref(), final_models.first(), resolved_key.as_deref()) {
        (Some(base), Some(model), Some(k)) => {
            out.push("testing the connection…".to_string());
            match verify_api(base, k, model).await {
                Ok(()) => out.push(format!("✔ verified — `{model}` responded. `{name}` is ready (metered).")),
                Err(e) => {
                    out.push(format!("✗ test request failed: {e}"));
                    out.push("  check the base_url, model id, and key are for the SAME platform.".to_string());
                }
            }
        }
        (_, None, _) => {
            out.push(format!("Set a model to finish: `rinne connect {name} --model <id>` (or `rinne models {name}` to list)."))
        }
        _ => {}
    }
}

/// Send a minimal request to confirm endpoint + key + model actually work.
async fn verify_api(base_url: &str, key: &str, model: &str) -> std::result::Result<(), String> {
    use rinne_workers::transport::http::{ChatMessage, ChatRequest, OpenAiClient};
    let client = OpenAiClient::new(base_url, Some(key.to_string()));
    let req = ChatRequest {
        model: model.to_string(),
        messages: vec![ChatMessage::user("ping")],
        temperature: None,
        extra: None,
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = tokio_util::sync::CancellationToken::new();
    match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        client.chat_stream(&req, &tx, &cancel),
    )
    .await
    {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("timed out after 30s (endpoint slow, model cold-starting, or unreachable)".into()),
    }
}

fn unknown(backend: &str, out: &mut Vec<String>) {
    out.push(format!("Unknown backend `{backend}`."));
    out.push("Known harnesses (auto-detected once installed + logged in):".to_string());
    for h in rinne_config::probe::KNOWN_HARNESSES {
        out.push(format!("  • {}", h.name));
    }
    out.push("Known API providers (`rinne connect <name>`):".to_string());
    out.push(format!("  {}", KNOWN_API_PROVIDERS.iter().map(|p| p.name).collect::<Vec<_>>().join(", ")));
}

fn status_word(s: &WorkerStatus) -> String {
    match s {
        WorkerStatus::Available => "available".to_string(),
        WorkerStatus::NotInstalled => "not installed".to_string(),
        WorkerStatus::SmokeTestFailed(why) => format!("error: {why}"),
    }
}
