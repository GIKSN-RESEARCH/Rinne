//! The worker capability descriptor (`CONTEXT.md` §8).
//!
//! Every worker advertises what it can do, how it authenticates, its quota
//! model, latency profile, and transport. The scheduler resolves a node's
//! `needs` against these descriptors at dispatch time (`CONTEXT.md` §7, §13).

use serde::{Deserialize, Serialize};

/// A capability a worker can satisfy (`CONTEXT.md` §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Capability {
    CodeEdit,
    RepoAware,
    WebSearch,
    Vision,
    LongContext,
    ToolRun,
    CodeReview,
    Reasoning,
    Writing,
}

/// How a worker authenticates (`CONTEXT.md` §9). Surfaced by `doctor` so the
/// user always knows which workers are free and which are metered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthMode {
    /// Honors an existing subscription login (Claude Pro/Max, Grok, ChatGPT).
    /// Not metered per-call.
    Subscription,
    /// Uses the user's own API key. Always metered.
    ApiKey,
    /// A free tier (e.g. a conductor backend's daily allowance).
    Free,
    /// Could not be determined.
    Unknown,
}

impl AuthMode {
    /// Whether using this worker bills the user per call.
    pub fn is_metered(self) -> bool {
        matches!(self, AuthMode::ApiKey)
    }

    pub fn label(self) -> &'static str {
        match self {
            AuthMode::Subscription => "subscription",
            AuthMode::ApiKey => "api-key",
            AuthMode::Free => "free",
            AuthMode::Unknown => "unknown",
        }
    }
}

/// The worker family (`CONTEXT.md` §8). Drives dispatch and context assembly:
/// a harness is an autonomous agent given a chunky task; an API worker is a raw
/// model given a precise instruction with context inlined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkerFamily {
    /// Wraps a native headless CLI call (e.g. `claude -p`). Reads the repo
    /// itself.
    Harness,
    /// A direct model API call on the user's own key. Stateless; sees only what
    /// is sent.
    Api,
}

/// The transport a worker speaks over (`CONTEXT.md` §8, §14). `Acp` is reserved
/// for V2 and not implemented in v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    /// `tokio::process` with piped stdio, structured-output parsing.
    SubprocessJson,
    /// `reqwest` streaming, for API workers and the conductor backend.
    Http,
    /// Thin JSON-RPC 2.0 over stdio (V2).
    Acp,
}

/// A worker's quota modeled as a refilling token bucket (`CONTEXT.md` §13).
///
/// The scheduler spreads load to avoid hitting limits. A subscription's
/// rate-limit window, an API tier's RPM, and a free tier's daily reset all map
/// onto this one shape.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct QuotaModel {
    /// Maximum tokens (request units) the bucket holds.
    pub capacity: f64,
    /// Tokens refilled per minute.
    pub refill_per_minute: f64,
}

impl QuotaModel {
    /// An effectively-unmetered bucket, for workers without a known limit.
    pub fn unlimited() -> Self {
        Self {
            capacity: f64::INFINITY,
            refill_per_minute: f64::INFINITY,
        }
    }
}

/// A coarse latency expectation (`CONTEXT.md` §8, §21). Harness workers spawn a
/// process and wait on model think time, so they are inherently slower than a
/// raw API call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LatencyProfile {
    /// Raw API call, low latency.
    Fast,
    /// Typical harness round-trip.
    Medium,
    /// Heavy or known-slow harness.
    Slow,
}

/// The full self-description a worker advertises to the scheduler.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerDescriptor {
    /// Stable worker name, e.g. `claude-code`, `codex`, `anthropic`.
    pub name: String,
    pub family: WorkerFamily,
    pub capabilities: Vec<Capability>,
    pub auth_mode: AuthMode,
    pub quota: QuotaModel,
    pub latency: LatencyProfile,
    pub transport: Transport,
    /// Models this worker can run (e.g. `["opus", "sonnet", "haiku"]`). Empty
    /// means the worker has a single fixed model. The conductor picks from this
    /// list per node to optimize cost/latency (`CONTEXT.md` §7).
    #[serde(default)]
    pub models: Vec<String>,
}

impl WorkerDescriptor {
    /// Whether this worker advertises the given capability.
    pub fn has(&self, cap: Capability) -> bool {
        self.capabilities.contains(&cap)
    }

    /// Whether this worker satisfies every capability in `needs` — the
    /// scheduler's core match test (`CONTEXT.md` §7, §13).
    pub fn satisfies(&self, needs: &[Capability]) -> bool {
        needs.iter().all(|n| self.has(*n))
    }
}
