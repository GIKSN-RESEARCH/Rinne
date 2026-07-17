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
    Constraints, ContextPacket, EventSink, ExecStatus, ExecuteRequest, Role, Worker, WorkerEvent,
};
use rinne_core::{Result, RinneError};
use rinne_workers::transport::http::{ChatMessage, ChatRequest, OpenAiClient};

/// Whether Stage visibility is requested (set by CLI/GUI via env).
fn stage_visible_from_env() -> bool {
    matches!(
        std::env::var("RINNE_HARNESS_STAGE_VISIBLE").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

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

    /// Switch the model for the next `complete()` call (conductor self-escalation).
    pub fn set_model(&mut self, model: impl Into<String>) {
        self.model = model.into();
    }

    pub fn model(&self) -> &str {
        &self.model
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
///
/// When Stage is visible, this opens the same PTY/session path as generator
/// nodes so the planner harness is leveraged and shown on the Stage (`plan.md`).
pub struct HarnessBackend {
    worker: Arc<dyn Worker>,
    workspace: PathBuf,
    /// Optional sink so SessionOpened / tool events reach the Stage UI.
    events: Option<EventSink>,
}

impl HarnessBackend {
    pub fn new(worker: Arc<dyn Worker>, workspace: PathBuf) -> Self {
        Self {
            worker,
            workspace,
            events: None,
        }
    }

    /// Forward harness worker events (Stage opens, messages, tools) to the UI.
    pub fn with_events(mut self, events: EventSink) -> Self {
        self.events = Some(events);
        self
    }
}

#[async_trait]
impl PlanBackend for HarnessBackend {
    fn name(&self) -> &str {
        &self.worker.descriptor().name
    }

    async fn complete(&self, system: &str, user: &str) -> Result<String> {
        let visible = stage_visible_from_env();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        // Remap SessionOpened so Stage can key the pane as `conductor` (planner).
        let forward = self.events.clone();
        let worker_name = self.worker.descriptor().name.clone();
        let forwarder = tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                if let Some(sink) = &forward {
                    let ev = match ev {
                        WorkerEvent::SessionOpened {
                            worker,
                            model,
                            backend,
                        } => WorkerEvent::SessionOpened {
                            worker: format!("{worker} (conductor)"),
                            model,
                            backend,
                        },
                        other => other,
                    };
                    let _ = sink.send(ev);
                } else if matches!(ev, WorkerEvent::SessionOpened { .. }) {
                    tracing::info!(worker = %worker_name, "conductor harness session opened");
                }
            }
        });
        // Prefer a stronger model from the harness ladder when available.
        let model = self
            .worker
            .descriptor()
            .models
            .last()
            .cloned()
            .or_else(|| self.worker.descriptor().models.first().cloned());
        // Single-turn planner in Terminal (when Stage is on). Needs enough time
        // for a real plan — 90s was killing grok mid-run and falling through.
        // Auth failures still fail-fast via non-zero exit / login text below.
        let request = ExecuteRequest {
            role: Role::Planner,
            instruction: format!("{system}\n\n{user}"),
            context: ContextPacket::default(),
            workspace: self.workspace.clone(),
            constraints: Constraints {
                timeout_secs: Some(if visible { 600 } else { 300 }),
                visible_stage: visible,
                model,
                ..Default::default()
            },
            tools: Vec::new(),
            mcp_servers: Vec::new(),
        };

        let result = self
            .worker
            .execute(request, tx, CancellationToken::new())
            .await?;
        let _ = forwarder.await;
        if !matches!(result.status, ExecStatus::Success) {
            // Surface the worker's own output (stdout + stderr) so the failure is
            // diagnosable — "exited 1" alone hides why (auth, a rejected flag…).
            let detail = result.transcript.trim();
            let snippet = tail(detail, 800);
            let hint = if snippet.is_empty() {
                format!(
                    "no output — check `{0}` works standalone and is logged in \
                     (run `{0} --prompt-file /tmp/p.txt` or `{0} -p hi`)",
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
        // Auth failure sometimes exits 0 with a login message — treat as failure.
        let low = result.result.to_ascii_lowercase();
        let transcript_low = result.transcript.to_ascii_lowercase();
        if low.contains("not logged in")
            || low.contains("please run /login")
            || transcript_low.contains("not logged in")
            || transcript_low.contains("please run /login")
        {
            return Err(RinneError::Conductor(format!(
                "harness conductor `{}` not logged in — trying next backend",
                self.name()
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
    if !config.backend.needs_api_key() {
        return None;
    }
    let provider = config.backend.as_str().to_string();
    let default_env = match config.backend {
        ConductorBackend::Cloudflare => "CLOUDFLARE_API_KEY",
        ConductorBackend::Groq => "GROQ_API_KEY",
        ConductorBackend::Nvidia => "NVIDIA_API_KEY",
        other => {
            // Prefer catalog defaults so connect + conductor share the same env name.
            rinne_config::known::known_api_provider(other.as_str())
                .map(|p| p.key_env)
                .unwrap_or("API_KEY")
        }
    };
    // Cloudflare historically accepted API_TOKEN; keep both via key_env override.
    let env = config
        .key_env
        .clone()
        .unwrap_or_else(|| default_env.to_string());
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
        ConductorBackend::Local => Some("http://localhost:11434/v1".into()),
        ConductorBackend::Harness => None,
        ConductorBackend::Cloudflare => config
            .account_id
            .as_ref()
            .map(|id| format!("https://api.cloudflare.com/client/v4/accounts/{id}/ai/v1")),
        ConductorBackend::Groq => Some("https://api.groq.com/openai/v1".into()),
        ConductorBackend::Nvidia => Some("https://integrate.api.nvidia.com/v1".into()),
        other => rinne_config::known::known_api_provider(other.as_str())
            .map(|p| p.base_url.to_string()),
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

    let name = config.backend.as_str().to_string();
    Ok(Some(OpenAiBackend::new(
        name,
        &base_url,
        api_key,
        &config.model,
    )))
}

/// Like [`resolve_openai`] but pins the model id (planner ladder rung).
pub fn resolve_openai_model(config: &ConductorConfig, model: &str) -> Result<Option<OpenAiBackend>> {
    let mut backend = match resolve_openai(config)? {
        Some(b) => b,
        None => return Ok(None),
    };
    backend.set_model(model);
    Ok(Some(backend))
}
