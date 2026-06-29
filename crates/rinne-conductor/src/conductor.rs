//! The Conductor: prompt → JSON DAG, with backend fallback and a JSON-repair
//! retry (`CONTEXT.md` §7, §21).

use async_trait::async_trait;

use rinne_core::dag::Plan;
use rinne_core::replanner::Replanner;
use rinne_core::{Result, RinneError};

use crate::backend::PlanBackend;
use crate::parse::parse_plan;
use crate::prompt::{system_prompt, user_prompt, ConductorInput};

/// The conductor drives one or more backends in preference order. Each backend
/// gets one repair retry if its first output does not parse, before falling
/// through to the next backend (`CONTEXT.md` §21 graceful fallback).
pub struct Conductor {
    backends: Vec<Box<dyn PlanBackend>>,
    /// The planning context captured at build time (workers, tools, skills,
    /// preference, budgets). Reused on replan so an amended plan stays pool- and
    /// catalog-aware — the engine's `Replanner` hook only hands us goal+digest.
    context: ConductorInput,
}

impl Conductor {
    /// Build a conductor from backends in preference order (primary first).
    /// At least one backend is required.
    pub fn new(backends: Vec<Box<dyn PlanBackend>>) -> Result<Self> {
        if backends.is_empty() {
            return Err(RinneError::Conductor(
                "no conductor backend available — configure one or install a harness".into(),
            ));
        }
        Ok(Self {
            backends,
            context: ConductorInput::default(),
        })
    }

    /// Capture the planning context (workers, tools, skills, preference, budgets)
    /// so replans reuse it. The per-call `goal`/`digest`/`mentioned` are still
    /// supplied per `plan()` call; everything else falls back to this template.
    pub fn with_context(mut self, context: ConductorInput) -> Self {
        self.context = context;
        self
    }

    /// Names of the configured backends, primary first (for narration).
    pub fn backend_names(&self) -> Vec<String> {
        self.backends.iter().map(|b| b.name().to_string()).collect()
    }

    /// Produce a fresh plan from a goal and context.
    pub async fn plan(&self, input: &ConductorInput) -> Result<Plan> {
        let system = system_prompt();
        let user = user_prompt(input);
        let mut plan = self.run(&system, &user).await?;
        // Carry the @-mentioned files onto the plan deterministically. The
        // assembler inlines their contents for API workers; relying on the LLM
        // to echo the paths back in its JSON is unreliable, so set them here.
        plan.mentioned = input.mentioned.clone();
        Ok(plan)
    }

    /// Amend an existing plan given new state. For Phase 4 this re-plans from
    /// scratch with the current plan summarized into the digest; structural
    /// amendment lands with the replanner hook in Phase 5.
    pub async fn replan(&self, input: &ConductorInput) -> Result<Plan> {
        self.plan(input).await
    }

    /// Try each backend in order; within a backend, retry once with a repair
    /// nudge if the first response does not parse.
    async fn run(&self, system: &str, user: &str) -> Result<Plan> {
        let mut last_err: Option<RinneError> = None;

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
        // Start from the captured planning context (workers, tools, skills,
        // preference, budgets) so the amendment is as pool-aware as the initial
        // plan; only the goal and the fresh digest change.
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

    /// A backend that records the last user prompt it saw and returns a canned plan.
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

        // Drive the engine-facing replan hook, which only hands over goal + digest.
        let current = parse_plan(
            r#"{"goal":"g","nodes":[{"id":"n1","role":"generator","instruction":"do"}]}"#,
        )
        .unwrap();
        let plan = Replanner::replan(&conductor, "amend it", "node n1 failed", &current)
            .await
            .unwrap();
        assert_eq!(plan.nodes.len(), 1);

        // The captured catalog must have reached the prompt regardless.
        let prompt = recorded.lock().unwrap().clone();
        assert!(prompt.contains("github.search_issues"), "tool catalog flowed into replan");
        assert!(prompt.contains("pdf-forms"), "skill catalog flowed into replan");
        assert!(prompt.contains("node n1 failed"), "digest flowed into replan");
    }
}
