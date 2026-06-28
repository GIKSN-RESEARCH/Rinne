//! The loop engine: the run lifecycle with verification (`CONTEXT.md` §12).
//!
//! Phase 3 built the serial spine; Phase 5 adds the loop control that makes
//! Rinne a verifying loop rather than a dispatcher: the evaluator gate (tool /
//! AI / human), the test ratchet, `on_fail` loop-back with critique artifacts,
//! the stuck-detector, human parking, and the replanner hook.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::assembler::ContextAssembler;
use crate::dag::{Checkpoint, EvaluatorKind, Node, OnFail, Plan};
use crate::evaluator::{self, AiEvaluator, HumanEvaluator, ToolEvaluator};
use crate::ratchet;
use crate::registry::WorkerRegistry;
use crate::worker::{
    Constraints, EventSink, ExecStatus, ExecuteRequest, Usage, WorkerDescriptor, WorkerEvent,
    WorkerFamily,
};
use crate::Result;
use rinne_types::{Blackboard, EvalContext, Evaluator, Gate, NodeStatus, Replanner};

/// Tunable limits for a run, merged from config and the plan's own budget.
#[derive(Debug, Clone)]
pub struct EngineOptions {
    pub max_iterations_per_node: u32,
    pub global_budget_minutes: Option<u64>,
    pub max_total_iterations: Option<u32>,
    /// Identical-failure loops before escalating to a human (`CONTEXT.md` §11).
    pub stuck_loop_threshold: u32,
    /// Apply the test ratchet at evaluator gates by default.
    pub test_ratchet: bool,
    /// Per-role model defaults (role name → model), from config.
    pub role_models: HashMap<String, String>,
    /// Per-worker model defaults (worker name → model), from config.
    pub worker_models: HashMap<String, String>,
    /// Per-worker cascade ladders (worker name → models cheap→strong), used to
    /// escalate a node's model on evaluator failure (`CONTEXT.md` §7).
    pub model_ladders: HashMap<String, Vec<String>>,
    /// Whether the pool is single-family; if so, AI evaluators are same-family
    /// and the engine narrates that independence is limited.
    pub single_family_pool: bool,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            max_iterations_per_node: 8,
            global_budget_minutes: Some(120),
            max_total_iterations: None,
            stuck_loop_threshold: 3,
            test_ratchet: true,
            role_models: HashMap::new(),
            worker_models: HashMap::new(),
            model_ladders: HashMap::new(),
            single_family_pool: false,
        }
    }
}

/// Why a run stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    Completed,
    Blocked,
    BudgetMinutes,
    BudgetIterations,
    NoCapableWorker(String),
    Cancelled,
    /// A node is parked awaiting the user (checkpoint, human evaluator, or a
    /// stuck escalation). Resume with [`ResumeInput`] (`CONTEXT.md` §11).
    NeedsHuman { node: String, question: String },
}

/// A user's response when resuming a parked run (`CONTEXT.md` §11).
#[derive(Debug, Clone)]
pub struct ResumeInput {
    /// The parked node this addresses; defaults to the sole parked node.
    pub node: Option<String>,
    pub decision: HumanDecision,
}

/// A human's decision at a checkpoint / evaluator (`CONTEXT.md` §11).
#[derive(Debug, Clone)]
pub enum HumanDecision {
    /// Accept the current state and move on.
    Approve,
    /// Throw out this approach; replan from scratch.
    Reject,
    /// Give the missing guidance; it becomes the critique fed back to the loop.
    Steer(String),
}

/// The outcome of a run.
#[derive(Debug, Clone)]
pub struct RunReport {
    pub completed: bool,
    pub stop_reason: StopReason,
    pub node_statuses: Vec<(String, NodeStatus)>,
    pub total_usage: Usage,
    pub total_iterations: u32,
}

/// Engine events for the interface / `status` to render.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    NodeStarted { id: String, worker: String },
    NodeStream { id: String, event: WorkerEvent },
    NodeFinished { id: String, status: NodeStatus },
    Narration(String),
    /// A node parked awaiting the user, with the sharp question to answer.
    Parked { id: String, question: String },
}

pub type EngineSink = tokio::sync::mpsc::UnboundedSender<EngineEvent>;

