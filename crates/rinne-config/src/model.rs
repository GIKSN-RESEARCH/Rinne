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
    pub update: UpdateConfig,
    /// Connected MCP servers (`MCP_SKILLS.md` §10), keyed by name.
    pub mcp: McpConfig,
    /// Tier routing rules (`CONDUCTOR_LOOP_PLAN.md` §3.5).
    pub routing: RoutingConfig,
    /// Live harness limit probes and status-line chip (`/limit-usage`).
    pub limits: LimitsConfig,
    /// Visible harness delegation (“Harness Stage”) — `plan.md`.
    pub harness_stage: HarnessStageConfig,
}

/// `[harness_stage]` — open harness agent CLIs so the user can see them work
/// (PTY / Stage UI) while Rinne still conducts the DAG (`plan.md`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HarnessStageConfig {
    /// When to run harness workers under a visible session (PTY / Stage UI).
    pub mode: HarnessStageMode,
    /// `auto` = pass non-interactive approve flags; `human` = user approves in the pane.
    pub approvals: HarnessApprovals,
    /// Cap concurrent visible harness sessions (subscriptions + CPU).
    pub max_sessions: u8,
}

impl Default for HarnessStageConfig {
    fn default() -> Self {
        Self {
            // hybrid: visible in interactive TUI/GUI; headless for `rinne -p` / CI
            mode: HarnessStageMode::Hybrid,
            approvals: HarnessApprovals::Auto,
            max_sessions: 3,
        }
    }
}

/// When harness workers open a visible Stage session vs stay headless.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum HarnessStageMode {
    /// Never open visible sessions (always piped headless).
    Headless,
    /// Always try visible sessions (even non-TTY — may fall back).
    Visible,
    /// Visible when Rinne is interactive (TTY/GUI); headless for automation.
    #[default]
    Hybrid,
    /// Do not run harness workers (API-only). Rare escape hatch.
    Off,
}

impl HarnessStageMode {
    /// Whether a harness node should request a visible Stage/PTY session.
    pub fn wants_visible(self, interactive: bool) -> bool {
        match self {
            HarnessStageMode::Headless | HarnessStageMode::Off => false,
            HarnessStageMode::Visible => true,
            HarnessStageMode::Hybrid => interactive,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            HarnessStageMode::Headless => "headless",
            HarnessStageMode::Visible => "visible",
            HarnessStageMode::Hybrid => "hybrid",
            HarnessStageMode::Off => "off",
        }
    }
}

/// How permission prompts inside a visible harness are handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum HarnessApprovals {
    #[default]
    Auto,
    Human,
}

/// `[limits]` — subscription usage probes, status chip, threshold alerts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsConfig {
    /// Show the compact `limits N%` chip on the TUI status line.
    pub show_status: bool,
    /// How often (seconds) the TUI re-probes while idle. Floor 30.
    /// Default is intentionally high (~3 min) so we stay under Anthropic's
    /// OAuth usage-endpoint budget when the Claude Code–shaped UA is used.
    pub poll_secs: u64,
    /// Fire one-shot narration when a window crosses these % thresholds.
    pub alert_at: Vec<u8>,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            show_status: true,
            poll_secs: 180,
            alert_at: vec![50, 75, 90, 100],
        }
    }
}

/// `[update]` — automatic new-release notification.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UpdateConfig {
    /// Whether to check GitHub Releases for a newer version on startup. The
    /// check is cached for a day, runs in the background, and never blocks a
    /// command. Set to `false`, or export `RINNE_NO_UPDATE_CHECK=1`, to disable.
    pub check: bool,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self { check: true }
    }
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
    /// OpenAI-compatible planner backend or `harness` / `local`.
    /// See [`ConductorBackend`].
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
    /// Frontier planner model on the same backend (used when auto-escalating).
    #[serde(default)]
    pub escalation_model: Option<String>,
    /// Full conductor ladder cheap→frontier. When non-empty, overrides `model` +
    /// `escalation_model`.
    #[serde(default)]
    pub models: Vec<String>,
    /// Escalate the planner among eligible models when the goal or validation demands it.
    #[serde(default = "default_true")]
    pub auto_escalate: bool,
    /// Max planner rung steps per plan/replan invocation.
    #[serde(default = "default_max_conductor_escalations")]
    pub max_escalations: u8,
    /// Reject non-conductor-eligible models in the ladder (`CONDUCTOR_LOOP_PLAN.md` §3.9.1).
    #[serde(default = "default_true")]
    pub only_eligible_models: bool,
    /// Extra model ids the user trusts as planners (bypasses the built-in gate).
    #[serde(default)]
    pub allowlist: Vec<String>,
}

fn default_max_conductor_escalations() -> u8 {
    2
}

fn default_true() -> bool {
    true
}

