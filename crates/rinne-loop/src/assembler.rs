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

const COMMON_NAMES: &[&str] = &["new", "run", "get", "set", "build", "main", "init", "from", "into"];
const MIN_IDENT_LEN: usize = 4;

/// Picks symbol names to attach from the graph for a given node instruction.
///
/// Returns exact identifier tokens in `instruction` that match a known symbol
/// name, plus symbols named like a mentioned file's stem. Deduplicates output.
/// Skips tokens shorter than `MIN_IDENT_LEN` and tokens in `COMMON_NAMES`.
pub fn resolve_symbols(
    graph: &dyn rinne_types::graph::CodeGraph,
    instruction: &str,
    mentioned: &[std::path::PathBuf],
    known: &[String],
) -> Vec<String> {
    let known_set: std::collections::HashSet<&str> = known.iter().map(|s| s.as_str()).collect();
    let mut out: Vec<String> = Vec::new();
    // Exact identifier tokens from the instruction that name a known symbol.
    for tok in instruction.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if tok.len() >= MIN_IDENT_LEN
            && !COMMON_NAMES.contains(&tok)
            && known_set.contains(tok)
            && !out.iter().any(|o| o == tok)
        {
            out.push(tok.to_string());
        }
    }
    // Symbols named like a mentioned file's stem (best-effort).
    for m in mentioned {
        if let Some(stem) = m.file_stem().and_then(|s| s.to_str()) {
            if known_set.contains(stem) && !out.iter().any(|o| o == stem) {
                out.push(stem.to_string());
            }
        }
    }
    let _ = graph;
    out
}

/// Builds context packets against a plan and its blackboard.
pub struct ContextAssembler<'a> {
    blackboard: &'a dyn Blackboard,
    plan: &'a Plan,
    graph: Option<&'a dyn rinne_types::graph::CodeGraph>,
}

impl<'a> ContextAssembler<'a> {
    pub fn new(
        blackboard: &'a dyn Blackboard,
        plan: &'a Plan,
        graph: Option<&'a dyn rinne_types::graph::CodeGraph>,
    ) -> Self {
        Self { blackboard, plan, graph }
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

        // Attach graph neighborhoods for resolved symbols (additive; never
        // replaces pinned_paths / inlined_files set above).
        if let Some(graph) = self.graph {
            let known = graph.symbol_names();
            let picked = resolve_symbols(graph, &node.instruction, mentioned, &known);
            for name in &picked {
                if let Some(neighborhood) = graph.neighborhood(name) {
                    packet.symbol_map.push(neighborhood);
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

/// Cap on a single inlined file's size, so `@`-mentioning a huge (or binary)
/// file can't blow up the API request's tokens/cost/memory. Content past the cap
/// is truncated with a visible marker rather than sent whole.
const MAX_INLINE_BYTES: usize = 256 * 1024;

/// Read a mentioned file's contents for inlining, resolving it against the
/// workspace. Returns `None` if it cannot be read (e.g. a directory or missing);
/// oversized files are truncated with a marker rather than dropped.
fn read_inlined(workspace: &Path, rel: &Path) -> Option<InlinedFile> {
    let abs = if rel.is_absolute() {
        rel.to_path_buf()
    } else {
        workspace.join(rel)
    };
    let mut contents = std::fs::read_to_string(&abs).ok()?;
    if contents.len() > MAX_INLINE_BYTES {
        // Truncate on a char boundary at or below the cap.
        let mut cut = MAX_INLINE_BYTES;
        while cut > 0 && !contents.is_char_boundary(cut) {
            cut -= 1;
        }
        contents.truncate(cut);
        contents.push_str("\n…[file truncated: exceeded inline size limit]\n");
    }
    Some(InlinedFile {
        path: rel.to_path_buf(),
        contents,
    })
}

#[cfg(test)]
mod graph_tests {
    use super::*;
    use rinne_types::graph::{CodeGraph, Neighborhood, SymbolRef};

    struct FakeGraph;
    impl CodeGraph for FakeGraph {
        fn neighborhood(&self, symbol: &str) -> Option<Neighborhood> {
            (symbol == "HttpTransport").then(|| Neighborhood {
                definition: SymbolRef { name: "HttpTransport".into(), file: "t.rs".into(), line: 10 },
                callers: vec![SymbolRef { name: "send".into(), file: "s.rs".into(), line: 3 }],
                callees: vec![],
                imports: vec![],
                stale: false,
            })
        }
        fn resolve_in_file(&self, _f: &str, _n: &str) -> Option<SymbolRef> { None }
        fn symbol_names(&self) -> Vec<String> { vec!["HttpTransport".into()] }
    }

    #[test]
    fn attaches_neighborhood_for_exact_identifier_in_instruction() {
        let names = FakeGraph.symbol_names();
        let picked = resolve_symbols(&FakeGraph, "add retry to HttpTransport", &[], &names);
        assert_eq!(picked, vec!["HttpTransport".to_string()]);
    }

    #[test]
    fn skips_common_short_names() {
        let names = vec!["new".to_string(), "run".to_string()];
        let picked = resolve_symbols(&FakeGraph, "run the new thing", &[], &names);
        assert!(picked.is_empty());
    }
}
