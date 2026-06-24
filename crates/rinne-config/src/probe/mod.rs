//! The `doctor` probe (`CONTEXT.md` §9, §17, §19).
//!
//! Detects installed harnesses on `PATH`, smoke-tests their headless surface,
//! reads configured API keys, classifies each worker's auth mode, and catches
//! the Claude `ANTHROPIC_API_KEY` billing footgun. Results are cacheable so
//! `doctor` need not re-probe on every invocation.

mod registry;
mod types;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use tokio::process::Command;

use rinne_core::Result;

use crate::model::Config;

pub use registry::{harness_by_name, KnownHarness, KNOWN_HARNESSES};
pub use types::{AuthMode, DoctorReport, WorkerFamily, WorkerProbe, WorkerStatus};

/// How long a smoke test may run before it is considered hung. Cursor's `-p` is
/// known to hang, so every probe is bounded (`CONTEXT.md` §16, §21).
const SMOKE_TIMEOUT: Duration = Duration::from_secs(8);

/// Cacheable installation status for each harness, keyed by worker name.
///
/// Only the expensive, env-independent part of the probe (PATH lookup + smoke
/// test) lives here. Auth mode, the footgun, and API-key presence are derived
/// from the live environment on every run and are never cached.
pub type InstallMap = BTreeMap<String, WorkerStatus>;

/// The result of a probe: the report plus the cacheable installation map.
pub struct ProbeOutcome {
    pub report: DoctorReport,
    pub installs: InstallMap,
}

/// Run the full probe.
///
/// `cached_installs` supplies previously-detected harness installation statuses
/// to skip the expensive PATH/smoke-test step; pass `None` to force fresh
/// detection. Auth classification and API-key checks are always recomputed from
/// the current environment regardless.
pub async fn run(config: &Config, cached_installs: Option<&InstallMap>) -> Result<ProbeOutcome> {
    let mut workers = Vec::new();
    let mut warnings = Vec::new();
    let mut installs = InstallMap::new();

    for harness in KNOWN_HARNESSES {
        let enabled = config
            .backends
            .harness
            .enabled
            .iter()
            .any(|n| n == harness.name);

        // Installation status: reuse the cache when available, else detect now.
        let status = match cached_installs.and_then(|m| m.get(harness.name)) {
            Some(s) => s.clone(),
            None => detect_installation(harness).await,
        };
        installs.insert(harness.name.to_string(), status.clone());

        let probe = classify_harness(harness, enabled, status);
        // Bubble per-worker footgun warnings up to the run level too, so a user
        // skimming the summary cannot miss them.
        for w in &probe.warnings {
            warnings.push(format!("{}: {}", probe.name, w));
        }
        workers.push(probe);
    }

    for (name, provider) in &config.backends.api.providers {
        workers.push(probe_api(name, &provider.key_env));
    }

    let recommendations = pool_recommendations(&workers);

    Ok(ProbeOutcome {
        report: DoctorReport {
            workers,
            warnings,
            recommendations,
        },
        installs,
    })
}

/// Suggest the single cheapest quality upgrade for a thin pool: when every
/// available worker is the same vendor family, recommend a cheap second-family
/// API key used purely as the evaluator (`CONTEXT.md` §7).
fn pool_recommendations(workers: &[WorkerProbe]) -> Vec<String> {
    use std::collections::BTreeSet;
    // Consider the pool Rinne will actually route to: enabled + available.
    let families: BTreeSet<&str> = workers
        .iter()
        .filter(|w| w.enabled && w.status.is_available())
        .map(|w| rinne_core::priors::family_of_worker(&w.name))
        .filter(|f| *f != "unknown")
        .collect();

    if families.len() == 1 {
        let fam = families.iter().next().copied().unwrap_or("one family");
        vec![format!(
            "Single-family pool ({fam}). For stronger evaluator independence, add a cheap \
             second-family API key (e.g. DeepSeek or Gemini Flash) used only for the evaluator \
             role — it restores blind-spot independence for pennies, and Rinne meters nothing."
        )]
    } else {
        Vec::new()
    }
}

/// The expensive, cacheable half: is the harness binary present and healthy?
async fn detect_installation(harness: &KnownHarness) -> WorkerStatus {
    match find_on_path(harness.binary) {
        None => WorkerStatus::NotInstalled,
        Some(path) => smoke_test(&path, harness.smoke_args).await,
    }
}

