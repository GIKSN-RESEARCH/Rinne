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
    Constraints, ContextPacket, ExecStatus, ExecuteRequest, ExecuteResult, InlinedFile, Role,
    Usage,
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
}
