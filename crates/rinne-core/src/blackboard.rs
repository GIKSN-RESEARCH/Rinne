//! The blackboard: the single source of truth on disk (`CONTEXT.md` §12).
//!
//! ```text
//! .rinne/
//!   plan.json          the DAG (document, conductor emits and amends)
//!   state.db           SQLite machine state
//!   progress.md        human-readable run log
//!   context/           assembled context packets, cached
//!   artifacts/         named node outputs
//!   transcripts/       per-node worker transcripts
//! ```

use std::path::{Path, PathBuf};

use crate::dag::Plan;
use crate::{Result, RinneError, BLACKBOARD_DIR};

/// A handle to a run's `.rinne/` blackboard rooted in a repo.
pub struct Blackboard {
    /// The `.rinne/` directory itself.
    root: PathBuf,
    /// The repo / workspace the run operates on (parent of `.rinne/`).
    workspace: PathBuf,
}

impl Blackboard {
    /// Open (creating the directory tree if needed) the blackboard for a repo.
    pub fn open(workspace: &Path) -> Result<Self> {
        let root = workspace.join(BLACKBOARD_DIR);
        for sub in ["", "context", "artifacts", "transcripts"] {
            std::fs::create_dir_all(root.join(sub))?;
        }
        Ok(Self {
            root,
            workspace: workspace.to_path_buf(),
        })
    }

    /// Whether a blackboard already exists for a repo (i.e. there is a plan to
    /// resume).
    pub fn exists(workspace: &Path) -> bool {
        workspace.join(BLACKBOARD_DIR).join("plan.json").is_file()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn plan_path(&self) -> PathBuf {
        self.root.join("plan.json")
    }

    pub fn state_db_path(&self) -> PathBuf {
        self.root.join("state.db")
    }

    pub fn progress_path(&self) -> PathBuf {
        self.root.join("progress.md")
    }

    /// Load the plan document, validating its structure.
    pub fn load_plan(&self) -> Result<Plan> {
        let path = self.plan_path();
        let bytes = std::fs::read(&path).map_err(|_| RinneError::NotFound(path.clone()))?;
        let plan: Plan = serde_json::from_slice(&bytes)?;
        plan.validate()?;
        Ok(plan)
    }

    /// Persist the plan document atomically (write-temp-then-rename).
    pub fn save_plan(&self, plan: &Plan) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(plan)?;
        atomic_write(&self.plan_path(), &bytes)
    }

    /// Reset per-run state so a brand-new goal starts clean: clears the SQLite
    /// state (node statuses, iteration counts, budget ledger, the run clock) and
    /// the progress log. Resume must NOT call this — only a fresh goal does.
    pub fn reset_run(&self) -> Result<()> {
        for f in ["state.db", "state.db-wal", "state.db-shm"] {
            let _ = std::fs::remove_file(self.root.join(f));
        }
        let _ = std::fs::remove_file(self.progress_path());
        Ok(())
    }

    /// Path to a named artifact, e.g. `design.md`. A leading `artifacts/` (as
    /// the §10 plan example writes it) is stripped so the path never doubles up.
    pub fn artifact_path(&self, name: &str) -> PathBuf {
        let name = name.strip_prefix("artifacts/").unwrap_or(name);
        self.root.join("artifacts").join(name)
    }

    pub fn write_artifact(&self, name: &str, contents: &str) -> Result<()> {
        let path = self.artifact_path(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        atomic_write(&path, contents.as_bytes())
    }

    pub fn read_artifact(&self, name: &str) -> Result<String> {
        let path = self.artifact_path(name);
        std::fs::read_to_string(&path).map_err(|_| RinneError::NotFound(path))
    }

    pub fn artifact_exists(&self, name: &str) -> bool {
        self.artifact_path(name).is_file()
    }

    /// Write a node's worker transcript.
    pub fn write_transcript(&self, node_id: &str, contents: &str) -> Result<()> {
        let path = self.root.join("transcripts").join(format!("{node_id}.txt"));
        atomic_write(&path, contents.as_bytes())
    }

    /// Cache a node's assembled context packet.
    pub fn write_context(&self, node_id: &str, contents: &str) -> Result<()> {
        let path = self.root.join("context").join(format!("{node_id}.json"));
        atomic_write(&path, contents.as_bytes())
    }

    /// Append a line to the human-readable progress log.
    pub fn append_progress(&self, line: &str) -> Result<()> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.progress_path())?;
        writeln!(f, "{line}")?;
        Ok(())
    }
}

/// Write a file atomically: write to a sibling temp file, then rename over the
/// target so a crash mid-write never leaves a half-written file
/// (`CONTEXT.md` §14 atomic updates, clean resume).
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
