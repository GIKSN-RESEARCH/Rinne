//! `rinne-types` — the shared base every Rinne crate depends inward on
//! (`MCP_SKILLS.md` §15).
//!
//! Holds the DAG schema, the capability vocabulary, the worker contract, the
//! node/result types, and the cross-crate trait seams (`Worker`, `Replanner`).
//! Nothing here depends on a concrete crate, so the conductor, loop engine, and
//! workers can each evolve behind these stable types without a tangle.

pub mod blackboard;
pub mod dag;
pub mod error;
pub mod evaluator;
pub mod replanner;
pub mod skill;
pub mod worker;

pub use blackboard::{Blackboard, NodeStatus};
pub use error::{Result, RinneError};
pub use evaluator::{EvalContext, Evaluator, Gate};
pub use replanner::Replanner;
pub use skill::Skill;
pub use worker::{
    AuthMode, Capability, Constraints, ContextPacket, EventSink, ExecStatus, ExecuteRequest,
    ExecuteResult, InlinedFile, LatencyProfile, McpServerSpec, McpTransportKind, QuotaModel, Role,
    ToolExecutor, ToolSpec, Transport, Usage, Worker, WorkerDescriptor, WorkerEvent, WorkerFamily,
};

/// The on-disk blackboard directory name, relative to the working repo.
///
/// Single source of truth for a run (`MCP_SKILLS.md` §12). Defined here so every
/// crate refers to the same constant.
pub const BLACKBOARD_DIR: &str = ".rinne";
