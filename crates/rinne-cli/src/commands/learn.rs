//! `rinne learn explain` — two-phase code graph refresh, symbol clustering,
//! snippet assembly, optional AI narration, HTML output.

use std::path::PathBuf;

use anyhow::Result;
use rinne_core::{Blackboard, WorkerRegistry};
use rinne_types::graph::CodeGraph;

use crate::learn::{Cluster, LearnDoc};

/// Subcommands for `rinne learn`.
pub enum LearnCmd {
    Explain { topic: String },
}

/// Collect (caller, sym) and (sym, callee) pairs from each cluster symbol's
/// neighborhood. Deduplicates the resulting list.
fn cluster_flow(graph: &dyn CodeGraph, cluster: &Cluster) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    for sym in &cluster.symbols {
        if let Some(nb) = graph.neighborhood(&sym.name) {
            for caller in &nb.callers {
                pairs.push((caller.name.clone(), sym.name.clone()));
            }
            for callee in &nb.callees {
                pairs.push((sym.name.clone(), callee.name.clone()));
            }
        }
    }
    pairs.dedup();
    pairs
}

/// Orchestrate `rinne learn explain <topic>`:
/// 1. Open the blackboard with the graph enabled.
/// 2. Phase-1 refresh: reindex files matching the topic so resolution is fresh.
/// 3. Resolve the cluster.
/// 4. Phase-2 refresh: reindex the cluster's files before reading.
/// 5. Assemble snippets + doc sections.
/// 6. Build a translator (NullTranslator on --no-ai or no registry).
/// 7. Render HTML and write to `.rinne/learn/<topic>.html`.
pub async fn run(cmd: LearnCmd, cwd: PathBuf, no_ai: bool, open: bool) -> Result<()> {
    let LearnCmd::Explain { topic } = cmd;

    let bb = Blackboard::open_with(&cwd, true)?;

    // Phase 1: reindex candidate files whose symbol names contain the topic,
    // so the graph is fresh before resolution.
    // Collect paths into an owned Vec first to drop the concrete_graph Arc
    // before calling reindex_file, avoiding any aliasing concern.
    let phase1_files: Vec<PathBuf> = {
        let topic_lower = topic.to_lowercase();
        match bb.concrete_graph() {
            None => vec![],
            Some(arc_g) => {
                let g: &dyn CodeGraph = arc_g.as_ref();
                g.symbol_names()
                    .into_iter()
                    .filter(|name| name.to_lowercase().contains(&topic_lower))
                    .filter_map(|name| {
                        g.neighborhood(&name)
                            .map(|nb| cwd.join(&nb.definition.file))
                    })
                    .collect()
            }
        }
    };
    for path in &phase1_files {
        bb.reindex_file(path);
    }

    // If the graph is empty, index the whole repo so a fresh project can
    // resolve any symbol.
    if bb.concrete_graph()
        .as_ref()
        .map(|g| g.stats().0)
        .unwrap_or(0)
        == 0
    {
        bb.index_repo();
    }

    // Resolve the cluster — clone the Arc so we don't hold a borrow of bb.
    let cluster: Cluster = {
        let Some(arc_g) = bb.concrete_graph() else {
            anyhow::bail!("code graph unavailable");
        };
        crate::learn::resolve::resolve_cluster(arc_g.as_ref(), &topic, 40)
    };

    if cluster.symbols.is_empty() {
        println!("no code found for topic `{topic}`. try a symbol or path fragment.");
        return Ok(());
    }

    // Phase 2: reindex the cluster's files before reading snippets.
    // cluster.files contains owned Strings — no borrow conflict.
    for f in &cluster.files {
        bb.reindex_file(&cwd.join(f));
    }

    let workspace = bb.workspace().to_path_buf();
    let (snippets, doc_sections) = crate::learn::source::assemble(&workspace, &cluster);

    // Build flow pairs — get Arc, coerce to trait object, compute, drop.
    let flow: Vec<(String, String)> = match bb.concrete_graph() {
        None => vec![],
        Some(arc_g) => cluster_flow(arc_g.as_ref(), &cluster),
    };

    let doc = LearnDoc {
        topic: topic.clone(),
        snippets,
        flow,
        doc_sections,
    };

    // Build the translator, degrading gracefully when there is no config or
    // no available workers. Both --no-ai and the no-worker path produce
    // template-only HTML and never error out.
    let (registry, _) = match rinne_config::load_cwd() {
        Ok(cfg) => crate::runner::build_registry(&cfg)
            .await
            .unwrap_or_else(|_| (WorkerRegistry::new(), vec![])),
        Err(_) => (WorkerRegistry::new(), vec![]),
    };

    let translator = crate::learn::translate::build_translator(no_ai, &registry, &workspace);
    let narration = translator.translate(&doc).await;

    let html = crate::learn::render::render_html(&doc, narration.as_ref());

    let out = bb.root().join("learn").join(format!("{topic}.html"));
    std::fs::create_dir_all(out.parent().unwrap())?;
    std::fs::write(&out, html)?;
    println!("wrote {}", out.display());

    if open {
        println!("open it in your browser: {}", out.display());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn learn_explain_no_ai_writes_html_with_real_symbol() {
        let dir = std::env::temp_dir().join(format!("rinne-learn-e2e-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/harness.rs"),
            "/// The harness adapter.\npub fn harness_run() { helper(); }\nfn helper() {}\n",
        )
        .unwrap();

        // no_ai = true → template-only, no worker required.
        run(LearnCmd::Explain { topic: "harness".into() }, dir.clone(), true, false)
            .await
            .unwrap();

        let out = dir.join(".rinne/learn/harness.html");
        assert!(out.is_file(), "html artifact written");
        let html = std::fs::read_to_string(&out).unwrap();
        assert!(html.contains("harness_run"), "real symbol in output");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
