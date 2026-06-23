//! Streaming worker events (`CONTEXT.md` §8 lifecycle, §6 stream pane).
//!
//! A worker emits events as it runs; the dispatcher forwards them to the
//! interface's stream pane. The channel is the worker's only way to narrate
//! progress before its final [`ExecuteResult`](super::ExecuteResult).

use serde::{Deserialize, Serialize};

/// A single streamed event from a running worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "data")]
pub enum WorkerEvent {
    /// A line of human-readable narration ("reading src/, editing app.ts").
    Message(String),
    /// The worker began reading a file/path.
    Reading(String),
    /// The worker edited a file/path.
    Editing(String),
    /// The worker invoked a tool/command.
    ToolUse(String),
    /// Raw stdout/stderr passthrough, used when no richer structure exists.
    Raw(String),
    /// Terminal marker: the worker finished emitting events.
    Done,
}

/// The sink a worker writes events to. The dispatcher owns the receiver.
///
/// Unbounded so a worker never blocks on a slow consumer; events are cheap and
/// the UI drains them promptly.
pub type EventSink = tokio::sync::mpsc::UnboundedSender<WorkerEvent>;

/// Convenience: send an event, ignoring the error if the receiver is gone
/// (e.g. the run was cancelled and the dispatcher dropped its end).
pub fn emit(sink: &EventSink, event: WorkerEvent) {
    let _ = sink.send(event);
}
