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
    // Sort before dedup so non-adjacent duplicate edges (the same caller→callee
    // reached via different cluster symbols) collapse, not just consecutive ones.
    pairs.sort();
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
    let mut lines = explain_to_lines(&topic, cwd, no_ai).await?;
    // The CLI's `--open` adds a browser hint after the "wrote …" line; the TUI
    // path (run_lines) never sets it.
    if open {
        if let Some(path) = lines.last().and_then(|l| l.strip_prefix("wrote ")) {
            lines.push(format!("open it in your browser: {path}"));
        }
    }
    for line in lines {
        println!("{line}");
    }
    Ok(())
}

/// TUI-facing entry point for `/learn <topic>`: runs the same pipeline as [`run`]
/// but returns the output lines for the feed instead of printing to stdout
/// (printing would corrupt the inline TUI). Always narrates when a worker is
/// available, degrading to template-only otherwise.
/// Reports phase milestones through `on_progress` while the pipeline runs, so
/// the TUI can surface progress during the slow AI phase. Pass a no-op sink for
/// silent operation.
pub async fn run_lines_with_progress(
    topic: &str,
    cwd: PathBuf,
    on_progress: &(dyn Fn(String) + Send + Sync),
) -> Vec<String> {
    match explain_with_progress(topic, cwd, false, on_progress).await {
        Ok(lines) => lines,
        Err(e) => vec![format!("learn failed: {e}")],
    }
}

/// The shared pipeline with a no-op progress sink. See [`explain_with_progress`].
async fn explain_to_lines(topic: &str, cwd: PathBuf, no_ai: bool) -> Result<Vec<String>> {
    explain_with_progress(topic, cwd, no_ai, &|_| {}).await
}

/// The shared pipeline: resolve → refresh → assemble → translate → render →
/// write. Returns the human-readable result lines (e.g. `wrote <path>` or the
/// "no code found" note); the caller decides how to surface them.
///
/// `on_progress` is invoked with a short milestone string at each phase
/// boundary so a long-running caller (the TUI) can show it is alive; the CLI
/// passes a no-op. The AI-narration phase is the slow one (up to the worker
/// timeout), so its milestone fires before the `translate` await, not after.
async fn explain_with_progress(
    topic: &str,
    cwd: PathBuf,
    no_ai: bool,
    on_progress: &(dyn Fn(String) + Send + Sync),
) -> Result<Vec<String>> {
    let topic = topic.to_string();

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
        on_progress("indexing repository…".to_string());
        bb.index_repo();
    }

    let workspace = bb.workspace().to_path_buf();

    // Build the worker registry up front: it drives both the AI fallback for
    // resolving a free-form query and the later narration. Degrades to an empty
    // registry (template-only, deterministic-only) when there's no config or no
    // available worker — neither path errors out.
    let (registry, _) = match rinne_config::load_cwd() {
        Ok(cfg) => crate::runner::build_registry(&cfg)
            .await
            .unwrap_or_else(|_| (WorkerRegistry::new(), vec![])),
        Err(_) => (WorkerRegistry::new(), vec![]),
    };

    // Resolve the cluster — clone the Arc so we don't hold a borrow of bb.
    // Literal/per-word matching first; on a miss, fall back to letting a worker
    // pick relevant symbols from the graph so a plain description still resolves.
    let cluster: Cluster = {
        let Some(arc_g) = bb.concrete_graph() else {
            anyhow::bail!("code graph unavailable");
        };
        let g = arc_g.as_ref();
        let mut c = crate::learn::resolve::resolve_cluster(g, &topic, 40);

        if c.symbols.is_empty() && !no_ai && !registry.is_empty() {
            on_progress("no literal match — asking AI to find relevant code…".to_string());
            let known = g.symbol_names();
            let picked = crate::learn::translate::ai_pick_symbols(
                no_ai, &registry, &workspace, &topic, &known,
            )
            .await;
            if !picked.is_empty() {
                c = crate::learn::resolve::cluster_from_seeds(g, &topic, &picked, 40);
            }
        }
        c
    };

    if cluster.symbols.is_empty() {
        let hint = if registry.is_empty() {
            "no code found for topic `{topic}`. try a symbol or path fragment, \
             or connect a worker so `learn` can resolve plain descriptions."
        } else {
            "no code found for topic `{topic}`. try a symbol, path fragment, or \
             a more specific description."
        };
        return Ok(vec![hint.replace("{topic}", &topic)]);
    }

    on_progress(format!(
        "resolved {} symbols across {} files",
        cluster.symbols.len(),
        cluster.files.len(),
    ));

    // Phase 2: reindex the cluster's files before reading snippets.
    // cluster.files contains owned Strings — no borrow conflict.
    for f in &cluster.files {
        bb.reindex_file(&cwd.join(f));
    }

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

    // The AI phase is the slow one; narrate its start before awaiting so the
    // TUI isn't silent for up to the worker timeout. Only when a real worker
    // will run — the template-only path (no_ai / empty registry) is instant.
    if !no_ai && !registry.is_empty() {
        on_progress("narrating with AI (may take ~1–2 min)…".to_string());
    }

    let translator = crate::learn::translate::build_translator(no_ai, &registry, &workspace);
    let narration = translator.translate(&doc).await;

    let html = crate::learn::render::render_html(&doc, narration.as_ref());

    let out = bb.root().join("learn").join(format!("{topic}.html"));
    std::fs::create_dir_all(out.parent().unwrap())?;
    std::fs::write(&out, html)?;

    Ok(vec![format!("wrote {}", out.display())])
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

    #[tokio::test]
    async fn run_lines_returns_wrote_path_for_tui() {
        let dir = std::env::temp_dir().join(format!("rinne-learn-tui-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/harness.rs"),
            "/// The harness adapter.\npub fn harness_run() { helper(); }\nfn helper() {}\n",
        )
        .unwrap();

        // No worker configured in the test env → template-only, must not error.
        let lines = run_lines_with_progress("harness", dir.clone(), &|_| {}).await;

        assert!(
            lines.iter().any(|l| l.starts_with("wrote ") && l.contains("harness.html")),
            "run_lines returns the wrote-path line, got: {lines:?}"
        );
        assert!(dir.join(".rinne/learn/harness.html").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn progress_callback_reports_phase_milestones() {
        use std::sync::Mutex;
        let dir = std::env::temp_dir().join(format!("rinne-learn-prog-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/harness.rs"),
            "/// The harness adapter.\npub fn harness_run() { helper(); }\nfn helper() {}\n",
        )
        .unwrap();

        let seen: Mutex<Vec<String>> = Mutex::new(Vec::new());
        // no_ai = true → template-only path, no worker required; the resolve
        // milestone must still fire.
        let lines = explain_with_progress("harness", dir.clone(), true, &|m| {
            seen.lock().unwrap().push(m);
        })
        .await
        .unwrap();

        let seen = seen.into_inner().unwrap();
        assert!(
            seen.iter().any(|m| m.contains("resolved")),
            "resolve milestone reported, got: {seen:?}"
        );
        assert!(
            lines.iter().any(|l| l.starts_with("wrote ")),
            "still returns wrote path, got: {lines:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn run_lines_reports_no_match_topic() {
        let dir = std::env::temp_dir().join(format!("rinne-learn-nomatch-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "fn alpha() {}\n").unwrap();

        let lines = run_lines_with_progress("zzz-nonexistent-topic", dir.clone(), &|_| {}).await;
        assert!(
            lines.iter().any(|l| l.contains("no code found")),
            "reports no-match, got: {lines:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