/// In-memory loop bookkeeping for a single `run` call.
#[derive(Default)]
struct LoopTracker {
    /// Pending critique to inject into a node's next assembly (loop-back).
    critiques: HashMap<String, String>,
    /// Per-evaluator history of failure signatures, for stuck detection.
    failures: HashMap<String, Vec<String>>,
    /// Per-node escalated model: on evaluator failure the loop-back runs the
    /// generator on the next-stronger model rather than re-running the same one
    /// (the start-cheap-escalate cascade, `CONTEXT.md` §7).
    escalated_models: HashMap<String, String>,
}

/// The loop engine over a blackboard, owned plan, and worker registry.
pub struct Engine<'a> {
    blackboard: &'a dyn Blackboard,
    plan: Plan,
    registry: &'a WorkerRegistry,
    options: EngineOptions,
    replanner: Option<Arc<dyn Replanner>>,
}

impl<'a> Engine<'a> {
    pub fn new(
        blackboard: &'a dyn Blackboard,
        plan: Plan,
        registry: &'a WorkerRegistry,
        options: EngineOptions,
    ) -> Self {
        Self {
            blackboard,
            plan,
            registry,
            options,
            replanner: None,
        }
    }

    /// Attach a replanner (the conductor) so the engine can amend the DAG.
    pub fn with_replanner(mut self, replanner: Arc<dyn Replanner>) -> Self {
        self.replanner = Some(replanner);
        self
    }

    /// Run (or resume) the plan to completion, a stop condition, or a park.
    pub async fn run(
        &mut self,
        cancel: CancellationToken,
        sink: Option<EngineSink>,
        resume: Option<ResumeInput>,
    ) -> Result<RunReport> {
        // The engine reaches persistence only through the blackboard seam (it
        // never names the concrete `State`), so it can live behind the trait.
        let state = self.blackboard;
        for node in &self.plan.nodes {
            state.ensure_node(&node.id)?;
        }
        if state.meta("started_at")?.is_none() {
            state.set_meta("started_at", &now_secs().to_string())?;
        }
        let started_at: u64 = state
            .meta("started_at")?
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(now_secs);

        let mut tracker = LoopTracker::default();

        // Apply any resume input to a parked node before scheduling.
        if let Some(input) = resume {
            if let Some(stop) = self.apply_resume(state, &mut tracker, input).await? {
                return self.finish(state, stop);
            }
        }

        let effective_minutes = self.plan.budget.minutes.or(self.options.global_budget_minutes);
        let effective_max_iters = self
            .plan
            .budget
            .max_total_iterations
            .or(self.options.max_total_iterations);

        let stop_reason = loop {
            if cancel.is_cancelled() {
                break StopReason::Cancelled;
            }
            if let Some(mins) = effective_minutes {
                if now_secs().saturating_sub(started_at) >= mins * 60 {
                    break StopReason::BudgetMinutes;
                }
            }
            if let Some(max) = effective_max_iters {
                if state.total_iterations()? >= max {
                    break StopReason::BudgetIterations;
                }
            }

            let ready = self.ready_nodes(state)?;
            if ready.is_empty() {
                let all_ok = self
                    .plan
                    .nodes
                    .iter()
                    .all(|n| matches!(state.status(&n.id), Ok(NodeStatus::Succeeded)));
                break if all_ok {
                    StopReason::Completed
                } else {
                    StopReason::Blocked
                };
            }

            // Parallelize independent read-only evaluators; everything that
            // writes the workspace stays serial (`CONTEXT.md` §14, §19).
            let batch: Vec<Node> = ready
                .iter()
                .filter(|n| self.is_parallel_evaluator(n))
                .cloned()
                .collect();
            if batch.len() >= 2 {
                if let Some(stop) = self
                    .run_parallel_evaluators(&batch, state, &mut tracker, &sink, &cancel)
                    .await?
                {
                    break stop;
                }
                continue;
            }

            // Otherwise, handle one node serially (the first ready).
            let node = ready.into_iter().next().unwrap();

            // Checkpoint-before gate: pause for approval unless already granted.
            if node.checkpoint == Some(Checkpoint::Before)
                && state.meta(&ckpt_key(&node.id))?.is_none()
            {
                self.park(state, &sink, &node.id, "checkpoint", &node.id, &format!(
                    "approve before running {}?",
                    node.id
                ))?;
                break StopReason::NeedsHuman {
                    node: node.id.clone(),
                    question: format!("approve before running {}?", node.id),
                };
            }

            let is_evaluator =
                node.evaluator.is_some() || matches!(node.role, crate::worker::Role::Evaluator);

            if is_evaluator {
                let gate = self.grade(&node, state, &sink, &cancel).await?;
                if let Some(stop) = self.apply_gate(&node, gate, state, &mut tracker, &sink).await? {
                    break stop;
                }
            } else {
                // A normal worker node.
                if let Some(stop) = self.run_worker_node(&node, state, &mut tracker, &sink, &cancel).await? {
                    break stop;
                }
            }
        };

        self.finish(state, stop_reason)
    }

