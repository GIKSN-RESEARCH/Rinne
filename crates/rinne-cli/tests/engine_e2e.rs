//! Phase 3 exit-gate tests: a hand-written DAG runs end-to-end against the mock
//! worker, with budgets enforced and clean kill-then-resume (`PHASE.md` P3).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use rinne_core::state::State;
use rinne_core::worker::Worker;
use rinne_core::{Blackboard, Engine, EngineOptions, NodeStatus, Plan, StopReason, WorkerRegistry};
use rinne_workers::mock::{MockScript, MockWorker};

fn temp_ws(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("rinne-e2e-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// A linear plan: planner → generator (emits a diff) → synthesizer.
fn sample_plan() -> Plan {
    serde_json::from_value(serde_json::json!({
        "goal": "Add a limiter and prove it",
        "nodes": [
            {
                "id": "n1", "role": "planner",
                "instruction": "design it",
                "needs": ["reasoning"],
                "prefer": "harness:planner",
                "outputs": ["design.md"]
            },
            {
                "id": "n2", "role": "generator",
                "instruction": "implement it",
                "needs": ["code-edit"],
                "prefer": "harness:coder",
                "depends_on": ["n1"],
                "inputs": ["design.md"],
                "outputs": ["diff", "impl.md"]
            },
            {
                "id": "n3", "role": "synthesizer",
                "instruction": "summarize it",
                "needs": ["writing"],
                "prefer": "harness:writer",
                "depends_on": ["n2"],
                "inputs": ["impl.md"],
                "outputs": ["summary.md"]
            }
        ]
    }))
    .unwrap()
}

fn registry(slow: bool) -> WorkerRegistry {
    let mk = |name: &str, text: &str| -> MockScript {
        let mut s = MockScript::success(name, text);
        if slow {
            s.per_event_ms = 40;
            s = s.with_events(vec![
                rinne_core::worker::WorkerEvent::Message("working".into()),
                rinne_core::worker::WorkerEvent::Message("still working".into()),
            ]);
        }
        s
    };

    let mut reg = WorkerRegistry::new();
    reg.register(Arc::new(MockWorker::new(mk("planner", "design: middleware shape"))) as Arc<dyn Worker>);
    reg.register(Arc::new(MockWorker::new(mk("coder", "implemented").with_diff("--- the diff ---"))) as Arc<dyn Worker>);
    reg.register(Arc::new(MockWorker::new(mk("writer", "summary text"))) as Arc<dyn Worker>);
    reg
}

#[tokio::test]
async fn runs_dag_end_to_end() {
    let ws = temp_ws("full");
    let bb = Blackboard::open(&ws).unwrap();
    let plan = sample_plan();
    bb.save_plan(&plan).unwrap();

    let reg = registry(false);
    let mut engine = Engine::new(&bb, plan.clone(), &reg, EngineOptions::default());
    let report = engine.run(CancellationToken::new(), None, None).await.unwrap();

    assert!(report.completed, "expected completion, got {:?}", report.stop_reason);
    assert_eq!(report.stop_reason, StopReason::Completed);
    for (id, status) in &report.node_statuses {
        assert_eq!(*status, NodeStatus::Succeeded, "node {id} not succeeded");
    }

    // Artifacts flowed through the blackboard.
    assert_eq!(bb.read_artifact("design.md").unwrap(), "design: middleware shape");
    assert_eq!(bb.read_artifact("impl.md").unwrap(), "implemented");
    assert_eq!(bb.read_artifact("n2.diff").unwrap(), "--- the diff ---");
    assert_eq!(bb.read_artifact("summary.md").unwrap(), "summary text");

    // Progress log and usage ledger populated.
    assert!(bb.progress_path().exists());
    assert!(report.total_usage.total_tokens() > 0);
    assert_eq!(report.total_iterations, 3);

    let _ = std::fs::remove_dir_all(&ws);
}

#[tokio::test]
async fn resume_skips_already_succeeded_nodes() {
    let ws = temp_ws("resume-skip");
    let bb = Blackboard::open(&ws).unwrap();
    let plan = sample_plan();
    bb.save_plan(&plan).unwrap();

    // Simulate a prior run that finished n1 before being killed.
    {
        let state = State::open(&bb.state_db_path()).unwrap();
        for n in &plan.nodes {
            state.ensure_node(&n.id).unwrap();
        }
        state.set_status("n1", NodeStatus::Succeeded).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        state.set_meta("started_at", &now.to_string()).unwrap();
        bb.write_artifact("design.md", "preexisting design").unwrap();
    }

    let reg = registry(false);
    let mut engine = Engine::new(&bb, plan.clone(), &reg, EngineOptions::default());
    let report = engine.run(CancellationToken::new(), None, None).await.unwrap();

    assert!(report.completed);

    // n1 was never re-run (its iteration count stayed 0); n2/n3 ran once each.
    let state = State::open(&bb.state_db_path()).unwrap();
    assert_eq!(state.iterations("n1").unwrap(), 0, "n1 should have been skipped");
    assert_eq!(state.iterations("n2").unwrap(), 1);
    assert_eq!(state.iterations("n3").unwrap(), 1);
    // The pre-existing artifact was not overwritten by a re-run.
    assert_eq!(bb.read_artifact("design.md").unwrap(), "preexisting design");

    let _ = std::fs::remove_dir_all(&ws);
}

#[tokio::test]
async fn kill_then_resume_reaches_same_final_state() {
    let ws = temp_ws("kill-resume");
    let bb = Blackboard::open(&ws).unwrap();
    let plan = sample_plan();
    bb.save_plan(&plan).unwrap();

    // First run with slow workers, cancelled mid-flight.
    let reg = registry(true);
    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel2.cancel();
    });
    let mut engine = Engine::new(&bb, plan.clone(), &reg, EngineOptions::default());
    let first = engine.run(cancel, None, None).await.unwrap();
    assert!(!first.completed, "first run should have been interrupted");

    // Resume with a fresh token and fast workers: must reach full completion.
    let reg2 = registry(false);
    let mut engine2 = Engine::new(&bb, plan.clone(), &reg2, EngineOptions::default());
    let second = engine2.run(CancellationToken::new(), None, None).await.unwrap();

    assert!(second.completed, "resume should complete, got {:?}", second.stop_reason);
    for (id, status) in &second.node_statuses {
        assert_eq!(*status, NodeStatus::Succeeded, "node {id} not succeeded after resume");
    }
    assert_eq!(bb.read_artifact("summary.md").unwrap(), "summary text");

    let _ = std::fs::remove_dir_all(&ws);
}

#[tokio::test]
async fn iteration_budget_stops_the_run() {
    let ws = temp_ws("budget");
    let bb = Blackboard::open(&ws).unwrap();
    let plan = sample_plan();
    bb.save_plan(&plan).unwrap();

    let reg = registry(false);
    let opts = EngineOptions {
        max_total_iterations: Some(1),
        ..EngineOptions::default()
    };
    let mut engine = Engine::new(&bb, plan.clone(), &reg, opts);
    let report = engine.run(CancellationToken::new(), None, None).await.unwrap();

    assert_eq!(report.stop_reason, StopReason::BudgetIterations);
    assert!(!report.completed);

    let _ = std::fs::remove_dir_all(&ws);
}
