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
/// 7. Render HTML and write to `.rinne/learn/<slug>.html` where `<slug>` is a
///    standardised form of the topic (see [`crate::learn::topic_slug`]).
///
/// Live progress goes to **stderr** so it doesn't mix with the final result
/// lines on stdout (and so piping still works).
pub async fn run(cmd: LearnCmd, cwd: PathBuf, no_ai: bool, open: bool) -> Result<()> {
    let LearnCmd::Explain { topic } = cmd;
    let on_progress: crate::learn::translate::ProgressSink =
        std::sync::Arc::new(|m: String| eprintln!("  learn  {m}"));
    let mut lines = explain_with_progress(&topic, cwd, no_ai, on_progress).await?;
    // The CLI's `--open` adds a browser hint after the "wrote …" line; the TUI
    // path (run_lines) never sets it.
    if open {
        if let Some(path) = lines.iter().find_map(|l| l.strip_prefix("wrote ")) {
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
    on_progress: crate::learn::translate::ProgressSink,
) -> Vec<String> {
    match explain_with_progress(topic, cwd, false, on_progress).await {
        Ok(lines) => lines,
        Err(e) => vec![format!("learn failed: {e}")],
    }
}

/// The shared pipeline: resolve → refresh → assemble → translate → render →
/// write. Returns the human-readable result lines (e.g. `wrote <path>` or the
/// "no code found" note); the caller decides how to surface them.
///
/// `on_progress` is invoked with short milestone / worker-event strings while
/// the pipeline runs (resolve, AI tool use, elapsed heartbeats). The CLI prints
/// these to stderr; the TUI pushes them into the feed.
async fn explain_with_progress(
    topic: &str,
    cwd: PathBuf,
    no_ai: bool,
    on_progress: crate::learn::translate::ProgressSink,
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
        on_progress(crate::learn::translate::progress("index", "repository"));
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
    let mut total_usage = rinne_loop::worker::Usage::default();
    let cluster: Cluster = {
        let Some(arc_g) = bb.concrete_graph() else {
            anyhow::bail!("code graph unavailable");
        };
        let g = arc_g.as_ref();
        let mut c = crate::learn::resolve::resolve_cluster(g, &topic, 40);

        if c.symbols.is_empty() && !no_ai && !registry.is_empty() {
            let pick_label = crate::learn::translate::planned_worker_model(&registry)
                .map(|m| m.label())
                .unwrap_or_else(|| "AI".into());
            on_progress(crate::learn::translate::progress(
                "pick",
                format!("no literal match · {pick_label}"),
            ));
            let known = g.symbol_names();
            let picked = crate::learn::translate::ai_pick_symbols(
                no_ai,
                &registry,
                &workspace,
                &topic,
                &known,
                &on_progress,
            )
            .await;
            total_usage =
                crate::learn::translate::add_usage(total_usage, picked.usage);
            if !picked.symbols.is_empty() {
                c = crate::learn::resolve::cluster_from_seeds(g, &topic, &picked.symbols, 40);
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

    on_progress(crate::learn::translate::progress(
        "resolve",
        format!(
            "{} symbols · {} files",
            cluster.symbols.len(),
            cluster.files.len(),
        ),
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

    // The AI phase is the slow one; name the cheap tier up front and stream
    // worker events + elapsed heartbeats via `on_progress` during the await.
    if !no_ai && !registry.is_empty() {
        let label = crate::learn::translate::planned_worker_model(&registry)
            .map(|m| m.label())
            .unwrap_or_else(|| "AI".into());
        on_progress(crate::learn::translate::progress(
            "narrate",
            format!("{label} · up to ~5 min"),
        ));
    }

    let translator =
        crate::learn::translate::build_translator(no_ai, &registry, &workspace, on_progress);
    let translation = translator.translate(&doc).await;
    let (narration, used) = match translation {
        Some(t) => {
            total_usage = crate::learn::translate::add_usage(total_usage, t.usage);
            (Some(t.narration), Some(t.used))
        }
        None => (None, None),
    };

    let html = crate::learn::render::render_html(&doc, narration.as_ref());

    // Artifact name is always a stable slug. Display title inside the HTML keeps
    // the original free-form topic (e.g. "accounts module" → accounts-module.html).
    let slug = crate::learn::topic_slug(&topic);
    let out = bb.root().join("learn").join(format!("{slug}.html"));
    std::fs::create_dir_all(out.parent().unwrap())?;
    std::fs::write(&out, html)?;

    // Final lines: path, which model ran, and tokens/time (CLI + TUI both see these).
    let mut lines = vec![format!("wrote {}", out.display())];
    if slug != topic {
        lines.push(format!("slug: {slug}  (from `{topic}`)"));
    }
    if let Some(used) = used {
        lines.push(format!("narrated with {}", used.label()));
        if total_usage.total_tokens() > 0 || total_usage.wall_ms > 0 {
            lines.push(format!(
                "tokens: {}",
                crate::learn::translate::format_usage(&total_usage)
            ));
        }
    } else if !no_ai && !registry.is_empty() {
        lines.push("template only — narration failed".into());
        if total_usage.total_tokens() > 0 || total_usage.wall_ms > 0 {
            lines.push(format!(
                "tokens: {}",
                crate::learn::translate::format_usage(&total_usage)
            ));
        }
    } else {
        lines.push("template only".into());
    }
    // Point at the local browser so people don't hunt for HTML files on disk.
    // TUI: `/serve` · CLI: `rinne learn serve`.
    lines.push(format!(
        "browse all docs: /serve   (or: rinne learn serve · http://127.0.0.1:7420 · this topic: {slug})"
    ));
    Ok(lines)
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
    async fn free_form_topic_writes_slug_filename() {
        let dir = std::env::temp_dir().join(format!("rinne-learn-slug-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/accounts.rs"),
            "/// Accounts module.\npub fn accounts_open() {}\n",
        )
        .unwrap();

        run(
            LearnCmd::Explain {
                topic: "accounts module".into(),
            },
            dir.clone(),
            true,
            false,
        )
        .await
        .unwrap();

        // Standardised name — not "accounts module.html".
        assert!(
            dir.join(".rinne/learn/accounts-module.html").is_file(),
            "expected accounts-module.html"
        );
        assert!(
            !dir.join(".rinne/learn/accounts module.html").exists(),
            "must not write raw spaced filename"
        );
        // Page title still uses the human query.
        let html = std::fs::read_to_string(dir.join(".rinne/learn/accounts-module.html")).unwrap();
        assert!(html.contains("accounts module") || html.contains("accounts_open"));
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
        let noop: crate::learn::translate::ProgressSink = std::sync::Arc::new(|_: String| {});
        let lines = run_lines_with_progress("harness", dir.clone(), noop).await;

        assert!(
            lines.iter().any(|l| l.starts_with("wrote ") && l.contains("harness.html")),
            "run_lines returns the wrote-path line, got: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("rinne learn serve")),
            "should guide to learn serve, got: {lines:?}"
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

        let seen: std::sync::Arc<Mutex<Vec<String>>> =
            std::sync::Arc::new(Mutex::new(Vec::new()));
        // no_ai = true → template-only path, no worker required; the resolve
        // milestone must still fire.
        let seen_cb = seen.clone();
        let on_progress: crate::learn::translate::ProgressSink =
            std::sync::Arc::new(move |m: String| {
                seen_cb.lock().unwrap().push(m);
            });
        let lines = explain_with_progress("harness", dir.clone(), true, on_progress)
            .await
            .unwrap();

        let seen = seen.lock().unwrap().clone();
        assert!(
            seen.iter().any(|m| m.contains("resolve") && m.contains("symbols")),
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

        let noop: crate::learn::translate::ProgressSink = std::sync::Arc::new(|_: String| {});
        let lines = run_lines_with_progress("zzz-nonexistent-topic", dir.clone(), noop).await;
        assert!(
            lines.iter().any(|l| l.contains("no code found")),
            "reports no-match, got: {lines:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