    /// Execute a non-evaluator node by dispatching to a worker.
    async fn run_worker_node(
        &self,
        node: &Node,
        state: &dyn Blackboard,
        tracker: &mut LoopTracker,
        sink: &Option<EngineSink>,
        cancel: &CancellationToken,
    ) -> Result<Option<StopReason>> {
        let Some(worker) = self.registry.resolve(&node.needs, node.prefer.as_deref()) else {
            // Unsatisfiable node: never silently assign an incapable worker —
            // park for the human instead (`CONTEXT.md` §7).
            let question = format!(
                "no available worker satisfies {:?} for node {} — add a capable worker (e.g. an \
                 API key) or tell me how to proceed",
                node.needs, node.id
            );
            self.park(state, sink, &node.id, "human", &node.id, &question)?;
            return Ok(Some(StopReason::NeedsHuman {
                node: node.id.clone(),
                question,
            }));
        };
        let worker_name = worker.descriptor().name.clone();
        let family = worker.descriptor().family;

        narrate(sink, format!(
            "routed {} ({:?}) to {} [{}]",
            node.id, node.role, worker_name, family_label(family)
        ));
        emit_engine(sink, EngineEvent::NodeStarted {
            id: node.id.clone(),
            worker: worker_name.clone(),
        });

        state.set_status(&node.id, NodeStatus::Running)?;
        state.set_worker(&node.id, &worker_name)?;
        let iteration = state.incr_iteration(&node.id)?;

        // Inject any pending critique from a loop-back into this node's context.
        let critique = tracker.critiques.remove(&node.id);
        self.blackboard.append_progress(&format!(
            "node {} → {} (iteration {iteration}){}",
            node.id,
            worker_name,
            if critique.is_some() { " [with critique]" } else { "" }
        ))?;

        let assembler = ContextAssembler::new(self.blackboard, &self.plan);
        let packet = assembler.build(node, family, critique)?;
        if let Ok(json) = serde_json::to_string_pretty(&packet) {
            let _ = self.blackboard.write_context(&node.id, &json);
        }

        // A cascade escalation (from a prior evaluator failure) overrides the
        // node's assigned model. Validate against the worker that actually runs
        // the node: a model belongs to one worker, so if the scheduler resolved
        // a different worker than the conductor intended, drop the stale model
        // rather than passing e.g. an NVIDIA model id to Claude.
        let candidate = tracker
            .escalated_models
            .get(&node.id)
            .cloned()
            .or_else(|| self.resolve_model(node, &worker_name));
        let model = self.valid_model_for(candidate, worker.descriptor(), &worker_name, sink);
        if let Some(m) = &model {
            narrate(sink, format!("{} on {worker_name}:{m}", node.id));
        }
        let request = ExecuteRequest {
            role: node.role,
            instruction: node.instruction.clone(),
            context: packet,
            workspace: self.blackboard.workspace().to_path_buf(),
            constraints: Constraints {
                model,
                ..Default::default()
            },
        };

        let result = self.dispatch(worker.as_ref(), &node.id, request, sink, cancel).await?;
        self.blackboard.write_transcript(&node.id, &result.transcript)?;
        self.persist_outputs(node, &result)?;
        state.record_usage(&node.id, &worker_name, &result.usage)?;

        let status = match &result.status {
            ExecStatus::Success => NodeStatus::Succeeded,
            ExecStatus::Cancelled => NodeStatus::Pending,
            ExecStatus::Failed(_) | ExecStatus::TimedOut => NodeStatus::Failed,
        };
        state.set_status(&node.id, status)?;
        self.blackboard.append_progress(&format!(
            "node {} {} ({} tok, {} ms)",
            node.id, status.label(), result.usage.total_tokens(), result.usage.wall_ms
        ))?;
        emit_engine(sink, EngineEvent::NodeFinished { id: node.id.clone(), status });

        if result.status == ExecStatus::Cancelled {
            return Ok(Some(StopReason::Cancelled));
        }
        Ok(None)
    }

