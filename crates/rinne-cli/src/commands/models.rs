//! `rinne models [provider]` — with a provider, list the models its key can
//! access (live `/v1/models` call, with pricing/context where reported). With no
//! provider, list every available worker and its model ladder, mirroring the
//! startup intro (`CONTEXT.md` §7).
//!
//! `rinne models --json` emits a stable machine-readable map used by the macOS app.

use std::collections::BTreeMap;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

/// List models. With a provider, the provider's live catalog; otherwise the full
/// worker/ladder overview (same data as the intro). `--json` emits structured data.
/// `--catalog` (with a provider) attaches the live remote model list for browsing.
pub async fn run(provider: Option<&str>, json: bool, catalog: bool) -> Result<()> {
    if json {
        let payload = match provider {
            Some(p) => json_for_provider(p, catalog).await,
            None => json_overview().await,
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }
    // Text mode: `--catalog` alone still means "show this provider's list".
    let lines = match provider {
        Some(p) => list_lines(p).await,
        None => overview_lines().await,
    };
    for line in lines {
        println!("{line}");
    }
    Ok(())
}

/// Exact model ids currently used by each available worker (descriptor + config).
pub async fn collect_worker_models() -> BTreeMap<String, Vec<String>> {
    let config = match rinne_config::load_cwd() {
        Ok(c) => c,
        Err(_) => return BTreeMap::new(),
    };
    let (registry, names) = match crate::runner::build_registry(&config).await {
        Ok(r) => r,
        Err(_) => return BTreeMap::new(),
    };
    let descriptors = registry.descriptors();
    let ladders = rinne_core::pool::profile(&descriptors).ladders();
    let mut out = BTreeMap::new();
    for name in &names {
        let from_desc: Vec<String> = descriptors
            .iter()
            .find(|d| d.name == *name)
            .map(|d| d.models.clone())
            .unwrap_or_default()
            .into_iter()
            .filter(|m| !m.is_empty())
            .collect();
        let models = if !from_desc.is_empty() {
            from_desc
        } else {
            model_ids_for(&config, &ladders, name)
        };
        out.insert(name.clone(), models);
    }
    out
}

async fn json_overview() -> Value {
    let config = rinne_config::load_cwd().ok();
    let workers = collect_worker_models().await;
    let mut body = json!({ "workers": workers });
    if let Some(c) = config {
        body["conductor"] = json!({
            "backend": c.conductor.backend.as_str(),
            "model": c.conductor.model,
        });
    }
    body
}

async fn json_for_provider(provider: &str, include_catalog: bool) -> Value {
    let config = match rinne_config::load_cwd() {
        Ok(c) => c,
        Err(_) => {
            return json!({
                "provider": provider,
                "workers": {},
                "configured": [],
            });
        }
    };

    // Configured / runtime ladder — what Rinne actually uses.
    let all = collect_worker_models().await;
    let configured = if let Some(m) = all.get(provider) {
        m.clone()
    } else {
        let empty = std::collections::HashMap::new();
        model_ids_for(&config, &empty, provider)
    };
    let mut workers = BTreeMap::new();
    if !configured.is_empty() {
        workers.insert(provider.to_string(), configured.clone());
    }

    let mut body = json!({
        "provider": provider,
        "workers": workers,
        "configured": configured,
        "conductor": {
            "backend": config.conductor.backend.as_str(),
            "model": config.conductor.model,
        },
    });

    if include_catalog {
        match live_catalog(&config, provider).await {
            Ok(models) => {
                let entries: Vec<Value> = models
                    .iter()
                    .map(|m| {
                        json!({
                            "id": m.id,
                            "prompt_price": m.prompt_price,
                            "context": m.context,
                        })
                    })
                    .collect();
                body["catalog"] = Value::Array(entries);
                body["catalog_count"] = json!(models.len());
            }
            Err(e) => {
                body["catalog"] = Value::Array(vec![]);
                body["catalog_count"] = json!(0);
                body["catalog_error"] = json!(e);
            }
        }
    }

    body
}

/// Live model catalog for an API provider (or conductor backend). Harnesses have no catalog.
///
/// Cloudflare Workers AI does **not** support OpenAI-compatible `GET /models`
/// (HTTP 405). For `cloudflare` we use the native
/// `GET /accounts/{id}/ai/models/search` API instead.
async fn live_catalog(
    config: &rinne_config::Config,
    provider: &str,
) -> Result<Vec<rinne_workers::transport::http::DiscoveredModel>, String> {
    if config.backends.harness.enabled.iter().any(|h| h == provider) {
        return Err(format!("`{provider}` is a harness CLI — no remote model catalog."));
    }

    // Cloudflare: never hit /ai/v1/models — it always 405s.
    if provider.eq_ignore_ascii_case("cloudflare") || provider.eq_ignore_ascii_case("cf") {
        return cloudflare_catalog(config).await;
    }

    let (base, key) = resolve_endpoint(config, provider)?;
    match fetch(&base, &key).await {
        Ok(models) => Ok(models),
        Err(e) => {
            let msg = e.to_string();
            // Some CF-shaped base_urls are registered under a custom name.
            if msg.contains("405")
                && (base.contains("cloudflare.com") || base.contains("/ai/v1"))
            {
                if let Ok(cf) = cloudflare_catalog(config).await {
                    return Ok(cf);
                }
            }
            Err(msg)
        }
    }
}

/// Cloudflare Workers AI catalog via native REST (not OpenAI /models).
async fn cloudflare_catalog(
    config: &rinne_config::Config,
) -> Result<Vec<rinne_workers::transport::http::DiscoveredModel>, String> {
    let (base, key) = resolve_endpoint(config, "cloudflare").or_else(|_| {
        // Allow conductor-only cloudflare setup without [backends.api.cloudflare].
        resolve_endpoint(config, "cf").or_else(|_| {
            let cond = &config.conductor;
            if matches!(
                cond.backend,
                rinne_config::model::ConductorBackend::Cloudflare
            ) {
                let base = rinne_conductor::conductor_base_url(cond).ok_or_else(|| {
                    "cloudflare needs account_id — `rinne config set conductor.account_id <id>`"
                        .to_string()
                })?;
                let key = match rinne_conductor::conductor_credential(cond) {
                    Some((provider, env)) => {
                        rinne_config::secrets::resolve_api_key(&provider, &env).ok_or_else(|| {
                            "no key for cloudflare — `rinne connect cloudflare <token>`".to_string()
                        })?
                    }
                    None => {
                        return Err(
                            "no key for cloudflare — `rinne connect cloudflare <token>`".to_string(),
                        )
                    }
                };
                Ok((base, key))
            } else {
                Err("cloudflare is not configured — `rinne connect cloudflare <token> --base-url …`".into())
            }
        })
    })?;

    let account_id = config
        .conductor
        .account_id
        .clone()
        .or_else(|| {
            rinne_workers::transport::http::cloudflare_account_id_from_base_url(&base)
        })
        .ok_or_else(|| {
            "cloudflare catalog needs account_id — set conductor.account_id or use base_url …/accounts/<id>/ai/v1"
                .to_string()
        })?;

    match rinne_workers::transport::http::list_cloudflare_workers_ai_models(&account_id, &key)
        .await
    {
        Ok(models) if !models.is_empty() => Ok(models),
        Ok(_) => Ok(rinne_workers::transport::http::cloudflare_text_model_fallback()),
        Err(e) => {
            // Prefer a usable shortlist over a hard error in the Models tab.
            // Surface the failure as empty-catalog is worse UX than a curated list.
            tracing::warn!(error = %e, "cloudflare models/search failed — using curated fallback");
            let mut models = rinne_workers::transport::http::cloudflare_text_model_fallback();
            // Ensure the user's configured pin appears at the top.
            for id in configured_api_model_ids(config, "cloudflare") {
                if !models.iter().any(|m| m.id == id) {
                    models.insert(
                        0,
                        rinne_workers::transport::http::DiscoveredModel {
                            id,
                            prompt_price: None,
                            context: None,
                        },
                    );
                }
            }
            Ok(models)
        }
    }
}

/// A text overview of every available worker and its model ladder — the same
/// data the startup intro shows, formatted for the `/models` (no-arg) command.
pub async fn overview_lines() -> Vec<String> {
    let config = match rinne_config::load_cwd() {
        Ok(c) => c,
        Err(e) => return vec![format!("config error: {e}")],
    };
    let map = collect_worker_models().await;
    if map.is_empty() {
        return vec![
            "no workers available — `rinne doctor` to see why, or `/connect` to add one.".to_string(),
        ];
    }
    let mut out = vec![format!("{} worker(s) available:", map.len())];
    for (name, models) in &map {
        let detail = if models.is_empty() {
            "(no model configured)".to_string()
        } else {
            models.join(" · ")
        };
        out.push(format!("  ✔ {name:<14} {detail}"));
    }
    out.push(format!(
        "conductor: {} · {}",
        config.conductor.backend.as_str(),
        config.conductor.model
    ));
    out.push("`/models <provider>` for a provider's full catalog with pricing.".to_string());
    out
}

/// Resolved model id list for a worker (never "default model").
fn model_ids_for(
    config: &rinne_config::Config,
    ladders: &std::collections::HashMap<String, Vec<String>>,
    name: &str,
) -> Vec<String> {
    if let Some(l) = ladders.get(name) {
        if !l.is_empty() {
            return l.clone();
        }
    }
    if let Some(p) = config.backends.api.providers.get(name) {
        if !p.models.is_empty() {
            return p.models.clone();
        }
        if let Some(m) = &p.model {
            if !m.is_empty() {
                return vec![m.clone()];
            }
        }
    }
    if let Some(m) = config.models.by_worker.get(name) {
        if !m.is_empty() {
            return vec![m.clone()];
        }
    }
    Vec::new()
}

/// Model list for a harness worker — its adapter ladder (cheap→strong) from the
/// live registry. Harnesses are CLIs with no `/v1/models` catalog, so this is the
/// authoritative model set Rinne will cascade through.
async fn harness_ladder_lines(config: &rinne_config::Config, harness: &str) -> Vec<String> {
    let (registry, names) = match crate::runner::build_registry(config).await {
        Ok(r) => r,
        Err(e) => return vec![format!("could not probe workers: {e}")],
    };
    if !names.iter().any(|n| n == harness) {
        return vec![format!(
            "`{harness}` is enabled but not available — `rinne doctor` to see why."
        )];
    }
    let ladders = rinne_core::pool::profile(&registry.descriptors()).ladders();
    match ladders.get(harness) {
        Some(l) if !l.is_empty() => {
            let mut out = vec![format!("`{harness}` model ladder (cheap→strong):")];
            for m in l {
                out.push(format!("  • {m}"));
            }
            out.push(format!("set a default: `rinne config set models.{harness} <model>`"));
            out
        }
        _ => {
            // Fall back to configured pin if the adapter exposes no ladder.
            if let Some(m) = config.models.by_worker.get(harness) {
                if !m.is_empty() {
                    return vec![
                        format!("`{harness}` configured model:"),
                        format!("  • {m}"),
                    ];
                }
            }
            vec![format!(
                "`{harness}` has no Rinne-managed model list (adapter default only)."
            )]
        }
    }
}

/// Fetch and format the model list for a provider (shared with the TUI).
/// Resolves the endpoint from either a configured API provider OR the conductor
/// backend (so e.g. `/models groq` works when groq is the conductor).
pub async fn list_lines(provider: &str) -> Vec<String> {
    let config = match rinne_config::load_cwd() {
        Ok(c) => c,
        Err(e) => return vec![format!("config error: {e}")],
    };

    // A harness has no HTTP catalog — show its model ladder from the registry.
    if config.backends.harness.enabled.iter().any(|h| h == provider) {
        return harness_ladder_lines(&config, provider).await;
    }

    let (base, key) = match resolve_endpoint(&config, provider) {
        Ok(bk) => bk,
        Err(msg) => return vec![msg],
    };

    // Configured ladder is what Rinne actually runs — always prefer it when set.
    let configured = configured_api_model_ids(&config, provider);

    match fetch(&base, &key).await {
        Ok(models) if models.is_empty() => {
            if !configured.is_empty() {
                return format_configured_api_models(provider, &configured);
            }
            configured_api_model_lines(&config, provider)
        }
        Ok(models) => {
            // If the user pinned exact model ids, list those first as the worker's models.
            if !configured.is_empty() {
                let mut out = format_configured_api_models(provider, &configured);
                out.push(String::new());
                out.push(format!(
                    "{} more model(s) available on `{provider}` (live catalog, sample):",
                    models.len()
                ));
                for m in models.iter().take(15) {
                    out.push(format!("  · {}", m.id));
                }
                if models.len() > 15 {
                    out.push(format!("  … +{} more", models.len() - 15));
                }
                out.push(format!(
                    "change with: `rinne connect {provider} --model <id>`"
                ));
                return out;
            }
            let mut out = vec![format!(
                "{} model(s) on `{provider}` (cheapest first):",
                models.len()
            )];
            for m in models.iter().take(40) {
                let price = m
                    .prompt_price
                    .map(|p| format!("${:.2}/M tok", p * 1_000_000.0))
                    .unwrap_or_else(|| "price n/a".into());
                let ctx = m
                    .context
                    .map(|c| format!("{}k ctx", c / 1000))
                    .unwrap_or_default();
                out.push(format!("  {:<48} {:<14} {}", m.id, price, ctx));
            }
            if models.len() > 40 {
                out.push(format!("  … +{} more", models.len() - 40));
            }
            out.push(format!(
                "set the ones you want: `rinne connect {provider} --model <id> --model <id>` (cheap→strong)"
            ));
            out
        }
        Err(e) => {
            // Cloudflare and some hosts reject GET /models — use configured ids.
            if !configured.is_empty() {
                let mut out = format_configured_api_models(provider, &configured);
                out.insert(
                    0,
                    format!("live catalog unavailable for `{provider}` — using configured model(s):"),
                );
                return out;
            }
            let mut out = configured_api_model_lines(&config, provider);
            if out.len() <= 1 {
                out = vec![format!("could not list models for `{provider}`: {e}")];
            } else {
                out.insert(
                    0,
                    format!("live catalog unavailable for `{provider}` ({e}) — using configured models:"),
                );
            }
            out
        }
    }
}

/// Exact model ids from config for an API provider (used by Rinne at runtime).
fn configured_api_model_ids(config: &rinne_config::Config, provider: &str) -> Vec<String> {
    if let Some(p) = config.backends.api.providers.get(provider) {
        if !p.models.is_empty() {
            return p.models.clone();
        }
        if let Some(m) = &p.model {
            if !m.is_empty() {
                return vec![m.clone()];
            }
        }
    }
    if let Some(m) = config.models.by_worker.get(provider) {
        if !m.is_empty() {
            return vec![m.clone()];
        }
    }
    Vec::new()
}

fn format_configured_api_models(provider: &str, ids: &[String]) -> Vec<String> {
    let mut out = vec![format!("`{provider}` configured model(s) (used by Rinne):")];
    for m in ids {
        out.push(format!("  • {m}"));
    }
    out
}

/// Models from `[backends.api.<provider>].models` / `.model` (never "default model").
fn configured_api_model_lines(config: &rinne_config::Config, provider: &str) -> Vec<String> {
    if let Some(p) = config.backends.api.providers.get(provider) {
        let mut ids: Vec<String> = p.models.clone();
        if ids.is_empty() {
            if let Some(m) = &p.model {
                if !m.is_empty() {
                    ids.push(m.clone());
                }
            }
        }
        if !ids.is_empty() {
            let mut out = vec![format!("`{provider}` configured model(s) (used by Rinne):")];
            for m in ids {
                out.push(format!("  • {m}"));
            }
            out.push(format!(
                "change with: `rinne connect {provider} --model <id>` or `rinne config set backends.api.{provider}.models`"
            ));
            return out;
        }
    }
    if let Some(m) = config.models.by_worker.get(provider) {
        if !m.is_empty() {
            return vec![
                format!("`{provider}` default model pin:"),
                format!("  • {m}"),
            ];
        }
    }
    vec![format!(
        "`{provider}` has no model ids configured — `rinne connect {provider} --model <id>`"
    )]
}

/// Resolve `(base_url, api_key)` for a name that is either a configured API
/// provider or the conductor backend. Returns a user-facing error string on
/// failure (not configured / no base_url / no key).
fn resolve_endpoint(config: &rinne_config::Config, name: &str) -> Result<(String, String), String> {
    // 1) A configured `[backends.api.<name>]` provider.
    if let Some(p) = config.backends.api.providers.get(name) {
        let base = p
            .base_url
            .clone()
            .ok_or_else(|| format!("`{name}` has no base_url set in config."))?;
        let key = rinne_config::secrets::resolve_api_key(name, &p.key_env).ok_or_else(|| {
            format!("no key for `{name}` — `rinne connect {name} <key>` or export {}.", p.key_env)
        })?;
        return Ok((base, key));
    }

    // 2) The conductor backend (e.g. groq/nvidia/cloudflare), which is OpenAI-
    //    compatible and reuses its own credential. This is what makes
    //    `/models groq` work when groq is the conductor.
    let cond = &config.conductor;
    let backend_name = format!("{:?}", cond.backend).to_lowercase();
    if backend_name == name {
        let base = rinne_conductor::conductor_base_url(cond).ok_or_else(|| {
            format!("`{name}` (conductor) has no endpoint — set [conductor].base_url or account_id.")
        })?;
        // A keyless backend (e.g. local Ollama) has no credential — query it with
        // an empty key. A backend that DOES expect a key but has none configured
        // is a real error.
        let key = match rinne_conductor::conductor_credential(cond) {
            None => String::new(),
            Some((provider, env)) => rinne_config::secrets::resolve_api_key(&provider, &env)
                .ok_or_else(|| format!("no key for the `{name}` conductor backend."))?,
        };
        return Ok((base, key));
    }

    // Unknown name: list what IS valid so the user knows what to type.
    let mut valid: Vec<String> = config.backends.harness.enabled.clone();
    valid.extend(config.backends.api.providers.keys().cloned());
    valid.push(format!("{:?}", config.conductor.backend).to_lowercase()); // conductor
    valid.sort();
    valid.dedup();
    Err(format!(
        "`{name}` is not a known worker. Try one of: {}. \
         Or `rinne connect {name} <key> --base-url <url>` to add an API provider.",
        valid.join(", ")
    ))
}

async fn fetch(
    base: &str,
    key: &str,
) -> Result<Vec<rinne_workers::transport::http::DiscoveredModel>> {
    let client = rinne_workers::transport::http::OpenAiClient::new(base, Some(key.to_string()));
    client.list_models().await.map_err(|e| anyhow!(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::resolve_endpoint;
    use rinne_config::model::{ConductorBackend, Config};

    #[test]
    fn resolve_endpoint_matches_conductor_backend() {
        // The conductor backend name must hit the conductor branch — i.e. it must
        // NOT fall through to the generic "not configured" error. Whether a key
        // exists (env or keychain) is environment-dependent, so we only assert
        // that resolution either succeeds or fails on the key, never on identity.
        let mut cfg = Config::default();
        cfg.conductor.backend = ConductorBackend::Groq;
        cfg.conductor.model = "openai/gpt-oss-120b".into();
        match resolve_endpoint(&cfg, "groq") {
            Ok((base, _key)) => assert!(base.contains("groq"), "wrong base: {base}"),
            Err(e) => assert!(
                e.contains("no key for the `groq` conductor backend"),
                "should fail only on key, not identity: {e}"
            ),
        }
    }

    #[test]
    fn resolve_endpoint_unknown_name_lists_valid() {
        let cfg = Config::default();
        let err = resolve_endpoint(&cfg, "definitely-not-a-backend").unwrap_err();
        assert!(err.contains("not a known worker"), "{err}");
        // It should suggest valid names (default config enables claude-code).
        assert!(err.contains("claude-code"), "should list valid names: {err}");
    }
}
