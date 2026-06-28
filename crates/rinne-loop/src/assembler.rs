//! The context assembler (`CONTEXT.md` §12).
//!
//! Builds each node's context packet from the blackboard. This is the hardest
//! component because no two workers share a context window. For a **harness**
//! worker it writes a thin packet and *pins file paths* — the worker reads the
//! repo itself. For an **API** worker it *inlines file contents* — the model
//! sees only what is sent. Get this right or workers talk past each other.

use std::path::{Path, PathBuf};

use crate::dag::{Node, Plan};
use crate::worker::{ContextPacket, InlinedFile, WorkerFamily};
use crate::{Result, BLACKBOARD_DIR};
use rinne_types::Blackboard;

/// Builds context packets against a plan and its blackboard.
pub struct ContextAssembler<'a> {
    blackboard: &'a dyn Blackboard,
    plan: &'a Plan,
}

impl<'a> ContextAssembler<'a> {
    pub fn new(blackboard: &'a dyn Blackboard, plan: &'a Plan) -> Self {
        Self { blackboard, plan }
    }

    /// Assemble the packet for `node`, shaped for the target worker `family`.
    ///
    /// `critique` carries an evaluator's feedback on loop-back (P5); pass `None`
    /// on the first attempt.
    pub fn build(
        &self,
        node: &Node,
        family: WorkerFamily,
        critique: Option<String>,
    ) -> Result<ContextPacket> {
        // The context sources are the same regardless of family: the plan's
        // pinned `@`-mentions plus this node's named input artifacts. Only the
        // *shaping* (paths vs. contents) differs.
        let mentioned = &self.plan.mentioned;
        let input_artifacts: Vec<String> = node
            .inputs
            .iter()
            .filter(|i| i.as_str() != "diff") // `diff` is a special pseudo-input
            .cloned()
            .collect();

        let mut packet = ContextPacket {
            critique,
            ..Default::default()
        };

        match family {
            WorkerFamily::Harness => {
                // Pin repo-relative mention paths, and the on-disk paths of input
                // artifacts (under .rinne/artifacts/) for the worker to read.
                for m in mentioned {
                    packet.pinned_paths.push(m.clone());
                }
                for name in &input_artifacts {
                    if self.blackboard.artifact_exists(name) {
                        packet
                            .pinned_paths
                            .push(artifact_rel_path(name));
                    }
                }
            }
            WorkerFamily::Api => {
                // Inline contents: the model sees only what we send.
                let workspace = self.blackboard.workspace();
                for m in mentioned {
                    if let Some(file) = read_inlined(workspace, m) {
                        packet.inlined_files.push(file);
                    }
                }
                for name in &input_artifacts {
                    if let Ok(contents) = self.blackboard.read_artifact(name) {
                        packet.inlined_files.push(InlinedFile {
                            path: artifact_rel_path(name),
                            contents,
                        });
                    }
                }
            }
        }

        Ok(packet)
    }
}

/// The workspace-relative path of a named artifact (e.g.
/// `.rinne/artifacts/design.md`), usable by a harness worker from the repo root.
fn artifact_rel_path(name: &str) -> PathBuf {
    Path::new(BLACKBOARD_DIR).join("artifacts").join(name)
}

/// Read a mentioned file's contents for inlining, resolving it against the
/// workspace. Returns `None` if it cannot be read (e.g. a directory or missing).
fn read_inlined(workspace: &Path, rel: &Path) -> Option<InlinedFile> {
    let abs = if rel.is_absolute() {
        rel.to_path_buf()
    } else {
        workspace.join(rel)
    };
    let contents = std::fs::read_to_string(&abs).ok()?;
    Some(InlinedFile {
        path: rel.to_path_buf(),
        contents,
    })
}