    /// Grade an evaluator node by dispatching to the matching `Evaluator` impl
    /// through the seam. The engine keeps the cross-cutting bits (status,
    /// iteration count, the test ratchet); the kind-specific grading lives in
    /// the evaluator strategies (`MCP_SKILLS.md` §15).
    async fn grade(
        &self,
        node: &Node,
        state: &dyn Blackboard,
        sink: &Option<EngineSink>,
        cancel: &CancellationToken,
    ) -> Result<Gate> {
        state.set_status(&node.id, NodeStatus::Running)?;
        let _ = state.incr_iteration(&node.id);
        let kind = node.evaluator.unwrap_or(EvaluatorKind::Tool);

        // The test ratchet runs first: a diff that deletes tests fails the gate
        // regardless of what the tests say (`CONTEXT.md` §12).
        if node.test_ratchet || (self.options.test_ratchet && node.evaluator == Some(EvaluatorKind::Tool)) {
            if let Some(verdict) = self.ratchet_block(node) {
                self.blackboard
                    .append_progress(&format!("eval {} BLOCKED by test ratchet", node.id))?;
                return Ok(Gate::Fail {
                    critique: verdict,
                    policy: node.parsed_on_fail().unwrap_or(OnFail::Replan),
                });
            }
        }

        let ctx = GradeCtx { engine: self, sink, cancel };
        let evaluator: Box<dyn Evaluator> = match kind {
            EvaluatorKind::Tool => Box::new(ToolEvaluator),
            EvaluatorKind::Ai => Box::new(AiEvaluator),
            EvaluatorKind::Human => Box::new(HumanEvaluator),
        };
        evaluator.grade(node, &ctx).await
    }

    /// Apply an evaluator failure: write the critique, check the stuck-detector
    /// and the iteration budget, then loop back, fix, or replan.
    async fn on_fail(
        &mut self,
        node: &Node,
        state: &dyn Blackboard,
        tracker: &mut LoopTracker,
        sink: &Option<EngineSink>,
        critique: String,
        policy: OnFail,
    ) -> Result<Option<StopReason>> {
        // Persist the critique artifact for visibility / resume.
        let critique_name = match &policy {
            OnFail::LoopBack { critique: Some(p), .. } => p.clone(),
            _ => format!("eval-{}.md", node.id),
        };
        self.blackboard.write_artifact(&critique_name, &critique)?;
        self.blackboard
            .append_progress(&format!("eval {} FAIL → {:?}", node.id, policy))?;

        match policy {
            OnFail::Replan => self.do_replan(state, sink).await,
            OnFail::Fixer => {
                // Route the fix to the evaluated generator (first dependency).
                let target = node.depends_on.first().cloned();
                self.loop_back(node, state, tracker, sink, target, critique).await
            }
            OnFail::LoopBack { node: target, .. } | OnFail::LoopWith { node: target } => {
                self.loop_back(node, state, tracker, sink, Some(target), critique).await
            }
        }
    }

