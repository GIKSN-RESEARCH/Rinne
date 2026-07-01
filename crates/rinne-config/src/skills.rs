//! Skill storage: install, discover, and remove Agent Skills on disk
//! (`MCP_SKILLS.md` §11).
//!
//! Skills live as folders under a global dir (`~/.config/rinne/skills/`) or a
//! per-project dir (`<repo>/.rinne/skills/`). Each folder holds a `SKILL.md`
//! and any bundled scripts. Project skills shadow global ones of the same name,
//! so a repo can pin its own version.

use std::fs;
use std::path::{Path, PathBuf};

use rinne_core::{Result, RinneError, Skill};

use crate::paths;
use crate::write::Scope;

/// The skills directory for a scope. `Global` may be `None` if no home dir is
/// resolvable; `Project` is always available under the repo.
pub fn skills_dir(scope: Scope, project_root: &Path) -> Option<PathBuf> {
    match scope {
        Scope::Global => paths::global_skills_dir(),
        Scope::Project => Some(paths::project_skills_dir(project_root)),
    }
}

/// Read one skill folder (containing a `SKILL.md`) into a [`Skill`].
pub fn load_skill(dir: &Path) -> Result<Skill> {
    let md = dir.join("SKILL.md");
    let content = fs::read_to_string(&md).map_err(|_| RinneError::NotFound(md))?;
    let fallback = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    Ok(Skill::parse_md(&content, dir.to_path_buf(), &fallback))
}

/// All installed skills, project shadowing global on name collision. Sorted by
/// name. Folders without a `SKILL.md` are skipped.
pub fn discover(project_root: &Path) -> Vec<Skill> {
    let mut by_name: std::collections::BTreeMap<String, Skill> = std::collections::BTreeMap::new();
    // Global first, then project — project overwrites on the same name.
    for scope in [Scope::Global, Scope::Project] {
        let Some(dir) = skills_dir(scope, project_root) else {
            continue;
        };
        for skill in read_dir_skills(&dir) {
            by_name.insert(skill.name.clone(), skill);
        }
    }
    by_name.into_values().collect()
}

/// Look up one installed skill by name (project shadowing global).
pub fn get(name: &str, project_root: &Path) -> Option<Skill> {
    discover(project_root).into_iter().find(|s| s.name == name)
}

/// Install a skill from a local folder (which must contain a `SKILL.md`) into
/// the given scope, copying its full contents. Returns the installed skill.
pub fn install(source: &Path, scope: Scope, project_root: &Path) -> Result<Skill> {
    if !source.join("SKILL.md").is_file() {
        return Err(RinneError::Skill(format!(
            "{} has no SKILL.md — point at a skill folder",
            source.display()
        )));
    }
    // Parse first so the destination uses the skill's declared name.
    let parsed = load_skill(source)?;
    let root = skills_dir(scope, project_root)
        .ok_or_else(|| RinneError::Skill("no home directory for the global skills dir".into()))?;
    let dest = root.join(&parsed.name);
    if dest.exists() {
        fs::remove_dir_all(&dest)?;
    }
    copy_dir(source, &dest)?;
    load_skill(&dest)
}

/// Remove an installed skill by name from a scope. Returns whether it existed.
pub fn remove(name: &str, scope: Scope, project_root: &Path) -> Result<bool> {
    let Some(root) = skills_dir(scope, project_root) else {
        return Ok(false);
    };
    let dir = root.join(name);
    if dir.join("SKILL.md").is_file() {
        fs::remove_dir_all(&dir)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Parse every skill subfolder of `dir`. Missing dir → empty.
fn read_dir_skills(dir: &Path) -> Vec<Skill> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.join("SKILL.md").is_file() {
            if let Ok(skill) = load_skill(&path) {
                out.push(skill);
            }
        }
    }
    out
}

/// Recursively copy `src` into `dst` (creating `dst`).
fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(dir: &Path, name: &str, desc: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {desc}\n---\nbody for {name}\n"),
        )
        .unwrap();
    }

    #[test]
    fn install_discover_remove_roundtrip() {
        let tmp = std::env::temp_dir().join(format!("rinne-skills-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let project = tmp.join("repo");
        fs::create_dir_all(&project).unwrap();

        // A source skill folder to install from.
        let src = tmp.join("src-skill");
        write_skill(&src, "pdf-forms", "Fill PDF forms");
        fs::write(src.join("helper.py"), "print('hi')").unwrap();

        let installed = install(&src, Scope::Project, &project).unwrap();
        assert_eq!(installed.name, "pdf-forms");
        assert!(installed.dir.join("helper.py").is_file(), "bundled script copied");

        let found = discover(&project);
        assert!(found.iter().any(|s| s.name == "pdf-forms"));
        assert!(get("pdf-forms", &project).is_some());

        assert!(remove("pdf-forms", Scope::Project, &project).unwrap());
        assert!(get("pdf-forms", &project).is_none());
        assert!(!remove("pdf-forms", Scope::Project, &project).unwrap());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn install_rejects_a_folder_without_skill_md() {
        let tmp = std::env::temp_dir().join(format!("rinne-skills-noskill-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let src = tmp.join("not-a-skill");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("README.txt"), "no frontmatter here").unwrap();

        assert!(install(&src, Scope::Project, &tmp.join("repo")).is_err());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn discover_skips_folders_without_skill_md() {
        let tmp = std::env::temp_dir().join(format!("rinne-skills-skip-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let project = tmp.join("repo");
        let dir = skills_dir(Scope::Project, &project).unwrap();
        // A junk folder (no SKILL.md) alongside a real one.
        fs::create_dir_all(dir.join("junk")).unwrap();
        fs::write(dir.join("junk").join("notes.md"), "ignore me").unwrap();
        write_skill(&dir.join("real"), "real", "a real skill");

        let names: Vec<String> = discover(&project).into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["real"], "only the folder with a SKILL.md is a skill");

        let _ = fs::remove_dir_all(&tmp);
    }
}
