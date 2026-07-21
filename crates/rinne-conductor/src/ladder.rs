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
        }
    }

    /// Starting rung from tier classification (T3+ may skip rung 0).
    pub fn select_starting_rung(&mut self, classification: &Classification) {
        if !self.auto_escalate {
            return;
        }
        let floor = classification.goal_tier_floor;
        if floor >= ComplexityTier::T3 && self.rungs.len() > 1 {
            self.active_index = 1.min(self.rungs.len() - 1);
        }
        if floor >= ComplexityTier::T4 && self.rungs.len() > 2 {
            self.active_index = 2.min(self.rungs.len() - 1);
        }
        if classification.confidence < 0.2 && self.active_index == 0 && self.rungs.len() > 1 {
            self.active_index = 1;
        }
    }

    pub fn active_model(&self) -> Option<&str> {
        self.rungs.get(self.active_index).map(String::as_str)
    }

    pub fn can_escalate(&self) -> bool {
        self.auto_escalate
            && self.escalations_used < self.max_escalations
            && self.active_index + 1 < self.rungs.len()
    }

    pub fn escalate(&mut self, reason: EscalationReason) -> Option<(String, String, String)> {
        if !self.can_escalate() {
            return None;
        }
        let from = self.rungs[self.active_index].clone();
        self.active_index += 1;
        self.escalations_used += 1;
        let to = self.rungs[self.active_index].clone();
        Some((from, to, reason.narration()))
    }
}
