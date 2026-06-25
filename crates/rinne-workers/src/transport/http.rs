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
    /// Extra JSON merged into the request body (provider-specific params).
    pub extra: Option<serde_json::Value>,
}

/// The accumulated result of a (streamed) chat completion.
#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub content: String,
    pub usage: Usage,
    pub finish_reason: Option<String>,
}

/// A model offered by a platform's `/v1/models` endpoint, with optional cost
/// and context metadata where the platform provides it (e.g. OpenRouter).
#[derive(Debug, Clone)]
pub struct DiscoveredModel {
    pub id: String,
    /// Prompt price per token (USD), if the platform reports it. Lower = cheaper.
    pub prompt_price: Option<f64>,
    /// Context window in tokens, if reported.
    pub context: Option<u64>,
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
            base_url: normalize_base_url(&base_url.into()),
            api_key,
        }
    }

    /// Discover the models this endpoint+key can access via `GET /models`.
    /// Sorted cheapest→most-expensive where pricing is reported (others last),
    /// so the result doubles as a price-ordered tier ladder.
    pub async fn list_models(&self) -> Result<Vec<DiscoveredModel>> {
        let url = format!("{}/models", self.base_url);
        let mut builder = self.http.get(&url);
        if let Some(key) = &self.api_key {
            builder = builder.bearer_auth(key);
        }
        let resp = builder
            .send()
            .await
            .map_err(|e| RinneError::Worker(format!("models request failed: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(RinneError::Worker(format!("GET /models HTTP {status}: {text}")));
        }
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| RinneError::Worker(format!("bad /models json: {e}")))?;

        // OpenAI shape: { "data": [ { "id", ... } ] }. Some platforms return a
        // bare array. Pricing/context fields are platform-specific (OpenRouter).
        let items = v
            .get("data")
            .and_then(|d| d.as_array())
            .or_else(|| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut models: Vec<DiscoveredModel> = items
            .iter()
            .filter_map(|m| {
                let id = m.get("id").and_then(|x| x.as_str())?.to_string();
                let prompt_price = m
                    .get("pricing")
                    .and_then(|p| p.get("prompt"))
                    .and_then(to_f64);
                let context = m
                    .get("context_length")
                    .or_else(|| m.get("context_window"))
                    .and_then(|c| c.as_u64());
                Some(DiscoveredModel {
                    id,
                    prompt_price,
                    context,
                })
            })
            .collect();

        // Cheapest first; models without pricing sink to the end.
        models.sort_by(|a, b| match (a.prompt_price, b.prompt_price) {
            (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.id.cmp(&b.id),
        });
        Ok(models)
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
        let mut body = serde_json::json!({
            "model": req.model,
            "messages": req.messages,
            "temperature": req.temperature,
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        // Merge provider-specific extra params (e.g. chat_template_kwargs).
        if let (Some(serde_json::Value::Object(extra)), Some(obj)) =
            (&req.extra, body.as_object_mut())
        {
            for (k, v) in extra {
                obj.insert(k.clone(), v.clone());
            }
        }

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
                        let parsed = parse_sse_line(&line, &mut usage, &mut finish_reason)?;
                        if let Some(reasoning) = parsed.reasoning {
                            if !reasoning.is_empty() {
                                emit(events, WorkerEvent::Thinking(reasoning));
                            }
                        }
                        if let Some(delta) = parsed.content {
                            if !delta.is_empty() {
                                emit(events, WorkerEvent::Token(delta.clone()));
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

/// Normalize an OpenAI-compatible base URL. We append our own path
/// (`/chat/completions`, `/models`), so a user who pastes a full endpoint URL
/// from a provider's docs gets it stripped back to the base rather than a
/// doubled path (`…/chat/completions/chat/completions` → 404).
pub fn normalize_base_url(raw: &str) -> String {
    let mut b = raw.trim().trim_end_matches('/');
    for suffix in ["/chat/completions", "/responses", "/completions", "/models"] {
        if let Some(stripped) = b.strip_suffix(suffix) {
            b = stripped.trim_end_matches('/');
            break;
        }
    }
    b.to_string()
}

/// Parse a JSON value that may be a number or a numeric string into `f64`
/// (OpenRouter reports prices as strings like `"0.0000001"`).
fn to_f64(v: &serde_json::Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_normalization_strips_pasted_endpoints() {
        let base = "https://openrouter.ai/api/v1";
        assert_eq!(normalize_base_url(base), base);
        assert_eq!(normalize_base_url("https://openrouter.ai/api/v1/"), base);
        assert_eq!(normalize_base_url("https://openrouter.ai/api/v1/chat/completions"), base);
        assert_eq!(normalize_base_url("https://openrouter.ai/api/v1/responses"), base);
        assert_eq!(normalize_base_url("  https://openrouter.ai/api/v1/models  "), base);
    }
}

/// The content + reasoning deltas extracted from one SSE chunk.
#[derive(Default)]
struct SseDelta {
    content: Option<String>,
    reasoning: Option<String>,
}

/// Parse one SSE line into its content and reasoning deltas. Side-effects usage
/// and finish_reason as they appear. Reasoning is read from the de-facto fields
/// (`reasoning_content` — DeepSeek; `reasoning` — OpenRouter and others).
fn parse_sse_line(
    line: &str,
    usage: &mut Usage,
    finish_reason: &mut Option<String>,
) -> Result<SseDelta> {
    let Some(data) = line.strip_prefix("data:") else {
        return Ok(SseDelta::default());
    };
    let data = data.trim();
    if data.is_empty() || data == "[DONE]" {
        return Ok(SseDelta::default());
    }

    let chunk: ChatChunk =
        serde_json::from_str(data).map_err(|e| RinneError::Worker(format!("bad SSE json: {e}")))?;

    if let Some(u) = chunk.usage {
        usage.prompt_tokens = u.prompt_tokens;
        usage.completion_tokens = u.completion_tokens;
    }

    let mut content = String::new();
    let mut reasoning = String::new();
    for choice in chunk.choices {
        if let Some(fr) = choice.finish_reason {
            *finish_reason = Some(fr);
        }
        if let Some(c) = choice.delta.content {
            content.push_str(&c);
        }
        if let Some(r) = choice.delta.reasoning_content.or(choice.delta.reasoning) {
            reasoning.push_str(&r);
        }
    }
    Ok(SseDelta {
        content: (!content.is_empty()).then_some(content),
        reasoning: (!reasoning.is_empty()).then_some(reasoning),
    })
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
    /// DeepSeek-style reasoning stream.
    #[serde(default)]
    reasoning_content: Option<String>,
    /// OpenRouter/others-style reasoning stream.
    #[serde(default)]
    reasoning: Option<String>,
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