impl Default for ConductorConfig {
    fn default() -> Self {
        Self {
            backend: ConductorBackend::Cloudflare,
            model: "@cf/moonshotai/kimi-k2.7-code".to_string(),
            base_url: None,
            key_env: None,
            account_id: None,
            escalation_model: None,
            models: Vec::new(),
            auto_escalate: true,
            max_escalations: 2,
            only_eligible_models: true,
            allowlist: Vec::new(),
        }
    }
}

impl ConductorConfig {
    /// Resolved planner ladder: `models[]` if set, else `[model, escalation?]`.
    pub fn planner_ladder(&self) -> Vec<String> {
        if !self.models.is_empty() {
            return self.models.clone();
        }
        let mut ladder = vec![self.model.clone()];
        if let Some(ref esc) = self.escalation_model {
            if esc != &self.model {
                ladder.push(esc.clone());
            }
        }
        ladder
    }
}

/// The configurable conductor backends (all OpenAI-compatible, §7).
///
/// API variants match `KNOWN_API_PROVIDERS` / `rinne connect` names so a key
/// stored once is reused by the planner. `Harness` uses installed CLI workers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConductorBackend {
    Cloudflare,
    Groq,
    Nvidia,
    OpenRouter,
    OpenAi,
    Deepseek,
    Gemini,
    Mistral,
    Together,
    Xai,
    /// Local via Ollama, fully offline.
    Local,
    /// Fall back to the user's cheapest installed harness as conductor.
    Harness,
}

impl ConductorBackend {
    /// Stable config / CLI name (`openrouter`, `cloudflare`, …).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cloudflare => "cloudflare",
            Self::Groq => "groq",
            Self::Nvidia => "nvidia",
            Self::OpenRouter => "openrouter",
            Self::OpenAi => "openai",
            Self::Deepseek => "deepseek",
            Self::Gemini => "gemini",
            Self::Mistral => "mistral",
            Self::Together => "together",
            Self::Xai => "xai",
            Self::Local => "local",
            Self::Harness => "harness",
        }
    }

    /// Parse a user/CLI backend name (aliases included).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "cloudflare" | "cf" => Some(Self::Cloudflare),
            "groq" => Some(Self::Groq),
            "nvidia" => Some(Self::Nvidia),
            "openrouter" => Some(Self::OpenRouter),
            "openai" => Some(Self::OpenAi),
            "deepseek" => Some(Self::Deepseek),
            "gemini" | "google" => Some(Self::Gemini),
            "mistral" => Some(Self::Mistral),
            "together" => Some(Self::Together),
            "xai" => Some(Self::Xai),
            "local" | "ollama" => Some(Self::Local),
            "harness" => Some(Self::Harness),
            _ => None,
        }
    }

    /// Whether this backend needs an API key (vs local/harness).
    pub fn needs_api_key(self) -> bool {
        !matches!(self, Self::Local | Self::Harness)
    }
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

/// `[routing]` — tier matrix overrides (`CONDUCTOR_LOOP_PLAN.md` §3.5).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoutingConfig {
    pub rules_file: Option<String>,
    pub exemplars_file: Option<String>,
    #[serde(default)]
    pub tiers: std::collections::BTreeMap<String, TierRoutingRule>,
    #[serde(default)]
    pub combinations: CombinationRules,
    #[serde(default)]
    pub goal_keywords: std::collections::BTreeMap<String, String>,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        let mut tiers = std::collections::BTreeMap::new();
        tiers.insert(
            "T4".into(),
            TierRoutingRule {
                require_human_checkpoint: true,
                ..Default::default()
            },
        );
        Self {
            rules_file: None,
            exemplars_file: Some(".rinne/routing-exemplars.toml".into()),
            tiers,
            combinations: CombinationRules::default(),
            goal_keywords: std::collections::BTreeMap::new(),
        }
    }
}

/// Per-tier routing rule from config.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct TierRoutingRule {
    pub min_cost: Option<String>,
    pub max_cost: Option<String>,
    pub require_tool_eval: bool,
    pub require_human_checkpoint: bool,
}

/// Cross-cutting combination rules.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CombinationRules {
    pub allow_same_family_ai_review: bool,
}

