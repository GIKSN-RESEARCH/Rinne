//! The `http` transport (`CONTEXT.md` §8, §14).
//!
//! One OpenAI-compatible chat client over `reqwest`, with a configurable base
//! URL. It covers every OpenAI-format API worker and (in Phase 4) every
//! conductor backend. Responses are streamed via SSE so deltas reach the event
//! sink as they arrive, and final token usage is captured.

use async_trait::async_trait;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use rinne_core::worker::{emit, EventSink, Usage, WorkerEvent};
use rinne_core::{Result, RinneError};

/// A chat message in the OpenAI format.
#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: content.into(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
        }
    }
}

/// A chat completion request.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub temperature: Option<f32>,
}

/// The accumulated result of a (streamed) chat completion.
#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub content: String,
    pub usage: Usage,
    pub finish_reason: Option<String>,
}

/// An OpenAI-compatible chat client over a configurable base URL.
#[derive(Clone)]
pub struct OpenAiClient {
    http: reqwest::Client,
    /// Base URL including the API version, e.g. `https://api.openai.com/v1`.
    base_url: String,
    api_key: Option<String>,
}

impl OpenAiClient {
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
        }
    }

    /// Stream a chat completion, emitting each content delta as a
    /// [`WorkerEvent::Message`] and returning the accumulated response.
    pub async fn chat_stream(
        &self,
        req: &ChatRequest,
        events: &EventSink,
        cancel: &CancellationToken,
    ) -> Result<ChatResponse> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = serde_json::json!({
            "model": req.model,
            "messages": req.messages,
            "temperature": req.temperature,
            "stream": true,
            "stream_options": { "include_usage": true },
        });

        let mut builder = self.http.post(&url).json(&body);
        if let Some(key) = &self.api_key {
            builder = builder.bearer_auth(key);
        }

        let resp = builder
            .send()
            .await
            .map_err(|e| RinneError::Worker(format!("request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(RinneError::Worker(format!(
                "chat completion HTTP {status}: {text}"
            )));
        }

        let mut content = String::new();
        let mut usage = Usage::default();
        let mut finish_reason = None;
        let mut buf = String::new();

        let mut stream = resp.bytes_stream();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    return Ok(ChatResponse { content, usage, finish_reason: Some("cancelled".into()) });
                }
                chunk = stream.next() => {
                    let Some(chunk) = chunk else { break };
                    let bytes = chunk.map_err(|e| RinneError::Worker(format!("stream error: {e}")))?;
                    buf.push_str(&String::from_utf8_lossy(&bytes));
                    // SSE frames are separated by newlines; process complete lines.
                    while let Some(nl) = buf.find('\n') {
                        let line = buf[..nl].trim().to_string();
                        buf.drain(..=nl);
                        if let Some(delta) = parse_sse_line(&line, &mut usage, &mut finish_reason)? {
                            if !delta.is_empty() {
                                emit(events, WorkerEvent::Message(delta.clone()));
                                content.push_str(&delta);
                            }
                        }
                    }
                }
            }
        }

        emit(events, WorkerEvent::Done);
        Ok(ChatResponse {
            content,
            usage,
            finish_reason,
        })
    }
}

/// Parse one SSE line. Returns the content delta (if any). Side-effects usage
/// and finish_reason as they appear.
fn parse_sse_line(
    line: &str,
    usage: &mut Usage,
    finish_reason: &mut Option<String>,
) -> Result<Option<String>> {
    let Some(data) = line.strip_prefix("data:") else {
        return Ok(None);
    };
    let data = data.trim();
    if data.is_empty() || data == "[DONE]" {
        return Ok(None);
    }

    let chunk: ChatChunk =
        serde_json::from_str(data).map_err(|e| RinneError::Worker(format!("bad SSE json: {e}")))?;

    if let Some(u) = chunk.usage {
        usage.prompt_tokens = u.prompt_tokens;
        usage.completion_tokens = u.completion_tokens;
    }

    let mut delta_text = String::new();
    for choice in chunk.choices {
        if let Some(fr) = choice.finish_reason {
            *finish_reason = Some(fr);
        }
        if let Some(c) = choice.delta.content {
            delta_text.push_str(&c);
        }
    }
    Ok(Some(delta_text))
}

#[derive(Deserialize)]
struct ChatChunk {
    #[serde(default)]
    choices: Vec<ChunkChoice>,
    #[serde(default)]
    usage: Option<ApiUsage>,
}

#[derive(Deserialize)]
struct ChunkChoice {
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize)]
struct ApiUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

/// Marker trait tie-in kept minimal for Phase 2; the API worker adapter wraps
/// this client and implements `Worker`.
#[async_trait]
pub trait ChatBackend: Send + Sync {
    async fn complete(
        &self,
        req: &ChatRequest,
        events: &EventSink,
        cancel: &CancellationToken,
    ) -> Result<ChatResponse>;
}

#[async_trait]
impl ChatBackend for OpenAiClient {
    async fn complete(
        &self,
        req: &ChatRequest,
        events: &EventSink,
        cancel: &CancellationToken,
    ) -> Result<ChatResponse> {
        self.chat_stream(req, events, cancel).await
    }
}
