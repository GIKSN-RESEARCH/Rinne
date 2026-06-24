//! `rinne forget <provider>` — delete a stored API key from the OS keychain
//! (`CONTEXT.md` §9). The companion to `rinne connect <provider> <key>`.

use anyhow::Result;

/// CLI entry: print the forget report.
pub async fn run(provider: &str) -> Result<()> {
    for line in forget_lines(provider) {
        println!("{line}");
    }
    Ok(())
}

/// Remove a provider's keychain key and return report lines (shared by the CLI
/// and the TUI `/forget`).
pub fn forget_lines(provider: &str) -> Vec<String> {
    let mut out = Vec::new();
    let had = rinne_config::secrets::keychain_key(provider).is_some();
    let _ = rinne_config::secrets::delete_api_key(provider);

    if had {
        out.push(format!("✔ removed `{provider}`'s key from your OS keychain — it's forgotten."));
    } else {
        out.push(format!("No keychain key stored for `{provider}` (nothing to remove)."));
    }

    // Forgetting the keychain copy doesn't unset an environment variable, so say
    // so if one is still providing the key.
    let key_env = rinne_config::load_cwd()
        .ok()
        .and_then(|c| c.backends.api.providers.get(provider).map(|p| p.key_env.clone()))
        .or_else(|| {
            rinne_config::known::known_api_provider(provider).map(|k| k.key_env.to_string())
        });
    if let Some(env) = key_env {
        if std::env::var_os(&env).is_some() {
            out.push(format!(
                "Note: {env} is still set in your environment — unset it to fully forget."
            ));
        }
    }
    out
}
