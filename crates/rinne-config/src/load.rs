//! Layered configuration loading with figment (`CONTEXT.md` §14, §18).
//!
//! Precedence, lowest to highest:
//!   1. built-in [`Config`] defaults
//!   2. global `~/.config/rinne/config.toml`
//!   3. per-project `<root>/.rinne/config.toml`
//!   4. environment variables prefixed `RINNE_`
//!
//! Later layers override earlier ones field-by-field.

use std::path::Path;

use figment::{
    providers::{Env, Format, Serialized, Toml},
    Figment,
};

use rinne_core::{Result, RinneError};

use crate::model::Config;
use crate::paths;

/// Suffixes of `RINNE_*` env vars that are process flags, not Config fields.
/// Figment's `Env::prefixed("RINNE_")` matches the part after the prefix.
const NON_CONFIG_ENV_SUFFIXES: &[&str] = &[
    // macOS app / harness UI: machine-readable engine JSONL stream
    "STREAM_JSON",
    // opt-out of crates.io/GitHub update probe
    "NO_UPDATE_CHECK",
    // optional path override for locating the `rinne` binary (used by wrappers)
    "BIN",
    // harness Stage plumbing: the CLI exports these for child worker processes
    // (`runner.rs`, `session_gate.rs`, `transport/external_terminal.rs`). They
    // share the RINNE_ prefix but are not `[harness_stage]` fields — with
    // `.split("_")` they would deserialize as `harness.stage.*` and fail
    // `deny_unknown_fields`, breaking every config load inside a Stage session.
    "HARNESS_STAGE_VISIBLE",
    "HARNESS_STAGE_MODE",
    "HARNESS_STAGE_MAX",
    // `[harness_stage].approvals`, exported as `RINNE_HARNESS_APPROVALS` by
    // `runner.rs` and read back by the adapters — `harness.approvals` is not a
    // Config field, so it must be ignored like the rest of the Stage plumbing.
    "HARNESS_APPROVALS",
    "STAGE_TAG",
];

/// Load configuration for the given project root, applying the full layering.
///
/// Missing config files are skipped, not errors — a zero-config install loads
/// pure defaults.
pub fn load(project_root: &Path) -> Result<Config> {
    let global = paths::global_config_file();
    let project = paths::project_config_file(project_root);
    load_layered(global.as_deref(), Some(&project), true)
}

/// The testable core of [`load`]: layer explicit file paths over the defaults,
/// optionally merging `RINNE_`-prefixed env vars on top.
///
/// Precedence (low → high): defaults, `global`, `project`, env. Paths that are
/// `None` or do not exist are skipped.
pub fn load_layered(
    global: Option<&Path>,
    project: Option<&Path>,
    merge_env: bool,
) -> Result<Config> {
    let mut figment = Figment::from(Serialized::defaults(Config::default()));

    if let Some(global) = global {
        if global.exists() {
            figment = figment.merge(Toml::file(global));
        }
    }

    if let Some(project) = project {
        if project.exists() {
            figment = figment.merge(Toml::file(project));
        }
    }

    if merge_env {
        // `RINNE_LOOP_TEST_RATCHET=false`, `RINNE_CONDUCTOR_BACKEND=groq`, etc.
        //
        // Process/protocol flags also use the RINNE_ prefix for discoverability
        // but are NOT Config fields (Config uses deny_unknown_fields). Without
        // ignoring them, `RINNE_STREAM_JSON=1` (set by the macOS app for live
        // JSONL streaming) fails as unknown field `stream` and blocks doctor,
        // connect, and every other command that loads config.
        figment = figment.merge(
            Env::prefixed("RINNE_")
                .ignore(NON_CONFIG_ENV_SUFFIXES)
                .split("_"),
        );
    }

    figment
        .extract()
        .map_err(|e| RinneError::Config(e.to_string()))
}

/// Load configuration using the current working directory as the project root.
pub fn load_cwd() -> Result<Config> {
    let cwd = std::env::current_dir()?;
    load(&cwd)
}
