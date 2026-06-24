//! The configuration model (`CONTEXT.md` §18).
//!
//! Mirrors the documented `config.toml` shape. Every field has a sensible
//! default so a zero-config install still runs; layering (defaults ← global ←
//! per-project ← env) is applied in [`crate::load`].

use serde::{Deserialize, Serialize};

/// Top-level Rinne configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub conductor: ConductorConfig,
    #[serde(rename = "loop")]
    pub loop_: LoopConfig,
    pub preferences: Preferences,
    pub backends: Backends,
    /// Per-harness default model, e.g. `claude-code = "sonnet"`. Switchable
    /// between sessions by editing config (`CONTEXT.md` §7).
    pub models: ModelDefaults,
}

/// `[models]` — default model per worker name.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ModelDefaults {
    #[serde(flatten)]
    pub by_worker: std::collections::BTreeMap<String, String>,
}

/// `[conductor]` — the cheap, decoupled planning backend (`CONTEXT.md` §7).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConductorConfig {
    /// `cloudflare | groq | nvidia | local | harness`.
    pub backend: ConductorBackend,
    /// The model id on that backend.
    pub model: String,
    /// Override the backend base URL (else a per-backend default is used).
    #[serde(default)]
    pub base_url: Option<String>,
    /// Override the env var holding the backend's API key.
    #[serde(default)]
    pub key_env: Option<String>,
    /// Cloudflare account id, required to build its OpenAI-compatible URL.
    #[serde(default)]
    pub account_id: Option<String>,
}

impl Default for ConductorConfig {
    fn default() -> Self {
        Self {
            backend: ConductorBackend::Cloudflare,
            model: "@cf/moonshotai/kimi-k2.7-code".to_string(),
            base_url: None,
            key_env: None,
            account_id: None,
        }
    }
}

/// The configurable conductor backends (all OpenAI-compatible, §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConductorBackend {
    Cloudflare,
    Groq,
    Nvidia,
    /// Local via Ollama, fully offline.
    Local,
    /// Fall back to the user's cheapest installed harness as conductor.
    Harness,
}

/// `[loop]` — loop engine limits and safety rails (`CONTEXT.md` §18).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoopConfig {
    pub max_iterations_per_node: u32,
    pub global_budget_minutes: u32,
    /// Block any diff that weakens or deletes tests.
    pub test_ratchet: bool,
    /// Identical-failure loops before escalating to a human evaluator.
    pub stuck_loop_threshold: u32,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            max_iterations_per_node: 8,
            global_budget_minutes: 120,
            test_ratchet: true,
            stuck_loop_threshold: 3,
        }
    }
}

/// `[preferences]` — routing preferences (`CONTEXT.md` §18).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Preferences {
    /// `harness | api | balanced` — the family preference order.
    pub prefer: PreferFamily,
    /// Optional per-role pins, e.g. `evaluator = "api:gpt-5.5"`.
    pub roles: std::collections::BTreeMap<String, String>,
    /// Optional per-role model pins, e.g. `evaluator = "haiku"`.
    pub models: std::collections::BTreeMap<String, String>,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            prefer: PreferFamily::Harness,
            roles: std::collections::BTreeMap::new(),
            models: std::collections::BTreeMap::new(),
        }
    }
}

/// The worker-family preference (`CONTEXT.md` §13, §18).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PreferFamily {
    Harness,
    Api,
    Balanced,
}

/// `[backends]` — which workers exist and how they authenticate.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Backends {
    pub harness: HarnessBackends,
    pub api: ApiBackends,
}

/// `[backends.harness]` — enabled harness CLIs (`CONTEXT.md` §18).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HarnessBackends {
    /// Harness names the user has opted into, e.g. `["claude-code", "codex"]`.
    pub enabled: Vec<String>,
}

impl Default for HarnessBackends {
    fn default() -> Self {
        Self {
            enabled: vec![
                "claude-code".to_string(),
                "codex".to_string(),
                "opencode".to_string(),
                "grok".to_string(),
                "cursor-agent".to_string(),
                "aider".to_string(),
                "antigravity".to_string(),
            ],
        }
    }
}

/// `[backends.api.*]` — API workers keyed by provider name.
///
/// Each provider names the environment variable that holds its key; Rinne never
/// stores the key itself (`CONTEXT.md` §9).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ApiBackends {
    // `flatten` collects each `[backends.api.<provider>]` table into the map;
    // it is incompatible with `deny_unknown_fields`.
    #[serde(flatten)]
    pub providers: std::collections::BTreeMap<String, ApiProvider>,
}

/// A single `[backends.api.<provider>]` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiProvider {
    /// The env var holding this provider's key, e.g. `OPENAI_API_KEY`. Rinne
    /// reads the key from this var at call time and never stores it.
    pub key_env: String,
    /// Optional base URL override (else a per-provider default is used).
    #[serde(default)]
    pub base_url: Option<String>,
    /// Default model for this provider, e.g. `gpt-5-mini`.
    #[serde(default)]
    pub model: Option<String>,
    /// Optional model ladder cheap→strong, powering tiering and the cascade.
    #[serde(default)]
    pub models: Vec<String>,
    /// Extra JSON merged into every chat request to this provider, for
    /// provider-specific params (e.g. NVIDIA's
    /// `chat_template_kwargs = { thinking = false }` to disable a reasoning
    /// model's slow thinking mode). A TOML table here becomes request JSON.
    #[serde(default)]
    pub extra_body: Option<serde_json::Value>,
}
