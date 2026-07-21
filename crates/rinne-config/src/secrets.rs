//! Optional persistent storage for API keys in the OS keychain.
//!
//! NOTE — this is a deliberate, user-requested deviation from the locked §9
//! principle that "Rinne holds no credentials." To let a user set a key once and
//! forget it, a key can be saved to the platform keychain (macOS Keychain,
//! Windows Credential Manager, Linux Secret Service) — encrypted and OS-managed,
//! never written to Rinne's config files in plaintext. Resolution always prefers
//! an explicit environment variable, so the env-var workflow is unchanged.

use rinne_core::{Result, RinneError};

/// Keychain service name under which Rinne stores keys.
const SERVICE: &str = "rinne";

/// Read a provider's key pool from the keychain (a JSON array; tolerates a bare
/// legacy single-key string).
fn read_keys(provider: &str) -> Vec<String> {
    // See `limits::read_claude_credentials_raw` — a freshly built binary is not
    // in the Keychain item's ACL, so macOS blocks on a password prompt. Skipping
    // the read yields "no key", which every caller already handles.
    if std::env::var_os("NO_KEYCHAIN_RINNE").is_some() {
        return Vec::new();
    }

    let entry = match keyring::Entry::new(SERVICE, provider) {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!("keychain unavailable for `{provider}`: {e}");
            return Vec::new();
        }
    };
    let raw = match entry.get_password() {
        Ok(raw) => raw,
        // "not found" is the normal no-key case; anything else (locked/denied)
        // is worth a breadcrumb so a stored-but-unreadable key is diagnosable.
        Err(keyring::Error::NoEntry) => return Vec::new(),
        Err(e) => {
            tracing::debug!("keychain read failed for `{provider}`: {e}");
            return Vec::new();
        }
    };
    serde_json::from_str::<Vec<String>>(&raw).unwrap_or_else(|_| vec![raw])
}

fn write_keys(provider: &str, keys: &[String]) -> Result<()> {
    let entry = keyring::Entry::new(SERVICE, provider)
        .map_err(|e| RinneError::Config(format!("keychain unavailable: {e}")))?;
    if keys.is_empty() {
        let _ = entry.delete_credential();
        return Ok(());
    }
    let json = serde_json::to_string(keys).map_err(RinneError::Json)?;
    entry
        .set_password(&json)
        .map_err(|e| RinneError::Config(format!("could not store key: {e}")))?;
    Ok(())
}

/// Store a provider's key, REPLACING any existing pool.
pub fn store_api_key(provider: &str, key: &str) -> Result<()> {
    write_keys(provider, &[key.to_string()])
}

/// Add a key to a provider's pool (for rotation across rate limits).
pub fn add_api_key(provider: &str, key: &str) -> Result<usize> {
    let mut keys = read_keys(provider);
    if !keys.iter().any(|k| k == key) {
        keys.push(key.to_string());
    }
    let n = keys.len();
    write_keys(provider, &keys)?;
    Ok(n)
}

/// Remove a provider's entire key pool.
pub fn delete_api_key(provider: &str) -> Result<()> {
    if let Ok(entry) = keyring::Entry::new(SERVICE, provider) {
        let _ = entry.delete_credential();
    }
    Ok(())
}

/// The first keychain key for a provider (back-compat single-key accessor).
pub fn keychain_key(provider: &str) -> Option<String> {
    read_keys(provider).into_iter().next()
}

/// Store an opaque secret blob (e.g. a serialized OAuth session) verbatim under
/// `provider`, replacing any existing value. Distinct from the api-key pool
/// format used by [`store_api_key`].
pub fn store_secret(provider: &str, value: &str) -> Result<()> {
    let entry = keyring::Entry::new(SERVICE, provider)
        .map_err(|e| RinneError::Config(format!("keychain unavailable: {e}")))?;
    entry
        .set_password(value)
        .map_err(|e| RinneError::Config(format!("could not store secret: {e}")))?;
    Ok(())
}

/// Load an opaque secret blob for `provider`, if present.
pub fn load_secret(provider: &str) -> Option<String> {
    keyring::Entry::new(SERVICE, provider)
        .ok()?
        .get_password()
        .ok()
}

/// All keys for a provider, env var first then the keychain pool, deduped.
pub fn resolve_api_keys(provider: &str, key_env: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(k) = std::env::var(key_env) {
        if !k.is_empty() {
            out.push(k);
        }
    }
    out.extend(read_keys(provider));
    out.dedup();
    out
}

/// The first usable key for a provider (env or keychain).
pub fn resolve_api_key(provider: &str, key_env: &str) -> Option<String> {
    resolve_api_keys(provider, key_env).into_iter().next()
}

/// Whether any usable key exists for a provider (env or keychain).
pub fn has_api_key(provider: &str, key_env: &str) -> bool {
    !resolve_api_keys(provider, key_env).is_empty()
}

/// Where a resolved key came from, for honest reporting.
pub fn key_source(provider: &str, key_env: &str) -> Option<&'static str> {
    if std::env::var(key_env)
        .map(|k| !k.is_empty())
        .unwrap_or(false)
    {
        Some("env")
    } else if keychain_key(provider).is_some() {
        Some("keychain")
    } else {
        None
    }
}
