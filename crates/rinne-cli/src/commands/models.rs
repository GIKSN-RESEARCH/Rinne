//! `rinne models [provider]` — with a provider, list the models its key can
//! access (live `/v1/models` call, with pricing/context where reported). With no
//! provider, list every available worker and its model ladder, mirroring the
//! startup intro (`CONTEXT.md` §7).

use anyhow::{anyhow, Result};

/// List models. With a provider, the provider's live catalog; otherwise the full
/// worker/ladder overview (same data as the intro).
pub async fn run(provider: Option<&str>) -> Result<()> {
    let lines = match provider {
        Some(p) => list_lines(p).await,
        None => overview_lines().await,
    };
    for line in lines {
        println!("{line}");
    }
    Ok(())
}

/// A text overview of every available worker and its model ladder — the same
/// data the startup intro shows, formatted for the `/models` (no-arg) command.
pub async fn overview_lines() -> Vec<String> {
    let config = match rinne_config::load_cwd() {
        Ok(c) => c,
        Err(e) => return vec![format!("config error: {e}")],
    };
    let (registry, names) = match crate::runner::build_registry(&config).await {
        Ok(r) => r,
        Err(e) => return vec![format!("could not probe workers: {e}")],
    };
    if names.is_empty() {
        return vec![
            "no workers available — `rinne doctor` to see why, or `/connect` to add one.".to_string(),
        ];
    }
    let ladders = rinne_core::pool::profile(&registry.descriptors()).ladders();
    let mut out = vec![format!("{} worker(s) available:", names.len())];
    for name in &names {
        let detail = match ladders.get(name) {
            Some(l) if l.len() > 1 => l.join(" · "),
            _ => "default model".to_string(),
        };
        out.push(format!("  ✔ {name:<14} {detail}"));
    }
    out.push(format!(
        "conductor: {} · {}",
        format!("{:?}", config.conductor.backend).to_lowercase(),
        config.conductor.model
    ));
    out.push("`/models <provider>` for a provider's full catalog with pricing.".to_string());
    out
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
        Some(l) if l.len() > 1 => {
            let mut out = vec![format!("`{harness}` model ladder (cheap→strong):")];
            for m in l {
                out.push(format!("  • {m}"));
            }
            out.push(format!("set a default: `rinne config set models.{harness} <model>`"));
            out
        }
        _ => vec![format!(
            "`{harness}` uses its own default model (no Rinne-managed ladder)."
        )],
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

    match fetch(&base, &key).await {
        Ok(models) if models.is_empty() => vec![format!("`{provider}` returned no models.")],
        Ok(models) => {
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
        Err(e) => vec![format!("could not list models for `{provider}`: {e}")],
    }
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