impl Default for CombinationRules {
    fn default() -> Self {
        Self {
            allow_same_family_ai_review: true,
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

/// `[mcp]` — connected MCP servers, keyed by name (`MCP_SKILLS.md` §10).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct McpConfig {
    /// Each `[mcp.servers.<name>]` table.
    pub servers: std::collections::BTreeMap<String, McpServer>,
}

/// How a server is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    /// A local subprocess server, spoken to over stdio.
    Stdio,
    /// A remote server reached over Streamable HTTP.
    Http,
}

/// A single `[mcp.servers.<name>]` table. Secrets are never stored here — a
/// remote server's bearer token lives in the OS keychain, named by `key_env`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServer {
    /// `stdio | http`.
    pub transport: McpTransport,
    /// For stdio: the server program (e.g. `npx`).
    #[serde(default)]
    pub command: Option<String>,
    /// For stdio: the program's arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// For stdio: extra (non-secret) environment variables for the server.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// For http: the server endpoint URL.
    #[serde(default)]
    pub url: Option<String>,
    /// For http: extra (non-secret) request headers.
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    /// The env var / keychain name holding a bearer token, if the server needs
    /// one. Resolved env-first then keychain, like an API key.
    #[serde(default)]
    pub key_env: Option<String>,
    /// Whether this server is active. Disabled servers are kept but not used.
    #[serde(default = "mcp_default_true")]
    pub enabled: bool,
    /// Allowlisted tool names (`["*"]` = all). A tool not on the list is never
    /// offered to a worker (`MCP_SKILLS.md` §12).
    #[serde(default = "mcp_default_tools_allow")]
    pub tools_allow: Vec<String>,
    /// Force the host path for this server's tool nodes even on a harness
    /// worker, so Rinne gates every call's arguments — for sensitive/write
    /// servers (`MCP_SKILLS.md` §6 host-only).
    #[serde(default)]
    pub host_only: bool,
    /// How a stored token is presented to the server (`MCP_SKILLS.md` §10):
    /// - `bearer` — HTTP `Authorization: Bearer <token>` (the common default),
    /// - `apikey` — a custom HTTP header (`auth_header`, default `X-API-Key`),
    /// - `env`    — a stdio server environment variable (`auth_header`).
    ///
    /// Absent means bearer for an HTTP server with a token (back-compat) and no
    /// token injection for stdio. The token itself lives in the keychain, never
    /// here.
    #[serde(default)]
    pub auth: Option<String>,
    /// The header name (for `apikey`) or environment variable (for `env`) the
    /// token is placed in. Defaults to `X-API-Key` for `apikey`.
    #[serde(default)]
    pub auth_header: Option<String>,
}

fn mcp_default_true() -> bool {
    true
}

fn mcp_default_tools_allow() -> Vec<String> {
    vec!["*".to_string()]
}

impl McpServer {
    /// Whether a tool name is allowed by this server's allowlist.
    pub fn allows_tool(&self, tool: &str) -> bool {
        self.tools_allow.iter().any(|t| t == "*" || t == tool)
    }

    /// For an HTTP server, the `(header_name, value_prefix)` a resolved token is
    /// injected as. Defaults to bearer so a server with a token but no explicit
    /// `auth` keeps working. `None` only for a stdio server (see [`stdio_auth_env`]).
    ///
    /// [`stdio_auth_env`]: McpServer::stdio_auth_env
    pub fn http_auth(&self) -> (String, String) {
        match self.auth.as_deref() {
            Some("apikey") => (
                self.auth_header.clone().unwrap_or_else(|| "X-API-Key".into()),
                String::new(),
            ),
            // "bearer" or unset → Authorization: Bearer <token>
            _ => ("Authorization".into(), "Bearer ".into()),
        }
    }

    /// For a stdio server, the environment variable a resolved token is set in,
    /// if this server uses env-based auth.
    pub fn stdio_auth_env(&self) -> Option<String> {
        if self.auth.as_deref() == Some("env") {
            self.auth_header.clone()
        } else {
            None
        }
    }
}

#[cfg(test)]
mod mcp_tests {
    use super::*;

    #[test]
    fn parses_with_sensible_defaults() {
        let s: McpServer = toml::from_str("transport = \"stdio\"\ncommand = \"npx\"\n").unwrap();
        assert_eq!(s.transport, McpTransport::Stdio);
        assert_eq!(s.command.as_deref(), Some("npx"));
        assert!(s.enabled, "enabled defaults true");
        assert_eq!(s.tools_allow, vec!["*"], "allowlist defaults to all");
        assert!(!s.host_only);
    }

    #[test]
    fn rejects_unknown_fields() {
        let bad = "transport = \"stdio\"\nbogus = 1\n";
        assert!(toml::from_str::<McpServer>(bad).is_err());
    }

    #[test]
    fn requires_a_transport() {
        assert!(toml::from_str::<McpServer>("command = \"npx\"\n").is_err());
    }

    #[test]
    fn allowlist_gates_tool_names() {
        let mut s: McpServer = toml::from_str("transport = \"http\"\n").unwrap();
        assert!(s.allows_tool("anything"), "default `*` allows all");
        s.tools_allow = vec!["read".into(), "list".into()];
        assert!(s.allows_tool("read"));
        assert!(!s.allows_tool("delete"));
    }
}
