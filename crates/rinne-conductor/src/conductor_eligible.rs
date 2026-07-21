//! Conductor Eligible Model Registry (CEMR) — only these models may plan
//! (`CONDUCTOR_LOOP_PLAN.md` §3.9.1).

/// Returns true when `model_id` may serve as the conductor (planner).
pub fn is_conductor_eligible(model_id: &str, user_allowlist: &[String]) -> bool {
    let m = model_id.to_lowercase();
    if user_allowlist
        .iter()
        .any(|a| a.eq_ignore_ascii_case(model_id))
    {
        return true;
    }
    if m.contains("fable") {
        return true;
    }
    if m.contains("opus") && version_at_least(&m, "opus", 4, 8) {
        return true;
    }
    if m.contains("sonnet") && version_at_least(&m, "sonnet", 5, 0) {
        return true;
    }
    if (m.contains("gpt") || m.contains("codex")) && version_at_least(&m, "gpt", 5, 5) {
        return true;
    }
    if m.contains("kimi") && version_at_least(&m, "kimi", 2, 7) {
        return true;
    }
    if m.contains("k2.7") || m.contains("k2-7") {
        return true;
    }
    if m.contains("glm") && version_at_least(&m, "glm", 5, 2) {
        return true;
    }
    if m.contains("grok-4") || m.contains("grok-build") || m.contains("grok-3") {
        return true;
    }
    false
}

/// Filter a ladder to conductor-eligible models only.
pub fn filter_eligible_ladder(
    ladder: &[String],
    only_eligible: bool,
    allowlist: &[String],
) -> Vec<String> {
    if !only_eligible {
        return ladder.to_vec();
    }
    ladder
        .iter()
        .filter(|m| is_conductor_eligible(m, allowlist))
        .cloned()
        .collect()
}

/// Best-effort numeric version gate on patterns like `opus-4-8`, `gpt-5.5`, `glm-5.2`.
fn version_at_least(m: &str, family: &str, major: u32, minor: u32) -> bool {
    let Some(idx) = m.find(family) else {
        return false;
    };
    let tail = &m[idx + family.len()..];
    let digits: String = tail
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .map(|c| if c == '-' { '.' } else { c })
        .collect();
    if digits.is_empty() {
        // Named tier without a parsed version (e.g. bare "sonnet") — conservative pass for 5+ families.
        return major <= 5;
    }
    let parts: Vec<u32> = digits.split('.').filter_map(|p| p.parse().ok()).collect();
    let (maj, min) = (
        parts.first().copied().unwrap_or(0),
        parts.get(1).copied().unwrap_or(0),
    );
    maj > major || (maj == major && min >= minor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kimi_k27_is_eligible() {
        assert!(is_conductor_eligible("@cf/moonshotai/kimi-k2.7-code", &[]));
    }

    #[test]
    fn haiku_is_not_eligible() {
        assert!(!is_conductor_eligible("claude-haiku-4", &[]));
    }

    #[test]
    fn allowlist_overrides() {
        assert!(is_conductor_eligible(
            "my-custom-planner",
            &["my-custom-planner".into()]
        ));
    }

    #[test]
    fn filter_drops_ineligible() {
        let ladder = vec!["kimi-k2.7".into(), "haiku".into(), "claude-opus-4-8".into()];
        let out = filter_eligible_ladder(&ladder, true, &[]);
        assert_eq!(out, vec!["kimi-k2.7", "claude-opus-4-8"]);
    }
}
