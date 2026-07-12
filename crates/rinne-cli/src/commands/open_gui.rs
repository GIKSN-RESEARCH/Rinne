//! Open the Rinne macOS app on a folder (`rinne .` / `rinne open .`).
//!
//! Same idea as VS Code's `code .`: from any directory, launch the GUI against
//! that project. Works cold-start and when the app is already running via a
//! pending-project handoff file + `open -b com.giksn.rinne`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Bundle id of the macOS app (must match PRODUCT_BUNDLE_IDENTIFIER).
pub const APP_BUNDLE_ID: &str = "com.giksn.rinne";

/// Well-known subcommand names that must never be treated as folder paths.
const RESERVED: &[&str] = &[
    "doctor",
    "run",
    "connect",
    "forget",
    "models",
    "status",
    "resume",
    "config",
    "logs",
    "human",
    "limit-usage",
    "usage",
    "mcp",
    "skill",
    "open",
    "help",
    "completions",
];

/// True if `arg` should open the GUI instead of the TUI / a subcommand.
pub fn looks_like_folder_open(arg: &str) -> bool {
    if arg.is_empty() || arg.starts_with('-') {
        return false;
    }
    let lower = arg.to_ascii_lowercase();
    if RESERVED.iter().any(|r| *r == lower.as_str()) {
        return false;
    }
    // `.` / `..` / path separators → treat as path intent
    if arg == "." || arg == ".." || arg.contains('/') || arg.contains('\\') {
        return Path::new(arg).is_dir() || arg == "." || arg == "..";
    }
    // Bare name: only if it exists as a directory (e.g. `rinne src`)
    let p = Path::new(arg);
    p.is_dir()
}

/// Absolute, canonical project directory.
pub fn resolve_project_dir(path: &str) -> Result<PathBuf> {
    let p = if path == "." || path.is_empty() {
        env::current_dir().context("current directory")?
    } else {
        let raw = PathBuf::from(path);
        if raw.is_absolute() {
            raw
        } else {
            env::current_dir()?.join(raw)
        }
    };
    let canon = p
        .canonicalize()
        .with_context(|| format!("path does not exist: {}", p.display()))?;
    if !canon.is_dir() {
        bail!("not a directory: {}", canon.display());
    }
    Ok(canon)
}

/// Pending-open file the GUI polls (Application Support / com.giksn.rinne).
pub fn pending_project_file() -> PathBuf {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join("Library/Application Support/com.giksn.rinne/pending-project")
}

/// Write handoff + activate the macOS app.
pub fn open_gui(project: &Path) -> Result<()> {
    let abs = project
        .canonicalize()
        .with_context(|| format!("canonicalize {}", project.display()))?;
    if !abs.is_dir() {
        bail!("not a directory: {}", abs.display());
    }

    let pending = pending_project_file();
    if let Some(parent) = pending.parent() {
        fs::create_dir_all(parent).context("create Application Support dir")?;
    }
    fs::write(&pending, abs.to_string_lossy().as_bytes()).context("write pending-project")?;

    // Prefer bundle id (works for /Applications and user-built apps registered with Launch Services)
    let status = Command::new("open")
        .args([
            "-b",
            APP_BUNDLE_ID,
            "--args",
            "--project",
            abs.to_str().context("project path utf-8")?,
        ])
        .status();

    match status {
        Ok(s) if s.success() => {
            println!("Opening Rinne → {}", abs.display());
            Ok(())
        }
        Ok(s) => {
            // Fallback: try by app name
            let alt = Command::new("open")
                .args([
                    "-a",
                    "Rinne",
                    "--args",
                    "--project",
                    abs.to_str().unwrap_or("."),
                ])
                .status()
                .context("open -a Rinne")?;
            if alt.success() {
                println!("Opening Rinne → {}", abs.display());
                Ok(())
            } else {
                bail!(
                    "could not open Rinne.app (exit {s}).\n\
                     Install the app or run it once so Launch Services knows bundle id `{APP_BUNDLE_ID}`.\n\
                     Pending path written to {}.",
                    pending.display()
                );
            }
        }
        Err(e) => bail!("failed to run `open`: {e}"),
    }
}

/// CLI entry for `rinne open [PATH]` (default `.`).
pub async fn run(path: Option<&str>) -> Result<()> {
    let dir = resolve_project_dir(path.unwrap_or("."))?;
    open_gui(&dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_not_folder() {
        assert!(!looks_like_folder_open("doctor"));
        assert!(!looks_like_folder_open("config"));
        assert!(!looks_like_folder_open("-p"));
    }

    #[test]
    fn dot_is_folder_intent() {
        assert!(looks_like_folder_open("."));
    }
}