    /// Loop back to `target`, with stuck-detection and budget guards. Escalates
    /// to a human (parks) rather than burning budget (`CONTEXT.md` §11).
    async fn loop_back(
        &self,
        node: &Node,
        state: &dyn Blackboard,
        tracker: &mut LoopTracker,
        sink: &Option<EngineSink>,
        target: Option<String>,
        critique: String,
    ) -> Result<Option<StopReason>> {
        let Some(target) = target else {
            state.set_status(&node.id, NodeStatus::Failed)?;
            return Ok(Some(StopReason::Blocked));
        };

        // Record the failure signature and detect the wall-ramming signature:
        // the same failure repeating `stuck_loop_threshold` times.
        let sig = signature(&critique);
        let history = tracker.failures.entry(node.id.clone()).or_default();
        history.push(sig.clone());
        let n = self.options.stuck_loop_threshold.max(1) as usize;
        let stuck = history.len() >= n && history.iter().rev().take(n).all(|s| s == &sig);

        // Iteration budget for the target generator.
        let cap = self
            .plan
            .node(&target)
            .and_then(|t| t.budget.iterations)
            .unwrap_or(self.options.max_iterations_per_node);
        let over_budget = state.iterations(&target)? >= cap;

        if stuck || over_budget {
            let why = if stuck {
                format!("{}↔{} looped {n}× with the same failure", target, node.id)
            } else {
                format!("{} hit its iteration budget ({cap})", target)
            };
            let question = format!(
                "stuck: {why}. Give the missing constraint (/steer), accept (/approve), or restart (/reject).",
            );
            // Park the evaluator and remember the loop target for resume.
            self.park(state, sink, &node.id, "stuck", &target, &question)?;
            return Ok(Some(StopReason::NeedsHuman {
                node: node.id.clone(),
                question,
            }));
        }

        // Start-cheap-escalate cascade: on failure, climb the target's model
        // ladder rather than re-running the same model (`CONTEXT.md` §7).
        self.escalate_model(&target, tracker, sink);
        // Feed the critique to the target and re-run the affected subtree.
        tracker.critiques.insert(target.clone(), critique);
        self.reset_subtree(state, &target)?;
        narrate(sink, format!("loop-back to {target} with critique"));
        Ok(None)
    }

    /// Climb the target node's cascade ladder by one tier (or jump to the top if
    /// its starting model is unknown). No-op when there's no ladder or it's
    /// already at the frontier.
    fn escalate_model(&self, target_id: &str, tracker: &mut LoopTracker, sink: &Option<EngineSink>) {
        let Some(node) = self.plan.node(target_id) else {
            return;
        };
        let Some(worker) = self.registry.resolve(&node.needs, node.prefer.as_deref()) else {
            return;
        };
        let name = worker.descriptor().name.clone();
        let Some(ladder) = self.options.model_ladders.get(&name) else {
            return;
        };
        if ladder.len() < 2 {
            return;
        }
        let current = tracker
            .escalated_models
            .get(target_id)
            .cloned()
            .or_else(|| self.resolve_model(node, &name));
        let next = match current.as_deref().and_then(|c| ladder.iter().position(|m| m == c)) {
            Some(idx) if idx + 1 < ladder.len() => ladder[idx + 1].clone(),
            Some(_) => return,                              // already at the top tier
            None => ladder.last().cloned().unwrap_or_default(), // unknown start → frontier
        };
        if next.is_empty() || Some(&next) == current.as_ref() {
            return;
        }
        narrate(sink, format!("escalating {target_id} → {name}:{next} after evaluator failure"));
        tracker.escalated_models.insert(target_id.to_string(), next);
    }

    /// Ask the replanner (the conductor) to amend the DAG (`CONTEXT.md` §12).
    async fn do_replan(
        &mut self,
        state: &dyn Blackboard,
        sink: &Option<EngineSink>,
    ) -> Result<Option<StopReason>> {
        let Some(replanner) = self.replanner.clone() else {
            narrate(sink, "replan requested but no replanner is attached".into());
            return Ok(Some(StopReason::Blocked));
        };
        let digest = self.digest(state)?;
        narrate(sink, "replanning the DAG".into());
        let new_plan = replanner
            .replan(&self.plan.goal, &digest, &self.plan)
            .await?;
        new_plan.validate()?;
        self.blackboard.save_plan(&new_plan)?;
        self.plan = new_plan;
        for n in &self.plan.nodes {
            state.ensure_node(&n.id)?;
        }
        self.blackboard.append_progress("DAG amended by replanner")?;
        Ok(None)
    }

