//! Plan parsing at the conductor boundary (`CONTEXT.md` §14).
//!
//! The model may emit comments, trailing commas, or wrap JSON in a ```code
//! fence. Tolerate all of that *here*, sanitize, then parse strictly into the
//! internal [`Plan`] — everything internal stays strict.

use rinne_core::dag::Plan;
use rinne_core::{Result, RinneError};

/// Sanitize and parse a model response into a validated [`Plan`].
pub fn parse_plan(raw: &str) -> Result<Plan> {
    let stripped = strip_code_fence(raw);
    let object = extract_outer_object(stripped).ok_or_else(|| {
        RinneError::Conductor(format!(
            "no JSON object found in conductor output (got: {})",
            snippet(raw)
        ))
    })?;
    let sanitized = strip_jsonc(object);

    let plan: Plan = serde_json::from_str(&sanitized)
        .map_err(|e| RinneError::Conductor(format!("plan JSON did not parse: {e}")))?;
    plan.validate()?;
    Ok(plan)
}

/// A short, single-line preview of raw model output for error messages.
fn snippet(raw: &str) -> String {
    let one_line: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let preview: String = one_line.chars().take(160).collect();
    if one_line.chars().count() > 160 {
        format!("{preview}…")
    } else if preview.is_empty() {
        "<empty>".to_string()
    } else {
        preview
    }
}

/// Remove a leading/trailing Markdown code fence (```json ... ```), if present.
fn strip_code_fence(s: &str) -> &str {
    let t = s.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t;
    };
    // Drop the optional language tag on the opening fence line.
    let rest = rest.splitn(2, '\n').nth(1).unwrap_or("");
    rest.trim_end()
        .strip_suffix("```")
        .unwrap_or(rest)
        .trim()
}

/// Extract the substring from the first `{` to its matching closing `}`,
/// respecting strings so braces inside string literals don't confuse the count.
fn extract_outer_object(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let start = s.find('{')?;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for i in start..bytes.len() {
        let c = bytes[i] as char;
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Remove `//` line comments, `/* */` block comments, and trailing commas —
/// all while preserving string literals (so `https://` and commas in strings
/// survive). The input is expected to be a single JSON object.
fn strip_jsonc(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut in_string = false;
    let mut escaped = false;

    while i < bytes.len() {
        let c = bytes[i] as char;
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        // Line comment.
        if c == '/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Block comment.
        if c == '/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
            continue;
        }

        if c == '"' {
            in_string = true;
        }
        out.push(c);
        i += 1;
    }

    remove_trailing_commas(&out)
}

/// Drop commas that immediately precede a `}` or `]` (ignoring whitespace),
/// outside of strings.
fn remove_trailing_commas(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut in_string = false;
    let mut escaped = false;

    let chars: Vec<char> = s.chars().collect();
    let _ = bytes; // chars drives indexing below
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == ',' {
            // Look ahead past whitespace for a closing bracket.
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j < chars.len() && (chars[j] == '}' || chars[j] == ']') {
                // Skip the comma.
                i += 1;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_clean_json() {
        let raw = r#"{"goal":"g","nodes":[{"id":"n1","role":"generator","instruction":"do"}]}"#;
        let plan = parse_plan(raw).unwrap();
        assert_eq!(plan.goal, "g");
        assert_eq!(plan.nodes.len(), 1);
    }

    #[test]
    fn tolerates_code_fence_comments_and_trailing_commas() {
        let raw = r#"```json
{
  // the goal
  "goal": "build it",
  "nodes": [
    {
      "id": "n1",
      "role": "generator",
      "instruction": "implement", /* inline */
      "needs": ["code-edit",],
    },
  ],
}
```"#;
        let plan = parse_plan(raw).unwrap();
        assert_eq!(plan.goal, "build it");
        assert_eq!(plan.nodes[0].needs.len(), 1);
    }

    #[test]
    fn preserves_urls_and_commas_in_strings() {
        let raw = r#"{"goal":"see https://x.io, really","nodes":[{"id":"n1","role":"planner","instruction":"a, b, c"}]}"#;
        let plan = parse_plan(raw).unwrap();
        assert_eq!(plan.goal, "see https://x.io, really");
        assert_eq!(plan.nodes[0].instruction, "a, b, c");
    }

    #[test]
    fn ignores_prose_around_json() {
        let raw = "Sure! Here is the plan:\n{\"goal\":\"g\",\"nodes\":[{\"id\":\"n1\",\"role\":\"planner\",\"instruction\":\"x\"}]}\nLet me know!";
        let plan = parse_plan(raw).unwrap();
        assert_eq!(plan.goal, "g");
    }

    #[test]
    fn rejects_invalid_plan() {
        // Duplicate node ids fail structural validation.
        let raw = r#"{"goal":"g","nodes":[{"id":"n1","role":"planner","instruction":"x"},{"id":"n1","role":"planner","instruction":"y"}]}"#;
        assert!(parse_plan(raw).is_err());
    }
}
