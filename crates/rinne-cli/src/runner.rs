//! Wiring between the CLI and the loop engine (`PHASE.md` P3).
//!
//! Builds a [`WorkerRegistry`] from the real adapters that `doctor` reports as
//! available, then runs (or resumes) a plan through the engine, streaming
//! engine events to the terminal. This is the pre-TUI, plain-output path; the
//! four-region TUI lands in Phase 6.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use tokio_util::sync::CancellationToken;

use rinne_conductor::{resolve_openai, Conductor, ConductorInput, HarnessBackend, PlanBackend};
use rinne_config::model::PreferFamily;
use rinne_config::probe::WorkerFamily;
use rinne_config::Config;
use rinne_core::worker::{Capability, Worker};
use rinne_core::{Blackboard, Engine, EngineEvent, EngineOptions, RunReport, WorkerRegistry};
use rinne_workers::adapters::{
    aider, antigravity, claude_code, codex, cursor, grok, opencode, OpenAiWorker,
};

/// Build a worker registry from configured + available harness adapters.
///
/// Workers are added in the user's family preference order so the registry's
/// insertion order encodes preference for tie-breaking (`CONTEXT.md` §13).
/// Returns the registry plus the names registered, for narration.
pub async fn build_registry(config: &Config) -> Result<(WorkerRegistry, Vec<String>)> {
    let report = rinne_config::doctor(config, false).await?;

    let mut reg = WorkerRegistry::new();
    for w in report.workers.iter().filter(|w| {
        w.family == WorkerFamily::Harness && w.enabled && w.status.is_available()
    }) {
        let adapter: Option<Arc<dyn Worker>> = match w.name.as_str() {
            "claude-code" => Some(Arc::new(claude_code::worker())),
            "codex" => Some(Arc::new(codex::worker())),
            "opencode" => Some(Arc::new(opencode::worker())),
            "grok" => Some(Arc::new(grok::worker())),
            "cursor-agent" => Some(Arc::new(cursor::worker())),
            "aider" => Some(Arc::new(aider::worker())),
            "antigravity" => Some(Arc::new(antigravity::worker())),
            _ => None,
        };
        if let Some(a) = adapter {
            reg.register(a);
        }
    }

    // API workers: one per configured `[backends.api.<provider>]` whose key env
    // var is set. Rinne reads the key from the env at build time; it is never
    // stored. An API-only user gets these as their full pool (generator
    // included), with the configured models forming the cascade ladder.
    for (name, provider) in &config.backends.api.providers {
        let keys = rinne_config::secrets::resolve_api_keys(name, &provider.key_env);
        if keys.is_empty() {
            continue; // no key (env or keychain) — skip silently; `doctor` surfaces it
        }
        let base = provider
            .base_url
            .clone()
            .or_else(|| default_api_base(name).map(String::from));
        let Some(base) = base else {
            continue; // unknown provider with no base_url — can't construct
        };
        let models: Vec<String> = if !provider.models.is_empty() {
            provider.models.clone()
        } else if let Some(m) = &provider.model {
            vec![m.clone()]
        } else {
            continue; // need at least one model id to call
        };
        reg.register(Arc::new(OpenAiWorker::new(
            name,
            &base,
            keys,
            models,
            api_capabilities(),
            provider.extra_body.clone(),
        )));
    }

    let names = reg.names();
    Ok((reg, names))
}

/// Capabilities a raw API model can satisfy. Notably NOT `repo-aware` (it can't
/// explore the repo — context is inlined) or `tool-run` (no tools).
fn api_capabilities() -> Vec<Capability> {
    vec![
        Capability::CodeEdit,
        Capability::Reasoning,
        Capability::Writing,
        Capability::CodeReview,
        Capability::LongContext,
    ]
}

/// Default OpenAI-compatible base URL for known providers.
fn default_api_base(provider: &str) -> Option<&'static str> {
    match provider {
        "openai" => Some("https://api.openai.com/v1"),
        "deepseek" => Some("https://api.deepseek.com/v1"),
        "groq" => Some("https://api.groq.com/openai/v1"),
        "openrouter" => Some("https://openrouter.ai/api/v1"),
        "google" | "gemini" => Some("https://generativelanguage.googleapis.com/v1beta/openai"),
        "together" => Some("https://api.together.xyz/v1"),
        "mistral" => Some("https://api.mistral.ai/v1"),
        "xai" => Some("https://api.x.ai/v1"),
        _ => None,
    }
}

/// Build the conductor: the configured OpenAI-compatible backend first (if its
/// key is available), then the cheapest installed harness as the fallback
/// conductor (`CONTEXT.md` §7). Needs a non-empty registry for the fallback.
pub fn build_conductor(
    config: &Config,
    registry: &WorkerRegistry,
    workspace: std::path::PathBuf,
) -> Result<Conductor> {
    let mut backends: Vec<Box<dyn PlanBackend>> = Vec::new();

    match resolve_openai(&config.conductor) {
        Ok(Some(api)) => backends.push(Box::new(api)),
        Ok(None) => {} // not configured or key missing → rely on fallback
        Err(e) => return Err(anyhow!(e.to_string())),
    }

    if let Some(worker) = registry.first() {
        backends.push(Box::new(HarnessBackend::new(worker, workspace)));
    }

    Conductor::new(backends).map_err(|e| {
        anyhow!(
            "{} (configure a conductor backend or install a harness)",
            e
        )
    })
}

