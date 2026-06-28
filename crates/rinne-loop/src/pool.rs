//! Pool profiling: tier whatever workers are actually present (`CONTEXT.md` §7).
//!
//! Tiers are computed, not hardcoded. The conductor takes the worker registry
//! as input and tiers it at runtime: cheap / workhorse / frontier are ranks
//! within the present pool, not fixed names. With only Claude Code the ladder is
//! Haiku → Sonnet → Opus; add a DeepSeek key and the cheap rung gets cheaper and
//! an open option appears; add Gemini and cross-family review returns.

use std::collections::BTreeSet;

use crate::priors::{self, family_of, family_of_worker, tier_ladder};
use crate::worker::{AuthMode, WorkerDescriptor};

/// The shape of the available worker pool, which selects the routing strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolShape {
    /// One vendor family, subscription-backed (e.g. only Claude Code). Quota is
    /// the binding constraint; cascade within the family.
    SingleFamily,
    /// One vendor family on a metered API key. Cost-aware cascade.
    SingleApi,
    /// Several families. Full cross-family routing, cross-family evaluators.
    MultiVendor,
    /// Subscription(s) plus API key(s). Prefer subscriptions for the bulk; spend
    /// API only where a capability or independence is worth it.
    Mixed,
}

impl PoolShape {
    pub fn label(self) -> &'static str {
        match self {
            PoolShape::SingleFamily => "single-family (quota-aware)",
            PoolShape::SingleApi => "single-api (cost-aware)",
            PoolShape::MultiVendor => "multi-vendor (cross-family)",
            PoolShape::Mixed => "mixed subscription + api",
        }
    }
}

/// A worker tiered into its cascade ladder.
#[derive(Debug, Clone)]
pub struct WorkerTiers {
    pub worker: String,
    pub family: String,
    pub auth: AuthMode,
    /// Models cheapest → strongest. Empty means a single fixed model.
    pub ladder: Vec<String>,
}

/// The profiled pool.
#[derive(Debug, Clone)]
pub struct PoolProfile {
    pub shape: PoolShape,
    pub workers: Vec<WorkerTiers>,
    /// Distinct vendor families present (excluding `unknown`).
    pub families: Vec<String>,
    /// A single-family pool loses evaluator independence; recommend a cheap
    /// second-family key for the evaluator role.
    pub recommend_eval_key: bool,
}

impl PoolProfile {
    pub fn is_single_family(&self) -> bool {
        matches!(self.shape, PoolShape::SingleFamily | PoolShape::SingleApi)
    }

    /// Map of worker name → cascade ladder, for the engine's escalation.
    pub fn ladders(&self) -> std::collections::HashMap<String, Vec<String>> {
        self.workers
            .iter()
            .filter(|w| w.ladder.len() > 1)
            .map(|w| (w.worker.clone(), w.ladder.clone()))
            .collect()
    }
}

/// Profile a set of worker descriptors into a pool profile.
pub fn profile(descriptors: &[WorkerDescriptor]) -> PoolProfile {
    let mut workers = Vec::new();
    let mut families: BTreeSet<String> = BTreeSet::new();
    let mut has_sub = false;
    let mut has_api = false;

    for d in descriptors {
        // Family: prefer the model's family; fall back to the worker name.
        let family = d
            .models
            .first()
            .map(|m| family_of(m))
            .filter(|f| *f != "unknown")
            .unwrap_or_else(|| family_of_worker(&d.name))
            .to_string();

        if family != "unknown" {
            families.insert(family.clone());
        }
        match d.auth_mode {
            AuthMode::Subscription => has_sub = true,
            AuthMode::ApiKey => has_api = true,
            _ => {}
        }

        workers.push(WorkerTiers {
            worker: d.name.clone(),
            family,
            auth: d.auth_mode,
            ladder: tier_ladder(&d.models),
        });
    }

    let distinct = families.len();
    let shape = if distinct <= 1 {
        if has_sub {
            PoolShape::SingleFamily
        } else {
            PoolShape::SingleApi
        }
    } else if has_sub && has_api {
        PoolShape::Mixed
    } else {
        PoolShape::MultiVendor
    };

    PoolProfile {
        shape,
        workers,
        families: families.into_iter().collect(),
        recommend_eval_key: distinct <= 1,
    }
}

/// Whether the pool can field a cross-family evaluator: at least two families.
pub fn has_cross_family(profile: &PoolProfile) -> bool {
    profile.families.len() >= 2
}

/// A friendly one-line recommendation for a thin pool, or `None` if the pool is
/// already cross-family. The single cheapest quality upgrade a single-family
/// user can make (`CONTEXT.md` §7).
pub fn eval_key_recommendation(profile: &PoolProfile) -> Option<String> {
    if !profile.recommend_eval_key {
        return None;
    }
    let fam = profile.families.first().map(String::as_str).unwrap_or("one family");
    let _ = priors::CostClass::Cheap; // priors seed this recommendation
    Some(format!(
        "Single-family pool ({fam}). For blind-spot independence on the highest-leverage role, \
         add a cheap second-family API key (e.g. DeepSeek or Gemini Flash) used only as the \
         evaluator — pennies, and Rinne charges nothing either way."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker::{Capability, LatencyProfile, QuotaModel, Transport, WorkerFamily};

    fn desc(name: &str, auth: AuthMode, models: &[&str]) -> WorkerDescriptor {
        WorkerDescriptor {
            name: name.into(),
            family: WorkerFamily::Harness,
            capabilities: vec![Capability::CodeEdit, Capability::Reasoning],
            auth_mode: auth,
            quota: QuotaModel::unlimited(),
            latency: LatencyProfile::Medium,
            transport: Transport::SubprocessJson,
            models: models.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn only_claude_is_single_family_with_a_ladder() {
        // Models are declared cheap→strong; the ladder preserves that order.
        let p = profile(&[desc("claude-code", AuthMode::Subscription, &["haiku", "sonnet", "opus"])]);
        assert_eq!(p.shape, PoolShape::SingleFamily);
        assert!(p.recommend_eval_key);
        assert!(!has_cross_family(&p));
        assert_eq!(p.workers[0].ladder, vec!["haiku", "sonnet", "opus"]);
    }

    #[test]
    fn claude_plus_grok_is_multi_vendor() {
        let p = profile(&[
            desc("claude-code", AuthMode::Subscription, &["sonnet", "opus"]),
            desc("grok", AuthMode::Subscription, &["grok-composer-2.5-fast", "grok-build"]),
        ]);
        assert_eq!(p.shape, PoolShape::MultiVendor);
        assert!(!p.recommend_eval_key);
        assert!(has_cross_family(&p));
    }

    #[test]
    fn subscription_plus_api_is_mixed() {
        let p = profile(&[
            desc("claude-code", AuthMode::Subscription, &["sonnet"]),
            desc("deepseek", AuthMode::ApiKey, &["deepseek-chat"]),
        ]);
        assert_eq!(p.shape, PoolShape::Mixed);
    }

    #[test]
    fn single_api_pool() {
        let p = profile(&[desc("openai", AuthMode::ApiKey, &["gpt-mini", "gpt-pro"])]);
        assert_eq!(p.shape, PoolShape::SingleApi);
        assert!(eval_key_recommendation(&p).is_some());
    }
}
