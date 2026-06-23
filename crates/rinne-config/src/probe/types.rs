//! Types for the `doctor` probe report (`CONTEXT.md` §9, §17).
//!
//! `AuthMode` and `WorkerFamily` are defined once in `rinne-core` (the worker
//! contract owns them) and re-exported here so the probe and the worker
//! descriptor never disagree.

use serde::{Deserialize, Serialize};

pub use rinne_core::worker::{AuthMode, WorkerFamily};

/// The detected state of a worker on this machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "state", content = "detail")]
pub enum WorkerStatus {
    /// Installed and the smoke test passed.
    Available,
    /// Binary not found on `PATH` (harness) or key env var unset (API).
    NotInstalled,
    /// Found, but the smoke test failed — reason attached.
    SmokeTestFailed(String),
}

impl WorkerStatus {
    pub fn is_available(&self) -> bool {
        matches!(self, WorkerStatus::Available)
    }
}

/// One worker's probe result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerProbe {
    /// Stable worker name, e.g. `claude-code`, `codex`, `anthropic`.
    pub name: String,
    pub family: WorkerFamily,
    pub status: WorkerStatus,
    pub auth_mode: AuthMode,
    /// Whether the user enabled this worker in config.
    pub enabled: bool,
    /// Per-worker warnings (e.g. the Claude billing footgun).
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// The full `doctor` report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub workers: Vec<WorkerProbe>,
    /// Run-level warnings not tied to a single worker.
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl DoctorReport {
    /// All available workers, harness or API.
    pub fn available(&self) -> impl Iterator<Item = &WorkerProbe> {
        self.workers.iter().filter(|w| w.status.is_available())
    }
}
