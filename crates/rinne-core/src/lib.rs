//! `rinne-core` — the blackboard implementation (file tree + SQLite state) and
//! the wiring glue. The shared types and seams live in `rinne-types` and the
//! loop engine in `rinne-loop` (`MCP_SKILLS.md` §15); this crate re-exports both
//! so existing `rinne_core::…` paths keep resolving.

// The shared base. Re-export the modules and items so both this crate's
// internals (`crate::dag`, `crate::Result`, …) and downstream crates'
// `rinne_core::…` paths keep working unchanged.
pub use rinne_types::dag::{Node, OnFail, Plan};
pub use rinne_types::error::{Result, RinneError};
pub use rinne_types::replanner::Replanner;
pub use rinne_types::{dag, error, replanner, worker};
pub use rinne_types::{
    AuthMode, Capability, Constraints, ContextPacket, EventSink, ExecStatus, ExecuteRequest,
    ExecuteResult, InlinedFile, LatencyProfile, McpServerSpec, McpTransportKind, NodeStatus,
    QuotaModel, Role, Skill, ToolExecutor, ToolSpec, Transport, Usage, Worker, WorkerDescriptor,
    WorkerEvent, WorkerFamily, BLACKBOARD_DIR,
};

// The loop engine. Re-export so existing `rinne_core::Engine`, `rinne_core::pool`,
// `rinne_core::priors`, … paths keep resolving.
pub use rinne_loop::{
    pool, priors, Engine, EngineEvent, EngineOptions, EngineSink, HumanDecision, ResumeInput,
    RunReport, StopReason, WorkerRegistry,
};

// This crate owns the blackboard (file tree) and the SQLite state.
pub mod blackboard;
pub mod state;

pub use blackboard::{require_plan, Blackboard};
pub use state::State;
