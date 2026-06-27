//! Conductor backends (`CONTEXT.md` §7).
//!
//! The conductor runs prompted on a cheap, decoupled backend so planning never
//! burns the quota meant for real work. Every backend is reached through one
//! [`PlanBackend`] trait, so the conductor is agnostic to whether it is talking
//! to an OpenAI-compatible HTTP endpoint or a local harness used as conductor
//! (the §7 fallback). All configured options are OpenAI-compatible, so one HTTP
//! client covers Cloudflare Workers AI, Groq, NVIDIA NIM, and local Ollama.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use rinne_config::model::{ConductorBackend, ConductorConfig};
use rinne_core::worker::{
    Constraints, ContextPacket, ExecStatus, ExecuteRequest, Role, Worker,
};
use rinne_core::{Result, RinneError};
use rinne_workers::transport::http::{ChatMessage, ChatRequest, OpenAiClient};

/// A backend that completes a planning prompt and returns the raw model text.
#[async_trait]
pub trait PlanBackend: Send + Sync {
    /// A short label for narration / logs.
    fn name(&self) -> &str;
    /// Complete a system+user prompt, returning the raw response text.
    async fn complete(&self, system: &str, user: &str) -> Result<String>;
}

/// An OpenAI-compatible HTTP backend.
pub struct OpenAiBackend {
    name: String,
    client: OpenAiClient,
    model: String,
}

impl OpenAiBackend {
    pub fn new(name: impl Into<String>, base_url: &str, api_key: Option<String>, model: &str) -> Self {
        Self {
            name: name.into(),
            client: OpenAiClient::new(base_url, api_key),
            model: model.to_string(),
        }
    }
}

#[async_trait]
impl PlanBackend for OpenAiBackend {
    fn name(&self) -> &str {
        &self.name
    }

    async fn complete(&self, system: &str, user: &str) -> Result<String> {
        let req = ChatRequest {
            model: self.model.clone(),
            messages: vec![ChatMessage::system(system), ChatMessage::user(user)],
            temperature: Some(0.2),
            extra: None,
        };
        // Planning is not streamed to the user; discard events.
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let resp = self
            .client
            .chat_stream(&req, &tx, &CancellationToken::new())
            .await?;
        Ok(resp.content)
    }
}

/// A harness worker pressed into service as the conductor — the §7 fallback when
/// no API backend is configured ("the user's cheapest installed harness").
pub struct HarnessBackend {
    worker: Arc<dyn Worker>,
    workspace: PathBuf,
}

impl HarnessBackend {
    pub fn new(worker: Arc<dyn Worker>, workspace: PathBuf) -> Self {
        Self { worker, workspace }
    }
}

#[async_trait]
impl PlanBackend for HarnessBackend {
    fn name(&self) -> &str {
        &self.worker.descriptor().name
    }

    async fn complete(&self, system: &str, user: &str) -> Result<String> {
        let request = ExecuteRequest {
            role: Role::Planner,
            instruction: format!("{system}\n\n{user}"),
            context: ContextPacket::default(),
            workspace: self.workspace.clone(),
            constraints: Constraints {
                timeout_secs: Some(180),
                ..Default::default()
            },
        };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let result = self
            .worker
            .execute(request, tx, CancellationToken::new())
            .await?;
        if !matches!(result.status, ExecStatus::Success) {
            // Surface the worker's own output (stdout + stderr) so the failure is
            // diagnosable — "exited 1" alone hides why (auth, a rejected flag…).
            let detail = result.transcript.trim();
            let snippet = tail(detail, 800);
            let hint = if snippet.is_empty() {
                format!(
                    "no output — check `{0}` works standalone and is logged in (run `{0} -p hi`)",
                    self.name()
                )
            } else {
                snippet.to_string()
            };
            return Err(RinneError::Conductor(format!(
                "harness conductor `{}` failed: {:?}\n{hint}",
                self.name(),
                result.status
            )));
        }
        Ok(result.result)
    }
}

/// Keep the last `max` chars of `s` (where the real error usually is).
fn tail(s: &str, max: usize) -> &str {
    if s.chars().count() <= max {
        return s;
    }
    let start = s.char_indices().rev().nth(max - 1).map(|(i, _)| i).unwrap_or(0);
    &s[start..]
}

/// The keychain provider name and env var a conductor backend authenticates
/// with, or `None` for keyless backends (`local`, `harness`). A config
/// `key_env` overrides the default env var. The provider name is the backend's
/// own name, so a key stored via `/connect groq` is reused by the conductor.
pub fn conductor_credential(config: &ConductorConfig) -> Option<(String, String)> {
    let default_env = match config.backend {
        ConductorBackend::Groq => "GROQ_API_KEY",
        ConductorBackend::Nvidia => "NVIDIA_API_KEY",
        ConductorBackend::Cloudflare => "CLOUDFLARE_API_TOKEN",
        ConductorBackend::Local | ConductorBackend::Harness => return None,
    };
    let env = config.key_env.clone().unwrap_or_else(|| default_env.to_string());
    let provider = format!("{:?}", config.backend).to_lowercase();
    Some((provider, env))
}

/// The endpoint a conductor backend talks to (explicit `base_url`, else a
/// per-backend default), or `None` when it cannot be constructed (Cloudflare
/// without an `account_id`) or is the harness fallback.
pub fn conductor_base_url(config: &ConductorConfig) -> Option<String> {
    if let Some(base) = config.base_url.clone() {
        return Some(base);
    }
    match config.backend {
        ConductorBackend::Groq => Some("https://api.groq.com/openai/v1".into()),
        ConductorBackend::Nvidia => Some("https://integrate.api.nvidia.com/v1".into()),
        ConductorBackend::Local => Some("http://localhost:11434/v1".into()),
        ConductorBackend::Cloudflare => config
            .account_id
            .as_ref()
            .map(|id| format!("https://api.cloudflare.com/client/v4/accounts/{id}/ai/v1")),
        ConductorBackend::Harness => None,
    }
}

/// Resolve an OpenAI-compatible backend from config, if one is both selected and
/// has its key available. The key is resolved env-first then OS keychain (so a
/// token stored once persists across shells). Returns `Ok(None)` when the
/// backend needs a key that is unset (so the caller falls back), cannot build a
/// URL, or is `harness`.
pub fn resolve_openai(config: &ConductorConfig) -> Result<Option<OpenAiBackend>> {
    let base_url = match conductor_base_url(config) {
        Some(b) => b,
        None => return Ok(None), // harness, or Cloudflare without account_id/base_url
    };

    // Keyless backends (local Ollama) need no credential; keyed backends must
    // resolve a key from env or keychain, else we signal a fallback.
    let api_key = match conductor_credential(config) {
        Some((provider, env)) => match rinne_config::secrets::resolve_api_key(&provider, &env) {
            Some(k) => Some(k),
            None => return Ok(None),
        },
        None => None,
    };

    let name = format!("{:?}", config.backend).to_lowercase();
    Ok(Some(OpenAiBackend::new(
        name,
        &base_url,
        api_key,
        &config.model,
    )))
}
