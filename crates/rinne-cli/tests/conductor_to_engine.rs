//! Phase 4 exit-gate capstone: a conductor-generated DAG is accepted by the
//! scheduler and executed to completion against mock workers (`PHASE.md` P4).

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use rinne_conductor::{Conductor, ConductorInput, PlanBackend};
use rinne_core::worker::Worker;
use rinne_core::{Blackboard, Engine, EngineOptions, NodeStatus, WorkerRegistry};
use rinne_workers::mock::MockWorker;

/// A backend that always returns the same canned DAG.
struct CannedBackend(String);

#[async_trait]
impl PlanBackend for CannedBackend {
    fn name(&self) -> &str {
        "canned"
    }
    async fn complete(&self, _system: &str, _user: &str) -> rinne_core::Result<String> {
        Ok(self.0.clone())
    }
}

fn temp_ws(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("rinne-c2e-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

const DAG: &str = r#"```json
{
  "goal": "ship a small feature",
  "nodes": [
    // design first
    {"id":"n1","role":"planner","instruction":"design","needs":["reasoning"],"prefer":"harness:coder","outputs":["design.md"]},
    {"id":"n2","role":"generator","instruction":"build","needs":["code-edit"],
     "depends_on":["n1"],"inputs":["design.md"],"outputs":["impl.md"],},
    {"id":"n3","role":"synthesizer","instruction":"summarize","needs":["writing"],
     "depends_on":["n2"],"outputs":["summary.md"],}
  ],
}
```"#;

#[tokio::test]
async fn conductor_plan_runs_through_engine() {
    // 1. Conductor turns (messy, fenced, comment-laden) backend output into a plan.
    let conductor = Conductor::new(vec![Box::new(CannedBackend(DAG.into()))]).unwrap();
    let plan = conductor
        .plan(&ConductorInput {
            goal: "ship a small feature".into(),
            max_iterations_per_node: 8,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(plan.nodes.len(), 3);

    // 2. Persist it and run it through the real engine with mock workers.
    let ws = temp_ws("run");
    let bb = Blackboard::open(&ws).unwrap();
    bb.save_plan(&plan).unwrap();

    let mut reg = WorkerRegistry::new();
    reg.register(Arc::new(MockWorker::success("coder", "done")) as Arc<dyn Worker>);
    reg.register(Arc::new(MockWorker::success("writer", "summary")) as Arc<dyn Worker>);

    let mut engine = Engine::new(&bb, plan.clone(), &reg, EngineOptions::default());
    let report = engine.run(CancellationToken::new(), None, None).await.unwrap();

    assert!(report.completed, "generated plan should run to completion: {:?}", report.stop_reason);
    for (id, status) in &report.node_statuses {
        assert_eq!(*status, NodeStatus::Succeeded, "node {id}");
    }

    let _ = std::fs::remove_dir_all(&ws);
}
