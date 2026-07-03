//! Source code extraction and retrieval.

use std::collections::HashMap;
use std::path::Path;

use crate::learn::{Cluster, DocSection, Snippet};

const SNIPPET_CAP: usize = 40;

pub fn assemble(workspace: &Path, cluster: &Cluster) -> (Vec<Snippet>, Vec<DocSection>) {
    // Cache file contents by path so each file is read once.
    let mut file_cache: HashMap<String, Vec<String>> = HashMap::new();

    let mut snippets: Vec<Snippet> = Vec::new();
    let mut doc_refs: Vec<(String, u32)> = Vec::new(); // (docfile, section_N)

    for sym in &cluster.symbols {
        let lines = file_cache
            .entry(sym.file.clone())
            .or_insert_with(|| {
                let path = workspace.join(&sym.file);
                std::fs::read_to_string(&path)
                    .unwrap_or_default()
                    .lines()
                    .map(|l| l.to_string())
                    .collect()
            });

        // `line` is 1-based; symbol's first line in 0-based index is `line - 1`.
        let sym_idx = (sym.line as usize).saturating_sub(1);

        // --- Extract code snippet ---
        let code = extract_code(lines, sym_idx);

        // --- Extract doc comment (walk upward from sym_idx - 1) ---
        let doc = extract_doc(lines, sym_idx);

        // --- Scan combined text for doc-file references ---
        let combined = format!("{}\n{}", doc, code);
        collect_refs(&combined, &mut doc_refs);

        snippets.push(Snippet {
            symbol: sym.name.clone(),
            file: sym.file.clone(),
            line: sym.line,
            code,
            doc,
        });
    }

    // Dedup refs by (docfile, N) preserving order.
    doc_refs.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
    let mut seen_refs: Vec<(String, u32)> = Vec::new();
    let mut deduped_refs: Vec<(String, u32)> = Vec::new();
    for r in doc_refs {
        if !seen_refs.contains(&r) {
            seen_refs.push(r.clone());
            deduped_refs.push(r);
        }
    }

    // Load referenced doc sections.
    let mut sections: Vec<DocSection> = Vec::new();
    let mut seen_sections: Vec<(String, String)> = Vec::new();

    for (docfile, n) in deduped_refs {
        let path = workspace.join(&docfile);
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue, // missing doc file — skip silently
        };
        if let Some(section) = extract_section(&content, n) {
            let key = (docfile.clone(), section.heading.clone());
            if !seen_sections.contains(&key) {
                seen_sections.push(key);
                sections.push(DocSection {
                    source: docfile,
                    heading: section.heading,
                    body: section.body,
                });
            }
        }
    }

    (snippets, sections)
}

fn extract_code(lines: &[String], sym_idx: usize) -> String {
    if sym_idx >= lines.len() {
        return String::new();
    }
    let mut result: Vec<&str> = Vec::new();
    let mut had_code = false;
    for line in lines.iter().skip(sym_idx).take(SNIPPET_CAP) {
        if line.trim().is_empty() {
            if had_code {
                break;
            }
        } else {
            had_code = true;
        }
        result.push(line.as_str());
    }
    result.join("\n")
}

fn extract_doc(lines: &[String], sym_idx: usize) -> String {
    if sym_idx == 0 {
        return String::new();
    }
    let mut doc_lines: Vec<&str> = Vec::new();
    let mut idx = sym_idx - 1;
    loop {
        let trimmed = lines[idx].trim();
        if trimmed.starts_with("///") || trimmed.starts_with("//!") {
            doc_lines.push(lines[idx].as_str());
        } else {
            break;
        }
        if idx == 0 {
            break;
        }
        idx -= 1;
    }
    doc_lines.reverse();
    doc_lines.join("\n")
}

/// Scan `text` for doc-file section references without regex.
///
/// Matches plain `CONTEXT.md §N` and backtick-wrapped `` `CONTEXT.md` §N `` forms
/// for each of CONTEXT.md, PHASE.md, and MCP_SKILLS.md.
fn collect_refs(text: &str, out: &mut Vec<(String, u32)>) {
    // `§` is 2 bytes in UTF-8: 0xC2 0xA7.
    const SECTION_SIGN: &str = "§";

    let mut i = 0;
    while i < text.len() {
        // Find next `§` character.
        let rest = &text[i..];
        let Some(sign_pos) = rest.find(SECTION_SIGN) else {
            break;
        };

        // Parse digits after `§`.
        let after_sign = &rest[sign_pos + SECTION_SIGN.len()..];
        let digits: String = after_sign.chars().take_while(|c| c.is_ascii_digit()).collect();
        let abs_sign = i + sign_pos;
        if digits.is_empty() {
            // Advance past this `§` (2 bytes) and keep scanning.
            i = abs_sign + SECTION_SIGN.len();
            continue;
        }

        // Look backward in the ~20-char window before `§` for a known docfile name.
        let window_start = abs_sign.saturating_sub(20);
        // Strip backticks and spaces from the window to find the filename.
        let window = &text[window_start..abs_sign];
        let clean: String = window.chars().filter(|&c| c != '`' && c != ' ').collect();

        for docfile in &["CONTEXT.md", "PHASE.md", "MCP_SKILLS.md"] {
            if clean.ends_with(docfile) {
                if let Ok(n) = digits.parse::<u32>() {
                    out.push((docfile.to_string(), n));
                }
                break;
            }
        }

        // Advance past this `§` occurrence (§ is 2 bytes in UTF-8).
        i = abs_sign + SECTION_SIGN.len();
    }
}

