//! `rinne skill <subcommand>` — install and manage Agent Skills
//! (`MCP_SKILLS.md` §11). Skills are folders with a `SKILL.md`; the conductor
//! plans over their one-line descriptions and a worker gets the full body only
//! when a skill is attached to the node it runs. The same logic backs the CLI
//! and the TUI `/skill`.

use std::path::{Path, PathBuf};

use anyhow::Result;

use rinne_config::skills;
use rinne_config::write::Scope;

/// CLI entry: run a subcommand and print its report.
pub async fn run(args: &[String]) -> Result<()> {
    let cwd = std::env::current_dir()?;
    for line in run_lines(args, &cwd) {
        println!("{line}");
    }
    Ok(())
}

/// Dispatch a `skill` subcommand, returning report lines. Synchronous — skills
/// are pure filesystem work.
pub fn run_lines(args: &[String], cwd: &Path) -> Vec<String> {
    // Pull scope flags from anywhere; default to global for install/remove.
    let mut scope = Scope::Global;
    let toks: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|t| match *t {
            "--project" | "--proj" => {
                scope = Scope::Project;
                false
            }
            "--global" => {
                scope = Scope::Global;
                false
            }
            _ => true,
        })
        .collect();

    let Some((head, rest)) = toks.split_first() else {
        return list(cwd); // bare `skill` lists, like `skill list`
    };
    match *head {
        "install" | "add" => install(scope, cwd, rest),
        "list" | "ls" => list(cwd),
        "show" | "view" => show(cwd, rest),
        "remove" | "rm" | "uninstall" => remove(scope, cwd, rest),
        other => vec![format!("unknown skill subcommand `{other}`"), usage()],
    }
}

fn usage() -> String {
    "usage: rinne skill install <path> [--project]\n       rinne skill list · show <name> · remove <name>".to_string()
}

fn install(scope: Scope, cwd: &Path, rest: &[&str]) -> Vec<String> {
    let Some(src) = rest.first().copied() else {
        return vec!["usage: rinne skill install <path-to-skill-folder>".to_string()];
    };
    let source = resolve(cwd, src);
    match skills::install(&source, scope, cwd) {
        Ok(s) => {
            let mut out = vec![format!(
                "✔ installed skill `{}` ({}) → {}",
                s.name,
                scope.label(),
                s.dir.display()
            )];
            if !s.description.is_empty() {
                out.push(format!("  {}", s.description));
            }
            if !s.allowed_tools.is_empty() {
                out.push(format!("  tools: {}", s.allowed_tools.join(", ")));
            }
            out
        }
        Err(e) => vec![format!("✗ {e}")],
    }
}

fn list(cwd: &Path) -> Vec<String> {
    let found = skills::discover(cwd);
    if found.is_empty() {
        return vec![
            "No skills installed.".to_string(),
            "Install one: rinne skill install ./path/to/skill".to_string(),
        ];
    }
    let mut out = vec!["SKILLS:".to_string()];
    for s in found {
        let desc = s.description.lines().next().unwrap_or("");
        out.push(format!("  {:<20} {}", s.name, desc));
    }
    out
}

fn show(cwd: &Path, rest: &[&str]) -> Vec<String> {
    let Some(name) = rest.first().copied() else {
        return vec!["usage: rinne skill show <name>".to_string()];
    };
    match skills::get(name, cwd) {
        Some(s) => {
            let mut out = vec![
                format!("# {}", s.name),
                format!("{}", s.description),
                format!("dir: {}", s.dir.display()),
            ];
            if !s.allowed_tools.is_empty() {
                out.push(format!("tools: {}", s.allowed_tools.join(", ")));
            }
            out.push(String::new());
            out.extend(s.body.lines().map(String::from));
            out
        }
        None => vec![format!("no skill named `{name}` — `rinne skill list` to see them")],
    }
}

fn remove(scope: Scope, cwd: &Path, rest: &[&str]) -> Vec<String> {
    let Some(name) = rest.first().copied() else {
        return vec!["usage: rinne skill remove <name>".to_string()];
    };
    match skills::remove(name, scope, cwd) {
        Ok(true) => vec![format!("✔ removed skill `{name}` ({})", scope.label())],
        Ok(false) => vec![format!("· `{name}` is not installed in the {} scope", scope.label())],
        Err(e) => vec![format!("✗ {e}")],
    }
}

/// Resolve a possibly-relative source path against the working directory.
fn resolve(cwd: &Path, src: &str) -> PathBuf {
    let p = PathBuf::from(src);
    if p.is_absolute() {
        p
    } else {
        cwd.join(p)
    }
}
