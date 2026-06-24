//! The mock worker (`CONTEXT.md` §14, §21).
//!
//! Replays a canned script — a sequence of streamed events plus a final result
//! — without spawning any process or making any network call. This is what lets
//! the loop engine be integration-tested (loop-backs, stuck-detection, resume)
//! deterministically, without burning tokens or depending on a flaky beta CLI.

use std::time::Instant;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use rinne_core::worker::{
    emit, AuthMode, Capability, EventSink, ExecStatus, ExecuteRequest, ExecuteResult,
    LatencyProfile, QuotaModel, Transport, Worker, WorkerDescriptor, WorkerEvent, WorkerFamily,
};
use rinne_core::Result;

/// A canned scenario the mock replays. Deserializable so tests can load
/// transcripts from JSON fixtures on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MockScript {
    pub descriptor: WorkerDescriptor,
    /// Events streamed before the result, in order. A trailing
    /// [`WorkerEvent::Done`] is appended automatically if absent.
    #[serde(default)]
    pub events: Vec<WorkerEvent>,
    /// The final normalized result.
    pub result: ExecuteResult,
    /// Optional delay between streamed events, to simulate real streaming and
    /// give cancellation a window to fire.
    #[serde(default)]
    pub per_event_ms: u64,
}

impl MockScript {
    /// A minimal successful script with a sensible default descriptor.
    pub fn success(name: &str, result_text: &str) -> Self {
        Self {
            descriptor: default_descriptor(name),
            events: vec![
                WorkerEvent::Message(format!("{name}: starting")),
                WorkerEvent::Message(format!("{name}: done")),
            ],
            result: ExecuteResult {
                result: result_text.to_string(),
                file_diff: None,
                transcript: format!("[mock {name}] {result_text}"),
                status: ExecStatus::Success,
                usage: rinne_core::Usage {
                    prompt_tokens: 100,
                    completion_tokens: 50,
                    wall_ms: 0,
                },
                session_id: None,
            },
            per_event_ms: 0,
        }
    }

    /// A failing script (worker ran but reported failure).
    pub fn failure(name: &str, reason: &str) -> Self {
        let mut s = Self::success(name, "");
        s.result.status = ExecStatus::Failed(reason.to_string());
        s.result.result = format!("failed: {reason}");
        s
    }

    /// Attach a unified diff to the result, e.g. for generator scripts.
    pub fn with_diff(mut self, diff: &str) -> Self {
        self.result.file_diff = Some(diff.to_string());
        self
    }

    /// Replace the streamed events.
    pub fn with_events(mut self, events: Vec<WorkerEvent>) -> Self {
        self.events = events;
        self
    }
}

/// A default descriptor for a mock worker: capable of everything, free, fast.
fn default_descriptor(name: &str) -> WorkerDescriptor {
    WorkerDescriptor {
        name: name.to_string(),
        family: WorkerFamily::Harness,
        capabilities: vec![
            Capability::CodeEdit,
            Capability::RepoAware,
            Capability::ToolRun,
            Capability::CodeReview,
            Capability::Reasoning,
            Capability::Writing,
            Capability::LongContext,
        ],
        auth_mode: AuthMode::Free,
        quota: QuotaModel::unlimited(),
        latency: LatencyProfile::Fast,
        transport: Transport::SubprocessJson,
        models: Vec::new(),
    }
}

/// A worker that replays a [`MockScript`].
pub struct MockWorker {
    script: MockScript,
}

impl MockWorker {
    pub fn new(script: MockScript) -> Self {
        Self { script }
    }

    /// Convenience constructor for a simple successful worker.
    pub fn success(name: &str, result_text: &str) -> Self {
        Self::new(MockScript::success(name, result_text))
    }
}

#[async_trait]
impl Worker for MockWorker {
    fn descriptor(&self) -> &WorkerDescriptor {
        &self.script.descriptor
    }

    async fn execute(
        &self,
        _request: ExecuteRequest,
        events: EventSink,
        cancel: CancellationToken,
    ) -> Result<ExecuteResult> {
        let started = Instant::now();

        for event in &self.script.events {
            if cancel.is_cancelled() {
                return Ok(cancelled_result(started));
            }
            if self.script.per_event_ms > 0 {
                // Race the delay against cancellation so `/pause` is responsive.
                let sleep =
                    tokio::time::sleep(std::time::Duration::from_millis(self.script.per_event_ms));
                tokio::select! {
                    _ = sleep => {}
                    _ = cancel.cancelled() => return Ok(cancelled_result(started)),
                }
            }
            emit(&events, event.clone());
        }
        emit(&events, WorkerEvent::Done);

        let mut result = self.script.result.clone();
        result.usage.wall_ms = started.elapsed().as_millis() as u64;
        Ok(result)
    }
}

fn cancelled_result(started: Instant) -> ExecuteResult {
    ExecuteResult {
        result: String::new(),
        file_diff: None,
        transcript: "[mock] cancelled".to_string(),
        status: ExecStatus::Cancelled,
        usage: rinne_core::Usage {
            prompt_tokens: 0,
            completion_tokens: 0,
            wall_ms: started.elapsed().as_millis() as u64,
        },
        session_id: None,
    }
}
