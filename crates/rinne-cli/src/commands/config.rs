//! `rinne config [subcommand]` — view and edit configuration (`CONTEXT.md` §17,
//! §18).
//!
//! No args shows the resolved config and its source files. Subcommands edit a
//! config file in place (format-preserving), defaulting to the global file with
//! `--project` to scope a change to the current repo. Guided subcommands cover
//! the common knobs; `set`/`unset` reach any field. `init`/`edit` scaffold a
//! fully-commented file for hand-editing. Secrets are never written to the file
//! — API tokens go to the OS keychain (§9), including the conductor's.

use std::path::Path;

use anyhow::Result;

use rinne_config::paths;
use rinne_config::write::{self, Scope};

/// CLI entry: dispatch on the trailing args. Empty → show; else edit. For
/// `edit`, also open the file in `$EDITOR` (or the OS default).
pub async fn run(args: &[String]) -> Result<()> {
    let cwd = std::env::current_dir()?;
    if args.is_empty() {
        for line in show_lines(&cwd)? {
            println!("{line}");
        }
        return Ok(());
    }
    for line in edit_lines(&args.join(" "), &cwd) {
        println!("{line}");
    }
    // From the shell we can hand off to a terminal editor and block.
    if matches!(args[0].as_str(), "edit" | "open") {
        let scope = if args.iter().any(|a| a == "--project" || a == "--proj") {
            Scope::Project
        } else {
            Scope::Global
        };
        if let Ok(path) = write::target_path(scope, &cwd) {
            open_in_editor(&path, true);
        }
    }
    Ok(())
}

/// Render the resolved config, the conductor's auth status, and where each layer
/// comes from. Backs `/config` with no subcommand and `rinne config`.
pub fn show_lines(cwd: &Path) -> Result<Vec<String>> {
    let config = rinne_config::load(cwd)?;
    let mut out = Vec::new();
    out.push("rinne config — resolved (defaults ← global ← project ← env)".to_string());
    out.push(String::new());
    out.push(toml::to_string_pretty(&config)?.trim_end().to_string());
    out.push(String::new());

    // Conductor auth — secrets are never printed, only their presence + source.
    if let Some((provider, env)) = rinne_conductor::conductor_credential(&config.conductor) {
        let status = match rinne_config::secrets::key_source(&provider, &env) {
            Some(src) => format!("key present ({src})"),
            None => format!(
                "NO KEY — add it: /config conductor {} --key <token>",
                backend_label(&config)
            ),
        };
        out.push(format!("Conductor `{provider}` ({env}): {status}"));
    } else {
        out.push(format!(
            "Conductor `{}`: needs no API key.",
            backend_label(&config)
        ));
    }
    out.push(String::new());

    out.push("Sources (later overrides earlier):".to_string());
    match paths::global_config_file() {
        Some(p) => out.push(format!("  global   {}  {}", existence(&p), p.display())),
        None => out.push("  global   (no home directory found)".to_string()),
    }
    let project = paths::project_config_file(cwd);
    out.push(format!("  project  {}  {}", existence(&project), project.display()));
    out.push("  env      RINNE_* environment variables".to_string());
    out.push(String::new());
    out.push("Edit: /config conductor <backend> [model] [--key <token>] · /config prefer <harness|api|balanced>".to_string());
    out.push("      /config set <key> <val> · /config init (scaffold file) · /config edit (open it)".to_string());
    out.push("      add --project to scope a change to this repo (default is global).".to_string());
    Ok(out)
}

