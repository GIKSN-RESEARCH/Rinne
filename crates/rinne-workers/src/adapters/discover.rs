//! Live model discovery from harness CLIs.
//!
//! Static `descriptor.models` ladders go stale when a harness renames or retires
//! models (e.g. Grok dropped `grok-build` for `grok-4.5`). At registry build
//! time we shell out to `<cli> models` and rebuild the cheap→strong ladder so
//! routing never schedules a dead model id.

use std::time::Duration;

/// Models advertised by a harness CLI, after a successful `models` probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredModels {
    /// Cheap → strong ladder for cascade / tier routing.
    pub ladder: Vec<String>,
    /// The CLI's default model id, when reported.
    pub default: Option<String>,
}

impl DiscoveredModels {
    /// Build a cheap→strong ladder: non-default models first (as listed), then
    /// the default as the frontier rung when present.
    pub fn from_listing(listed: Vec<String>, default: Option<String>) -> Self {
        let mut ladder: Vec<String> = Vec::new();
        // Non-default entries first (listing order ≈ preference/cheap first).
        for m in &listed {
            if default.as_deref() == Some(m.as_str()) {
                continue;
            }
            if !ladder.iter().any(|x| x == m) {
                ladder.push(m.clone());
            }
        }
        // Default is the frontier rung for cascade / T3+ routing.
        if let Some(d) = &default {
            if !ladder.iter().any(|x| x == d) {
                ladder.push(d.clone());
            }
        } else if ladder.is_empty() {
            // No default and first loop skipped everything — use listing as-is.
            for m in listed {
                if !ladder.iter().any(|x| x == &m) {
                    ladder.push(m);
                }
            }
        }
        Self { ladder, default }
    }
}

/// Parse the free-form stdout of a `cli models` command.
///
/// Tolerates common shapes used by Grok Build and similar CLIs:
/// ```text
/// Default model: grok-4.5
/// Available models:
///   * grok-4.5 (default)
///   - grok-composer-2.5-fast
/// ```
/// Also accepts bare one-id-per-line listings and JSON arrays of strings.
pub fn parse_models_listing(stdout: &str) -> Option<DiscoveredModels> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }

    // JSON array: ["a","b"] or {"models":[...],"default":"..."}
    if let Some(d) = parse_json_models(trimmed) {
        if !d.ladder.is_empty() {
            return Some(d);
        }
    }

    let mut default: Option<String> = None;
    let mut listed: Vec<String> = Vec::new();

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // "Default model: foo" / "default: foo"
        if let Some(rest) = line
            .strip_prefix("Default model:")
            .or_else(|| line.strip_prefix("default model:"))
            .or_else(|| line.strip_prefix("Default:"))
            .or_else(|| line.strip_prefix("default:"))
        {
            let id = clean_model_token(rest);
            if !id.is_empty() {
                default = Some(id);
            }
            continue;
        }

        // Bullet / star lines: "* id (default)" / "- id" / "• id"
        let bullet = line
            .strip_prefix("* ")
            .or_else(|| line.strip_prefix("- "))
            .or_else(|| line.strip_prefix("• "))
            .or_else(|| line.strip_prefix("· "));
        if let Some(rest) = bullet {
            let is_default = rest.to_ascii_lowercase().contains("(default)");
            let id = clean_model_token(rest);
            if id.is_empty() || looks_like_header(&id) {
                continue;
            }
            if is_default {
                default = Some(id.clone());
            }
            if !listed.iter().any(|m| m == &id) {
                listed.push(id);
            }
            continue;
        }

        // Bare model id line (no spaces, not a sentence).
        if !line.contains(' ') && looks_like_model_id(line) {
            let id = line.to_string();
            if !listed.iter().any(|m| m == &id) {
                listed.push(id);
            }
        }
    }

    if listed.is_empty() && default.is_none() {
        return None;
    }
    Some(DiscoveredModels::from_listing(listed, default))
}

fn parse_json_models(s: &str) -> Option<DiscoveredModels> {
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    if let Some(arr) = v.as_array() {
        let listed: Vec<String> = arr
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect();
        if listed.is_empty() {
            return None;
        }
        return Some(DiscoveredModels::from_listing(listed, None));
    }
    let listed: Vec<String> = v
        .get("models")
        .and_then(|m| m.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    x.as_str()
                        .map(str::to_string)
                        .or_else(|| x.get("id").and_then(|i| i.as_str()).map(str::to_string))
                })
                .collect()
        })
        .unwrap_or_default();
    let default = v
        .get("default")
        .or_else(|| v.get("default_model"))
        .and_then(|d| d.as_str())
        .map(str::to_string);
    if listed.is_empty() && default.is_none() {
        return None;
    }
    Some(DiscoveredModels::from_listing(listed, default))
}

fn clean_model_token(s: &str) -> String {
    let s = s.trim();
    // Strip trailing annotations: "(default)", "[fast]", etc.
    let s = s.split_whitespace().next().unwrap_or(s);
    s.trim_matches(|c: char| c == '`' || c == '"' || c == '\'' || c == ',')
        .to_string()
}

fn looks_like_header(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    matches!(
        l.as_str(),
        "available" | "models" | "model" | "name" | "id" | "default"
    ) || l.contains("available")
}

fn looks_like_model_id(s: &str) -> bool {
    // Conservative: ids are kebab/snake/slash tokens, not prose.
    !s.is_empty()
        && s.len() < 80
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':'))
        && s.contains(|c: char| c.is_ascii_alphanumeric())
}

/// Run `<program> models` with a short timeout and parse the listing.
///
/// Returns `None` when the binary is missing, the subcommand fails with no
/// usable stdout, or parsing finds no model ids — callers keep their static
/// fallback ladder in that case.
pub async fn discover_cli_models(program: &str) -> Option<DiscoveredModels> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.arg("models")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let fut = cmd.output();
    let output = match tokio::time::timeout(Duration::from_secs(8), fut).await {
        Ok(Ok(o)) => o,
        _ => return None,
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Some CLIs print the listing on stderr when not fully logged in.
    let text = if stdout.trim().is_empty() {
        stderr.as_ref()
    } else {
        stdout.as_ref()
    };
    parse_models_listing(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_grok_models_listing() {
        let out = r#"
You are logged in with grok.com.

Default model: grok-4.5

Available models:
  * grok-4.5 (default)
  - grok-composer-2.5-fast
"#;
        let d = parse_models_listing(out).expect("parse");
        assert_eq!(d.default.as_deref(), Some("grok-4.5"));
        // Cheap first, default (frontier) last.
        assert_eq!(
            d.ladder,
            vec![
                "grok-composer-2.5-fast".to_string(),
                "grok-4.5".to_string()
            ]
        );
    }

    #[test]
    fn parses_json_array() {
        let d = parse_models_listing(r#"["haiku","sonnet","opus"]"#).unwrap();
        assert_eq!(d.ladder, vec!["haiku", "sonnet", "opus"]);
    }

    #[test]
    fn from_listing_puts_default_last() {
        let d = DiscoveredModels::from_listing(
            vec!["a".into(), "b".into(), "c".into()],
            Some("b".into()),
        );
        assert_eq!(d.ladder, vec!["a", "c", "b"]);
    }

    #[test]
    fn empty_listing_is_none() {
        assert!(parse_models_listing("Not logged in").is_none());
        assert!(parse_models_listing("").is_none());
    }
}
