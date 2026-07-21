//! Goal tier classification against the exemplar library.

use std::collections::BTreeMap;

use rinne_core::dag::ComplexityTier;

use crate::tier_exemplars::{all, LoadedExemplar, TierExemplar};

/// Result of classifying a user goal.
#[derive(Debug, Clone)]
pub struct Classification {
    pub goal_tier_floor: ComplexityTier,
    pub matched_ids: Vec<String>,
    pub confidence: f32,
}

/// Classify a goal by exemplar similarity and risk keywords.
pub fn classify_goal(goal: &str) -> Classification {
    classify_goal_with(goal, &BTreeMap::new(), &[])
}

/// Classify with config goal-keyword overrides and project exemplars.
pub fn classify_goal_with(
    goal: &str,
    goal_keywords: &BTreeMap<String, String>,
    user_exemplars: &[LoadedExemplar],
) -> Classification {
    let goal_l = goal.to_lowercase();
    let risk_floor = max_tier(
        risk_keyword_floor(&goal_l),
        config_keyword_floor(&goal_l, goal_keywords),
    );

    let mut best_score = 0.0f32;
    let mut best_tier = ComplexityTier::T0;
    let mut matched: Vec<(f32, String, ComplexityTier)> = Vec::new();

    for ex in all() {
        let score = similarity_static(&goal_l, ex);
        if score > 0.0 {
            matched.push((score, ex.id.to_string(), ex.tier));
        }
        if score > best_score {
            best_score = score;
            best_tier = ex.tier;
        }
    }
    for ex in user_exemplars {
        let score = similarity_loaded(&goal_l, ex);
        if score > 0.0 {
            matched.push((score, ex.id.clone(), ex.tier));
        }
        if score > best_score {
            best_score = score;
            best_tier = ex.tier;
        }
    }

    matched.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let matched_ids: Vec<String> = matched
        .iter()
        .take(3)
        .filter(|(s, _, _)| *s >= 0.15)
        .map(|(_, id, _)| id.clone())
        .collect();

    let mut floor = max_tier(best_tier, risk_floor);
    if best_score < 0.12 && risk_floor == ComplexityTier::T0 {
        floor = ComplexityTier::T1;
    }
    if looks_text_only(&goal_l) && risk_floor == ComplexityTier::T0 {
        floor = ComplexityTier::T0;
    }

    let confidence = best_score.clamp(0.0, 1.0);
    Classification {
        goal_tier_floor: floor,
        matched_ids,
        confidence,
    }
}

fn similarity_static(goal: &str, ex: &TierExemplar) -> f32 {
    similarity_text(goal, ex.prompt, ex.signals)
}

fn similarity_loaded(goal: &str, ex: &LoadedExemplar) -> f32 {
    let sigs: Vec<&str> = ex.signals.iter().map(String::as_str).collect();
    similarity_text(goal, &ex.prompt, &sigs)
}

fn similarity_text(goal: &str, prompt: &str, signals: &[&str]) -> f32 {
    let prompt_l = prompt.to_lowercase();
    let mut hits = 0u32;
    let mut total = 0u32;
    for sig in signals {
        total += 1;
        if goal.contains(sig) {
            hits += 2;
        }
    }
    for word in prompt_l.split_whitespace() {
        if word.len() < 4 {
            continue;
        }
        total += 1;
        if goal.contains(word) {
            hits += 1;
        }
    }
    if total == 0 {
        return 0.0;
    }
    (hits as f32) / (total as f32)
}

fn config_keyword_floor(goal: &str, keywords: &BTreeMap<String, String>) -> ComplexityTier {
    let mut floor = ComplexityTier::T0;
    for (kw, tier_label) in keywords {
        if goal.contains(kw) {
            if let Some(t) = ComplexityTier::parse(tier_label) {
                floor = max_tier(floor, t);
            }
        }
    }
    floor
}

fn risk_keyword_floor(goal: &str) -> ComplexityTier {
    const T4: &[&str] = &[
        "security",
        "audit",
        "soc2",
        "hipaa",
        "incident",
        "breach",
        "exploit",
        "zero-downtime",
        "production migration",
        "mainnet",
        "disaster",
    ];
    const T3: &[&str] = &[
        "refactor",
        "migrate",
        "architecture",
        "multi-tenant",
        "monolith",
        "microservice",
        "performance",
        "optimize",
        "upgrade react",
        "event bus",
    ];
    for k in T4 {
        if goal.contains(k) {
            return ComplexityTier::T4;
        }
    }
    for k in T3 {
        if goal.contains(k) {
            return ComplexityTier::T3;
        }
    }
    ComplexityTier::T0
}

fn max_tier(a: ComplexityTier, b: ComplexityTier) -> ComplexityTier {
    if a >= b {
        a
    } else {
        b
    }
}

fn looks_text_only(goal: &str) -> bool {
    const MARKERS: &[&str] = &[
        "summarize",
        "summary",
        "explain",
        "what does",
        "commit message",
        "release notes",
        "faq",
        "draft",
        "list env",
        "reformat",
    ];
    MARKERS.iter().any(|m| goal.contains(m))
        && !goal.contains("implement")
        && !goal.contains("add endpoint")
        && !goal.contains("refactor")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiting_matches_t2() {
        let c = classify_goal("Add per-IP rate limiting middleware on public API routes");
        assert!(c.goal_tier_floor >= ComplexityTier::T2);
        assert!(c.matched_ids.iter().any(|id| id.starts_with("T2")));
    }

    #[test]
    fn security_audit_matches_t4() {
        let c = classify_goal("Audit the codebase for SQL injection and fix all instances");
        assert_eq!(c.goal_tier_floor, ComplexityTier::T4);
    }
}
