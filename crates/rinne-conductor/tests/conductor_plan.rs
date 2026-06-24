//! Phase 4 exit-gate tests: the conductor turns backend output into validated
//! plans, repairs malformed JSON, and falls back across backends
//! (`PHASE.md` P4).

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;

use rinne_conductor::{Conductor, ConductorInput, PlanBackend};
use rinne_core::{RinneError, Result};

/// A scripted backend: each `complete` call pops the next canned response.
/// `Ok` is returned text; `Err` simulates a backend failure.
struct MockBackend {
    name: String,
    responses: Mutex<VecDeque<std::result::Result<String, String>>>,
}

impl MockBackend {
    fn new(name: &str, responses: Vec<std::result::Result<String, String>>) -> Self {
        Self {
            name: name.into(),
            responses: Mutex::new(responses.into_iter().collect()),
        }
    }
}

#[async_trait]
impl PlanBackend for MockBackend {
    fn name(&self) -> &str {
        &self.name
    }
    async fn complete(&self, _system: &str, _user: &str) -> Result<String> {
        match self.responses.lock().unwrap().pop_front() {
            Some(Ok(s)) => Ok(s),
            Some(Err(e)) => Err(RinneError::Conductor(e)),
            None => Err(RinneError::Conductor("no more scripted responses".into())),
        }
    }
}

const VALID: &str = r#"{
  "goal": "add rate limiting",
  "nodes": [
    {"id":"n1","role":"generator","instruction":"implement","needs":["code-edit"],"prefer":"harness:claude-code"},
    {"id":"n2","role":"evaluator","evaluator":"tool","instruction":"run tests",
     "depends_on":["n1"],"acceptance":{"command":"npm test","must_exit":0},
     "on_fail":"loop_back(n1, critique=artifacts/eval.md)"}
  ]
}"#;

fn input() -> ConductorInput {
    ConductorInput {
        goal: "add rate limiting".into(),
        max_iterations_per_node: 8,
        ..Default::default()
    }
}

#[tokio::test]
async fn produces_valid_plan() {
    let backend = Box::new(MockBackend::new("primary", vec![Ok(VALID.into())]));
    let conductor = Conductor::new(vec![backend]).unwrap();
    let plan = conductor.plan(&input()).await.unwrap();

    assert_eq!(plan.goal, "add rate limiting");
    assert_eq!(plan.nodes.len(), 2);
    assert_eq!(plan.nodes[1].depends_on, vec!["n1"]);
    plan.validate().unwrap();
}

#[tokio::test]
async fn repairs_malformed_json_on_retry() {
    // First response is prose (unparseable); the repair retry returns valid JSON.
    let backend = Box::new(MockBackend::new(
        "primary",
        vec![Ok("I cannot output JSON, sorry.".into()), Ok(VALID.into())],
    ));
    let conductor = Conductor::new(vec![backend]).unwrap();
    let plan = conductor.plan(&input()).await.unwrap();
    assert_eq!(plan.nodes.len(), 2);
}

#[tokio::test]
async fn falls_back_to_next_backend() {
    // Primary errors on both attempts; secondary succeeds.
    let primary = Box::new(MockBackend::new(
        "primary",
        vec![Err("503 down".into()), Err("503 down".into())],
    ));
    let secondary = Box::new(MockBackend::new("fallback", vec![Ok(VALID.into())]));
    let conductor = Conductor::new(vec![primary, secondary]).unwrap();
    let plan = conductor.plan(&input()).await.unwrap();
    assert_eq!(plan.goal, "add rate limiting");
}

#[tokio::test]
async fn errors_when_all_backends_fail() {
    let primary = Box::new(MockBackend::new(
        "primary",
        vec![Err("down".into()), Err("down".into())],
    ));
    let secondary = Box::new(MockBackend::new(
        "fallback",
        vec![Ok("still not json".into()), Ok("nope".into())],
    ));
    let conductor = Conductor::new(vec![primary, secondary]).unwrap();
    assert!(conductor.plan(&input()).await.is_err());
}

#[tokio::test]
async fn rejects_empty_backend_list() {
    assert!(Conductor::new(vec![]).is_err());
}
