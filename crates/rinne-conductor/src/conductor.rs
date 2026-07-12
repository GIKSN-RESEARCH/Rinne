//! The Conductor: prompt → JSON DAG, with backend fallback and a JSON-repair
//! retry (`CONTEXT.md` §7, §21).

use std::sync::Arc;

use async_trait::async_trait;

use rinne_config::model::ConductorConfig;
use rinne_core::dag::Plan;
use rinne_core::replanner::Replanner;
use rinne_core::{Result, RinneError};

use crate::backend::{resolve_openai_model, PlanBackend};
use crate::classifier::{classify_goal_with, Classification};
use crate::ladder::{ConductorLadder, EscalationReason};
use crate::parse::parse_plan;
use crate::prompt::{exemplar_section, system_prompt, user_prompt, ConductorInput};
use crate::routing::apply_routing;

/// The conductor drives one or more backends in preference order. Each backend
/// gets one repair retry if its first output does not parse, before falling
/// through to the next backend (`CONTEXT.md` §21 graceful fallback).
pub struct Conductor {
    backends: Vec<Box<dyn PlanBackend>>,
    /// The planning context captured at build time (workers, tools, skills,
    /// preference, budgets). Reused on replan so an amended plan stays pool- and
    /// catalog-aware — the engine's `Replanner` hook only hands us goal+digest.
    context: ConductorInput,
    config: ConductorConfig,
    narration: Option<Arc<dyn Fn(String) + Send + Sync>>,
}

impl Conductor {
    /// Build a conductor from harness fallback backends (primary first).
    /// An empty list is valid when the API planner in `run_once` is configured.
    pub fn new(backends: Vec<Box<dyn PlanBackend>>) -> Result<Self> {
        Ok(Self {
            backends,
            context: ConductorInput::default(),
            config: ConductorConfig::default(),
            narration: None,
        })
    }

    /// Stream planner escalation narration to the UI (`EngineEvent::Narration`).
    pub fn with_narration(mut self, f: impl Fn(String) + Send + Sync + 'static) -> Self {
        self.narration = Some(Arc::new(f));
        self
    }

    fn narrate(&self, line: String) {
        if let Some(f) = &self.narration {
            f(line);
        }
    }

    /// Capture the planning context (workers, tools, skills, preference, budgets)
    /// so replans reuse it. The per-call `goal`/`digest`/`mentioned` are still
    /// supplied per `plan()` call; everything else falls back to this template.
    pub fn with_context(mut self, context: ConductorInput) -> Self {
        self.context = context;
        self
    }

    /// Conductor config (planner ladder, auto-escalate).
    pub fn with_conductor_config(mut self, config: ConductorConfig) -> Self {
        self.config = config;
        self
    }

    /// Names of the configured backends, primary first (for narration).
    pub fn backend_names(&self) -> Vec<String> {
        self.backends.iter().map(|b| b.name().to_string()).collect()
    }