/// Apply an edit subcommand and return user-facing report lines. Backs `/config
/// <subcommand>` in the TUI and `rinne config <subcommand>` in the CLI.
pub fn edit_lines(args: &str, cwd: &Path) -> Vec<String> {
    // Pull `--project`/`--global` and `--key <token>` out of the args.
    let mut scope = Scope::Global;
    let mut key: Option<String> = None;
    let raw: Vec<&str> = args.split_whitespace().collect();
    let mut toks: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        match raw[i] {
            "--project" | "--proj" => scope = Scope::Project,
            "--global" => scope = Scope::Global,
            "--key" => {
                key = raw.get(i + 1).map(|s| s.to_string());
                i += 1;
            }
            t => toks.push(t),
        }
        i += 1;
    }

    let Some((head, rest)) = toks.split_first() else {
        return match show_lines(cwd) {
            Ok(lines) => lines,
            Err(e) => vec![format!("could not read config: {e}")],
        };
    };

    match *head {
        "show" | "view" => match show_lines(cwd) {
            Ok(lines) => lines,
            Err(e) => vec![format!("could not read config: {e}")],
        },
        "path" | "where" => path_lines(cwd),
        "init" => init_file(scope, cwd),
        "edit" | "open" => {
            let mut out = init_file(scope, cwd);
            out.push("Open the file above to hand-edit, or keep using /config commands.".to_string());
            out
        }
        "conductor" => match rest {
            [] => vec!["usage: /config conductor <cloudflare|groq|nvidia|local|harness> [model] [--key <token>]".to_string()],
            [backend, model_parts @ ..] => {
                let mut sets = vec![("conductor.backend".to_string(), backend.to_string())];
                if !model_parts.is_empty() {
                    sets.push(("conductor.model".to_string(), model_parts.join(" ")));
                }
                let mut out = apply_sets(scope, cwd, &sets);
                match (&key, keyed_backend(backend)) {
                    (Some(tok), _) => out.extend(store_backend_key(backend, tok)),
                    (None, Some((_, env))) => out.push(format!(
                        "  {backend} needs a key — add it once: /config conductor {backend} --key <token>  (or export {env})"
                    )),
                    (None, None) => {}
                }
                out
            }
        },
        "key" => {
            let tok = key.clone().or_else(|| rest.first().map(|s| s.to_string()));
            match tok {
                Some(t) => set_current_conductor_key(cwd, &t),
                None => vec!["usage: /config key <token>   (stores the token for the current conductor backend)".to_string()],
            }
        }
        "prefer" => match rest {
            [fam] => apply_sets(scope, cwd, &[("preferences.prefer".to_string(), fam.to_string())]),
            _ => vec!["usage: /config prefer <harness|api|balanced>".to_string()],
        },
        "role" => match rest {
            [role, worker] => apply_sets(
                scope,
                cwd,
                &[(format!("preferences.roles.{role}"), worker.to_string())],
            ),
            _ => vec!["usage: /config role <planner|generator|evaluator|...> <worker>".to_string()],
        },
        "model" => match rest {
            [worker, model_parts @ ..] if !model_parts.is_empty() => apply_sets(
                scope,
                cwd,
                &[(format!("models.{worker}"), model_parts.join(" "))],
            ),
            _ => vec!["usage: /config model <worker> <model-id>".to_string()],
        },
        "set" => match rest {
            [key_path, val_parts @ ..] if !val_parts.is_empty() => {
                apply_sets(scope, cwd, &[(key_path.to_string(), val_parts.join(" "))])
            }
            _ => vec!["usage: /config set <key> <value>   e.g. /config set loop.max_iterations_per_node 5".to_string()],
        },
        "unset" | "clear" => match rest {
            [key_path] => apply_unset(scope, cwd, key_path),
            _ => vec!["usage: /config unset <key>".to_string()],
        },
        other => vec![format!(
            "unknown /config subcommand `{other}` — try: show, conductor, key, prefer, role, model, set, unset, init, edit, path"
        )],
    }
}

/// The keychain provider + env var for a keyed conductor backend named `backend`
/// (`None` for keyless `local`/`harness`).
fn keyed_backend(backend: &str) -> Option<(&'static str, &'static str)> {
    match backend {
        "cloudflare" => Some(("cloudflare", "CLOUDFLARE_API_TOKEN")),
        "groq" => Some(("groq", "GROQ_API_KEY")),
        "nvidia" => Some(("nvidia", "NVIDIA_API_KEY")),
        _ => None,
    }
}

/// Store a token for a named backend in the OS keychain.
fn store_backend_key(backend: &str, token: &str) -> Vec<String> {
    match keyed_backend(backend) {
        Some((provider, env)) => match rinne_config::secrets::store_api_key(provider, token) {
            Ok(()) => vec![
                format!("✔ {backend} token stored in your OS keychain (set once — persists across shells)."),
                format!("  the conductor resolves it automatically now; no need to export {env}."),
            ],
            Err(e) => vec![
                format!("⚠ could not use the keychain ({e})."),
                format!("  fall back to: export {env}=<token>"),
            ],
        },
        None => vec![format!("· {backend} needs no API key.")],
    }
}

/// Store a token for whatever conductor backend is currently configured.
fn set_current_conductor_key(cwd: &Path, token: &str) -> Vec<String> {
    let config = match rinne_config::load(cwd) {
        Ok(c) => c,
        Err(e) => return vec![format!("✗ could not read config: {e}")],
    };
    match rinne_conductor::conductor_credential(&config.conductor) {
        Some((provider, env)) => match rinne_config::secrets::store_api_key(&provider, token) {
            Ok(()) => vec![
                format!("✔ token for conductor `{provider}` stored in your OS keychain (set once)."),
                format!("  resolved automatically now; no need to export {env}."),
            ],
            Err(e) => vec![format!("⚠ keychain unavailable ({e}); export {env}=<token> instead.")],
        },
        None => vec!["· the current conductor backend needs no API key.".to_string()],
    }
}

