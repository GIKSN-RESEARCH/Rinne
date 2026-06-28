//! Static capability priors for known models (`CONTEXT.md` §7).
//!
//! Day-one seed for tiering an unknown pool with no learned data: known models
//! mapped to per-dimension scores and a relative cost/quota class. Unknown
//! models get a conservative default, and the within-pool cascade discovers
//! their real level through evaluator outcomes. This same table is what the
//! phase-two learned router refines from logged trajectories, so the priors
//! degrade gracefully into learned weights.

/// Relative cost/quota rank, not an absolute price. `Cheap` < `Workhorse` <
/// `Frontier` defines the cascade order within whatever pool is present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CostClass {
    Cheap = 0,
    Workhorse = 1,
    Frontier = 2,
}

/// Per-dimension capability scores (0–100) plus family and cost class.
#[derive(Debug, Clone, Copy)]
pub struct ModelPrior {
    /// Vendor family, e.g. `claude`, `grok`, `openai`, `google`, `deepseek`.
    /// Drives cross-family evaluator independence.
    pub family: &'static str,
    pub cost: CostClass,
    pub coding: u8,
    pub reasoning: u8,
    pub long_context: u8,
    pub vision: u8,
    pub structured_output: u8,
}

impl ModelPrior {
    /// A coding-agent-weighted strength used to order models within a cost
    /// class (coding and reasoning dominate).
    pub fn strength(&self) -> u32 {
        (self.coding as u32) * 4
            + (self.reasoning as u32) * 4
            + (self.long_context as u32)
            + (self.structured_output as u32)
    }
}

/// The conservative default for an unrecognized model: mid-tier, unknown family,
/// so the cascade discovers its true level rather than over-trusting it.
const DEFAULT: ModelPrior = ModelPrior {
    family: "unknown",
    cost: CostClass::Workhorse,
    coding: 60,
    reasoning: 60,
    long_context: 60,
    vision: 40,
    structured_output: 60,
};

/// Look up priors for a model id/alias by case-insensitive substring match
/// (most specific patterns first). Returns [`DEFAULT`] for unknown models.
pub fn prior_for(model: &str) -> ModelPrior {
    let m = model.to_lowercase();
    let has = |needle: &str| m.contains(needle);

    // ----- Anthropic / Claude -----
    if has("opus") {
        return p("claude", CostClass::Frontier, 90, 95, 90, 85, 88);
    }
    if has("sonnet") {
        return p("claude", CostClass::Workhorse, 85, 82, 85, 80, 85);
    }
    if has("haiku") {
        return p("claude", CostClass::Cheap, 70, 65, 75, 70, 78);
    }

    // ----- xAI / Grok -----
    if has("grok-build") || has("grok-4") {
        return p("grok", CostClass::Frontier, 82, 80, 80, 60, 75);
    }
    if has("grok-composer") || has("composer") || has("grok") {
        return p("grok", CostClass::Workhorse, 72, 68, 70, 50, 70);
    }

    // ----- Google / Gemini -----
    if has("gemini") && (has("flash") || has("lite")) {
        return p("google", CostClass::Cheap, 68, 65, 88, 75, 72);
    }
    if has("gemini") {
        return p("google", CostClass::Frontier, 84, 86, 92, 88, 82);
    }

    // ----- DeepSeek -----
    if has("deepseek") && has("reason") {
        return p("deepseek", CostClass::Workhorse, 80, 88, 70, 40, 72);
    }
    if has("deepseek") {
        return p("deepseek", CostClass::Cheap, 78, 70, 70, 30, 70);
    }

    // ----- OpenAI ladder (nano < mini < standard < pro) -----
    if has("nano") {
        return p("openai", CostClass::Cheap, 68, 64, 70, 60, 80);
    }
    if has("mini") {
        return p("openai", CostClass::Workhorse, 80, 78, 80, 75, 85);
    }
    if has("gpt") || has("openai") || has("o3") || has("o4") {
        return p("openai", CostClass::Frontier, 88, 90, 85, 88, 90);
    }

    DEFAULT
}

const fn p(
    family: &'static str,
    cost: CostClass,
    coding: u8,
    reasoning: u8,
    long_context: u8,
    vision: u8,
    structured_output: u8,
) -> ModelPrior {
    ModelPrior {
        family,
        cost,
        coding,
        reasoning,
        long_context,
        vision,
        structured_output,
    }
}

/// The vendor family for a model id, or `"unknown"`.
pub fn family_of(model: &str) -> &'static str {
    prior_for(model).family
}

/// The vendor family for a worker by name, used where model ids aren't known
/// (e.g. `doctor`, which sees worker names but not their models).
pub fn family_of_worker(name: &str) -> &'static str {
    match name {
        "claude-code" | "anthropic" | "claude" => "claude",
        "grok" => "grok",
        "codex" | "openai" => "openai",
        "antigravity" | "google" | "gemini" => "google",
        "deepseek" => "deepseek",
        // OpenCode / Aider / Cursor depend on the user's provider config.
        _ => "unknown",
    }
}

/// The cascade ladder for a worker's models. The order is taken AS GIVEN —
/// config/discovery already orders models cheapest→strongest (discovery sorts by
/// the platform's pricing; users list them in tier order; adapters declare them
/// cheap→strong). Priors are no longer used to *reorder* the ladder — only for
/// family detection — so any model from any platform tiers by real cost or by
/// the user's intent rather than a hardcoded name table.
pub fn tier_ladder(models: &[String]) -> Vec<String> {
    models.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ladder_preserves_given_order() {
        // The ladder is the order as given (cheap→strong) — not re-sorted.
        let models = vec!["haiku".to_string(), "sonnet".to_string(), "opus".to_string()];
        assert_eq!(tier_ladder(&models), models);
    }

    #[test]
    fn families_classify() {
        assert_eq!(family_of("claude-opus-4-8"), "claude");
        assert_eq!(family_of("grok-build"), "grok");
        assert_eq!(family_of("gemini-2.5-flash"), "google");
        assert_eq!(family_of("deepseek-reasoner"), "deepseek");
        assert_eq!(family_of("something-unheard-of"), "unknown");
    }

    #[test]
    fn unknown_model_gets_conservative_default() {
        let pr = prior_for("mystery-model-9000");
        assert_eq!(pr.family, "unknown");
        assert_eq!(pr.cost, CostClass::Workhorse);
    }

    #[test]
    fn cost_class_orders_the_cascade() {
        assert!(CostClass::Cheap < CostClass::Workhorse);
        assert!(CostClass::Workhorse < CostClass::Frontier);
    }
}
