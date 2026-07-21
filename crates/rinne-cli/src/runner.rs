//! Wiring between the CLI and the loop engine (`PHASE.md` P3).
//!
//! Builds a [`WorkerRegistry`] from the real adapters that `doctor` reports as
//! available, then runs (or resumes) a plan through the engine, streaming
//! engine events to the terminal. This is the pre-TUI, plain-output path; the
//! four-region TUI lands in Phase 6.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use tokio_util::sync::CancellationToken;

use rinne_conductor::{
    load_user_exemplars, resolve_openai, Conductor, ConductorInput, HarnessBackend, PlanBackend,
};
// EventSink used when wiring harness conductor → Stage.
use rinne_config::model::{ConductorBackend, ConductorConfig, HarnessApprovals, PreferFamily};
use rinne_config::probe::WorkerFamily;
use rinne_config::Config;
use rinne_core::worker::Capability;
use rinne_core::{
    Blackboard, Engine, EngineEvent, EngineOptions, HumanSession, RunReport, WorkerRegistry,
};
use rinne_workers::adapters::{
    aider, antigravity, claude_code, codex, cursor, discover_cli_models, grok, opencode,
    HarnessAdapter, OpenAiWorker,
};

/// Build a worker registry from configured + available harness adapters.
///
/// Workers are added in the user's family preference order so the registry's
/// insertion order encodes preference for tie-breaking (`CONTEXT.md` §13).
/// Returns the registry plus the names registered, for narration.
pub async fn build_registry(config: &Config) -> Result<(WorkerRegistry, Vec<String>)> {
    build_registry_inner(config, None).await
}

/// Like [`build_registry`], but wires an MCP tool executor into the API workers
/// so nodes that attach `tools` get the host agentic loop (`MCP_SKILLS.md` §6).
/// The run paths use this; read-only paths (listing workers/models) use the
/// plain [`build_registry`].
pub async fn build_registry_with_tools(
    config: &Config,
    executor: Option<Arc<dyn rinne_core::ToolExecutor>>,
) -> Result<(WorkerRegistry, Vec<String>)> {
    build_registry_inner(config, executor).await
}

