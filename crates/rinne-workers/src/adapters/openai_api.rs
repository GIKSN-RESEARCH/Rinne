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

/// An API worker backed by an OpenAI-compatible endpoint.
pub struct OpenAiWorker {
    descriptor: WorkerDescriptor,
    client: OpenAiClient,
    model: String,
    system_prompt: String,
}

impl OpenAiWorker {
    /// Build an API worker.
    ///
    /// `name` is the Rinne worker name (e.g. `openai`, `openrouter`).
    /// `base_url` includes the version (e.g. `https://api.openai.com/v1`).
    pub fn new(
        name: &str,
        base_url: &str,
        api_key: Option<String>,
        model: &str,
        capabilities: Vec<Capability>,
    ) -> Self {
        Self {
            descriptor: WorkerDescriptor {
                name: name.to_string(),
                family: WorkerFamily::Api,
                capabilities,
                auth_mode: AuthMode::ApiKey,
                quota: QuotaModel::unlimited(),
                latency: LatencyProfile::Fast,
                transport: Transport::Http,
            },
            client: OpenAiClient::new(base_url, api_key),
            model: model.to_string(),
            system_prompt: "You are a focused worker inside an orchestration system. \
                Follow the instruction precisely and return only the requested result."
                .to_string(),
        }
    }
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

        let chat = ChatRequest {
            model: self.model.clone(),
            messages: vec![
                ChatMessage::system(self.system_prompt.clone()),
                ChatMessage::user(user),
            ],
            temperature: None,
        };

        let resp = self.client.chat_stream(&chat, &events, &cancel).await?;

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
