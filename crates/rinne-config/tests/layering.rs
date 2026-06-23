//! Phase 1 exit-gate test: configuration layering precedence
//! (defaults ← global ← project), verified with explicit paths.

use std::fs;
use std::path::PathBuf;

use rinne_config::load::load_layered;
use rinne_config::model::{ConductorBackend, PreferFamily};

/// A throwaway temp dir under the OS temp location, cleaned on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        // Unique-enough per process + tag without pulling in extra deps.
        p.push(format!("rinne-test-{}-{}", std::process::id(), tag));
        fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, contents).unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn defaults_load_with_no_files() {
    let cfg = load_layered(None, None, false).unwrap();
    assert_eq!(cfg.conductor.backend, ConductorBackend::Cloudflare);
    assert_eq!(cfg.loop_.max_iterations_per_node, 8);
    assert!(cfg.loop_.test_ratchet);
    assert_eq!(cfg.preferences.prefer, PreferFamily::Harness);
}

#[test]
fn project_overrides_global_overrides_default() {
    let dir = TempDir::new("layering");

    // Global sets backend=groq and max_iterations=4.
    let global = dir.write(
        "global.toml",
        r#"
[conductor]
backend = "groq"

[loop]
max_iterations_per_node = 4
"#,
    );

    // Project overrides only max_iterations=2, leaving backend from global.
    let project = dir.write(
        "project.toml",
        r#"
[loop]
max_iterations_per_node = 2
"#,
    );

    let cfg = load_layered(Some(&global), Some(&project), false).unwrap();

    // From global (not overridden by project): backend.
    assert_eq!(cfg.conductor.backend, ConductorBackend::Groq);
    // Project wins over global.
    assert_eq!(cfg.loop_.max_iterations_per_node, 2);
    // Untouched field keeps its default.
    assert!(cfg.loop_.test_ratchet);
}

#[test]
fn missing_files_are_skipped_not_errors() {
    let dir = TempDir::new("missing");
    let absent_global = dir.0.join("does-not-exist-global.toml");
    let absent_project = dir.0.join("does-not-exist-project.toml");
    let cfg = load_layered(Some(&absent_global), Some(&absent_project), false).unwrap();
    assert_eq!(cfg.conductor.backend, ConductorBackend::Cloudflare);
}
