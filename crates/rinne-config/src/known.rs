//! Catalog of known OpenAI-compatible API providers, so `rinne connect` can set
//! one up without the user looking up base URLs or model ids (`CONTEXT.md` §9,
//! §17). Rinne stores only the env-var name and metadata — never the key.

/// A known API provider with sensible defaults.
pub struct KnownApiProvider {
    /// Provider name (the `[backends.api.<name>]` table key).
    pub name: &'static str,
    /// The env var the user exports their key into.
    pub key_env: &'static str,
    /// OpenAI-compatible base URL.
    pub base_url: &'static str,
    /// A suggested cheap→strong model ladder. Model ids change over time, so
    /// these are starting points the user can edit in config.
    pub models: &'static [&'static str],
}

/// The built-in catalog. All speak the OpenAI-compatible chat API.
pub const KNOWN_API_PROVIDERS: &[KnownApiProvider] = &[
    KnownApiProvider {
        name: "openai",
        key_env: "OPENAI_API_KEY",
        base_url: "https://api.openai.com/v1",
        models: &["gpt-5-mini", "gpt-5"],
    },
    KnownApiProvider {
        name: "deepseek",
        key_env: "DEEPSEEK_API_KEY",
        base_url: "https://api.deepseek.com/v1",
        models: &["deepseek-chat", "deepseek-reasoner"],
    },
    KnownApiProvider {
        name: "gemini",
        key_env: "GEMINI_API_KEY",
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        models: &["gemini-2.5-flash", "gemini-2.5-pro"],
    },
    KnownApiProvider {
        name: "nvidia",
        key_env: "NVIDIA_API_KEY",
        base_url: "https://integrate.api.nvidia.com/v1",
        // NVIDIA NIM hosts many models under org/model ids; the user supplies
        // the one(s) their key covers, e.g. `deepseek-ai/deepseek-v4-pro`.
        models: &[],
    },
    KnownApiProvider {
        name: "groq",
        key_env: "GROQ_API_KEY",
        base_url: "https://api.groq.com/openai/v1",
        models: &["llama-3.3-70b-versatile"],
    },
    KnownApiProvider {
        name: "openrouter",
        key_env: "OPENROUTER_API_KEY",
        base_url: "https://openrouter.ai/api/v1",
        models: &[],
    },
    KnownApiProvider {
        name: "mistral",
        key_env: "MISTRAL_API_KEY",
        base_url: "https://api.mistral.ai/v1",
        models: &["mistral-small-latest", "mistral-large-latest"],
    },
    KnownApiProvider {
        name: "together",
        key_env: "TOGETHER_API_KEY",
        base_url: "https://api.together.xyz/v1",
        models: &[],
    },
    KnownApiProvider {
        name: "xai",
        key_env: "XAI_API_KEY",
        base_url: "https://api.x.ai/v1",
        models: &["grok-3-mini", "grok-3"],
    },
    // Note: cloudflare needs account-specific base_url
    // (`…/accounts/{ACCOUNT_ID}/ai/v1`); connect via --base-url or the app UI.
];

/// Look up a known API provider by name.
pub fn known_api_provider(name: &str) -> Option<&'static KnownApiProvider> {
    KNOWN_API_PROVIDERS.iter().find(|p| p.name == name)
}
