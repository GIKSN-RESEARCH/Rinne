//! Agent Skills (`MCP_SKILLS.md` §11).
//!
//! A skill is a folder with a `SKILL.md`: YAML-ish frontmatter (`name`,
//! `description`, optional `allowed-tools`) plus a markdown body of instructions,
//! and optionally bundled scripts. The format follows Anthropic Agent Skills so
//! existing skills work as-is, and it is model-agnostic — usable by any worker.
//!
//! Progressive disclosure: the conductor sees only the cheap [`Skill::summary`]
//! (name + description); the full [`Skill::body`] loads only when a skill is
//! attached to a node and that node runs.

use std::path::PathBuf;

use serde::Serialize;

/// One installed skill.
#[derive(Debug, Clone, Serialize)]
pub struct Skill {
    /// Unique skill name (from frontmatter, else the folder name).
    pub name: String,
    /// One-line description — the cheap layer the conductor plans over.
    pub description: String,
    /// Tools the skill declares it may use (optional; advisory).
    pub allowed_tools: Vec<String>,
    /// The instruction body injected into a worker's prompt for an attached node.
    pub body: String,
    /// The skill's directory (for resolving any bundled scripts).
    pub dir: PathBuf,
}

impl Skill {
    /// Parse a `SKILL.md`'s contents into a [`Skill`]. Pure string work — the
    /// caller does the filesystem read and passes a `fallback_name` (the folder)
    /// for a skill whose frontmatter omits `name`.
    pub fn parse_md(content: &str, dir: PathBuf, fallback_name: &str) -> Skill {
        let (front, body) = split_frontmatter(content);
        let mut name = None;
        let mut description = String::new();
        let mut allowed_tools = Vec::new();
        for line in front.lines() {
            let Some((k, v)) = line.split_once(':') else {
                continue;
            };
            let val = v.trim();
            match k.trim() {
                "name" if !val.is_empty() => name = Some(unquote(val)),
                "description" => description = unquote(val),
                "allowed-tools" | "allowed_tools" => allowed_tools = parse_list(val),
                _ => {}
            }
        }
        Skill {
            name: name.unwrap_or_else(|| fallback_name.to_string()),
            description,
            allowed_tools,
            body: body.trim().to_string(),
            dir,
        }
    }

    /// The cheap layer the conductor sees: `name: description`.
    pub fn summary(&self) -> String {
        format!("{}: {}", self.name, self.description)
    }
}

/// Split YAML-style frontmatter (between `---` fences) from the body. Returns
/// `(frontmatter, body)`; if there is no frontmatter, the whole content is body.
fn split_frontmatter(content: &str) -> (String, String) {
    let content = content.trim_start_matches('\u{feff}');
    if content.trim_start().starts_with("---") {
        let mut started = false;
        let mut closed = false;
        let mut front = String::new();
        let mut body: Vec<&str> = Vec::new();
        for line in content.lines() {
            if !started {
                if line.trim() == "---" {
                    started = true;
                }
                continue;
            }
            if !closed {
                if line.trim() == "---" {
                    closed = true;
                    continue;
                }
                front.push_str(line);
                front.push('\n');
            } else {
                body.push(line);
            }
        }
        if closed {
            return (front, body.join("\n"));
        }
    }
    (String::new(), content.to_string())
}

/// Strip a single layer of matching single or double quotes.
fn unquote(s: &str) -> String {
    let s = s.trim();
    for q in ['"', '\''] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

/// Parse a frontmatter list value: `[a, b]` or `a, b`.
fn parse_list(s: &str) -> Vec<String> {
    s.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|t| unquote(t.trim()))
        .filter(|t| !t.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_and_body() {
        let md = "---\nname: pdf-forms\ndescription: Fill PDF forms\nallowed-tools: [read, write]\n---\n# Steps\n1. open the pdf\n";
        let s = Skill::parse_md(md, PathBuf::from("/x"), "folder");
        assert_eq!(s.name, "pdf-forms");
        assert_eq!(s.description, "Fill PDF forms");
        assert_eq!(s.allowed_tools, vec!["read", "write"]);
        assert!(s.body.starts_with("# Steps"));
        assert_eq!(s.summary(), "pdf-forms: Fill PDF forms");
    }

    #[test]
    fn name_falls_back_to_folder() {
        let s = Skill::parse_md("just a body, no frontmatter", PathBuf::from("/x"), "myskill");
        assert_eq!(s.name, "myskill");
        assert_eq!(s.body, "just a body, no frontmatter");
        assert!(s.description.is_empty());
    }
}
