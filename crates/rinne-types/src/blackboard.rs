//! The `Blackboard` trait seam and node lifecycle status (`MCP_SKILLS.md` §15).
//!
//! The loop engine reads and writes plan, machine state, artifacts, and
//! transcripts only through this trait — never through a concrete persistence
//! type — so the engine can live in `rinne-loop` while the SQLite-backed impl
//! lives in `rinne-core`, and either can be swapped behind the seam.

use std::path::{Path, PathBuf};

use crate::dag::Plan;
use crate::worker::Usage;
use crate::Result;

/// The lifecycle status of a node (`MCP_SKILLS.md` §12 scheduler).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeStatus {
    /// Not yet started.
    Pending,
    /// Dispatched and running (or interrupted mid-run).
    Running,
    /// Completed successfully.
    Succeeded,
    /// Ran and failed.
    Failed,
    /// Parked awaiting a human (checkpoint / stuck escalation).
    Parked,
}

impl NodeStatus {
    /// The stored string form (used by the persistence impl).
    pub fn as_str(self) -> &'static str {
        match self {
            NodeStatus::Pending => "pending",
            NodeStatus::Running => "running",
            NodeStatus::Succeeded => "succeeded",
            NodeStatus::Failed => "failed",
            NodeStatus::Parked => "parked",
        }
    }

    /// Parse a stored string form back into a status (defaults to `Pending`).
    pub fn from_str(s: &str) -> NodeStatus {
        match s {
            "running" => NodeStatus::Running,
            "succeeded" => NodeStatus::Succeeded,
            "failed" => NodeStatus::Failed,
            "parked" => NodeStatus::Parked,
            _ => NodeStatus::Pending,
        }
    }

    pub fn label(self) -> &'static str {
        self.as_str()
    }

    pub fn is_terminal_success(self) -> bool {
        matches!(self, NodeStatus::Succeeded)
    }
}

/// The blackboard seam: the loop engine's only door to persistence. The concrete
/// impl (file tree + SQLite) lives in `rinne-core`.
///
/// Not `Send + Sync`: the SQLite-backed impl owns a `!Sync` connection and the
/// engine runs on a dedicated current-thread runtime, so a `&dyn Blackboard` is
/// used single-threaded.
pub trait Blackboard {
    // ----- files / plan -------------------------------------------------------
    fn workspace(&self) -> &Path;
    fn progress_path(&self) -> PathBuf;
    fn save_plan(&self, plan: &Plan) -> Result<()>;
    fn write_artifact(&self, name: &str, contents: &str) -> Result<()>;
    fn read_artifact(&self, name: &str) -> Result<String>;
    fn artifact_exists(&self, name: &str) -> bool;
    fn write_transcript(&self, node_id: &str, contents: &str) -> Result<()>;
    fn write_context(&self, node_id: &str, contents: &str) -> Result<()>;
    fn append_progress(&self, line: &str) -> Result<()>;

    // ----- machine state ------------------------------------------------------
    fn ensure_node(&self, node_id: &str) -> Result<()>;
    fn set_status(&self, node_id: &str, status: NodeStatus) -> Result<()>;
    fn set_worker(&self, node_id: &str, worker: &str) -> Result<()>;
    fn status(&self, node_id: &str) -> Result<NodeStatus>;
    fn incr_iteration(&self, node_id: &str) -> Result<u32>;
    fn iterations(&self, node_id: &str) -> Result<u32>;
    fn record_usage(&self, node_id: &str, worker: &str, usage: &Usage) -> Result<()>;
    fn total_iterations(&self) -> Result<u32>;
    fn total_usage(&self) -> Result<Usage>;
    fn set_meta(&self, key: &str, value: &str) -> Result<()>;
    fn meta(&self, key: &str) -> Result<Option<String>>;
}
