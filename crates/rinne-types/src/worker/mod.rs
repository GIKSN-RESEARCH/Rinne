//! The `Worker` contract (`CONTEXT.md` §8).
//!
//! Anything that takes a subtask and does it is a worker. Two families
//! (harness, API) share this one contract. The trait and its data types live in
//! `rinne-core` because the loop engine's dispatcher (Phase 3) consumes the
//! trait, while the concrete transports and adapters live in `rinne-workers`.

mod descriptor;
mod event;
mod exec;

pub use descriptor::{
    AuthMode, Capability, LatencyProfile, QuotaModel, Transport, WorkerDescriptor, WorkerFamily,
};
pub use event::{emit, EventSink, WorkerEvent};
pub use exec::{
    Constraints, ContextPacket, ExecStatus, ExecuteRequest, ExecuteResult, InlinedFile,
    McpServerSpec, McpTransportKind, Role, ToolSpec, Usage,
};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::Result;

/// A unit of work that can be dispatched to (`CONTEXT.md` §8).
///
/// Implementors normalize their underlying tool's output into [`ExecuteResult`].
/// Streaming events go to the provided [`EventSink`]; cancellation is observed
/// via the [`CancellationToken`] (used by `/pause`, budget kills, and
/// stuck-detector aborts — `CONTEXT.md` §14).
#[async_trait]
pub trait Worker: Send + Sync {
    /// The worker's self-description, used by the scheduler to resolve a node's
    /// capability `needs` to a concrete worker.
    fn descriptor(&self) -> &WorkerDescriptor;

    /// Do one unit of work. Returns the normalized result, or an error if the
    /// worker could not be driven at all (distinct from a worker that ran and
    /// reported [`ExecStatus::Failed`]).
    async fn execute(
        &self,
        request: ExecuteRequest,
        events: EventSink,
        cancel: CancellationToken,
    ) -> Result<ExecuteResult>;

    /// Whether this worker can actually run a node's attached MCP tools
    /// (`MCP_SKILLS.md` §6): an API worker with the host agentic loop wired, or a
    /// harness that can provision MCP servers into itself. The scheduler uses
    /// this to keep a tool node off a worker that would silently drop its tools.
    /// Default `false` — a worker opts in by overriding.
    fn serves_mcp_tools(&self) -> bool {
        false
    }
}

/// Executes an MCP tool call on behalf of the host agentic loop (`MCP_SKILLS.md`
/// §6). The trait seam keeps the worker adapters MCP-agnostic: an API worker
/// holds a `dyn ToolExecutor` and calls it when the model emits a tool call; the
/// concrete implementation (over a warm MCP connection pool) lives in the wiring
/// layer.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Invoke a tool by its qualified id (`server.tool`) with JSON `arguments`,
    /// returning the result rendered as text for the model to read. An error is
    /// returned as a string the loop feeds back so the model can recover.
    async fn call(&self, id: &str, arguments: serde_json::Value) -> std::result::Result<String, String>;
}
