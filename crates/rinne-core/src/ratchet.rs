//! The test ratchet (`CONTEXT.md` §10, §12).
//!
//! Refuses any diff that weakens or deletes tests. An agent under pressure to
//! make a suite pass can "win" by deleting the failing test; the ratchet blocks
//! that move so progress is real, not gamed.

/// The verdict of a ratchet check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RatchetVerdict {
    pub ok: bool,
    pub reason: String,
}

impl RatchetVerdict {
    pub fn pass() -> Self {
        Self {
            ok: true,
            reason: String::new(),
        }
    }
}

/// Inspect a unified diff and block it if it removes test code without adding
/// equivalent test code back.
///
/// Heuristic, language-agnostic: count removed vs. added lines that look like
/// test declarations (and whole removed test files). A net loss of test markers
/// fails the ratchet.
pub fn check_diff(diff: &str) -> RatchetVerdict {
    let mut removed = 0i32;
    let mut added = 0i32;
    let mut removed_test_file = false;

    for line in diff.lines() {
        // Whole test file deleted: `--- a/foo_test.go` followed by `+++ /dev/null`.
        if line.starts_with("+++ ") && line.contains("/dev/null") {
            // Look back is awkward line-by-line; flag conservatively when a
            // deletion target was a test path (handled via removed markers too).
        }
        if let Some(rest) = line.strip_prefix('-') {
            if rest.starts_with("--") {
                // diff header line like `--- a/x`; check for test file path.
                if is_test_path(rest) {
                    removed_test_file = true;
                }
                continue;
            }
            if looks_like_test(rest) {
                removed += 1;
            }
        } else if let Some(rest) = line.strip_prefix('+') {
            if rest.starts_with("++") {
                continue;
            }
            if looks_like_test(rest) {
                added += 1;
            }
        }
    }

    if removed_test_file {
        return RatchetVerdict {
            ok: false,
            reason: "diff deletes a test file".into(),
        };
    }
    if removed > added {
        return RatchetVerdict {
            ok: false,
            reason: format!(
                "diff removes {removed} test declaration(s) but adds {added} — tests weakened"
            ),
        };
    }
    RatchetVerdict::pass()
}

/// Whether a line looks like a test declaration across common languages.
fn looks_like_test(line: &str) -> bool {
    let t = line.trim();
    t.contains("#[test]")
        || t.contains("#[tokio::test]")
        || t.starts_with("def test_")
        || t.starts_with("func Test")
        || t.starts_with("it(")
        || t.starts_with("it.")
        || t.starts_with("test(")
        || t.starts_with("describe(")
        || t.contains("@Test")
}

/// Whether a diff header path points at a test file.
fn is_test_path(header: &str) -> bool {
    let p = header.trim_start_matches('-').trim();
    p.contains("test") || p.contains("spec") || p.contains("__tests__")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_a_normal_diff() {
        let diff = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@\n-let x = 1;\n+let x = 2;\n";
        assert!(check_diff(diff).ok);
    }

    #[test]
    fn blocks_removing_a_test() {
        let diff = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@\n-    #[test]\n-    fn it_works() { assert!(true); }\n";
        let v = check_diff(diff);
        assert!(!v.ok, "should block test deletion: {}", v.reason);
    }

    #[test]
    fn allows_replacing_a_test() {
        let diff = "@@\n-    #[test]\n-    fn old() {}\n+    #[test]\n+    fn renamed() {}\n";
        assert!(check_diff(diff).ok);
    }

    #[test]
    fn blocks_deleting_a_test_file() {
        let diff = "--- a/src/foo_test.go\n+++ /dev/null\n@@\n-func TestFoo(t *testing.T) {}\n";
        assert!(!check_diff(diff).ok);
    }
}
