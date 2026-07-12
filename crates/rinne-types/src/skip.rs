//! Shared "what counts as the repo" rules for the @-picker index and the code
//! graph, so the two cannot drift apart.

pub const SKIP_DIRS: &[&str] = &[
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

pub const MAX_INDEX_FILE_BYTES: u64 = 1_048_576;

pub fn is_skipped_dir(name: &str) -> bool {
    SKIP_DIRS.contains(&name) || name.starts_with('.')
}

pub fn source_lang(path: &str) -> Option<&'static str> {
    let ext = path.rsplit('.').next()?;
    Some(match ext {
        "rs" => "rust",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" => "javascript",
        "py" => "python",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_known_dirs_and_dotfiles() {
        assert!(is_skipped_dir("node_modules"));
        assert!(is_skipped_dir("target"));
        assert!(is_skipped_dir(".git"));
        assert!(is_skipped_dir(".hidden"));
        assert!(!is_skipped_dir("src"));
    }

    #[test]
    fn maps_supported_extensions() {
        assert_eq!(source_lang("a/b.rs"), Some("rust"));
        assert_eq!(source_lang("a/b.tsx"), Some("typescript"));
        assert_eq!(source_lang("a/b.jsx"), Some("javascript"));
        assert_eq!(source_lang("a/b.py"), Some("python"));
        assert_eq!(source_lang("a/b.md"), None);
    }
}