struct SectionResult {
    heading: String,
    body: String,
}

fn extract_section(content: &str, n: u32) -> Option<SectionResult> {
    let section_sign_needle = format!("§{}", n);
    let numbered_prefix = format!("## {}.", n);
    let numbered_space = format!("## {} ", n);
    let numbered_exact = format!("## {}", n);
    let lines: Vec<&str> = content.lines().collect();

    // Find a heading line that either:
    //   - starts with `## N.` or `## N ` or is exactly `## N` (the `## N. Title` form), OR
    //   - contains `§N` (fallback form used in tests/older docs).
    let start = lines.iter().position(|l| {
        let trimmed = l.trim();
        trimmed.contains(&section_sign_needle)
            || trimmed.starts_with(&numbered_prefix)
            || trimmed.starts_with(&numbered_space)
            || trimmed == numbered_exact.as_str()
    })?;
    let heading = lines[start].trim().to_string();

    let mut body_lines: Vec<&str> = Vec::new();
    for line in lines.iter().skip(start + 1) {
        if line.starts_with("## ") {
            break;
        }
        body_lines.push(line);
    }

    Some(SectionResult {
        heading,
        body: body_lines.join("\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::learn::{Cluster, ClusterSymbol};

    #[test]
    fn reads_code_doc_and_referenced_section() {
        let dir = std::env::temp_dir().join(format!("rinne-src-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("m.rs"),
            "/// Does the thing (CONTEXT.md §12).\nfn thing() { work(); }\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("CONTEXT.md"),
            "## §11 Something\nnope\n## §12 The Blackboard\nthe why lives here\n## §13 Other\nx\n",
        )
        .unwrap();

        let cluster = Cluster {
            topic: "thing".into(),
            symbols: vec![ClusterSymbol {
                name: "thing".into(),
                file: "m.rs".into(),
                line: 2,
                kind: "symbol".into(),
            }],
            files: vec!["m.rs".into()],
        };
        let (snippets, sections) = assemble(&dir, &cluster);
        assert_eq!(snippets.len(), 1);
        assert!(snippets[0].code.contains("fn thing"));
        assert!(snippets[0].doc.contains("Does the thing"));
        assert!(
            sections.iter().any(|s| s.body.contains("the why lives here")),
            "referenced CONTEXT.md §12 section pulled in"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collect_refs_handles_multiple_multibyte_markers_without_panic() {
        // The `§` character is 2 bytes; the re-search loop advances by `pos + 1`
        // into the ASCII prefix, so multiple markers in one text must not slice
        // on a non-char boundary. Two refs exercise the loop's second iteration.
        let mut out = Vec::new();
        collect_refs("see CONTEXT.md §12 and CONTEXT.md §7 and PHASE.md §3", &mut out);
        assert!(out.contains(&("CONTEXT.md".to_string(), 12)));
        assert!(out.contains(&("CONTEXT.md".to_string(), 7)));
        assert!(out.contains(&("PHASE.md".to_string(), 3)));
    }

    #[test]
    fn reads_backtick_ref_and_numbered_heading() {
        let dir = std::env::temp_dir().join(format!("rinne-backtick-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("m.rs"),
            "/// Key design (`CONTEXT.md` §7).\nfn thing() { work(); }\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("CONTEXT.md"),
            "## 7. The Section\nreal rationale here\n## 8. Next\nother content\n",
        )
        .unwrap();

        let cluster = Cluster {
            topic: "thing".into(),
            symbols: vec![ClusterSymbol {
                name: "thing".into(),
                file: "m.rs".into(),
                line: 2,
                kind: "symbol".into(),
            }],
            files: vec!["m.rs".into()],
        };
        let (_, sections) = assemble(&dir, &cluster);
        assert!(
            sections.iter().any(|s| s.body.contains("real rationale here")),
            "backtick ref `CONTEXT.md` §7 with ## N. heading not resolved"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collect_refs_handles_backticks_and_mcp_skills() {
        let mut out = Vec::new();
        collect_refs("see `CONTEXT.md` §7 and `MCP_SKILLS.md` §6", &mut out);
        assert!(
            out.contains(&("CONTEXT.md".to_string(), 7)),
            "backtick CONTEXT.md §7 not found"
        );
        assert!(
            out.contains(&("MCP_SKILLS.md".to_string(), 6)),
            "backtick MCP_SKILLS.md §6 not found"
        );
    }
}