/// One-shot headless run that returns a structured JSON result instead of
/// streaming human-readable progress (`CONTEXT.md` §6). Quiet: no narration to
/// stdout, so the only output is the JSON the caller prints.
pub async fn oneshot_json(goal: &str) -> Result<serde_json::Value> {
    let config = rinne_config::load_cwd()?;
    let cwd = std::env::current_dir()?;
    let bb = Blackboard::open(&cwd)?;
    let (registry, _) = build_registry(&config).await?;
    if registry.is_empty() {
        return Err(anyhow!("no available workers — run `rinne doctor`"));
    }
    let conductor = std::sync::Arc::new(build_conductor(&config, &registry, cwd.clone())?);

    let input = ConductorInput {
        goal: goal.to_string(),
        workers: registry.descriptors(),
        prefer: Some(prefer_label(config.preferences.prefer).to_string()),
        budget_minutes: Some(config.loop_.global_budget_minutes as u64),
        max_iterations_per_node: config.loop_.max_iterations_per_node,
        ..Default::default()
    };
    let plan = conductor.plan(&input).await?;
    bb.save_plan(&plan)?;
    bb.reset_run()?;

    let mut engine = Engine::new(&bb, plan.clone(), &registry, options_with_pool(&config, &registry));
    engine = engine.with_replanner(conductor);
    // None sink → no streaming output; just run to completion.
    let report = engine.run(CancellationToken::new(), None, None).await?;

    // The plan may have been amended by a replan; reload the current one.
    let final_plan = bb.load_plan().unwrap_or(plan);
    let state = rinne_core::state::State::open(&bb.state_db_path())?;
    let nodes: Vec<serde_json::Value> = final_plan
        .nodes
        .iter()
        .map(|n| {
            serde_json::json!({
                "id": n.id,
                "role": format!("{:?}", n.role).to_lowercase(),
                "status": state.status(&n.id).map(|s| s.label()).unwrap_or("pending"),
                "worker": state.worker(&n.id).ok().flatten(),
                "iterations": state.iterations(&n.id).unwrap_or(0),
            })
        })
        .collect();

    let (kind, detail) = stop_reason_parts(&report.stop_reason);
    Ok(serde_json::json!({
        "goal": if goal.is_empty() { final_plan.goal.clone() } else { goal.to_string() },
        "completed": report.completed,
        "stop_reason": { "kind": kind, "detail": detail },
        "nodes": nodes,
        "usage": {
            "total_tokens": report.total_usage.total_tokens(),
            "wall_ms": report.total_usage.wall_ms,
        },
        "total_iterations": report.total_iterations,
        "artifacts": list_artifacts(&bb),
    }))
}

fn stop_reason_parts(s: &rinne_core::StopReason) -> (&'static str, Option<String>) {
    use rinne_core::StopReason::*;
    match s {
        Completed => ("completed", None),
        Blocked => ("blocked", None),
        BudgetMinutes => ("budget_minutes", None),
        BudgetIterations => ("budget_iterations", None),
        Cancelled => ("cancelled", None),
        NoCapableWorker(n) => ("no_capable_worker", Some(n.clone())),
        NeedsHuman { node, question } => ("needs_human", Some(format!("{node}: {question}"))),
    }
}

fn list_artifacts(bb: &Blackboard) -> Vec<String> {
    let dir = bb.root().join("artifacts");
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

/// Generate a plan from a natural-language goal and persist it to the blackboard.
pub async fn plan_goal(blackboard: &Blackboard, goal: &str) -> Result<()> {
    let config = rinne_config::load_cwd()?;
    let (registry, _names) = build_registry(&config).await?;
    if registry.is_empty() {
        return Err(anyhow!(
            "no available workers — run `rinne doctor` (need an enabled, installed harness)"
        ));
    }

    let conductor = build_conductor(&config, &registry, blackboard.workspace().to_path_buf())?;
    let input = ConductorInput {
        goal: goal.to_string(),
        workers: registry.descriptors(),
        prefer: Some(prefer_label(config.preferences.prefer).to_string()),
        budget_minutes: Some(config.loop_.global_budget_minutes as u64),
        max_iterations_per_node: config.loop_.max_iterations_per_node,
        ..Default::default()
    };

    println!("planning with: {}", conductor.backend_names().join(" → "));
    let plan = conductor.plan(&input).await?;
    blackboard.save_plan(&plan)?;
    // A fresh goal is a fresh run: clear any stale state (node statuses, the run
    // clock) so a leftover `.rinne/` does not trip budgets or skip nodes.
    blackboard.reset_run()?;

    println!("\nplan ({} node{}):", plan.nodes.len(), if plan.nodes.len() == 1 { "" } else { "s" });
    for n in &plan.nodes {
        let dep = if n.depends_on.is_empty() {
            String::new()
        } else {
            format!("  ⟵ {}", n.depends_on.join(", "))
        };
        println!(
            "  {:<4} {:<12} {}{}",
            n.id,
            format!("{:?}", n.role).to_lowercase(),
            n.instruction.lines().next().unwrap_or(""),
            dep
        );
    }
    println!();
    Ok(())
}

fn prefer_label(p: PreferFamily) -> &'static str {
    match p {
        PreferFamily::Harness => "harness",
        PreferFamily::Api => "api",
        PreferFamily::Balanced => "balanced",
    }
}

