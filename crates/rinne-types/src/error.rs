//! Shared error and result types used across the Rinne library crates.
//!
//! Library crates surface `RinneError` (via `thiserror`); the binary boundary
//! (`rinne-cli`) wraps these in `anyhow` for top-level reporting.

use std::path::PathBuf;

/// The canonical error type for Rinne's library crates.
#[derive(Debug, thiserror::Error)]
pub enum RinneError {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("worker error: {0}")]
    Worker(String),

    #[error("conductor error: {0}")]
    Conductor(String),

    #[error("mcp error: {0}")]
    Mcp(String),

    #[error("skill error: {0}")]
    Skill(String),

    #[error("blackboard error: {0}")]
    Blackboard(String),

    #[error("plan/DAG error: {0}")]
    Plan(String),

    /// A run was cancelled (e.g. via `/pause`, a budget kill, or a
    /// stuck-detector abort).
    #[error("cancelled: {0}")]
    Cancelled(String),

    #[error("path not found: {0}")]
    NotFound(PathBuf),

    #[error("feature not implemented: {0}")]
    NotImplemented(&'static str),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// The crate-wide result alias.
pub type Result<T> = std::result::Result<T, RinneError>;