async fn build_registry_inner(
    config: &Config,
    executor: Option<Arc<dyn rinne_core::ToolExecutor>>,
) -> Result<(WorkerRegistry, Vec<String>)> {
    let report = rinne_config::doctor(config, false).await?;

    let mut reg = WorkerRegistry::new();
    for w in report
        .workers
        .iter()
        .filter(|w| w.family == WorkerFamily::Harness && w.enabled && w.status.is_available())
    {
        let adapter = match w.name.as_str() {
            "claude-code" => Some(claude_code::worker()),
            "codex" => Some(codex::worker()),
            "opencode" => Some(opencode::worker()),
            "grok" => Some(grok::worker()),
            "cursor-agent" => Some(cursor::worker()),
            "aider" => Some(aider::worker()),
            "antigravity" => Some(antigravity::worker()),
            _ => None,
        };
        if let Some(a) = adapter {
            // Live-refresh the model ladder from the CLI (`grok models`, etc.)
            // so retired ids never get scheduled; merge config pins.
            let a = refresh_harness_models(a, config).await;
            reg.register(Arc::new(a));
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
        let mut worker = OpenAiWorker::new(
            name,
            &base,
            keys,
            models,
            api_capabilities(),
            provider.extra_body.clone(),
        );
        if let Some(ex) = &executor {
            worker = worker.with_tool_executor(ex.clone());
        }
        reg.register(Arc::new(worker));
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

/// Replace a harness adapter's static model ladder with whatever the live CLI
/// reports (`<cli> models`), then ensure any `[models].by_worker` pin is on the
/// ladder. On probe failure the static fallback stays — never invent ids.
async fn refresh_harness_models(adapter: HarnessAdapter, config: &Config) -> HarnessAdapter {
    let name = adapter.descriptor.name.clone();
    let program = adapter.program.clone();
    let mut adapter = adapter;

    match discover_cli_models(&program).await {
        Some(disc) if !disc.ladder.is_empty() => {
            tracing::info!(
                worker = %name,
                models = ?disc.ladder,
                default = ?disc.default,
                "live harness model ladder from `{program} models`"
            );
            adapter = adapter.with_models(disc.ladder);
        }
        _ => {
            tracing::debug!(
                worker = %name,
                "no live model listing from `{program} models` — keeping static ladder {:?}",
                adapter.descriptor.models
            );
        }
    }

    // User config wins: ensure every configured pin for this worker is schedulable.
    if let Some(m) = config.models.by_worker.get(&name) {
        adapter = adapter.ensure_model(m);
    }
    // Preferences.models is role→model (not worker→model); still merge worker-shaped
    // keys if someone wrote `preferences.models.grok = "…"`.
    if let Some(m) = config.preferences.models.get(&name) {
        adapter = adapter.ensure_model(m);
    }

    adapter
}

fn parse_conductor_backend(s: &str) -> Option<ConductorBackend> {
    ConductorBackend::parse(s)
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
///
/// `planner_events`: optional sink so harness-based planning opens Stage panes
/// and streams tool/message events into the TUI (node id `conductor`).
pub fn build_conductor(
    config: &Config,
    registry: &WorkerRegistry,
    workspace: std::path::PathBuf,
) -> Result<Conductor> {
    build_conductor_with_events(config, registry, workspace, None)
}

/// Like [`build_conductor`], but forwards harness planner events to `events`
/// (wrapped as `EngineEvent::NodeStream` with id `conductor` by the caller).
pub fn build_conductor_with_events(
    config: &Config,
    registry: &WorkerRegistry,
    workspace: std::path::PathBuf,
    planner_events: Option<rinne_core::worker::EventSink>,
) -> Result<Conductor> {
    let mut backends: Vec<Box<dyn PlanBackend>> = Vec::new();

    // The API planner is invoked from `Conductor::run_once` via `resolve_openai_model`
    // (supports per-rung model switching). Harness workers are fallbacks only.
    if resolve_openai(&config.conductor)?.is_none() && registry.is_empty() {
        return Err(anyhow!(
            "no conductor API key and no harness workers — configure [conductor] or install a harness"
        ));
    }

    // Planners in preference order. Harness order respects
    // `preferences.roles.conductor` / generator pins so "use grok only" does not
    // always hit claude-code first. Auth failures fall through quickly.
    use rinne_core::worker::WorkerFamily as Fam;
    let mut harnesses = registry.by_family(Fam::Harness);
    harnesses = order_harnesses_for_conductor(config, harnesses);
    let mut fallbacks = harnesses;
    // When the user pinned a harness conductor (or backend = harness), do not
    // put API workers in the same chain as "silent" planners ahead of grok —
    // API is still tried first inside run_once when backend has a key.
    if config.conductor.backend != ConductorBackend::Harness
        && preferred_conductor_harness(config).is_none()
    {
        fallbacks.extend(registry.by_family(Fam::Api));
    }
    for worker in fallbacks {
        let is_harness = worker.descriptor().family == Fam::Harness;
        let mut hb = HarnessBackend::new(worker, workspace.clone());
        if is_harness {
            if let Some(ref sink) = planner_events {
                hb = hb.with_events(sink.clone());
            }
        }
        backends.push(Box::new(hb));
    }

    Ok(Conductor::new(backends)
        .map_err(|e| anyhow!("{e}"))?
        .with_conductor_config(config.conductor.clone()))
}

/// Parse `preferences.roles.conductor` / `generator` (and models keys) into a
/// bare worker name (`grok`, `claude-code`, …).
fn preferred_conductor_harness(config: &Config) -> Option<String> {
    let candidates = [
        config.preferences.roles.get("conductor"),
        config.preferences.roles.get("planner"),
        // Soft: if the user pinned generator to one harness, prefer it for planning too.
        config.preferences.roles.get("generator"),
    ];
    for raw in candidates.into_iter().flatten() {
        let name = strip_prefer_prefix(raw);
        if matches!(
            name.as_str(),
            "claude-code"
                | "codex"
                | "opencode"
                | "grok"
                | "cursor-agent"
                | "aider"
                | "antigravity"
        ) {
            return Some(name);
        }
    }
    None
}

fn strip_prefer_prefix(s: &str) -> String {
    let s = s.trim();
    s.strip_prefix("harness:")
        .or_else(|| s.strip_prefix("api:"))
        .unwrap_or(s)
        .to_string()
}

/// Put the user's preferred harness first; keep relative order of the rest.
fn order_harnesses_for_conductor(
    config: &Config,
    mut harnesses: Vec<std::sync::Arc<dyn rinne_core::Worker>>,
) -> Vec<std::sync::Arc<dyn rinne_core::Worker>> {
    let Some(want) = preferred_conductor_harness(config) else {
        return harnesses;
    };
    if let Some(i) = harnesses.iter().position(|w| w.descriptor().name == want) {
        let preferred = harnesses.remove(i);
        harnesses.insert(0, preferred);
        tracing::info!(
            preferred = %want,
            order = ?harnesses.iter().map(|w| w.descriptor().name.as_str()).collect::<Vec<_>>(),
            "conductor harness order"
        );
    }
    harnesses
}

/// One-shot headless run that returns a structured JSON result instead of
/// streaming human-readable progress (`CONTEXT.md` §6). Quiet: no narration to
/// stdout, so the only output is the JSON the caller prints.
pub async fn oneshot_json(goal: &str, no_graph: bool) -> Result<serde_json::Value> {
    let config = rinne_config::load_cwd()?;
    apply_harness_stage_env(&config, false);
    let cwd = std::env::current_dir()?;
    let bb = Blackboard::open_with(&cwd, !no_graph)?;
    let (executor, tool_specs, mcp_servers) = host_setup(&config).await;
    let (registry, _) = build_registry_with_tools(&config, executor).await?;
    if registry.is_empty() {
        return Err(anyhow!("no available workers — run `rinne doctor`"));
    }
    let catalog = crate::catalog::gather(&config, &cwd).await;
    let template = plan_template(&config, &registry, catalog, &cwd);
    let conductor = std::sync::Arc::new(
        build_conductor(&config, &registry, cwd.clone())?.with_context(template.clone()),
    );

    let input = ConductorInput {
        goal: goal.to_string(),
        ..template
    };
    let plan = conductor.plan(&input).await?;
    bb.save_plan(&plan)?;
    bb.reset_run()?;

    let session = HumanSession::load(bb.root());
    let mut opts = options_with_pool(&config, &registry, &cwd);
    apply_human_session(&session, &mut opts);
    opts.tool_specs = tool_specs;
    opts.mcp_servers = mcp_servers;
    let mut engine = Engine::new(&bb, plan.clone(), &registry, opts);
    engine = engine.with_replanner(conductor);
    // Ctrl-C cancels the headless run cleanly (state persists for `rinne resume`)
    // instead of leaving it unkillable short of SIGKILL.
    let cancel = CancellationToken::new();
    let cancel_handle = cancel.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        cancel_handle.cancel();
    });
    // None sink → no streaming output; just run to completion.
    let report = engine.run(cancel, None, None).await?;

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
        NeedsHuman {
            node,
            question,
            gate,
        } => {
            let detail = if let Some(g) = gate {
                format!("{node} (gate {g}): {question}")
            } else {
                format!("{node}: {question}")
            };
            ("needs_human", Some(detail))
        }
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

    let catalog = crate::catalog::gather(&config, blackboard.workspace()).await;
    let template = plan_template(&config, &registry, catalog, blackboard.workspace());
    let session = HumanSession::load(blackboard.root());
    let cond_cfg = conductor_config_with_session(&config, &session);
    let conductor = build_conductor(
        &Config {
            conductor: cond_cfg,
            ..config.clone()
        },
        &registry,
        blackboard.workspace().to_path_buf(),
    )?
    .with_context(template.clone());
    let structure: Vec<rinne_types::graph::Neighborhood> =
        if let Some(g) = rinne_types::Blackboard::code_graph(blackboard) {
            let known = g.symbol_names();
            let picked = rinne_loop::assembler::resolve_symbols(g, goal, &[], &known);
            picked
                .iter()
                .filter_map(|name| g.neighborhood(name))
                .collect()
        } else {
            Vec::new()
        };
    let input = ConductorInput {
        goal: goal.to_string(),
        structure,
        ..template
    };

    println!("planning with: {}", conductor.backend_names().join(" → "));
    let plan = conductor.plan(&input).await?;
    blackboard.save_plan(&plan)?;
    // A fresh goal is a fresh run: clear any stale state (node statuses, the run
    // clock) so a leftover `.rinne/` does not trip budgets or skip nodes.
    blackboard.reset_run()?;

    println!(
        "\nplan ({} node{}):",
        plan.nodes.len(),
        if plan.nodes.len() == 1 { "" } else { "s" }
    );
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

/// The reusable planning context (everything but the per-call goal/mentioned):
/// the worker pool, the tool/skill catalog, the family preference, and budgets.
/// Captured on the conductor via `with_context` so replans stay pool- and
/// catalog-aware, and spread into each `plan()` call's input.
pub fn plan_template(
    config: &Config,
    registry: &WorkerRegistry,
    catalog: crate::catalog::Catalog,
    workspace: &std::path::Path,
) -> ConductorInput {
    let exemplars_path = config
        .routing
        .exemplars_file
        .as_ref()
        .map(|p| workspace.join(p))
        .unwrap_or_else(|| workspace.join(".rinne/routing-exemplars.toml"));
    ConductorInput {
        workers: registry.descriptors(),
        tools: catalog.tools,
        skills: catalog.skills,
        prefer: Some(prefer_label(config.preferences.prefer).to_string()),
        role_prefers: config.preferences.roles.clone().into_iter().collect(),
        budget_minutes: Some(config.loop_.global_budget_minutes as u64),
        max_iterations_per_node: config.loop_.max_iterations_per_node,
        workspace: Some(workspace.to_path_buf()),
        routing: config.routing.clone(),
        user_exemplars: load_user_exemplars(&exemplars_path),
        ..Default::default()
    }
}

/// Merge an active human session over config-derived engine options.
pub fn apply_human_session(session: &HumanSession, opts: &mut EngineOptions) {
    if !session.active {
        return;
    }
    if let Some(w) = &session.pins.generator_worker {
        opts.role_prefers.insert("generator".into(), w.clone());
    }
    if let Some(m) = &session.pins.generator_model {
        opts.role_models.insert("generator".into(), m.clone());
    }
    if let Some(ev) = &session.pins.evaluator {
        apply_evaluator_pin(ev, opts);
    }
    opts.gates = session.active_gates().to_vec();
}

/// Parse a `/human evaluator` pin into engine overrides.
///
/// Formats written by [`commands::human`]:
/// - `tool` / `human` — force that evaluator kind
/// - `ai` — force AI evaluator (worker from pool default)
/// - `ai:<worker>` — AI evaluator on that worker
/// - `ai:<worker>:<model>` — AI evaluator with model pin
/// - bare worker name — soft-compat with `preferences.roles.evaluator`
fn apply_evaluator_pin(pin: &str, opts: &mut EngineOptions) {
    use rinne_core::dag::EvaluatorKind;

    let pin = pin.trim();
    if pin.eq_ignore_ascii_case("tool") {
        opts.evaluator_kind_override = Some(EvaluatorKind::Tool);
        return;
    }
    if pin.eq_ignore_ascii_case("human") {
        opts.evaluator_kind_override = Some(EvaluatorKind::Human);
        return;
    }
    if pin.eq_ignore_ascii_case("ai") {
        opts.evaluator_kind_override = Some(EvaluatorKind::Ai);
        return;
    }
    if let Some(rest) = pin.strip_prefix("ai:").or_else(|| pin.strip_prefix("AI:")) {
        opts.evaluator_kind_override = Some(EvaluatorKind::Ai);
        // `worker` or `worker:model` (model ids rarely contain `:`; first segment
        // is the worker name, remainder is the model if present).
        if rest.is_empty() {
            return;
        }
        match rest.split_once(':') {
            Some((worker, model)) if !worker.is_empty() => {
                opts.role_prefers
                    .insert("evaluator".into(), worker.to_string());
                if !model.is_empty() {
                    opts.role_models
                        .insert("evaluator".into(), model.to_string());
                }
            }
            _ => {
                opts.role_prefers
                    .insert("evaluator".into(), rest.to_string());
            }
        }
        return;
    }
    // Bare worker name (config-style role pin).
    opts.role_prefers
        .insert("evaluator".into(), pin.to_string());
}

/// Apply conductor pins from a human session onto a config clone.
pub fn conductor_config_with_session(config: &Config, session: &HumanSession) -> ConductorConfig {
    let mut c = config.conductor.clone();
    if !session.active {
        return c;
    }
    if session.pins.conductor_model.is_some() || session.pins.conductor_backend.is_some() {
        c.auto_escalate = false;
    }
    if let Some(m) = &session.pins.conductor_model {
        c.model = m.clone();
    }
    if let Some(b) = &session.pins.conductor_backend {
        c.backend = parse_conductor_backend(b).unwrap_or(c.backend);
    }
    c
}

/// Apply `[harness_stage]` to the process env so the engine/workers can open
/// visible PTY sessions when appropriate (`plan.md`).
///
/// `interactive` is true for the TUI / GUI and false for `rinne -p` / scripts.
pub fn apply_harness_stage_env(config: &Config, interactive: bool) {
    let mode = config.harness_stage.mode;
    let visible = mode.wants_visible(interactive);
    if visible {
        std::env::set_var("RINNE_HARNESS_STAGE_VISIBLE", "1");
    } else {
        std::env::remove_var("RINNE_HARNESS_STAGE_VISIBLE");
    }
    std::env::set_var("RINNE_HARNESS_STAGE_MODE", mode.as_str());
    std::env::set_var(
        "RINNE_HARNESS_STAGE_MAX",
        config.harness_stage.max_sessions.to_string(),
    );
    // Read by the adapters' interactive argv builders — without this the
    // approvals setting is inert and every visible session stops on its
    // harness's permission prompt.
    let approvals = match config.harness_stage.approvals {
        HarnessApprovals::Auto => "auto",
        HarnessApprovals::Human => "human",
    };
    std::env::set_var("RINNE_HARNESS_APPROVALS", approvals);
    tracing::info!(
        mode = mode.as_str(),
        visible,
        interactive,
        approvals,
        max = config.harness_stage.max_sessions,
        "harness stage"
    );
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
        role_prefers: config.preferences.roles.clone().into_iter().collect(),
        worker_models: config.models.by_worker.clone().into_iter().collect(),
        ..Default::default()
    }
}

/// Engine options merged with the live pool profile (tier ladders for the
/// cascade and the single-family flag), plus the installed skill bodies the
/// engine injects into nodes that attach a skill (`MCP_SKILLS.md` §11).
pub fn options_with_pool(
    config: &Config,
    registry: &WorkerRegistry,
    cwd: &std::path::Path,
) -> EngineOptions {
    let profile = rinne_core::pool::profile(&registry.descriptors());
    EngineOptions {
        model_ladders: profile.ladders(),
        single_family_pool: profile.is_single_family(),
        skill_bodies: skill_bodies(cwd),
        ..options_from_config(config)
    }
}

/// Installed skill bodies (name → instruction body) for the engine to inject.
pub fn skill_bodies(cwd: &std::path::Path) -> std::collections::HashMap<String, String> {
    rinne_config::skills::discover(cwd)
        .into_iter()
        .map(|s| (s.name, s.body))
        .collect()
}

/// The MCP wiring for a run, covering both paths (`MCP_SKILLS.md` §6):
/// - the tool executor + id→spec catalog for the **host path** (API workers),
/// - the server-name→spec map for the **provision path** (harnesses).
///
/// The host path needs live connections (a warm pool, opened once and shared by
/// the executor and the spec scan); the provision path needs only static
/// connection config plus resolved tokens, so it is built straight from config
/// even when no server is reachable.
pub async fn host_setup(
    config: &Config,
) -> (
    Option<Arc<dyn rinne_core::ToolExecutor>>,
    std::collections::HashMap<String, rinne_core::ToolSpec>,
    std::collections::HashMap<String, rinne_core::McpServerSpec>,
) {
    // Provision-path specs: every enabled server, with its token resolved into
    // memory (never to disk — the provisioner references it via env expansion).
    // OAuth tokens are refreshed here if expired, so the resolve is async.
    let mut mcp_servers: std::collections::HashMap<String, rinne_core::McpServerSpec> =
        std::collections::HashMap::new();
    for (name, s) in config.mcp.servers.iter().filter(|(_, s)| s.enabled) {
        mcp_servers.insert(name.clone(), server_spec(name, s).await);
    }

    let pool = Arc::new(crate::mcp_pool::McpPool::from_config(config));
    if pool.is_empty() {
        return (None, std::collections::HashMap::new(), mcp_servers);
    }
    let tool_specs = pool
        .list_all_tools()
        .await
        .into_iter()
        .map(|(id, t)| {
            (
                id.clone(),
                rinne_core::ToolSpec {
                    id,
                    description: t.description,
                    schema: t.input_schema,
                },
            )
        })
        .collect();
    let executor: Arc<dyn rinne_core::ToolExecutor> =
        Arc::new(crate::mcp_pool::McpToolExecutor::new(pool));
    (Some(executor), tool_specs, mcp_servers)
}

/// Map a configured MCP server to the engine's connection spec, resolving its
/// token (keychain/env, or a refreshed OAuth access token) into memory.
async fn server_spec(name: &str, s: &rinne_config::model::McpServer) -> rinne_core::McpServerSpec {
    use rinne_config::model::McpTransport;
    let token = crate::commands::mcp::resolve_token(name, s).await;
    rinne_core::McpServerSpec {
        name: name.to_string(),
        transport: match s.transport {
            McpTransport::Stdio => rinne_core::McpTransportKind::Stdio,
            McpTransport::Http => rinne_core::McpTransportKind::Http,
        },
        command: s.command.clone(),
        args: s.args.clone(),
        env: s.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        url: s.url.clone(),
        headers: s
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        token_env: s.key_env.clone(),
        token,
        auth: s.auth.clone(),
        auth_header: s.auth_header.clone(),
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
    // Headless CLI paths (`rinne -p`, `rinne run`) stay non-interactive → hybrid
    // Stage mode does not open PTYs (CI-safe). Override with mode = "visible".
    apply_harness_stage_env(&config, false);
    let (executor, tool_specs, mcp_servers) = host_setup(&config).await;
    let (registry, names) = build_registry_with_tools(&config, executor).await?;
    if registry.is_empty() {
        return Err(anyhow!(
            "no available workers — run `rinne doctor` (need an enabled, installed harness)"
        ));
    }
    println!("workers: {}", names.join(", "));

    let plan = rinne_core::require_plan(blackboard)?;
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

    let session = HumanSession::load(blackboard.root());
    let mut opts = options_with_pool(&config, &registry, blackboard.workspace());
    apply_human_session(&session, &mut opts);
    opts.tool_specs = tool_specs;
    opts.mcp_servers = mcp_servers;
    let mut engine = Engine::new(blackboard, plan, &registry, opts);
    // Attach the conductor as the replanner so the loop can amend the DAG
    // (best-effort: if no backend is available, replan paths simply block).
    let cond_cfg = conductor_config_with_session(&config, &session);
    if let Ok(conductor) = build_conductor(
        &Config {
            conductor: cond_cfg,
            ..config.clone()
        },
        &registry,
        blackboard.workspace().to_path_buf(),
    ) {
        engine = engine.with_replanner(std::sync::Arc::new(conductor));
    }
    let report = engine.run(cancel, Some(tx), resume).await?;
    let _ = printer.await;

    print_report(&report);
    Ok(report)
}

/// When `RINNE_STREAM_JSON=1` (or `true`/`yes`), emit machine-readable JSONL
/// engine events so native UIs (macOS app, etc.) can render live Token/Thinking
/// streams with correct kinds. Default remains human-readable terminal output.
fn stream_json_enabled() -> bool {
    match std::env::var("RINNE_STREAM_JSON") {
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
}

fn print_event(ev: EngineEvent) {
    if stream_json_enabled() {
        print_event_json(ev);
        return;
    }
    match ev {
        EngineEvent::Narration(line) => println!("conductor: {line}"),
        EngineEvent::NodeStarted { id, worker } => println!("▶ {id} → {worker}"),
        EngineEvent::NodeStream { id, event } => {
            use rinne_core::worker::WorkerEvent::*;
            match event {
                Token(t) | Thinking(t) => {
                    use std::io::Write;
                    print!("{t}");
                    let _ = std::io::stdout().flush();
                }
                Message(m) | Reading(m) | Editing(m) | ToolUse(m) => {
                    println!("   {id}  {m}")
                }
                SessionOpened {
                    worker,
                    model,
                    backend,
                } => {
                    let m = model
                        .as_deref()
                        .map(|m| format!(":{m}"))
                        .unwrap_or_default();
                    println!("   {id}  stage open {worker}{m} [{backend}]")
                }
                Raw(_) | Done => {}
            }
        }
        EngineEvent::NodeFinished { id, status, tokens } => {
            let mark = if status == rinne_core::NodeStatus::Succeeded {
                "✔"
            } else {
                "✗"
            };
            if tokens > 0 {
                println!(
                    "{mark} {id} {} · {} tok",
                    status.label(),
                    rinne_core::worker::format_token_count(tokens)
                );
            } else {
                println!("{mark} {id} {}", status.label());
            }
        }
        EngineEvent::Parked { id, question } => {
            println!("\n⏸ parked at {id}");
            println!("   {question}");
        }
    }
}

/// One JSON object per line, flushed immediately. Token/Thinking deltas keep
/// their kind so harness UIs do not need to scrape untagged `print!` bytes.
fn print_event_json(ev: EngineEvent) {
    use std::io::Write;
    let line = match ev {
        EngineEvent::Narration(text) => serde_json::json!({
            "type": "narration",
            "text": text,
        }),
        EngineEvent::NodeStarted { id, worker } => serde_json::json!({
            "type": "node_start",
            "node": id,
            "worker": worker,
        }),
        EngineEvent::NodeStream { id, event } => {
            use rinne_core::worker::WorkerEvent::*;
            match event {
                Token(t) => serde_json::json!({
                    "type": "token",
                    "node": id,
                    "text": t,
                }),
                Thinking(t) => serde_json::json!({
                    "type": "thinking",
                    "node": id,
                    "text": t,
                }),
                Message(m) => serde_json::json!({
                    "type": "message",
                    "node": id,
                    "text": m,
                }),
                Reading(m) => serde_json::json!({
                    "type": "reading",
                    "node": id,
                    "text": m,
                }),
                Editing(m) => serde_json::json!({
                    "type": "editing",
                    "node": id,
                    "text": m,
                }),
                ToolUse(m) => serde_json::json!({
                    "type": "tool_use",
                    "node": id,
                    "text": m,
                }),
                Raw(m) => serde_json::json!({
                    "type": "raw",
                    "node": id,
                    "text": m,
                }),
                SessionOpened {
                    worker,
                    model,
                    backend,
                } => serde_json::json!({
                    "type": "session_opened",
                    "node": id,
                    "worker": worker,
                    "model": model,
                    "backend": backend,
                }),
                Done => serde_json::json!({
                    "type": "done",
                    "node": id,
                }),
            }
        }
        EngineEvent::NodeFinished { id, status, tokens } => serde_json::json!({
            "type": "node_finish",
            "node": id,
            "status": status.label(),
            "tokens": tokens,
        }),
        EngineEvent::Parked { id, question } => serde_json::json!({
            "type": "parked",
            "node": id,
            "question": question,
        }),
    };
    println!("{line}");
    let _ = std::io::stdout().flush();
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
        println!("✗ not complete — `rinne --continue` or `rinne resume` to continue");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rinne_core::dag::EvaluatorKind;
    use rinne_core::HumanSession;

    #[test]
    fn evaluator_pin_tool_and_human_set_kind_override() {
        let mut opts = EngineOptions::default();
        let mut session = HumanSession {
            active: true,
            ..Default::default()
        };
        session.pins.evaluator = Some("tool".into());
        apply_human_session(&session, &mut opts);
        assert_eq!(opts.evaluator_kind_override, Some(EvaluatorKind::Tool));

        opts = EngineOptions::default();
        session.pins.evaluator = Some("human".into());
        apply_human_session(&session, &mut opts);
        assert_eq!(opts.evaluator_kind_override, Some(EvaluatorKind::Human));
    }

    #[test]
    fn evaluator_pin_ai_worker_model_splits_correctly() {
        let mut opts = EngineOptions::default();
        let mut session = HumanSession {
            active: true,
            ..Default::default()
        };
        session.pins.evaluator = Some("ai:openrouter:gpt-4o".into());
        apply_human_session(&session, &mut opts);
        assert_eq!(opts.evaluator_kind_override, Some(EvaluatorKind::Ai));
        assert_eq!(
            opts.role_prefers.get("evaluator").map(String::as_str),
            Some("openrouter")
        );
        assert_eq!(
            opts.role_models.get("evaluator").map(String::as_str),
            Some("gpt-4o")
        );
    }

    #[test]
    fn inactive_session_is_a_no_op() {
        let mut opts = EngineOptions::default();
        let session = HumanSession {
            active: false,
            pins: rinne_core::RolePins {
                generator_worker: Some("claude-code".into()),
                evaluator: Some("human".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        apply_human_session(&session, &mut opts);
        assert!(opts.role_prefers.is_empty());
        assert!(opts.evaluator_kind_override.is_none());
    }
}
