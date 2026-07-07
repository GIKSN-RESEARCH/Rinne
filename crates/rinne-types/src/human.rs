//! Human control session overlay (`CONDUCTOR_LOOP_PLAN.md` §4).
//!
//! Session-scoped role pins and flags persist under `.rinne/human-session.json`
//! so a parked run can resume with the same overrides.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Filename inside the blackboard directory.
pub const HUMAN_SESSION_FILE: &str = "human-session.json";

/// Per-run human control overlay, merged over config at plan/run time.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HumanSession {
    /// When true, pins in this file override config defaults for the run.
    pub active: bool,
    pub pins: RolePins,
    /// Named review gates registered via `/human checkpoint`.
    pub checkpoints: Vec<NamedCheckpoint>,
}

/// Role pins set via `/human` or `rinne human …`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RolePins {
    pub conductor_backend: Option<String>,
    pub conductor_model: Option<String>,
    pub generator_worker: Option<String>,
    pub generator_model: Option<String>,
    /// `tool`, `human`, or `ai` (optional `ai:<worker>:<model>`).
    pub evaluator: Option<String>,
}

/// A named human review gate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NamedCheckpoint {
    pub name: String,
    pub trigger: CheckpointTrigger,
}

/// When a named gate fires.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CheckpointTrigger {
    AfterNode { node: String },
    BeforeNode { node: String },
    OnBuildSuccess,
}

impl HumanSession {
    pub fn path(blackboard_root: &Path) -> PathBuf {
        blackboard_root.join(HUMAN_SESSION_FILE)
    }

    /// Load from disk, or default when missing.
    pub fn load(blackboard_root: &Path) -> Self {
        let path = Self::path(blackboard_root);
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, blackboard_root: &Path) -> Result<()> {
        let path = Self::path(blackboard_root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, json)?;
        Ok(())
    }

    /// Active named checkpoints (session must be on).
    pub fn active_gates(&self) -> &[NamedCheckpoint] {
        if self.active {
            &self.checkpoints
        } else {
            &[]
        }
    }

    /// One-line summary for `/human` with no args.
    pub fn summary(&self) -> String {
        if !self.active {
            return "human control: off (use `/human on` to enable session pins)".into();
        }
        let mut lines = vec!["human control: on".to_string()];
        if let Some(b) = &self.pins.conductor_backend {
            let m = self
                .pins
                .conductor_model
                .as_deref()
                .map(|x| format!(" {x}"))
                .unwrap_or_default();
            lines.push(format!("  conductor: {b}{m}"));
        }
        if let Some(w) = &self.pins.generator_worker {
            let m = self
                .pins
                .generator_model
                .as_deref()
                .map(|x| format!(":{x}"))
                .unwrap_or_default();
            lines.push(format!("  generator: {w}{m}"));
        }
        if let Some(e) = &self.pins.evaluator {
            lines.push(format!("  evaluator: {e}"));
        }
        if !self.checkpoints.is_empty() {
            lines.push(format!("  checkpoints: {}", self.checkpoints.len()));
            for cp in &self.checkpoints {
                lines.push(format!("    - {} ({:?})", cp.name, cp.trigger));
            }
        }
        if lines.len() == 1 {
            lines.push("  (no pins yet)".into());
        }
        lines.join("\n")
    }
}

/// Blackboard meta key: gate approved.
pub fn gate_ok_key(name: &str) -> String {
    format!("gate_ok:{name}")
}

/// Blackboard meta key: active gate name.
pub const GATE_ACTIVE_KEY: &str = "gate_active";

/// Blackboard meta key: gate review iteration.
pub fn gate_iter_key(name: &str) -> String {
    format!("gate_iter:{name}")
}