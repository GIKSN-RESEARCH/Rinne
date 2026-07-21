//! The registry of known harness workers and how to detect them
//! (`CONTEXT.md` §16).

use super::types::AuthMode;

/// A static description of a known harness CLI: how to find it, how to smoke
/// test it, and how it authenticates.
pub struct KnownHarness {
    /// Stable Rinne worker name (matches the `[backends.harness] enabled` list).
    pub name: &'static str,
    /// The executable to look for on `PATH`.
    pub binary: &'static str,
    /// A cheap, side-effect-free invocation used as a smoke test.
    pub smoke_args: &'static [&'static str],
    /// The auth mode when the worker uses its native login.
    pub base_auth: AuthMode,
    /// An env var that, when set, overrides the auth mode to API-key billing.
    pub override_env: Option<&'static str>,
    /// Whether that override is a silent-billing footgun warranting a loud
    /// warning (`CONTEXT.md` §9, §21) — Claude especially.
    pub footgun: bool,
    /// The native command the user runs to authenticate this worker. Rinne
    /// holds no credentials, so `connect` surfaces this rather than storing a
    /// token (`CONTEXT.md` §9).
    pub login_hint: &'static str,
}

/// All harness CLIs Rinne knows how to detect (`CONTEXT.md` §16).
///
/// Detection is cheap, so `doctor` probes every known harness regardless of the
/// enabled list; the enabled flag is reported separately.
pub const KNOWN_HARNESSES: &[KnownHarness] = &[
    KnownHarness {
        name: "claude-code",
        binary: "claude",
        smoke_args: &["--version"],
        base_auth: AuthMode::Subscription,
        // A stray ANTHROPIC_API_KEY silently moves a Max user onto metered API
        // billing and non-interactive `-p` prefers it (`CONTEXT.md` §9).
        override_env: Some("ANTHROPIC_API_KEY"),
        footgun: true,
        login_hint:
            "claude  (subscription login is honored automatically; run `claude` once to sign in)",
    },
    KnownHarness {
        name: "codex",
        binary: "codex",
        smoke_args: &["--version"],
        base_auth: AuthMode::Subscription,
        override_env: None,
        footgun: false,
        login_hint: "codex login  (ChatGPT login or set OPENAI_API_KEY)",
    },
    KnownHarness {
        name: "opencode",
        binary: "opencode",
        smoke_args: &["--version"],
        base_auth: AuthMode::Subscription,
        override_env: None,
        footgun: false,
        login_hint: "opencode auth login",
    },
    KnownHarness {
        name: "grok",
        binary: "grok",
        smoke_args: &["--version"],
        base_auth: AuthMode::Subscription,
        override_env: Some("XAI_API_KEY"),
        footgun: false,
        login_hint: "grok login  (or set XAI_API_KEY)",
    },
    KnownHarness {
        name: "cursor-agent",
        binary: "cursor-agent",
        smoke_args: &["--version"],
        base_auth: AuthMode::Subscription,
        override_env: None,
        footgun: false,
        login_hint: "cursor-agent login",
    },
    KnownHarness {
        name: "aider",
        binary: "aider",
        smoke_args: &["--version"],
        base_auth: AuthMode::ApiKey,
        override_env: None,
        footgun: false,
        login_hint: "set your provider API key env var (e.g. OPENAI_API_KEY)",
    },
    KnownHarness {
        name: "antigravity",
        binary: "agy",
        smoke_args: &["--version"],
        base_auth: AuthMode::Subscription,
        override_env: None,
        footgun: false,
        login_hint: "agy  (completes Google OAuth on first run)",
    },
];

/// Look up a known harness by its Rinne worker name.
pub fn harness_by_name(name: &str) -> Option<&'static KnownHarness> {
    KNOWN_HARNESSES.iter().find(|h| h.name == name)
}
