//! Safe, format-preserving edits to the user's config file (`CONTEXT.md` §17).
//!
//! Used by `rinne connect` to add an API provider without the user hand-editing
//! TOML. Only the env-var name and metadata are written — never the key itself.

use std::path::{Path, PathBuf};

use toml_edit::{value, Array, DocumentMut, Item, Table, Value};

use rinne_core::{Result, RinneError};

use crate::model::{Config, McpServer, McpTransport};
use crate::paths;

/// Which config file an edit targets (`CONTEXT.md` §18 layering).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Machine-wide `~/.config/rinne/config.toml` (default).
    Global,
    /// The current repo's `<root>/.rinne/config.toml`.
    Project,
}

impl Scope {
    pub fn label(self) -> &'static str {
        match self {
            Scope::Global => "global",
            Scope::Project => "project",
        }
    }
}

/// Resolve the file an edit at `scope` writes to.
pub fn target_path(scope: Scope, project_root: &Path) -> Result<PathBuf> {
    match scope {
        Scope::Global => paths::global_config_file()
            .ok_or_else(|| RinneError::Config("no home directory for global config".into())),
        Scope::Project => Ok(paths::project_config_file(project_root)),
    }
}

/// Set a dotted key (e.g. `conductor.backend`, `preferences.roles.evaluator`) to
/// `raw` in the config file at `path`, preserving formatting and comments. The
/// value type is inferred (bool, integer, else string). The edit is validated by
/// reparsing the whole file as a [`Config`] before it is written, so a bad key,
/// type, or enum variant fails loudly instead of corrupting the file.
pub fn set_value(path: &Path, dotted: &str, raw: &str) -> Result<()> {
    let mut doc = read_doc(path)?;
    let segments: Vec<&str> = dotted.split('.').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return Err(RinneError::Config("empty config key".into()));
    }
    let (leaf, parents) = segments.split_last().unwrap();

    let mut table = doc.as_table_mut();
    for seg in parents {
        table = table
            .entry(seg)
            .or_insert(Item::Table(Table::new()))
            .as_table_mut()
            .ok_or_else(|| RinneError::Config(format!("`{seg}` is not a table")))?;
    }
    table[leaf] = value(infer_value(raw));

    validate_and_write(path, doc)
}

/// Remove a dotted key from the config file at `path`. Returns whether a key was
/// actually present and removed.
pub fn unset_value(path: &Path, dotted: &str) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let mut doc = read_doc(path)?;
    let segments: Vec<&str> = dotted.split('.').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return Err(RinneError::Config("empty config key".into()));
    }
    let (leaf, parents) = segments.split_last().unwrap();

    let mut table = doc.as_table_mut();
    for seg in parents {
        match table.get_mut(seg).and_then(|i| i.as_table_mut()) {
            Some(t) => table = t,
            None => return Ok(false), // parent path absent → nothing to remove
        }
    }
    let removed = table.remove(leaf).is_some();
    if removed {
        validate_and_write(path, doc)?;
    }
    Ok(removed)
}

/// Read the config file at `path` as an editable document (empty doc if absent).
fn read_doc(path: &Path) -> Result<DocumentMut> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    existing
        .parse()
        .map_err(|e| RinneError::Config(format!("existing config is not valid TOML: {e}")))
}

/// Infer the TOML scalar type of a raw string: bool, then integer, else string.
fn infer_value(raw: &str) -> Value {
    match raw {
        "true" => true.into(),
        "false" => false.into(),
        _ => match raw.parse::<i64>() {
            Ok(i) => i.into(),
            Err(_) => raw.into(),
        },
    }
}

/// Validate the edited document parses as a [`Config`] (catching unknown keys via
/// `deny_unknown_fields`, type errors, and bad enum variants), then write it.
fn validate_and_write(path: &Path, doc: DocumentMut) -> Result<()> {
    let text = doc.to_string();
    toml::from_str::<Config>(&text)
        .map_err(|e| RinneError::Config(format!("rejected ({})", reason(&e.to_string()))))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, text)?;
    Ok(())
}

/// Pull the human-readable reason out of a `toml` deserialize error. The toml
/// crate renders a multi-line span with the actual cause (`unknown field …`,
/// `invalid type …`, `unknown variant …`) on the last line — surface that.
fn reason(s: &str) -> String {
    s.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('|') && !l.starts_with('^'))
        .unwrap_or(s)
        .to_string()
}

/// Add (or overwrite) a `[backends.api.<name>]` table in the global config file,
/// creating the file and directory if needed. Returns the config path written.
pub fn add_api_provider(
    name: &str,
    key_env: &str,
    base_url: &str,
    models: &[&str],
) -> Result<PathBuf> {
    let path = paths::global_config_file()
        .ok_or_else(|| RinneError::Config("no home directory for global config".into()))?;
    write_api_provider_to(&path, name, key_env, base_url, models)?;
    Ok(path)
}