/// Scaffold a commented config file at `scope` if it does not exist.
fn init_file(scope: Scope, cwd: &Path) -> Vec<String> {
    let path = match write::target_path(scope, cwd) {
        Ok(p) => p,
        Err(e) => return vec![format!("✗ {e}")],
    };
    if path.exists() {
        return vec![
            format!("· {} config already exists: {}", scope.label(), path.display()),
            "  open it to hand-edit, or use /config set <key> <val>.".to_string(),
        ];
    }
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return vec![format!("✗ could not create {}: {e}", parent.display())];
        }
    }
    match std::fs::write(&path, TEMPLATE) {
        Ok(()) => vec![
            format!("✔ wrote a commented config template ({}): {}", scope.label(), path.display()),
            "  edit it in any editor — uncomment and change what you need.".to_string(),
            "  secrets never go here: API tokens use /connect or /config ... --key (OS keychain).".to_string(),
        ],
        Err(e) => vec![format!("✗ could not write {}: {e}", path.display())],
    }
}

/// Best-effort open `path` in the user's editor. `block` runs a terminal editor
/// to completion (safe from the shell); otherwise a GUI/`open` handoff is used
/// so it never fights the inline TUI.
pub fn open_in_editor(path: &Path, block: bool) {
    let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).ok();
    let result = match editor {
        Some(ed) if block => std::process::Command::new(ed).arg(path).status().map(|_| ()),
        Some(ed) => std::process::Command::new(ed).arg(path).spawn().map(|_| ()),
        None => {
            // No $EDITOR: hand off to the OS default opener (non-blocking GUI).
            let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
            std::process::Command::new(opener).arg(path).spawn().map(|_| ())
        }
    };
    let _ = result; // best-effort; the path was already reported to the user
}

/// Apply one or more dotted-key sets to the target file, reporting each.
fn apply_sets(scope: Scope, cwd: &Path, sets: &[(String, String)]) -> Vec<String> {
    let path = match write::target_path(scope, cwd) {
        Ok(p) => p,
        Err(e) => return vec![format!("✗ {e}")],
    };
    let mut out = Vec::new();
    let mut wrote_any = false;
    for (key, val) in sets {
        match write::set_value(&path, key, val) {
            Ok(()) => {
                wrote_any = true;
                out.push(format!("✔ {key} = {val}  ({})", scope.label()));
            }
            Err(e) => out.push(format!("✗ {key}: {e}")),
        }
    }
    if wrote_any {
        out.push(format!("  wrote {}", path.display()));
        out.push("  (applies to the next run)".to_string());
    } else {
        out.push("  nothing written.".to_string());
    }
    out
}

fn apply_unset(scope: Scope, cwd: &Path, key: &str) -> Vec<String> {
    let path = match write::target_path(scope, cwd) {
        Ok(p) => p,
        Err(e) => return vec![format!("✗ {e}")],
    };
    match write::unset_value(&path, key) {
        Ok(true) => vec![
            format!("✔ removed {key} ({})", scope.label()),
            format!("  wrote {}", path.display()),
        ],
        Ok(false) => vec![format!("· {key} was not set in the {} file — nothing to remove", scope.label())],
        Err(e) => vec![format!("✗ {key}: {e}")],
    }
}

fn path_lines(cwd: &Path) -> Vec<String> {
    let mut out = Vec::new();
    match paths::global_config_file() {
        Some(p) => out.push(format!("global   {}  {}", existence(&p), p.display())),
        None => out.push("global   (no home directory found)".to_string()),
    }
    let project = paths::project_config_file(cwd);
    out.push(format!("project  {}  {}", existence(&project), project.display()));
    out
}

fn backend_label(config: &rinne_config::Config) -> String {
    format!("{:?}", config.conductor.backend).to_lowercase()
}

fn existence(p: &Path) -> &'static str {
    if p.exists() {
        "[present]"
    } else {
        "[absent] "
    }
}

/// A fully-commented starter config. Every value is the built-in default, shown
/// commented so uncommenting is an explicit opt-in. No secrets ever live here.
const TEMPLATE: &str = r#"# Rinne configuration. Uncomment and edit what you need; defaults shown.
# Layering: built-in defaults < global < this file < RINNE_* env vars.
# Secrets are NEVER stored here. API tokens live in the OS keychain via
# `/connect <provider> <key>` or `/config conductor <backend> --key <token>`.