    /// Apply a resume decision to a parked node before scheduling resumes.
    async fn apply_resume(
        &mut self,
        state: &dyn Blackboard,
        tracker: &mut LoopTracker,
        input: ResumeInput,
    ) -> Result<Option<StopReason>> {
        // Find the parked node this addresses.
        let parked = match input.node {
            Some(id) => Some(id),
            None => self
                .plan
                .nodes
                .iter()
                .find(|n| matches!(state.status(&n.id), Ok(NodeStatus::Parked)))
                .map(|n| n.id.clone()),
        };
        let Some(parked) = parked else {
            return Ok(None);
        };
        let kind = state.meta(&park_kind_key(&parked))?.unwrap_or_default();
        let target = state.meta(&park_target_key(&parked))?.unwrap_or_else(|| parked.clone());

        match input.decision {
            HumanDecision::Approve => {
                if kind == "checkpoint" {
                    // Grant the checkpoint and let the node run.
                    state.set_meta(&ckpt_key(&parked), "ok")?;
                    state.set_status(&parked, NodeStatus::Pending)?;
                } else {
                    // Accept current state: the parked evaluator passes.
                    state.set_status(&parked, NodeStatus::Succeeded)?;
                }
                self.blackboard
                    .append_progress(&format!("resume: approved {parked}"))?;
                Ok(None)
            }
            HumanDecision::Reject => {
                self.blackboard
                    .append_progress(&format!("resume: rejected {parked} → replan"))?;
                self.do_replan(state, &None).await
            }
            HumanDecision::Steer(text) => {
                // The user's words become the critique that flows into the loop.
                self.blackboard.write_artifact("eval-human.md", &text)?;
                tracker.critiques.insert(target.clone(), text);
                state.set_status(&parked, NodeStatus::Pending)?;
                self.reset_subtree(state, &target)?;
                // Fresh slate for the stuck-detector after human guidance.
                tracker.failures.remove(&parked);
                self.blackboard
                    .append_progress(&format!("resume: steered {parked} → loop-back {target}"))?;
                Ok(None)
            }
        }
    }

    /// Park a node for the user, recording how to resume it.
    fn park(
        &self,
        state: &dyn Blackboard,
        sink: &Option<EngineSink>,
        node_id: &str,
        kind: &str,
        target: &str,
        question: &str,
    ) -> Result<()> {
        state.set_status(node_id, NodeStatus::Parked)?;
        state.set_meta(&park_kind_key(node_id), kind)?;
        state.set_meta(&park_target_key(node_id), target)?;
        self.blackboard
            .append_progress(&format!("⏸ parked {node_id}: {question}"))?;
        emit_engine(sink, EngineEvent::Parked {
            id: node_id.to_string(),
            question: question.to_string(),
        });
        Ok(())
    }

    /// Check the test ratchet against the diff produced by the node(s) this
    /// evaluator depends on. Returns a critique if the ratchet blocks.
    fn ratchet_block(&self, node: &Node) -> Option<String> {
        for dep in &node.depends_on {
            let diff_artifact = format!("{dep}.diff");
            if let Ok(diff) = self.blackboard.read_artifact(&diff_artifact) {
                let verdict = ratchet::check_diff(&diff);
                if !verdict.ok {
                    return Some(format!(
                        "TEST RATCHET: {} (in {dep}'s diff). Do not weaken or delete tests.",
                        verdict.reason
                    ));
                }
            }
        }
        None
    }