/// The cheap, always-fresh half: classify auth mode and footgun warnings from
/// the live environment, combining with an already-known installation status.
fn classify_harness(
    harness: &KnownHarness,
    enabled: bool,
    status: WorkerStatus,
) -> WorkerProbe {
    let override_active = harness
        .override_env
        .map(|env| std::env::var_os(env).is_some())
        .unwrap_or(false);

    let (auth_mode, warnings) = classify_auth(harness, override_active);

    WorkerProbe {
        name: harness.name.to_string(),
        family: WorkerFamily::Harness,
        status,
        auth_mode,
        enabled,
        warnings,
    }
}

/// Pure auth classification: given whether the override env var is present,
/// return the effective auth mode and any warnings. Kept env-free so it can be
/// unit-tested deterministically.
fn classify_auth(harness: &KnownHarness, override_active: bool) -> (AuthMode, Vec<String>) {
    if !override_active {
        return (harness.base_auth, Vec::new());
    }
    let auth_mode = AuthMode::ApiKey;
    let mut warnings = Vec::new();
    if let Some(env) = harness.override_env {
        if harness.footgun {
            warnings.push(format!(
                "{env} is set — this OVERRIDES the subscription login and bills the \
                 metered API account. Unset it to use your subscription."
            ));
        } else {
            warnings.push(format!(
                "{env} is set — using metered API billing instead of the subscription login."
            ));
        }
    }
    (auth_mode, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude() -> &'static KnownHarness {
        harness_by_name("claude-code").unwrap()
    }

    #[test]
    fn subscription_when_no_override() {
        let (mode, warns) = classify_auth(claude(), false);
        assert_eq!(mode, AuthMode::Subscription);
        assert!(warns.is_empty());
    }

    #[test]
    fn footgun_flips_to_metered_with_loud_warning() {
        let (mode, warns) = classify_auth(claude(), true);
        assert_eq!(mode, AuthMode::ApiKey);
        assert!(mode.is_metered());
        assert_eq!(warns.len(), 1);
        assert!(warns[0].contains("ANTHROPIC_API_KEY"));
        assert!(warns[0].contains("OVERRIDES"));
    }

    #[test]
    fn non_footgun_override_warns_softly() {
        let grok = harness_by_name("grok").unwrap();
        let (mode, warns) = classify_auth(grok, true);
        assert_eq!(mode, AuthMode::ApiKey);
        assert_eq!(warns.len(), 1);
        assert!(warns[0].contains("XAI_API_KEY"));
        assert!(!warns[0].contains("OVERRIDES"));
    }
}

/// Probe an API worker: present and metered iff a key is available (from the
/// env var or the OS keychain).
fn probe_api(name: &str, key_env: &str) -> WorkerProbe {
    let key_set = crate::secrets::has_api_key(name, key_env);
    let status = if key_set {
        WorkerStatus::Available
    } else {
        WorkerStatus::NotInstalled
    };
    let mut warnings = Vec::new();
    if !key_set {
        warnings.push(format!(
            "no key — set {key_env} or run `rinne connect {name} <key>`"
        ));
    }
    WorkerProbe {
        name: name.to_string(),
        family: WorkerFamily::Api,
        // API workers are always the user's own key, always metered (§9).
        auth_mode: AuthMode::ApiKey,
        status,
        enabled: true,
        warnings,
    }
}

/// Run a bounded smoke test, mapping success/timeout/failure to a status.
async fn smoke_test(binary: &std::path::Path, args: &[&str]) -> WorkerStatus {
    let mut cmd = Command::new(binary);
    cmd.args(args);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);

    match tokio::time::timeout(SMOKE_TIMEOUT, cmd.output()).await {
        Ok(Ok(output)) if output.status.success() => WorkerStatus::Available,
        Ok(Ok(output)) => {
            let code = output
                .status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string());
            WorkerStatus::SmokeTestFailed(format!("exited {code}"))
        }
        Ok(Err(e)) => WorkerStatus::SmokeTestFailed(e.to_string()),
        Err(_) => WorkerStatus::SmokeTestFailed("smoke test timed out".to_string()),
    }
}

/// Find an executable on `PATH`, honoring `PATHEXT` on Windows.
fn find_on_path(binary: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".to_string())
            .split(';')
            .map(|s| s.to_string())
            .collect()
    } else {
        vec![String::new()]
    };

    for dir in std::env::split_paths(&path_var) {
        for ext in &exts {
            let candidate = dir.join(format!("{binary}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}
