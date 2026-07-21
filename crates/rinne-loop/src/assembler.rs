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

const MIN_IDENT_LEN: usize = 4;

/// Picks symbol names to attach from the graph for a given node instruction.
///
/// Returns exact identifier tokens in `instruction` that match a known symbol
/// name, plus symbols named like a mentioned file's stem. Deduplicates output.
/// Skips tokens shorter than `MIN_IDENT_LEN`.
///
/// Note: there is no hardcoded common-name denylist. Ambiguous names (`build`,
/// `run`, `new`) are disambiguated downstream by the mentioned-file hint in
/// [`ContextAssembler::build`], so they no longer need blanket suppression.
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
        if tok.len() >= MIN_IDENT_LEN && known_set.contains(tok) && !out.iter().any(|o| o == tok) {
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

/// Picks the neighborhoods to attach for a resolved symbol name, using the
/// node's mentioned files as a disambiguation hint.
///
/// A name may resolve to several definitions across files (e.g. `build` on
/// `ContextAssembler` vs `Blackboard`). When any definition lives in a mentioned
/// file, only those are attached — the mention is a strong signal of intent.
/// Otherwise every definition is attached, so a worker sees the full set rather
/// than one silently-chosen (possibly wrong) anchor.
fn neighborhoods_for(
    graph: &dyn rinne_types::graph::CodeGraph,
    name: &str,
    mentioned: &[std::path::PathBuf],
) -> Vec<rinne_types::graph::Neighborhood> {
    let all = graph.neighborhood_all(name);
    if all.len() <= 1 {
        return all;
    }
    let in_mentioned: Vec<_> = all
        .iter()
        .filter(|nb| {
            mentioned
                .iter()
                .any(|m| m.to_str().is_some_and(|p| p == nb.definition.file))
        })
        .cloned()
        .collect();
    if in_mentioned.is_empty() {
        all
    } else {
        in_mentioned
    }
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
        Self {
            blackboard,
            plan,
            graph,
        }
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
                        packet.pinned_paths.push(artifact_rel_path(name));
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
                for neighborhood in neighborhoods_for(graph, name, mentioned) {
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
                definition: SymbolRef {
                    name: "HttpTransport".into(),
                    file: "t.rs".into(),
                    line: 10,
                    end_line: 10,
                },
                callers: vec![SymbolRef {
                    name: "send".into(),
                    file: "s.rs".into(),
                    line: 3,
                    end_line: 3,
                }],
                callees: vec![],
                imports: vec![],
                stale: false,
            })
        }
        fn resolve_in_file(&self, _f: &str, _n: &str) -> Option<SymbolRef> {
            None
        }
        fn symbol_names(&self) -> Vec<String> {
            vec!["HttpTransport".into()]
        }
    }

    #[test]
    fn attaches_neighborhood_for_exact_identifier_in_instruction() {
        let names = FakeGraph.symbol_names();
        let picked = resolve_symbols(&FakeGraph, "add retry to HttpTransport", &[], &names);
        assert_eq!(picked, vec!["HttpTransport".to_string()]);
    }

    #[test]
    fn skips_short_names_below_min_ident_len() {
        // `new`/`run` are shorter than MIN_IDENT_LEN (4) and are skipped by the
        // length filter — NOT by any common-name denylist (which Stage 1 removed).
        let names = vec!["new".to_string(), "run".to_string()];
        let picked = resolve_symbols(&FakeGraph, "run the new thing", &[], &names);
        assert!(picked.is_empty());
    }

    #[test]
    fn keeps_formerly_denylisted_long_names() {
        // Stage 1 removed the COMMON_NAMES denylist: a 4+ char name like `build`
        // is now KEPT (to be disambiguated downstream by the mentioned-file hint),
        // whereas the old denylist suppressed it wholesale.
        let names = vec!["build".to_string()];
        let picked = resolve_symbols(&FakeGraph, "call build to assemble", &[], &names);
        assert_eq!(picked, vec!["build".to_string()]);
    }

    /// A graph where `build` is defined in two files, for testing the
    /// mentioned-file disambiguation in `neighborhoods_for`.
    struct AmbiguousGraph;
    impl AmbiguousGraph {
        fn nb(file: &str) -> Neighborhood {
            Neighborhood {
                definition: SymbolRef {
                    name: "build".into(),
                    file: file.into(),
                    line: 1,
                    end_line: 1,
                },
                callers: vec![],
                callees: vec![],
                imports: vec![],
                stale: false,
            }
        }
    }
    impl CodeGraph for AmbiguousGraph {
        fn neighborhood(&self, symbol: &str) -> Option<Neighborhood> {
            self.neighborhood_all(symbol).into_iter().next()
        }
        fn neighborhood_all(&self, symbol: &str) -> Vec<Neighborhood> {
            if symbol == "build" {
                vec![Self::nb("assembler.rs"), Self::nb("blackboard.rs")]
            } else {
                vec![]
            }
        }
        fn resolve_in_file(&self, _f: &str, _n: &str) -> Option<SymbolRef> {
            None
        }
        fn symbol_names(&self) -> Vec<String> {
            vec!["build".into()]
        }
    }

    #[test]
    fn mentioned_file_disambiguates_among_same_name_defs() {
        // Mention assembler.rs → only that file's `build` is attached, not blackboard's.
        let mentioned = vec![std::path::PathBuf::from("assembler.rs")];
        let got = neighborhoods_for(&AmbiguousGraph, "build", &mentioned);
        assert_eq!(got.len(), 1, "hint must narrow to the mentioned file");
        assert_eq!(got[0].definition.file, "assembler.rs");
    }

    #[test]
    fn without_hint_attaches_all_same_name_defs() {
        // No mention → attach BOTH, so the worker sees the full set rather than
        // one silently-chosen (possibly wrong) anchor. This is the core Stage 1 fix.
        let got = neighborhoods_for(&AmbiguousGraph, "build", &[]);
        let mut files: Vec<&str> = got.iter().map(|n| n.definition.file.as_str()).collect();
        files.sort_unstable();
        assert_eq!(files, vec!["assembler.rs", "blackboard.rs"]);
    }
}