/// Engine options derived from config (`[loop]`, `[models]`, `[preferences]`).
pub fn options_from_config(config: &Config) -> EngineOptions {
    EngineOptions {
        max_iterations_per_node: config.loop_.max_iterations_per_node,
        global_budget_minutes: Some(config.loop_.global_budget_minutes as u64),
        max_total_iterations: None,
        stuck_loop_threshold: config.loop_.stuck_loop_threshold,
        test_ratchet: config.loop_.test_ratchet,
        role_models: config.preferences.models.clone().into_iter().collect(),
        worker_models: config.models.by_worker.clone().into_iter().collect(),
        ..Default::default()
    }
}

/// Engine options merged with the live pool profile (tier ladders for the
/// cascade and the single-family flag for evaluator-independence narration).
pub fn options_with_pool(config: &Config, registry: &WorkerRegistry) -> EngineOptions {
    let profile = rinne_core::pool::profile(&registry.descriptors());
    EngineOptions {
        model_ladders: profile.ladders(),
        single_family_pool: profile.is_single_family(),
        ..options_from_config(config)
    }
}

/// Run (or resume) the plan currently in the blackboard, streaming progress.
pub async fn run_plan(blackboard: &Blackboard) -> Result<RunReport> {
    run_plan_with(blackboard, None).await
}

/// Like [`run_plan`], but applies a human decision to a parked node first
/// (`CONTEXT.md` §11).
pub async fn run_plan_with(
    blackboard: &Blackboard,
    resume: Option<rinne_core::ResumeInput>,
) -> Result<RunReport> {
    let config = rinne_config::load_cwd()?;
    let (registry, names) = build_registry(&config).await?;
    if registry.is_empty() {
        return Err(anyhow!(
            "no available workers — run `rinne doctor` (need an enabled, installed harness)"
        ));
    }
    println!("workers: {}", names.join(", "));

    let plan = rinne_core::engine::require_plan(blackboard)?;
    println!("goal: {}\n", plan.goal);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<EngineEvent>();
    let printer = tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            print_event(ev);
        }
    });

    // Ctrl-C cancels the run cleanly; state persists for `rinne resume`.
    let cancel = CancellationToken::new();
    let cancel_handle = cancel.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        cancel_handle.cancel();
    });

    let mut engine = Engine::new(blackboard, plan, &registry, options_with_pool(&config, &registry));
    // Attach the conductor as the replanner so the loop can amend the DAG
    // (best-effort: if no backend is available, replan paths simply block).
    if let Ok(conductor) =
        build_conductor(&config, &registry, blackboard.workspace().to_path_buf())
    {
        engine = engine.with_replanner(std::sync::Arc::new(conductor));
    }
    let report = engine.run(cancel, Some(tx), resume).await?;
    let _ = printer.await;

    print_report(&report);
    Ok(report)
}

fn print_event(ev: EngineEvent) {
    match ev {
        EngineEvent::Narration(line) => println!("conductor: {line}"),
        EngineEvent::NodeStarted { id, worker } => println!("▶ {id} → {worker}"),
        EngineEvent::NodeStream { id, event } => {
            use rinne_core::worker::WorkerEvent::*;
            match event {
                Message(m) | Reading(m) | Editing(m) | ToolUse(m) => {
                    println!("   {id}  {m}")
                }
                Raw(_) | Done => {}
            }
        }
        EngineEvent::NodeFinished { id, status } => {
            let mark = if status == rinne_core::NodeStatus::Succeeded {
                "✔"
            } else {
                "✗"
            };
            println!("{mark} {id} {}", status.label());
        }
        EngineEvent::Parked { id, question } => {
            println!("\n⏸ parked at {id}");
            println!("   {question}");
        }
    }
}

fn print_report(report: &RunReport) {
    println!("\n── run summary ──");
    for (id, status) in &report.node_statuses {
        println!("  {id:<6} {}", status.label());
    }
    println!(
        "stop: {:?} · {} iterations · {} tokens · {} ms",
        report.stop_reason,
        report.total_iterations,
        report.total_usage.total_tokens(),
        report.total_usage.wall_ms
    );
    if report.completed {
        println!("✔ completed");
    } else {
        println!("✗ not complete — `rinne resume` to continue");
    }
}
