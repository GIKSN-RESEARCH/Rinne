//! Background new-release checking against GitHub Releases.
//!
//! Mirrors the doctor-probe cache (`cache.rs`): the latest tag is fetched at
//! most once per [`CACHE_TTL`] and persisted under the global cache dir, so the
//! network call rarely runs and never blocks a command. Disabled by the
//! `[update] check = false` config or the `RINNE_NO_UPDATE_CHECK` env var.

use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use rinne_core::{Result, RinneError};

use crate::paths;

/// The repository whose releases are checked.
const REPO: &str = "GIKSN-RESEARCH/Rinne";

/// How long a cached release check is considered fresh.
const CACHE_TTL: Duration = Duration::from_secs(60 * 60 * 24);

/// Env var that disables the check entirely when set to a non-empty value.
const DISABLE_ENV: &str = "RINNE_NO_UPDATE_CHECK";

#[derive(Debug, Serialize, Deserialize)]
struct CachedRelease {
    /// Unix seconds when the check ran.
    checked_at: u64,
    /// The latest release tag, normalized without a leading `v`, e.g. `0.2.0`.
    latest: String,
}

/// A newer release than what is running.
#[derive(Debug, Clone)]
pub struct UpdateAvailable {
    pub current: String,
    pub latest: String,
}

/// Resolve whether a newer release is available, using the cache when fresh and
/// otherwise querying GitHub. `current` is typically `env!("CARGO_PKG_VERSION")`.
///
/// Returns `Ok(None)` when up to date, disabled, or the check could not run —
/// this is best-effort and must never surface an error to the caller.
pub async fn check(current: &str) -> Option<UpdateAvailable> {
    if std::env::var(DISABLE_ENV)
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        return None;
    }

    let latest = match load_fresh() {
        Some(tag) => tag,
        None => {
            let fetched = fetch_latest().await.ok()?;
            // Best-effort cache write; a failure must not block the check.
            if let Err(e) = store(&fetched) {
                tracing::debug!("could not cache update check: {e}");
            }
            fetched
        }
    };

    if is_newer(&latest, current) {
        Some(UpdateAvailable {
            current: current.to_string(),
            latest,
        })
    } else {
        None
    }
}

/// Load the cached latest tag if present and still fresh.
fn load_fresh() -> Option<String> {
    let path = paths::update_cache_file()?;
    let bytes = std::fs::read(&path).ok()?;
    let cached: CachedRelease = serde_json::from_slice(&bytes).ok()?;

    let age = now_secs().checked_sub(cached.checked_at)?;
    if Duration::from_secs(age) <= CACHE_TTL {
        Some(cached.latest)
    } else {
        None
    }
}

/// Persist the latest tag to the cache (best-effort dir creation).
fn store(latest: &str) -> Result<()> {
    let path = paths::update_cache_file()
        .ok_or_else(|| RinneError::Config("no cache directory available".into()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let cached = CachedRelease {
        checked_at: now_secs(),
        latest: latest.to_string(),
    };
    std::fs::write(&path, serde_json::to_vec(&cached)?)?;
    Ok(())
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
}

/// Fetch the latest release tag from the GitHub API, normalized without `v`.
async fn fetch_latest() -> Result<String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .user_agent(concat!("rinne/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| RinneError::Config(format!("http client: {e}")))?;

    let release: GithubRelease = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| RinneError::Config(format!("update check request: {e}")))?
        .error_for_status()
        .map_err(|e| RinneError::Config(format!("update check status: {e}")))?
        .json()
        .await
        .map_err(|e| RinneError::Config(format!("update check decode: {e}")))?;

    Ok(normalize(&release.tag_name))
}

/// Strip a leading `v` and surrounding whitespace from a tag.
fn normalize(tag: &str) -> String {
    tag.trim().trim_start_matches('v').to_string()
}

/// Compare dotted numeric versions: is `latest` strictly newer than `current`?
///
/// Pre-release suffixes (e.g. `-rc.1`) are ignored for the comparison; a tag
/// that fails to parse is treated as not newer, so a malformed release never
/// nags the user.
fn is_newer(latest: &str, current: &str) -> bool {
    match (parse(latest), parse(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

/// Parse `major.minor.patch` into a comparable tuple, ignoring any pre-release
/// or build suffix after the first `-` or `+`.
fn parse(version: &str) -> Option<(u64, u64, u64)> {
    let core = version.split(['-', '+']).next().unwrap_or(version);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_versions_detected() {
        assert!(is_newer("0.2.0", "0.1.6"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("0.1.7", "0.1.6"));
    }

    #[test]
    fn same_or_older_not_newer() {
        assert!(!is_newer("0.1.6", "0.1.6"));
        assert!(!is_newer("0.1.5", "0.1.6"));
        assert!(!is_newer("0.1.0", "0.2.0"));
    }

    #[test]
    fn normalizes_leading_v() {
        assert_eq!(normalize("v0.1.6"), "0.1.6");
        assert_eq!(normalize(" v0.2.0 "), "0.2.0");
    }

    #[test]
    fn prerelease_suffix_ignored() {
        assert_eq!(parse("0.2.0-rc.1"), Some((0, 2, 0)));
        assert!(!is_newer("0.1.6-rc.1", "0.1.6"));
    }

    #[test]
    fn malformed_tag_never_newer() {
        assert!(!is_newer("not-a-version", "0.1.6"));
    }
}
