//! Conductor planner ladder and escalation (`CONDUCTOR_LOOP_PLAN.md` §3.6).

use rinne_config::model::ConductorConfig;
use rinne_core::dag::ComplexityTier;

use crate::classifier::Classification;
use crate::conductor_eligible::filter_eligible_ladder;

/// Why the planner moved to a stronger model.
#[derive(Debug, Clone)]
pub enum EscalationReason {
    GoalTier(ComplexityTier),
    ParseFailure,
    ValidationFailure(Vec<String>),
    HighNodeCount(u32),
    LowConfidence,
}

impl EscalationReason {
    pub fn narration(&self) -> String {
        match self {
            EscalationReason::GoalTier(t) => format!("goal tier floor is {}", t.label()),
            EscalationReason::ParseFailure => "previous plan could not be parsed as JSON".into(),
            EscalationReason::ValidationFailure(errs) => {
                format!("routing validation failed: {}", errs.join("; "))
            }
            EscalationReason::HighNodeCount(n) => {
                format!("plan has {n} nodes — workhorse planner may over-split")
            }
            EscalationReason::LowConfidence => "tier classification confidence was low".into(),
        }
    }
}

/// Planner model ladder with escalation state for one plan invocation.
#[derive(Debug, Clone)]
pub struct ConductorLadder {
    pub rungs: Vec<String>,
    pub active_index: usize,
    pub max_escalations: u8,
    pub escalations_used: u8,
    pub auto_escalate: bool,
    /// Rung count contributed by non-API backends (see [`Self::widen_to`]).
    depth: usize,
}

impl ConductorLadder {
    pub fn from_config(config: &ConductorConfig) -> Self {
        let raw = config.planner_ladder();
        let rungs = filter_eligible_ladder(&raw, config.only_eligible_models, &config.allowlist);
        let rungs = if rungs.is_empty() {
            vec![config.model.clone()]
        } else {
            rungs
        };
        Self {
            rungs,
            active_index: 0,
            max_escalations: config.max_escalations,
            escalations_used: 0,
            auto_escalate: config.auto_escalate,
            depth: 0,
        }
    }

    /// Starting rung from tier classification (T3+ may skip rung 0).
    pub fn select_starting_rung(&mut self, classification: &Classification) {
        if !self.auto_escalate {
            return;
        }
        let depth = self.depth();
        let floor = classification.goal_tier_floor;
        if floor >= ComplexityTier::T3 && depth > 1 {
            self.active_index = 1.min(depth - 1);
        }
        if floor >= ComplexityTier::T4 && depth > 2 {
            self.active_index = 2.min(depth - 1);
        }
        if classification.confidence < 0.2 && self.active_index == 0 && depth > 1 {
            self.active_index = 1;
        }
    }

    pub fn active_model(&self) -> Option<&str> {
        self.rungs.get(self.active_index).map(String::as_str)
    }

    /// Widen the ladder's depth to cover a harness's own model ladder.
    ///
    /// `rungs` holds API model ids; a harness backend climbs the ladder on its
    /// descriptor instead (see `backend::planner_rung`). Without this the
    /// ladder collapses to one rung on a harness-only config — `can_escalate`
    /// is permanently false and a parse failure never reaches a stronger model.
    pub fn widen_to(&mut self, depth: usize) {
        self.depth = self.depth.max(depth);
    }

    /// How many rungs this ladder can address, across API and harness ladders.
    fn depth(&self) -> usize {
        self.depth.max(self.rungs.len())
    }

    pub fn can_escalate(&self) -> bool {
        self.auto_escalate
            && self.escalations_used < self.max_escalations
            && self.active_index + 1 < self.depth()
    }

    pub fn escalate(&mut self, reason: EscalationReason) -> Option<(String, String, String)> {
        if !self.can_escalate() {
            return None;
        }
        // Index defensively: a widened ladder addresses more rungs than `rungs`
        // has entries (the extra depth lives on a harness descriptor), so the
        // narration falls back to the rung number rather than panicking.
        let label = |i: usize| {
            self.rungs
                .get(i)
                .cloned()
                .unwrap_or_else(|| format!("rung {}", i + 1))
        };
        let from = label(self.active_index);
        self.active_index += 1;
        self.escalations_used += 1;
        let to = label(self.active_index);
        Some((from, to, reason.narration()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ladder(rungs: &[&str]) -> ConductorLadder {
        ConductorLadder {
            rungs: rungs.iter().map(|s| s.to_string()).collect(),
            active_index: 0,
            max_escalations: 3,
            escalations_used: 0,
            auto_escalate: true,
            depth: 0,
        }
    }

    #[test]
    fn a_single_api_rung_cannot_escalate() {
        assert!(!ladder(&["gpt-5-mini"]).can_escalate());
    }

    #[test]
    fn widening_lets_a_harness_only_config_escalate() {
        // Regression: `rungs` holds API models, so on a harness-only install it
        // collapsed to one entry and `can_escalate()` was permanently false —
        // a parse failure narrated an escalation that never happened and then
        // re-ran the identical cheap model.
        let mut l = ladder(&["gpt-5-mini"]);
        l.widen_to(3);
        assert!(l.can_escalate());
        assert!(l.escalate(EscalationReason::ParseFailure).is_some());
        assert_eq!(l.active_index, 1);
    }

    #[test]
    fn widening_never_shrinks_the_ladder() {
        let mut l = ladder(&["a", "b", "c"]);
        l.widen_to(1);
        assert!(l.can_escalate(), "a 3-rung API ladder must stay 3 deep");
    }

    #[test]
    fn escalation_stops_at_the_top_rung() {
        let mut l = ladder(&["a", "b"]);
        assert!(l.escalate(EscalationReason::ParseFailure).is_some());
        assert!(!l.can_escalate(), "no rung above the top");
        assert!(l.escalate(EscalationReason::ParseFailure).is_none());
    }

    #[test]
    fn max_escalations_caps_the_climb() {
        let mut l = ladder(&["a", "b", "c", "d", "e"]);
        l.max_escalations = 1;
        assert!(l.escalate(EscalationReason::ParseFailure).is_some());
        assert!(!l.can_escalate(), "budget spent even though rungs remain");
    }

    #[test]
    fn auto_escalate_off_pins_the_starting_rung() {
        let mut l = ladder(&["a", "b", "c"]);
        l.auto_escalate = false;
        assert!(!l.can_escalate());
    }

    #[test]
    fn a_t4_goal_starts_partway_up_a_widened_harness_ladder() {
        // `select_starting_rung` clamped against `rungs.len()`, so a widened
        // harness ladder still started every goal on the cheapest rung.
        let mut l = ladder(&["gpt-5-mini"]);
        l.widen_to(3);
        l.select_starting_rung(&Classification {
            goal_tier_floor: ComplexityTier::T4,
            matched_ids: Vec::new(),
            confidence: 1.0,
        });
        assert_eq!(l.active_index, 2);
    }
}
