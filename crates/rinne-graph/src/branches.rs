//! Tree-sitter extraction of branch/guard conditions — the conditionals that
//! encode a codebase's real rules (eligibility gates, thresholds, fallbacks).

use tree_sitter::Parser;

use crate::lang::support_for;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchKind {
    If,
    Match,
    Guard,
    ErrorPath,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch {
    pub line: u32,
    pub condition: String,
    pub kind: BranchKind,
}

/// Extract branch/guard conditions from `source`. Returns empty when the
/// language is unsupported or the source doesn't parse.
pub fn extract_branches(lang: &str, source: &str) -> Vec<Branch> {
    let Some(support) = support_for(lang) else {
        return Vec::new();
    };
    let mut parser = Parser::new();
    if parser.set_language(&support.language()).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let bytes = source.as_bytes();
    let mut out: Vec<Branch> = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        // Condition-bearing node kinds across rust / python / ts-js.
        let kind = match node.kind() {
            "if_expression" | "match_expression" | "while_expression"
            | "if_statement" | "while_statement" | "match_statement"
            | "switch_statement" | "ternary_expression" => Some(node.kind()),
            _ => None,
        };
        if let Some(k) = kind {
            // The condition child: rust/py/ts `condition`; match/switch use
            // the discriminee (`value`/`subject`). Try named fields in order.
            let cond_node = node
                .child_by_field_name("condition")
                .or_else(|| node.child_by_field_name("value"))
                .or_else(|| node.child_by_field_name("subject"));
            if let Some(cn) = cond_node {
                let text = cn.utf8_text(bytes).unwrap_or("").trim();
                // Collapse to a single line so the render stays scannable.
                let condition: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
                if !condition.is_empty() {
                    let branch_kind = if k.contains("match") || k.contains("switch") {
                        BranchKind::Match
                    } else {
                        BranchKind::If
                    };
                    out.push(Branch {
                        line: cn.start_position().row as u32 + 1,
                        condition,
                        kind: branch_kind,
                    });
                }
            }
        }
        // Error-path heuristic (deterministic): a Rust `?` is a real fallback rule.
        // Cheap to detect by node kind.
        if node.kind() == "try_expression" {
            out.push(Branch {
                line: node.start_position().row as u32 + 1,
                condition: node.utf8_text(bytes).unwrap_or("?").trim().chars().take(60).collect(),
                kind: BranchKind::ErrorPath,
            });
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    // Stable order by line; dedup identical (line, condition).
    out.sort_by(|a, b| a.line.cmp(&b.line).then_with(|| a.condition.cmp(&b.condition)));
    out.dedup_by(|a, b| a.line == b.line && a.condition == b.condition);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_rust_if_condition() {
        let src = "fn f(tier: &str, seats: u32) {\n    if tier == \"enterprise\" && seats > 50 {\n        gate();\n    }\n}\n";
        let branches = extract_branches("rust", src);
        assert!(
            branches.iter().any(|b| b.kind == BranchKind::If
                && b.condition.contains("enterprise")
                && b.condition.contains("seats > 50")),
            "if condition captured verbatim: {branches:?}"
        );
    }

    #[test]
    fn extracts_python_if_condition() {
        let src = "def f(x):\n    if x > 10:\n        return True\n";
        let branches = extract_branches("python", src);
        assert!(
            branches.iter().any(|b| b.kind == BranchKind::If && b.condition.contains("x > 10")),
            "python if captured: {branches:?}"
        );
    }

    #[test]
    fn unsupported_lang_is_empty() {
        assert!(extract_branches("cobol", "IF X GREATER 1").is_empty());
    }
}
