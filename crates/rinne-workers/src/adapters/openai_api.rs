//! OpenAI-compatible API worker adapter (`CONTEXT.md` §8, §16).
//!
//! A raw model on the user's own key. Unlike a harness, it sees only what is
//! sent, so the context assembler inlines file *contents* here, not paths
//! (`CONTEXT.md` §8 behavioral split, §12). Always metered (`CONTEXT.md` §9).

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use rinne_core::worker::{
    emit, AuthMode, Capability, EventSink, ExecStatus, ExecuteRequest, ExecuteResult,
    LatencyProfile, QuotaModel, ToolExecutor, Transport, Usage, Worker, WorkerDescriptor,
    WorkerEvent, WorkerFamily,
};
use rinne_core::{Result, RinneError};

use crate::transport::http::{ChatMessage, ChatRequest, ChatTurn, OpenAiClient};

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
    /// Executes MCP tool calls for the host agentic loop (`MCP_SKILLS.md` §6).
    /// `None` means no tools are wired; the worker then always runs the plain
    /// streaming path regardless of a node's `tools`.
    tool_executor: Option<Arc<dyn ToolExecutor>>,
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
            tool_executor: None,
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

    /// Wire an MCP tool executor so nodes that attach `tools` get the host
    /// agentic loop (`MCP_SKILLS.md` §6). Without one the worker ignores tools.
    pub fn with_tool_executor(mut self, executor: Arc<dyn ToolExecutor>) -> Self {
        self.tool_executor = Some(executor);
        self
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

    /// An API worker serves MCP tools when a tool executor is wired (the host
    /// agentic loop, `MCP_SKILLS.md` §6).
    fn serves_mcp_tools(&self) -> bool {
        self.tool_executor.is_some()
    }

    async fn execute(
        &self,
        request: ExecuteRequest,
        events: EventSink,
        cancel: CancellationToken,
    ) -> Result<ExecuteResult> {
        let started = Instant::now();

        // The per-node model (conductor's tier choice or a cascade escalation)
        // wins; otherwise the worker's default model.
        let model = request
            .constraints
            .model
            .clone()
            .unwrap_or_else(|| self.default_model.clone());

        // Host path: a node that attaches tools, with an executor wired, runs the
        // agentic loop instead of a single streamed completion (`MCP_SKILLS.md` §6).
        if !request.tools.is_empty() {
            if let Some(executor) = self.tool_executor.clone() {
                return self
                    .run_tool_loop(&request, model, executor, &events, &cancel)
                    .await;
            }
        }

        let user = compose_message(&request);
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

/// The largest number of model↔tool round-trips before the loop gives up and
/// returns whatever it has. A backstop against a model that never stops calling.
const MAX_TOOL_ROUNDS: usize = 8;

impl OpenAiWorker {
    /// The host agentic loop: offer the node's tools, execute the model's tool
    /// calls against the live MCP pool, feed results back, repeat until the model
    /// answers in text or the round cap is hit (`MCP_SKILLS.md` §6).
    async fn run_tool_loop(
        &self,
        request: &ExecuteRequest,
        model: String,
        executor: Arc<dyn ToolExecutor>,
        events: &EventSink,
        cancel: &CancellationToken,
    ) -> Result<ExecuteResult> {
        let started = Instant::now();
        let mut messages = vec![
            ChatMessage::system(self.system_prompt.clone()),
            ChatMessage::user(compose_message(request)),
        ];
        let mut usage = Usage::default();
        let mut transcript = String::new();
        let mut answer = String::new();
        let mut answered = false;

        for _ in 0..MAX_TOOL_ROUNDS {
            if cancel.is_cancelled() {
                break;
            }
            let chat = ChatRequest {
                model: model.clone(),
                messages: messages.clone(),
                temperature: None,
                extra: self.extra_body.clone(),
            };
            let turn = self.chat_tools_rotating(&chat, &request.tools, events).await?;
            usage.prompt_tokens += turn.usage.prompt_tokens;
            usage.completion_tokens += turn.usage.completion_tokens;

            if turn.tool_calls.is_empty() {
                // The model answered — done.
                answer = turn.content;
                answered = true;
                if !answer.is_empty() {
                    emit(events, WorkerEvent::Token(answer.clone()));
                }
                transcript.push_str(&answer);
                break;
            }

            // Echo the assistant's tool calls, then run each and feed results back.
            messages.push(ChatMessage::assistant_tool_calls(turn.tool_calls.clone()));
            for call in &turn.tool_calls {
                emit(
                    events,
                    WorkerEvent::ToolUse(format!(
                        "{}({})",
                        call.function.name,
                        truncate(&call.function.arguments, 120)
                    )),
                );
                transcript.push_str(&format!(
                    "\n[tool] {} {}\n",
                    call.function.name, call.function.arguments
                ));
                let args = serde_json::from_str(&call.function.arguments)
                    .unwrap_or(serde_json::Value::Null);
                let result = match executor.call(&call.function.name, args).await {
                    Ok(r) => r,
                    Err(e) => format!("tool error: {e}"),
                };
                transcript.push_str(&format!("[result] {}\n", truncate(&result, 400)));
                messages.push(ChatMessage::tool_result(call.id.clone(), result));
            }
        }

        emit(events, WorkerEvent::Done);
        usage.wall_ms = started.elapsed().as_millis() as u64;
        // A loop that ran out of rounds still calling tools never converged —
        // fail it cleanly (the engine can loop back / replan) rather than report
        // an empty success that silently starves downstream nodes.
        let status = if cancel.is_cancelled() {
            ExecStatus::Cancelled
        } else if !answered {
            emit(
                events,
                WorkerEvent::Message(format!(
                    "tool loop hit the {MAX_TOOL_ROUNDS}-round limit without a final answer"
                )),
            );
            ExecStatus::Failed(format!(
                "model did not finish within {MAX_TOOL_ROUNDS} tool rounds"
            ))
        } else {
            ExecStatus::Success
        };
        Ok(ExecuteResult {
            result: answer,
            file_diff: None,
            transcript,
            status,
            usage,
            session_id: None,
        })
    }

    /// One non-streaming tool-aware completion, rotating keys on a rate-limit
    /// just like the streaming path.
    async fn chat_tools_rotating(
        &self,
        chat: &ChatRequest,
        tools: &[rinne_core::worker::ToolSpec],
        events: &EventSink,
    ) -> Result<ChatTurn> {
        let mut last_err = None;
        for (i, key) in self.keys.iter().enumerate() {
            let client = OpenAiClient::new(&self.base_url, Some(key.clone()));
            match client.chat_with_tools(chat, tools).await {
                Ok(turn) => return Ok(turn),
                Err(e) => {
                    if is_rate_limited(&e) && i + 1 < self.keys.len() {
                        emit(
                            events,
                            WorkerEvent::Message(format!(
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
        Err(last_err.unwrap_or_else(|| RinneError::Worker("no API key available".into())))
    }
}

/// Shorten a string for an event line / transcript, marking elision.
fn truncate(s: &str, max: usize) -> String {
    let one_line = s.replace('\n', " ");
    if one_line.chars().count() > max {
        let kept: String = one_line.chars().take(max).collect();
        format!("{kept}…")
    } else {
        one_line
    }
}

/// Inline the full context an API worker needs: instruction, file contents,
/// prior context, critique, and steering.
fn compose_message(request: &ExecuteRequest) -> String {
    let mut out = String::new();
    out.push_str(&request.instruction);

    if !request.context.skill_text.is_empty() {
        out.push_str("\n\n");
        out.push_str(&request.context.skill_text);
    }

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