/// The testable core: write into an explicit path.
pub fn write_api_provider_to(
    path: &Path,
    name: &str,
    key_env: &str,
    base_url: &str,
    models: &[&str],
) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc: DocumentMut = existing
        .parse()
        .map_err(|e| RinneError::Config(format!("existing config is not valid TOML: {e}")))?;

    // Ensure [backends] and [backends.api] exist as implicit parents, so the
    // provider renders as a clean `[backends.api.<name>]` header.
    let backends = doc
        .entry("backends")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| RinneError::Config("`backends` is not a table".into()))?;
    backends.set_implicit(true);
    let api = backends
        .entry("api")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| RinneError::Config("`backends.api` is not a table".into()))?;
    api.set_implicit(true);

    let mut provider = Table::new();
    provider["key_env"] = value(key_env);
    provider["base_url"] = value(base_url);
    if !models.is_empty() {
        let mut arr = Array::new();
        for m in models {
            arr.push(*m);
        }
        provider["models"] = value(arr);
    }
    api[name] = Item::Table(provider);

    std::fs::write(path, doc.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;

    #[test]
    fn writes_provider_and_reparses() {
        let mut path = std::env::temp_dir();
        path.push(format!("rinne-write-{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&path);

        write_api_provider_to(
            &path,
            "deepseek",
            "DEEPSEEK_API_KEY",
            "https://api.deepseek.com/v1",
            &["deepseek-chat", "deepseek-reasoner"],
        )
        .unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[backends.api.deepseek]"));

        // It must parse back as a valid Config with the provider present.
        let cfg: Config = crate::load::load_layered(Some(&path), None, false).unwrap();
        let p = cfg.backends.api.providers.get("deepseek").unwrap();
        assert_eq!(p.key_env, "DEEPSEEK_API_KEY");
        assert_eq!(p.models.len(), 2);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn preserves_other_entries() {
        let mut path = std::env::temp_dir();
        path.push(format!("rinne-write2-{}.toml", std::process::id()));
        std::fs::write(&path, "[conductor]\nbackend = \"groq\"\n").unwrap();

        write_api_provider_to(&path, "openai", "OPENAI_API_KEY", "https://api.openai.com/v1", &[])
            .unwrap();

        let cfg: Config = crate::load::load_layered(Some(&path), None, false).unwrap();
        // Pre-existing setting survives the edit.
        assert_eq!(
            cfg.conductor.backend,
            crate::model::ConductorBackend::Groq
        );
        assert!(cfg.backends.api.providers.contains_key("openai"));

        let _ = std::fs::remove_file(&path);
    }
}

/// Write (or overwrite) an `[mcp.servers.<name>]` table at `path`,
/// format-preserving and validated. Secrets are never written here.
pub fn write_mcp_server_to(path: &Path, name: &str, server: &McpServer) -> Result<()> {
    let mut doc = read_doc(path)?;

    let mcp = doc
        .entry("mcp")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| RinneError::Config("`mcp` is not a table".into()))?;
    mcp.set_implicit(true);
    let servers = mcp
        .entry("servers")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| RinneError::Config("`mcp.servers` is not a table".into()))?;
    servers.set_implicit(true);

    let mut t = Table::new();
    t["transport"] = value(match server.transport {
        McpTransport::Stdio => "stdio",
        McpTransport::Http => "http",
    });
    if let Some(c) = &server.command {
        t["command"] = value(c);
    }
    if !server.args.is_empty() {
        let mut arr = Array::new();
        for a in &server.args {
            arr.push(a.as_str());
        }
        t["args"] = value(arr);
    }
    if let Some(u) = &server.url {
        t["url"] = value(u);
    }
    if let Some(k) = &server.key_env {
        t["key_env"] = value(k);
    }
    if !server.env.is_empty() {
        let mut tbl = Table::new();
        for (k, v) in &server.env {
            tbl[k] = value(v.as_str());
        }
        t["env"] = Item::Table(tbl);
    }
    if !server.headers.is_empty() {
        let mut tbl = Table::new();
        for (k, v) in &server.headers {
            tbl[k] = value(v.as_str());
        }
        t["headers"] = Item::Table(tbl);
    }
    // Only write non-default flags, to keep the table tidy.
    if !(server.tools_allow.len() == 1 && server.tools_allow[0] == "*") {
        let mut arr = Array::new();
        for a in &server.tools_allow {
            arr.push(a.as_str());
        }
        t["tools_allow"] = value(arr);
    }
    if server.host_only {
        t["host_only"] = value(true);
    }
    if !server.enabled {
        t["enabled"] = value(false);
    }

    servers[name] = Item::Table(t);
    validate_and_write(path, doc)
}

/// Remove an `[mcp.servers.<name>]` table from `path`. Returns whether it was
/// present.
pub fn remove_mcp_server_from(path: &Path, name: &str) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let mut doc = read_doc(path)?;
    let removed = doc
        .get_mut("mcp")
        .and_then(|m| m.as_table_mut())
        .and_then(|m| m.get_mut("servers"))
        .and_then(|s| s.as_table_mut())
        .map(|servers| servers.remove(name).is_some())
        .unwrap_or(false);
    if removed {
        validate_and_write(path, doc)?;
    }
    Ok(removed)
}
