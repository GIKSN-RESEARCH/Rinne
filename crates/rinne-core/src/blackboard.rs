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
use crate::state::{NodeStatus, State, UsageRow};
use crate::worker::Usage;
use crate::{Result, RinneError, BLACKBOARD_DIR};

/// A handle to a run's `.rinne/` blackboard rooted in a repo.
///
/// Owns both the file tree and the SQLite [`State`]; the loop engine reads and
/// writes plan, state, artifacts, and transcripts through this one handle
/// (`MCP_SKILLS.md` §15 Blackboard seam) and never touches the persistence layer
/// directly.
pub struct Blackboard {
    /// The `.rinne/` directory itself.
    root: PathBuf,
    /// The repo / workspace the run operates on (parent of `.rinne/`).
    workspace: PathBuf,
    /// Machine state (node statuses, iteration counts, budget ledger).
    state: State,
}

impl Blackboard {
    /// Open (creating the directory tree if needed) the blackboard for a repo.
    pub fn open(workspace: &Path) -> Result<Self> {
        let root = workspace.join(BLACKBOARD_DIR);
        for sub in ["", "context", "artifacts", "transcripts"] {
            std::fs::create_dir_all(root.join(sub))?;
        }
        let state = State::open(&root.join("state.db"))?;
        Ok(Self {
            root,
            workspace: workspace.to_path_buf(),
            state,
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
    /// Clears in place (the owned connection stays valid).
    pub fn reset_run(&self) -> Result<()> {
        self.state.reset()?;
        let _ = std::fs::remove_file(self.progress_path());
        Ok(())
    }

    // ----- machine state (delegated to the owned SQLite `State`) --------------
    // The engine reaches persistence only through these, so it never names the
    // concrete `State` type and can live behind the Blackboard seam.

    pub fn ensure_node(&self, node_id: &str) -> Result<()> {
        self.state.ensure_node(node_id)
    }
    pub fn set_status(&self, node_id: &str, status: NodeStatus) -> Result<()> {
        self.state.set_status(node_id, status)
    }
    pub fn set_worker(&self, node_id: &str, worker: &str) -> Result<()> {
        self.state.set_worker(node_id, worker)
    }
    pub fn status(&self, node_id: &str) -> Result<NodeStatus> {
        self.state.status(node_id)
    }
    pub fn worker(&self, node_id: &str) -> Result<Option<String>> {
        self.state.worker(node_id)
    }
    pub fn incr_iteration(&self, node_id: &str) -> Result<u32> {
        self.state.incr_iteration(node_id)
    }
    pub fn iterations(&self, node_id: &str) -> Result<u32> {
        self.state.iterations(node_id)
    }
    pub fn record_usage(&self, node_id: &str, worker: &str, usage: &Usage) -> Result<()> {
        self.state.record_usage(node_id, worker, usage)
    }
    pub fn total_iterations(&self) -> Result<u32> {
        self.state.total_iterations()
    }
    pub fn total_usage(&self) -> Result<Usage> {
        self.state.total_usage()
    }
    pub fn usage_rows(&self) -> Result<Vec<UsageRow>> {
        self.state.usage_rows()
    }
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.state.set_meta(key, value)
    }
    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        self.state.meta(key)
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

/// Satisfy the `rinne-types` Blackboard seam by delegating to the inherent
/// methods above. The loop engine talks to `&dyn Blackboard` through this; the
/// CLI keeps using the concrete inherent methods directly.
impl rinne_types::Blackboard for Blackboard {
    fn workspace(&self) -> &Path {
        Blackboard::workspace(self)
    }
    fn progress_path(&self) -> PathBuf {
        Blackboard::progress_path(self)
    }
    fn save_plan(&self, plan: &Plan) -> Result<()> {
        Blackboard::save_plan(self, plan)
    }
    fn write_artifact(&self, name: &str, contents: &str) -> Result<()> {
        Blackboard::write_artifact(self, name, contents)
    }
    fn read_artifact(&self, name: &str) -> Result<String> {
        Blackboard::read_artifact(self, name)
    }
    fn artifact_exists(&self, name: &str) -> bool {
        Blackboard::artifact_exists(self, name)
    }
    fn write_transcript(&self, node_id: &str, contents: &str) -> Result<()> {
        Blackboard::write_transcript(self, node_id, contents)
    }
    fn write_context(&self, node_id: &str, contents: &str) -> Result<()> {
        Blackboard::write_context(self, node_id, contents)
    }
    fn append_progress(&self, line: &str) -> Result<()> {
        Blackboard::append_progress(self, line)
    }
    fn ensure_node(&self, node_id: &str) -> Result<()> {
        Blackboard::ensure_node(self, node_id)
    }
    fn set_status(&self, node_id: &str, status: NodeStatus) -> Result<()> {
        Blackboard::set_status(self, node_id, status)
    }
    fn set_worker(&self, node_id: &str, worker: &str) -> Result<()> {
        Blackboard::set_worker(self, node_id, worker)
    }
    fn status(&self, node_id: &str) -> Result<NodeStatus> {
        Blackboard::status(self, node_id)
    }
    fn incr_iteration(&self, node_id: &str) -> Result<u32> {
        Blackboard::incr_iteration(self, node_id)
    }
    fn iterations(&self, node_id: &str) -> Result<u32> {
        Blackboard::iterations(self, node_id)
    }
    fn record_usage(&self, node_id: &str, worker: &str, usage: &Usage) -> Result<()> {
        Blackboard::record_usage(self, node_id, worker, usage)
    }
    fn total_iterations(&self) -> Result<u32> {
        Blackboard::total_iterations(self)
    }
    fn total_usage(&self) -> Result<Usage> {
        Blackboard::total_usage(self)
    }
    fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        Blackboard::set_meta(self, key, value)
    }
    fn meta(&self, key: &str) -> Result<Option<String>> {
        Blackboard::meta(self, key)
    }
}

/// Convenience for the binary boundary: a not-found plan maps to a clear error.
pub fn require_plan(blackboard: &Blackboard) -> Result<Plan> {
    if !Blackboard::exists(blackboard.workspace()) {
        return Err(RinneError::Plan(
            "no plan.json in .rinne/ — nothing to run or resume".into(),
        ));
    }
    blackboard.load_plan()
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
