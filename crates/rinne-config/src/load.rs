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
        figment = figment.merge(Env::prefixed("RINNE_").split("_"));
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
