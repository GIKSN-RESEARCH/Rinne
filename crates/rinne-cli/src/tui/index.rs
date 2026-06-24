//! The repo file index for the `@`-picker (`CONTEXT.md` §6, §14).
//!
//! Indexed once on session start and kept current with a `notify` filesystem
//! watcher. Directories that would explode the index (VCS, build output, the
//! blackboard itself) are skipped.

use std::path::{Path, PathBuf};

use notify::{RecursiveMode, Watcher};

/// Directory names never worth indexing.
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".rinne",
    "target",
    "node_modules",
    ".next",
    "dist",
    "build",
    ".venv",
    "venv",
    "__pycache__",
];

/// A flat list of repo-relative file paths for fuzzy matching.
pub struct FileIndex {
    root: PathBuf,
    files: Vec<String>,
}

impl FileIndex {
    /// Build the index by scanning `root` once.
    pub fn build(root: &Path) -> Self {
        let files = scan(root);
        Self {
            root: root.to_path_buf(),
            files,
        }
    }

    pub fn files(&self) -> &[String] {
        &self.files
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Re-scan from disk (called when the watcher reports a change).
    pub fn refresh(&mut self) {
        self.files = scan(&self.root);
    }
}

/// Recursively collect repo-relative file paths, skipping noisy directories.
fn scan(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                if SKIP_DIRS.contains(&name.as_ref()) || name.starts_with('.') {
                    continue;
                }
                stack.push(path);
            } else if ft.is_file() {
                if let Ok(rel) = path.strip_prefix(root) {
                    out.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }
    }
    out.sort();
    out
}

/// Start a `notify` watcher that sends `()` on every filesystem change, so the
/// TUI can debounce-refresh the index. Returns the watcher (which must be kept
/// alive) or `None` if watching could not be set up.
pub fn watch(root: &Path, tx: tokio::sync::mpsc::UnboundedSender<()>) -> Option<notify::RecommendedWatcher> {
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.is_ok() {
            let _ = tx.send(());
        }
    })
    .ok()?;
    watcher.watch(root, RecursiveMode::Recursive).ok()?;
    Some(watcher)
}