    /// Produce a fresh plan from a goal and context.
    pub async fn plan(&self, input: &ConductorInput) -> Result<Plan> {
        let classification = self.classify(input);
        let mut ladder = ConductorLadder::from_config(&self.config);
        ladder.select_starting_rung(&classification);

        let system = system_prompt();
        let mut user = user_prompt(input);
        user.push_str(&exemplar_section(&classification));

        loop {
            let model = ladder
                .active_model()
                .unwrap_or(&self.config.model)
                .to_string();

            match self.run_once(&system, &user, &model).await {
                Ok(mut plan) => {
                    if plan.nodes.len() > 12
                        && ladder.active_index == 0
                        && ladder.can_escalate()
                    {
                        if let Some((from, to, why)) =
                            ladder.escalate(EscalationReason::HighNodeCount(plan.nodes.len() as u32))
                        {
                            let msg = format!("escalating planner {from} → {to}: {why}");
                            tracing::info!("conductor: {msg}");
                            self.narrate(msg);
                            user.push_str(&format!(
                                "\n\nESCALATION NOTICE: {why}. Re-plan with fewer, coarser nodes.\n"
                            ));
                            continue;
                        }
                    }

                    let routing_errs = apply_routing(&mut plan, input, &classification);
                    if routing_errs.is_empty() {
                        plan.mentioned = input.mentioned.clone();
                        return Ok(plan);
                    }

                    if let Some((from, to, why)) =
                        ladder.escalate(EscalationReason::ValidationFailure(routing_errs.clone()))
                    {
                        let msg = format!("escalating planner {from} → {to}: {why}");
                        tracing::info!("conductor: {msg}");
                        self.narrate(msg);
                        user.push_str(&format!(
                            "\n\nESCALATION NOTICE: {why}. Re-plan and fix tier/worker assignments.\n"
                        ));
                        continue;
                    }

                    return Err(RinneError::Conductor(format!(
                        "plan failed routing validation: {}",
                        routing_errs.join("; ")
                    )));
                }
                Err(e) if is_parse_failure(&e) && ladder.can_escalate() => {
                    if let Some((from, to, why)) = ladder.escalate(EscalationReason::ParseFailure) {
                        let msg = format!("escalating planner {from} → {to}: {why}");
                        tracing::info!("conductor: {msg}");
                        self.narrate(msg);
                        user.push_str(&format!(
                            "\n\nESCALATION NOTICE: {why}. Return ONLY valid JSON.\n"
                        ));
                        continue;
                    }
                    return Err(e);
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Amend an existing plan given new state. For Phase 4 this re-plans from
    /// scratch with the current plan summarized into the digest; structural
    /// amendment lands with the replanner hook in Phase 5.
    pub async fn replan(&self, input: &ConductorInput) -> Result<Plan> {
        self.plan(input).await
    }

    /// One planning attempt: API backend at `model`, then harness fallbacks.
    async fn run_once(&self, system: &str, user: &str, model: &str) -> Result<Plan> {
        let mut last_err: Option<RinneError> = None;

        if let Ok(Some(api)) = resolve_openai_model(&self.config, model) {
            match self.try_backend(&api, system, user).await {
                Ok(plan) => return Ok(plan),
                Err(e) => {
                    tracing::warn!("conductor api `{model}` failed: {e}");
                    last_err = Some(e);
                }
            }
        }

        for backend in &self.backends {
            match self.try_backend(backend.as_ref(), system, user).await {
                Ok(plan) => return Ok(plan),
                Err(e) => {
                    tracing::warn!("conductor backend `{}` failed: {e}", backend.name());
                    last_err = Some(e);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| {
            RinneError::Conductor("all conductor backends failed".into())
        }))
    }

    async fn try_backend(
        &self,
        backend: &dyn PlanBackend,
        system: &str,
        user: &str,
    ) -> Result<Plan> {
        let raw = backend.complete(system, user).await?;
        match parse_plan(&raw) {
            Ok(plan) => Ok(finalize(plan)),
            Err(first) => {
                tracing::warn!(
                    "conductor `{}` produced unparseable plan ({first}); retrying with repair nudge",
                    backend.name()
                );
                let repair_user = format!(
                    "{user}\n\nYour previous response could not be parsed as the required JSON \
                     DAG. Return ONLY the JSON object, with no prose, comments, or code fence."
                );
                let raw2 = backend.complete(system, &repair_user).await?;
                parse_plan(&raw2).map(finalize)
            }
        }
    }
}

impl Conductor {
    fn classify(&self, input: &ConductorInput) -> Classification {
        classify_goal_with(
            &input.goal,
            &input.routing.goal_keywords,
            &input.user_exemplars,
        )
    }
}

fn is_parse_failure(err: &RinneError) -> bool {
    matches!(err, RinneError::Plan(_))
}

/// Normalize a freshly-parsed plan: Rinne owns budgets (via config), so a
/// model-supplied budget is discarded to avoid a too-tight `max_total_iterations`
/// killing an otherwise-healthy run.
fn finalize(mut plan: Plan) -> Plan {
    plan.budget = Default::default();
    plan
}

/// The conductor is the engine's replanner: a wrong-approach verdict or repeated
/// failure amends the DAG rather than grinding the same node (`CONTEXT.md` §12).
#[async_trait]
impl Replanner for Conductor {
    async fn replan(&self, goal: &str, digest: &str, _current: &Plan) -> Result<Plan> {
        let input = ConductorInput {
            goal: goal.to_string(),
            digest: Some(digest.to_string()),
            ..self.context.clone()
        };
        Conductor::replan(self, &input).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::{SkillInfo, ToolInfo};
    use std::sync::{Arc, Mutex};

    struct RecordingBackend {
        last_user: Arc<Mutex<String>>,
    }

    #[async_trait]
    impl PlanBackend for RecordingBackend {
        fn name(&self) -> &str {
            "recording"
        }
        async fn complete(&self, _system: &str, user: &str) -> Result<String> {
            *self.last_user.lock().unwrap() = user.to_string();
            Ok(r#"{"goal":"g","nodes":[{"id":"n1","role":"generator","instruction":"do"}]}"#
                .to_string())
        }
    }

    #[tokio::test]
    async fn replan_reuses_captured_catalog() {
        let recorded = Arc::new(Mutex::new(String::new()));
        let backend = Box::new(RecordingBackend {
            last_user: recorded.clone(),
        });
        let context = ConductorInput {
            tools: vec![ToolInfo {
                id: "github.search_issues".into(),
                description: "Search issues".into(),
            }],
            skills: vec![SkillInfo {
                name: "pdf-forms".into(),
                description: "Fill PDF forms".into(),
            }],
            ..Default::default()
        };
        let conductor = Conductor::new(vec![backend]).unwrap().with_context(context);

        let current = parse_plan(
            r#"{"goal":"g","nodes":[{"id":"n1","role":"generator","instruction":"do"}]}"#,
        )
        .unwrap();
        let plan = Replanner::replan(&conductor, "amend it", "node n1 failed", &current)
            .await
            .unwrap();
        assert!(!plan.nodes.is_empty(), "routing may inject evaluators");

        let prompt = recorded.lock().unwrap().clone();
        assert!(prompt.contains("github.search_issues"), "tool catalog flowed into replan");
        assert!(prompt.contains("pdf-forms"), "skill catalog flowed into replan");
        assert!(prompt.contains("node n1 failed"), "digest flowed into replan");
    }
}