[conductor]
# The cheap, decoupled planner. backend = cloudflare | groq | nvidia | local | harness
# backend = "cloudflare"
# model = "@cf/moonshotai/kimi-k2.7-code"
# base_url = "https://api.openai.com/v1"   # override the endpoint
# account_id = "..."                        # cloudflare only (builds its URL)
# key_env = "CLOUDFLARE_API_TOKEN"          # override which env var holds the key

[loop]
# max_iterations_per_node = 8     # generator <-> evaluator rounds before giving up
# global_budget_minutes = 120     # wall-clock ceiling for a run
# test_ratchet = true             # block any diff that weakens or deletes tests
# stuck_loop_threshold = 3        # identical failures before escalating to a human

[preferences]
# prefer = "harness"              # harness | api | balanced — family routing order

# Pin a whole role to a worker:
# [preferences.roles]
# evaluator = "openrouter"
# generator = "claude-code"

# Pin a role to a specific model:
# [preferences.models]
# evaluator = "haiku"

# Default model per worker:
# [models]
# claude-code = "sonnet"
# openrouter = "openai/gpt-4o-mini"

# API workers are usually written by `/connect`, but you can hand-add them:
# [backends.api.openrouter]
# key_env = "OPENROUTER_API_KEY"
# base_url = "https://openrouter.ai/api/v1"
# models = ["openai/gpt-4o-mini"]
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("rinne-cfgcmd-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn conductor_set_writes_project_file() {
        let dir = tmp("conductor");
        let out = edit_lines("conductor groq llama-3.3-70b --project", &dir);
        assert!(out.iter().any(|l| l.contains("conductor.backend = groq")), "{out:?}");
        assert!(out.iter().any(|l| l.contains("conductor.model = llama-3.3-70b")), "{out:?}");

        let cfg = rinne_config::load(&dir).unwrap();
        assert_eq!(cfg.conductor.backend, rinne_config::model::ConductorBackend::Groq);
        assert_eq!(cfg.conductor.model, "llama-3.3-70b");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_infers_integer_and_validates() {
        let dir = tmp("int");
        let out = edit_lines("set loop.max_iterations_per_node 5 --project", &dir);
        assert!(out.iter().any(|l| l.contains("✔")), "{out:?}");
        let cfg = rinne_config::load(&dir).unwrap();
        assert_eq!(cfg.loop_.max_iterations_per_node, 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_key_is_rejected_not_written() {
        let dir = tmp("bad");
        let out = edit_lines("set loop.nonsense 5 --project", &dir);
        assert!(out.iter().any(|l| l.starts_with("✗")), "{out:?}");
        let cfg = rinne_config::load(&dir).unwrap();
        assert_eq!(cfg.loop_.max_iterations_per_node, 8); // default intact
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn role_and_model_maps() {
        let dir = tmp("maps");
        edit_lines("role evaluator openrouter --project", &dir);
        edit_lines("model claude-code sonnet --project", &dir);
        let cfg = rinne_config::load(&dir).unwrap();
        assert_eq!(cfg.preferences.roles.get("evaluator").map(String::as_str), Some("openrouter"));
        assert_eq!(cfg.models.by_worker.get("claude-code").map(String::as_str), Some("sonnet"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prefer_enum_validates() {
        let dir = tmp("prefer");
        let ok = edit_lines("prefer api --project", &dir);
        assert!(ok.iter().any(|l| l.contains("✔")), "{ok:?}");
        let bad = edit_lines("prefer sideways --project", &dir);
        assert!(bad.iter().any(|l| l.starts_with("✗")), "{bad:?}");
        let cfg = rinne_config::load(&dir).unwrap();
        assert_eq!(cfg.preferences.prefer, rinne_config::model::PreferFamily::Api);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn init_scaffolds_valid_template() {
        let dir = tmp("init");
        let out = edit_lines("init --project", &dir);
        assert!(out.iter().any(|l| l.contains("✔")), "{out:?}");
        // The scaffolded file must parse back as a valid Config.
        let cfg = rinne_config::load(&dir).unwrap();
        assert_eq!(cfg.conductor.backend, rinne_config::model::ConductorBackend::Cloudflare);
        // Re-running reports it already exists rather than clobbering.
        let again = edit_lines("init --project", &dir);
        assert!(again.iter().any(|l| l.contains("already exists")), "{again:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn conductor_without_key_hints_how_to_add_it() {
        let dir = tmp("hint");
        let out = edit_lines("conductor cloudflare --project", &dir);
        assert!(out.iter().any(|l| l.contains("needs a key")), "{out:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