    /// Reset `target` and all its transitive dependents to Pending so the
    /// affected subtree re-runs after a loop-back.
    fn reset_subtree(&self, state: &dyn Blackboard, target: &str) -> Result<()> {
        let mut to_reset: HashSet<String> = HashSet::new();
        to_reset.insert(target.to_string());
        // Iteratively pull in anything that depends on something already marked.
        loop {
            let mut grew = false;
            for n in &self.plan.nodes {
                if to_reset.contains(&n.id) {
                    continue;
                }
                if n.depends_on.iter().any(|d| to_reset.contains(d)) {
                    to_reset.insert(n.id.clone());
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }
        for id in to_reset {
            state.set_status(&id, NodeStatus::Pending)?;
        }
        Ok(())
    }

    /// Resolve the model to run for a node, in precedence order: the node's own
    /// model (conductor's choice), then the per-role config default, then the
    /// per-worker config default. `None` means the harness default
    /// (`CONTEXT.md` §7 cost/latency optimization).
    fn resolve_model(&self, node: &Node, worker_name: &str) -> Option<String> {
        let role = format!("{:?}", node.role).to_lowercase();
        node.model
            .clone()
            .or_else(|| self.options.role_models.get(&role).cloned())
            .or_else(|| self.options.worker_models.get(worker_name).cloned())
    }

    /// Validate a candidate model against the worker that will run the node.
    /// A model belongs to a specific worker (Claude's are opus/sonnet/haiku;
    /// an NVIDIA worker's is `deepseek-ai/…`), so a model the worker doesn't
    /// advertise is dropped — the worker uses its default — rather than passed
    /// through and rejected. A worker with no advertised models accepts any.
    fn valid_model_for(
        &self,
        candidate: Option<String>,
        descriptor: &WorkerDescriptor,
        worker_name: &str,
        sink: &Option<EngineSink>,
    ) -> Option<String> {
        match candidate {
            Some(m) if descriptor.models.is_empty() || descriptor.models.iter().any(|wm| *wm == m) => {
                Some(m)
            }
            Some(m) => {
                narrate(
                    sink,
                    format!("model `{m}` isn't available on {worker_name} — using its default model"),
                );
                None
            }
            None => None,
        }
    }

    /// The node a failure on `node` loops back to (its generator).
    fn loop_target(&self, node: &Node) -> Option<String> {
        match node.parsed_on_fail() {
            Some(OnFail::LoopBack { node, .. }) | Some(OnFail::LoopWith { node }) => Some(node),
            _ => node.depends_on.first().cloned(),
        }
    }

    /// A digest of current state for the replanner.
    fn digest(&self, state: &dyn Blackboard) -> Result<String> {
        let mut s = String::new();
        s.push_str("Node statuses:\n");
        for n in &self.plan.nodes {
            s.push_str(&format!(
                "- {} ({:?}): {}\n",
                n.id,
                n.role,
                state.status(&n.id)?.label()
            ));
        }
        if let Ok(progress) = std::fs::read_to_string(self.blackboard.progress_path()) {
            let tail: Vec<&str> = progress.lines().rev().take(12).collect();
            s.push_str("\nRecent progress:\n");
            for line in tail.into_iter().rev() {
                s.push_str(line);
                s.push('\n');
            }
        }
        Ok(s)
    }

    /// All ready nodes, in plan order. Ready = not yet succeeded/failed/parked
    /// and all dependencies have succeeded. A node left `Running` by an
    /// interrupted run is re-runnable.
    fn ready_nodes(&self, state: &dyn Blackboard) -> Result<Vec<Node>> {
        let mut out = Vec::new();
        for node in &self.plan.nodes {
            let status = state.status(&node.id)?;
            if !matches!(status, NodeStatus::Pending | NodeStatus::Running) {
                continue;
            }
            let deps_ok = node
                .depends_on
                .iter()
                .all(|d| matches!(state.status(d), Ok(NodeStatus::Succeeded)));
            if deps_ok {
                out.push(node.clone());
            }
        }
        Ok(out)
    }

    /// Whether a node is a read-only evaluator safe to run concurrently with
    /// other ready evaluators (`CONTEXT.md` §19): an AI or tool evaluator that
    /// doesn't write the workspace, isn't gated by a checkpoint, and isn't a
    /// human node (which parks).
    fn is_parallel_evaluator(&self, node: &Node) -> bool {
        let is_eval =
            node.evaluator.is_some() || matches!(node.role, crate::worker::Role::Evaluator);
        is_eval
            && node.checkpoint != Some(Checkpoint::Before)
            && node.evaluator != Some(EvaluatorKind::Human)
            && !node.needs.contains(&crate::worker::Capability::CodeEdit)
    }

    /// Grade a batch of independent read-only evaluators concurrently, then
    /// apply their gates sequentially. The grade phase (worker calls, tool
    /// commands) runs in parallel — the slow part — while result handling, which
    /// mutates state and the plan, stays serial and deterministic.
    async fn run_parallel_evaluators(
        &mut self,
        batch: &[Node],
        state: &dyn Blackboard,
        tracker: &mut LoopTracker,
        sink: &Option<EngineSink>,
        cancel: &CancellationToken,
    ) -> Result<Option<StopReason>> {
        narrate(sink, format!("running {} evaluators concurrently", batch.len()));

        // Concurrent grade (read-only borrows of &self). Scoped so the borrows
        // end before the sequential, mutating apply phase.
        let gates: Vec<Result<Gate>> = {
            let futures: Vec<_> = batch
                .iter()
                .map(|n| self.grade(n, state, sink, cancel))
                .collect();
            futures_util::future::join_all(futures).await
        };

        for (node, gate) in batch.iter().zip(gates) {
            let gate = gate?;
            if let Some(stop) = self.apply_gate(node, gate, state, tracker, sink).await? {
                return Ok(Some(stop));
            }
        }
        Ok(None)
    }

    /// Apply an evaluator's gate: pass closes the node, park surfaces a human
    /// question, fail runs the `on_fail` policy.
    async fn apply_gate(
        &mut self,
        node: &Node,
        gate: Gate,
        state: &dyn Blackboard,
        tracker: &mut LoopTracker,
        sink: &Option<EngineSink>,
    ) -> Result<Option<StopReason>> {
        match gate {
            Gate::Pass => {
                state.set_status(&node.id, NodeStatus::Succeeded)?;
                self.blackboard
                    .append_progress(&format!("eval {} PASS", node.id))?;
                emit_engine(sink, EngineEvent::NodeFinished {
                    id: node.id.clone(),
                    status: NodeStatus::Succeeded,
                });
                Ok(None)
            }
            Gate::Park { question } => {
                let target = self.loop_target(node).unwrap_or_else(|| node.id.clone());
                self.park(state, sink, &node.id, "human", &target, &question)?;
                Ok(Some(StopReason::NeedsHuman {
                    node: node.id.clone(),
                    question,
                }))
            }
            Gate::Fail { critique, policy } => {
                self.on_fail(node, state, tracker, sink, critique, policy).await
            }
        }
    }

    async fn dispatch(
        &self,
        worker: &dyn crate::worker::Worker,
        node_id: &str,
        request: ExecuteRequest,
        sink: &Option<EngineSink>,
        cancel: &CancellationToken,
    ) -> Result<crate::worker::ExecuteResult> {
        let (tx, mut rx): (EventSink, _) = tokio::sync::mpsc::unbounded_channel();
        let forward_sink = sink.clone();
        let id = node_id.to_string();
        let forwarder = tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                emit_engine(&forward_sink, EngineEvent::NodeStream { id: id.clone(), event });
            }
        });
        let result = worker.execute(request, tx, cancel.clone()).await;
        let _ = forwarder.await;
        result
    }

    fn persist_outputs(&self, node: &Node, result: &crate::worker::ExecuteResult) -> Result<()> {
        let mut wrote_named = false;
        for name in &node.outputs {
            if name == "diff" {
                if let Some(diff) = &result.file_diff {
                    self.blackboard.write_artifact(&format!("{}.diff", node.id), diff)?;
                }
            } else {
                self.blackboard.write_artifact(name, &result.result)?;
                wrote_named = true;
            }
        }
        if !wrote_named {
            self.blackboard
                .write_artifact(&format!("{}.out.md", node.id), &result.result)?;
        }
        Ok(())
    }

    fn finish(&self, state: &dyn Blackboard, stop_reason: StopReason) -> Result<RunReport> {
        let node_statuses = self
            .plan
            .nodes
            .iter()
            .map(|n| (n.id.clone(), state.status(&n.id).unwrap_or(NodeStatus::Pending)))
            .collect();
        Ok(RunReport {
            completed: stop_reason == StopReason::Completed,
            stop_reason,
            node_statuses,
            total_usage: state.total_usage()?,
            total_iterations: state.total_iterations()?,
        })
    }
}

fn ckpt_key(id: &str) -> String {
    format!("ckpt_ok:{id}")
}
fn park_kind_key(id: &str) -> String {
    format!("park_kind:{id}")
}
fn park_target_key(id: &str) -> String {
    format!("park_target:{id}")
}

fn signature(s: &str) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.trim().hash(&mut h);
    format!("{:016x}", h.finish())
}

fn family_label(f: WorkerFamily) -> &'static str {
    match f {
        WorkerFamily::Harness => "harness",
        WorkerFamily::Api => "api",
    }
}

fn narrate(sink: &Option<EngineSink>, line: String) {
    emit_engine(sink, EngineEvent::Narration(line));
}

fn emit_engine(sink: &Option<EngineSink>, event: EngineEvent) {
    if let Some(s) = sink {
        let _ = s.send(event);
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
