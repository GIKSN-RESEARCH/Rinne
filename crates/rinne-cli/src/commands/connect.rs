//! `rinne connect <backend>` — surface the native login for a worker, then
//! re-probe to confirm (`CONTEXT.md` §9, §17).
//!
//! Rinne authenticates nothing and holds no credentials. `connect` tells the
//! user the exact native command their worker uses, then re-runs the probe so
//! they can see the worker flip to available.

use anyhow::Result;

use rinne_config::probe::{harness_by_name, WorkerStatus};

pub async fn run(backend: &str) -> Result<()> {
    let config = rinne_config::load_cwd()?;

    if let Some(harness) = harness_by_name(backend) {
        println!("Connecting harness worker `{}`.", harness.name);
        println!("\nRinne holds no credentials — authenticate it natively:\n");
        println!("    {}", harness.login_hint);
        if let Some(env) = harness.override_env {
            if harness.footgun {
                println!(
                    "\n⚠ Note: setting {env} will OVERRIDE the subscription and bill the metered \
                     API account."
                );
            }
        }
        println!("\nTip: run the command above with a leading `!` in this session, then re-run \
                  `rinne connect {}`.\n", harness.name);
    } else if let Some(provider) = config.backends.api.providers.get(backend) {
        println!("Connecting API worker `{backend}`.");
        println!(
            "\nRinne holds no credentials — export your key into {}:\n",
            provider.key_env
        );
        println!("    export {}=<your-key>\n", provider.key_env);
    } else {
        println!("Unknown backend `{backend}`.");
        println!("\nKnown harnesses:");
        for h in rinne_config::probe::KNOWN_HARNESSES {
            println!("  • {}", h.name);
        }
        if !config.backends.api.providers.is_empty() {
            println!("Configured API providers:");
            for name in config.backends.api.providers.keys() {
                println!("  • {name}");
            }
        }
        return Ok(());
    }

    // Force a fresh probe so the user sees the post-login state immediately.
    let report = rinne_config::doctor(&config, true).await?;
    if let Some(w) = report.workers.iter().find(|w| w.name == backend) {
        let state = match &w.status {
            WorkerStatus::Available => "available ✔",
            WorkerStatus::NotInstalled => "not detected yet ·",
            WorkerStatus::SmokeTestFailed(_) => "error ✗",
        };
        println!("Re-probed `{backend}`: {state} (auth: {}).", w.auth_mode.label());
    }
    Ok(())
}
