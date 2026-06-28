//! `rinne-loop` — the loop engine (`MCP_SKILLS.md` §15).
//!
//! The scheduler, context assembler, evaluator gate, cascade loop-back, and
//! stuck-detector. It depends only on `rinne-types`, reaching persistence
//! through the `Blackboard` trait seam and the conductor through the `Replanner`
//! seam — so it never takes a dependency on a concrete crate.

// Re-export the shared base so the moved modules' `crate::dag`, `crate::worker`,
// `crate::{Result, RinneError}`, `crate::Replanner` paths keep resolving.
pub use rinne_types::{dag, error, replanner, worker};
pub use rinne_types::{
    Blackboard, NodeStatus, Replanner, Result, RinneError, BLACKBOARD_DIR,
};

pub mod assembler;
pub mod engine;
pub mod evaluator;
pub mod pool;
pub mod priors;
pub mod ratchet;
pub mod registry;

pub use engine::{
    Engine, EngineEvent, EngineOptions, EngineSink, HumanDecision, ResumeInput, RunReport,
    StopReason,
};
pub use registry::WorkerRegistry;
