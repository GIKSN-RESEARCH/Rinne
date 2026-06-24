//! OpenAI-compatible API worker adapter (`CONTEXT.md` §8, §16).
//!
//! A raw model on the user's own key. Unlike a harness, it sees only what is
//! sent, so the context assembler inlines file *contents* here, not paths
//! (`CONTEXT.md` §8 behavioral split, §12). Always metered (`CONTEXT.md` §9).

use std::time::Instant;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use rinne_core::worker::{
    AuthMode, Capability, EventSink, ExecStatus, ExecuteRequest, ExecuteResult, LatencyProfile,
    QuotaModel, Transport, Worker, WorkerDescriptor, WorkerFamily,
};
use rinne_core::Result;

use crate::transport::http::{ChatMessage, ChatRequest, OpenAiClient};

/// An API worker backed by an OpenAI-compatible endpoint, with an optional pool
/// of keys it rotates across when one is rate-limited (`CONTEXT.md` §13).
pub struct OpenAiWorker {
    descriptor: WorkerDescriptor,
    base_url: String,
    /// Key pool; rotated on rate-limit. The user's own keys, never pooled across
    /// users (`CONTEXT.md` §4).
    keys: Vec<String>,
    /// Default model when a node doesn't pin one.
    default_model: String,
    system_prompt: String,
    /// Provider-specific params merged into each request.
    extra_body: Option<serde_json::Value>,
}

impl OpenAiWorker {
    /// Build an API worker.
    ///
    /// `name` is the Rinne worker name (e.g. `openrouter`, `deepseek`).
    /// `base_url` includes the version (e.g. `https://api.openai.com/v1`).
    /// `keys` is the user's key pool (rotated on rate-limit). `models` is the
    /// ladder cheap→strong; the first is the default. The per-node model
    /// (conductor's choice or a cascade escalation) overrides the default.
    pub fn new(
        name: &str,
        base_url: &str,
        keys: Vec<String>,
        models: Vec<String>,
        capabilities: Vec<Capability>,
        extra_body: Option<serde_json::Value>,
    ) -> Self {
        let default_model = models.first().cloned().unwrap_or_else(|| "gpt-4o-mini".to_string());
        Self {
            extra_body,
            descriptor: WorkerDescriptor {
                name: name.to_string(),
                family: WorkerFamily::Api,
                capabilities,
                auth_mode: AuthMode::ApiKey,
                quota: QuotaModel::unlimited(),
                latency: LatencyProfile::Fast,
                transport: Transport::Http,
                models,
            },
            base_url: base_url.to_string(),
            keys: if keys.is_empty() { vec![String::new()] } else { keys },
            default_model,
            system_prompt: "You are a focused worker inside an orchestration system. \
                Follow the instruction precisely and return only the requested result."
                .to_string(),
        }
    }
}

/// Whether an error looks like a rate-limit/quota condition worth rotating keys.
fn is_rate_limited(err: &rinne_core::RinneError) -> bool {
    let s = err.to_string().to_lowercase();
    s.contains("429") || s.contains("rate limit") || s.contains("quota") || s.contains("too many requests")
}

#[async_trait]
impl Worker for OpenAiWorker {
    fn descriptor(&self) -> &WorkerDescriptor {
        &self.descriptor
    }

    async fn execute(
        &self,
        request: ExecuteRequest,
        events: EventSink,
        cancel: CancellationToken,
    ) -> Result<ExecuteResult> {
        let started = Instant::now();
        let user = compose_message(&request);

        // The per-node model (conductor's tier choice or a cascade escalation)
        // wins; otherwise the worker's default model.
        let model = request
            .constraints
            .model
            .clone()
            .unwrap_or_else(|| self.default_model.clone());

        let chat = ChatRequest {
            model,
            messages: vec![
                ChatMessage::system(self.system_prompt.clone()),
                ChatMessage::user(user),
            ],
            temperature: None,
            extra: self.extra_body.clone(),
        };

        // Try keys in order, rotating to the next on a rate-limit/quota error.
        let mut last_err = None;
        let mut resp = None;
        for (i, key) in self.keys.iter().enumerate() {
            let client = OpenAiClient::new(&self.base_url, Some(key.clone()));
            match client.chat_stream(&chat, &events, &cancel).await {
                Ok(r) => {
                    resp = Some(r);
                    break;
                }
                Err(e) => {
                    let rotate = is_rate_limited(&e) && i + 1 < self.keys.len();
                    if rotate {
                        rinne_core::worker::emit(
                            &events,
                            rinne_core::worker::WorkerEvent::Message(format!(
                                "key {} rate-limited — rotating to key {}",
                                i + 1,
                                i + 2
                            )),
                        );
                        last_err = Some(e);
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        let resp = match resp {
            Some(r) => r,
            None => return Err(last_err.unwrap_or_else(|| {
                rinne_core::RinneError::Worker("no API key available".into())
            })),
        };

        let status = if cancel.is_cancelled() {
            ExecStatus::Cancelled
        } else {
            ExecStatus::Success
        };

        let mut usage = resp.usage;
        usage.wall_ms = started.elapsed().as_millis() as u64;

        Ok(ExecuteResult {
            result: resp.content.clone(),
            file_diff: None,
            transcript: resp.content,
            status,
            usage,
            session_id: None,
        })
    }
}

/// Inline the full context an API worker needs: instruction, file contents,
/// prior context, critique, and steering.
fn compose_message(request: &ExecuteRequest) -> String {
    let mut out = String::new();
    out.push_str(&request.instruction);

    if !request.context.prior_context.is_empty() {
        out.push_str("\n\n## Context\n");
        out.push_str(&request.context.prior_context);
    }

    if let Some(critique) = &request.context.critique {
        out.push_str("\n\n## Address this feedback from the previous attempt\n");
        out.push_str(critique);
    }

    if let Some(steer) = &request.constraints.steer {
        out.push_str("\n\n## Steering\n");
        out.push_str(steer);
    }

    for file in &request.context.inlined_files {
        out.push_str("\n\n## File: ");
        out.push_str(&file.path.display().to_string());
        out.push_str("\n```\n");
        out.push_str(&file.contents);
        out.push_str("\n```\n");
    }

    out
}
