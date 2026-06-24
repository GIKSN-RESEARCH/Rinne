//! Phase 5 exit-gate tests (`PHASE.md` P5):
//!   (a) tool-eval fail → loop_back → pass
//!   (b) the test ratchet blocks a test-deleting diff
//!   (c) the stuck-detector parks async without burning budget
//!   (d) a human critique flows into the next iteration
//!   (e) the replanner amends the DAG on a wrong-approach verdict

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use rinne_core::dag::Plan;
use rinne_core::replanner::Replanner;
use rinne_core::worker::{
    emit, AuthMode, Capability, EventSink, ExecStatus, ExecuteRequest, ExecuteResult,
    LatencyProfile, QuotaModel, Transport, Usage, Worker, WorkerDescriptor, WorkerEvent,
    WorkerFamily,
};
use rinne_core::{
    Blackboard, Engine, EngineOptions, HumanDecision, NodeStatus, ResumeInput, StopReason,
    WorkerRegistry,
};
use rinne_workers::mock::MockWorker;

fn temp_ws(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("rinne-loop-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn full_caps() -> Vec<Capability> {
    vec![
        Capability::CodeEdit,
        Capability::RepoAware,
        Capability::Reasoning,
        Capability::Writing,
        Capability::ToolRun,
        Capability::CodeReview,
        Capability::LongContext,
    ]
}

fn opts(stuck: u32) -> EngineOptions {
    EngineOptions {
        stuck_loop_threshold: stuck,
        ..EngineOptions::default()
    }
}

/// A worker that appends a line to `counter.txt` in the workspace each run, so
/// an external acceptance command can observe progress across loop-backs.
struct AppendWorker {
    descriptor: WorkerDescriptor,
}

impl AppendWorker {
    fn new() -> Self {
        Self {
            descriptor: WorkerDescriptor {
                name: "appender".into(),
                family: WorkerFamily::Harness,
                capabilities: full_caps(),
                auth_mode: AuthMode::Free,
                quota: QuotaModel::unlimited(),
                latency: LatencyProfile::Fast,
                transport: Transport::SubprocessJson,
                models: Vec::new(),
            },
        }
    }
}

#[async_trait]
impl Worker for AppendWorker {
    fn descriptor(&self) -> &WorkerDescriptor {
        &self.descriptor
    }
    async fn execute(
        &self,
        request: ExecuteRequest,
        events: EventSink,
        _cancel: CancellationToken,
    ) -> rinne_core::Result<ExecuteResult> {
        use std::io::Write;
        let path = request.workspace.join("counter.txt");
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(f, "x").unwrap();
        emit(&events, WorkerEvent::Editing("counter.txt".into()));
        Ok(ExecuteResult {
            result: "appended".into(),
            file_diff: None,
            transcript: "appended a line".into(),
            status: ExecStatus::Success,
            usage: Usage { prompt_tokens: 1, completion_tokens: 1, wall_ms: 0 },
            session_id: None,
        })
    }
}

// ----- (a) tool-eval fail → loop_back → pass -----------------------------------

#[tokio::test]
async fn tool_eval_loops_back_until_it_passes() {
    let ws = temp_ws("loopback");
    let bb = Blackboard::open(&ws).unwrap();
    let plan: Plan = serde_json::from_value(serde_json::json!({
        "goal": "make the counter reach two",
        "nodes": [
            {"id":"n1","role":"generator","instruction":"append","needs":["code-edit"]},
            {"id":"n2","role":"evaluator","evaluator":"tool","instruction":"check",
             "depends_on":["n1"],
             "acceptance":{"command":"test \"$(wc -l < counter.txt)\" -ge 2","must_exit":0},
             "on_fail":"loop_back(n1)"}
        ]
    })).unwrap();
    bb.save_plan(&plan).unwrap();

    let mut reg = WorkerRegistry::new();
    reg.register(Arc::new(AppendWorker::new()) as Arc<dyn Worker>);

    let mut engine = Engine::new(&bb, plan, &reg, opts(3));
    let report = engine.run(CancellationToken::new(), None, None).await.unwrap();

    assert!(report.completed, "should pass after loop-back: {:?}", report.stop_reason);
    // n1 ran exactly twice (first fail, second pass).
    let state = rinne_core::state::State::open(&bb.state_db_path()).unwrap();
    assert_eq!(state.iterations("n1").unwrap(), 2);

    let _ = std::fs::remove_dir_all(&ws);
}

// ----- (b) test ratchet blocks a test-deleting diff ----------------------------

#[tokio::test]
async fn test_ratchet_blocks_test_deleting_diff() {
    let ws = temp_ws("ratchet");
    let bb = Blackboard::open(&ws).unwrap();
    let plan: Plan = serde_json::from_value(serde_json::json!({
        "goal": "implement",
        "nodes": [
            {"id":"n1","role":"generator","instruction":"build","needs":["code-edit"],"outputs":["diff"]},
            {"id":"n2","role":"evaluator","evaluator":"tool","instruction":"tests",
             "depends_on":["n1"],"test_ratchet":true,
             "acceptance":{"command":"true","must_exit":0},
             "on_fail":"loop_back(n1)"}
        ]
    })).unwrap();
    bb.save_plan(&plan).unwrap();

    // The generator keeps producing a diff that deletes a test.
    let bad_diff = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@\n-    #[test]\n-    fn it_works() {}\n";
    let mut reg = WorkerRegistry::new();
    reg.register(
        Arc::new(MockWorker::new(
            rinne_workers::mock::MockScript::success("gen", "done").with_diff(bad_diff),
        )) as Arc<dyn Worker>,
    );

    let mut engine = Engine::new(&bb, plan, &reg, opts(2));
    let report = engine.run(CancellationToken::new(), None, None).await.unwrap();

    // The command `true` would pass, but the ratchet blocks the diff, so the run
    // never completes — it loops then parks on the repeated ratchet failure.
    assert!(!report.completed);
    assert!(matches!(report.stop_reason, StopReason::NeedsHuman { .. }));
    let critique = bb.read_artifact("eval-n2.md").unwrap();
    assert!(critique.contains("RATCHET"), "critique should cite the ratchet: {critique}");

    let _ = std::fs::remove_dir_all(&ws);
}

// ----- (c) stuck-detector parks async without burning budget -------------------

#[tokio::test]
async fn stuck_detector_parks_without_burning_budget() {
    let ws = temp_ws("stuck");
    let bb = Blackboard::open(&ws).unwrap();
    let plan: Plan = serde_json::from_value(serde_json::json!({
        "goal": "an impossible check",
        "nodes": [
            {"id":"n1","role":"generator","instruction":"try","needs":["code-edit"]},
            {"id":"n2","role":"evaluator","evaluator":"tool","instruction":"always fails",
             "depends_on":["n1"],
             "acceptance":{"command":"false","must_exit":0},
             "on_fail":"loop_back(n1)"}
        ]
    })).unwrap();
    bb.save_plan(&plan).unwrap();

    let mut reg = WorkerRegistry::new();
    reg.register(Arc::new(MockWorker::success("gen", "done")) as Arc<dyn Worker>);

    // Big per-node budget (8) but a stuck threshold of 2: it must park at ~2
    // loops, not grind to the iteration cap.
    let mut engine = Engine::new(&bb, plan, &reg, opts(2));
    let report = engine.run(CancellationToken::new(), None, None).await.unwrap();

    assert!(matches!(report.stop_reason, StopReason::NeedsHuman { .. }));
    assert!(!report.completed);
    // n1 ran ~2 times, far below the per-node cap of 8 — budget preserved.
    assert!(report.total_iterations <= 6, "burned too much: {}", report.total_iterations);

    let _ = std::fs::remove_dir_all(&ws);
}

// ----- (d) human critique flows into the next iteration ------------------------

#[tokio::test]
async fn human_critique_flows_into_next_iteration() {
    let ws = temp_ws("human");
    let bb = Blackboard::open(&ws).unwrap();
    let plan: Plan = serde_json::from_value(serde_json::json!({
        "goal": "needs human judgment",
        "nodes": [
            {"id":"n1","role":"generator","instruction":"build","needs":["code-edit"]},
            {"id":"n2","role":"evaluator","evaluator":"human","instruction":"is this right?",
             "depends_on":["n1"],"on_fail":"loop_back(n1)"}
        ]
    })).unwrap();
    bb.save_plan(&plan).unwrap();

    let mut reg = WorkerRegistry::new();
    reg.register(Arc::new(MockWorker::success("gen", "done")) as Arc<dyn Worker>);

    // First run parks at the human evaluator.
    let mut engine = Engine::new(&bb, plan.clone(), &reg, opts(3));
    let first = engine.run(CancellationToken::new(), None, None).await.unwrap();
    assert!(matches!(first.stop_reason, StopReason::NeedsHuman { .. }));

    // Resume with the user's steering; it must reach the generator's next run.
    let steer = "it's a Redis cluster, use a hash tag so keys co-locate";
    let mut engine2 = Engine::new(&bb, plan, &reg, opts(3));
    let _second = engine2
        .run(
            CancellationToken::new(),
            None,
            Some(ResumeInput { node: None, decision: HumanDecision::Steer(steer.into()) }),
        )
        .await
        .unwrap();

    // The critique was captured and flowed into n1's assembled context.
    assert_eq!(bb.read_artifact("eval-human.md").unwrap(), steer);
    let ctx = std::fs::read_to_string(bb.root().join("context/n1.json")).unwrap();
    assert!(ctx.contains("hash tag"), "generator did not receive the critique: {ctx}");

    let _ = std::fs::remove_dir_all(&ws);
}

// ----- parallelism: independent read-only evaluators run concurrently --------

/// A worker that tracks how many invocations overlap, to prove concurrency.
struct ConcWorker {
    descriptor: WorkerDescriptor,
    active: std::sync::Arc<std::sync::Mutex<i32>>,
    max: std::sync::Arc<std::sync::Mutex<i32>>,
}

#[async_trait]
impl Worker for ConcWorker {
    fn descriptor(&self) -> &WorkerDescriptor {
        &self.descriptor
    }
    async fn execute(
        &self,
        _request: ExecuteRequest,
        _events: EventSink,
        _cancel: CancellationToken,
    ) -> rinne_core::Result<ExecuteResult> {
        {
            let mut a = self.active.lock().unwrap();
            *a += 1;
            let mut m = self.max.lock().unwrap();
            if *a > *m {
                *m = *a;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        *self.active.lock().unwrap() -= 1;
        Ok(ExecuteResult {
            result: "looks correct\nVERDICT: PASS".into(),
            file_diff: None,
            transcript: String::new(),
            status: ExecStatus::Success,
            usage: Usage::default(),
            session_id: None,
        })
    }
}

#[tokio::test]
async fn independent_evaluators_run_concurrently() {
    let ws = temp_ws("parallel");
    let bb = Blackboard::open(&ws).unwrap();
    let plan: Plan = serde_json::from_value(serde_json::json!({
        "goal": "parallel evals",
        "nodes": [
            {"id":"n1","role":"generator","instruction":"build","needs":["code-edit"],"prefer":"harness:conc"},
            {"id":"n2","role":"evaluator","evaluator":"ai","instruction":"review A",
             "needs":["code-review"],"depends_on":["n1"],"prefer":"harness:conc"},
            {"id":"n3","role":"evaluator","evaluator":"ai","instruction":"review B",
             "needs":["code-review"],"depends_on":["n1"],"prefer":"harness:conc"}
        ]
    })).unwrap();
    bb.save_plan(&plan).unwrap();

    let max = std::sync::Arc::new(std::sync::Mutex::new(0));
    let mut reg = WorkerRegistry::new();
    reg.register(Arc::new(ConcWorker {
        descriptor: WorkerDescriptor {
            name: "conc".into(),
            family: WorkerFamily::Harness,
            capabilities: full_caps(),
            auth_mode: AuthMode::Free,
            quota: QuotaModel::unlimited(),
            latency: LatencyProfile::Fast,
            transport: Transport::SubprocessJson,
            models: Vec::new(),
        },
        active: std::sync::Arc::new(std::sync::Mutex::new(0)),
        max: max.clone(),
    }) as Arc<dyn Worker>);

    let mut engine = Engine::new(&bb, plan, &reg, opts(3));
    let report = engine.run(CancellationToken::new(), None, None).await.unwrap();

    assert!(report.completed, "should complete: {:?}", report.stop_reason);
    for (id, status) in &report.node_statuses {
        assert_eq!(*status, NodeStatus::Succeeded, "node {id}");
    }
    // n2 and n3 (independent read-only evaluators) overlapped → peak concurrency 2.
    assert_eq!(*max.lock().unwrap(), 2, "evaluators did not run concurrently");

    let _ = std::fs::remove_dir_all(&ws);
}

// ----- pool-aware cascade: escalate the model up the ladder on eval failure -----

/// A worker that records the model requested for each run.
struct RecordingWorker {
    descriptor: WorkerDescriptor,
    log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait]
impl Worker for RecordingWorker {
    fn descriptor(&self) -> &WorkerDescriptor {
        &self.descriptor
    }
    async fn execute(
        &self,
        request: ExecuteRequest,
        _events: EventSink,
        _cancel: CancellationToken,
    ) -> rinne_core::Result<ExecuteResult> {
        let model = request.constraints.model.clone().unwrap_or_else(|| "default".into());
        self.log.lock().unwrap().push(model);
        Ok(ExecuteResult {
            result: "done".into(),
            file_diff: None,
            transcript: String::new(),
            status: ExecStatus::Success,
            usage: Usage::default(),
            session_id: None,
        })
    }
}

#[tokio::test]
async fn cascade_escalates_model_on_evaluator_failure() {
    let ws = temp_ws("cascade");
    let bb = Blackboard::open(&ws).unwrap();
    let plan: Plan = serde_json::from_value(serde_json::json!({
        "goal": "cascade",
        "nodes": [
            {"id":"n1","role":"generator","instruction":"do","needs":["code-edit"],
             "prefer":"harness:recorder","model":"cheap"},
            {"id":"n2","role":"evaluator","evaluator":"tool","instruction":"always fails",
             "depends_on":["n1"],
             "acceptance":{"command":"false","must_exit":0},
             "on_fail":"loop_back(n1)"}
        ]
    })).unwrap();
    bb.save_plan(&plan).unwrap();

    let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut reg = WorkerRegistry::new();
    reg.register(Arc::new(RecordingWorker {
        descriptor: WorkerDescriptor {
            name: "recorder".into(),
            family: WorkerFamily::Harness,
            capabilities: full_caps(),
            auth_mode: AuthMode::Free,
            quota: QuotaModel::unlimited(),
            latency: LatencyProfile::Fast,
            transport: Transport::SubprocessJson,
            models: vec!["cheap".into(), "mid".into(), "strong".into()],
        },
        log: log.clone(),
    }) as Arc<dyn Worker>);

    let mut ladders = std::collections::HashMap::new();
    ladders.insert("recorder".to_string(), vec!["cheap".to_string(), "mid".to_string(), "strong".to_string()]);
    let options = EngineOptions {
        stuck_loop_threshold: 3,
        model_ladders: ladders,
        ..EngineOptions::default()
    };

    let mut engine = Engine::new(&bb, plan, &reg, options);
    let report = engine.run(CancellationToken::new(), None, None).await.unwrap();

    // The generator started cheap and climbed the ladder on each eval failure,
    // then parked when stuck rather than burning budget on one model.
    assert_eq!(*log.lock().unwrap(), vec!["cheap", "mid", "strong"]);
    assert!(matches!(report.stop_reason, StopReason::NeedsHuman { .. }));

    let _ = std::fs::remove_dir_all(&ws);
}

// ----- model validation: a foreign model is dropped, not passed to the worker ---

#[tokio::test]
async fn model_not_on_resolved_worker_is_dropped() {
    let ws = temp_ws("model-validate");
    let bb = Blackboard::open(&ws).unwrap();
    // The conductor assigned an NVIDIA model, but the node resolves to a worker
    // that only offers `sonnet`. The foreign model must be dropped.
    let plan: Plan = serde_json::from_value(serde_json::json!({
        "goal": "model validation",
        "nodes": [
            {"id":"n1","role":"generator","instruction":"do","needs":["code-edit"],
             "prefer":"harness:rec","model":"deepseek-ai/deepseek-v4-pro"}
        ]
    })).unwrap();
    bb.save_plan(&plan).unwrap();

    let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut reg = WorkerRegistry::new();
    reg.register(Arc::new(RecordingWorker {
        descriptor: WorkerDescriptor {
            name: "rec".into(),
            family: WorkerFamily::Harness,
            capabilities: full_caps(),
            auth_mode: AuthMode::Free,
            quota: QuotaModel::unlimited(),
            latency: LatencyProfile::Fast,
            transport: Transport::SubprocessJson,
            models: vec!["sonnet".into()], // does NOT include the NVIDIA model
        },
        log: log.clone(),
    }) as Arc<dyn Worker>);

    let mut engine = Engine::new(&bb, plan, &reg, opts(3));
    let report = engine.run(CancellationToken::new(), None, None).await.unwrap();

    assert!(report.completed, "should complete, not crash: {:?}", report.stop_reason);
    // The worker ran with its default (no foreign model forced onto it).
    assert_eq!(*log.lock().unwrap(), vec!["default"]);

    let _ = std::fs::remove_dir_all(&ws);
}

// ----- (e) replanner amends the DAG on a wrong-approach verdict -----------------

struct CannedReplanner;

#[async_trait]
impl Replanner for CannedReplanner {
    async fn replan(&self, _goal: &str, _digest: &str, _current: &Plan) -> rinne_core::Result<Plan> {
        // A simpler plan that just succeeds — proves the DAG was amended.
        Ok(serde_json::from_value(serde_json::json!({
            "goal": "amended",
            "nodes": [{"id":"fixed","role":"generator","instruction":"do it simply","needs":["code-edit"]}]
        })).unwrap())
    }
}

#[tokio::test]
async fn replanner_amends_dag_on_replan_verdict() {
    let ws = temp_ws("replan");
    let bb = Blackboard::open(&ws).unwrap();
    let plan: Plan = serde_json::from_value(serde_json::json!({
        "goal": "original approach",
        "nodes": [
            {"id":"n1","role":"generator","instruction":"build","needs":["code-edit"]},
            {"id":"n2","role":"evaluator","evaluator":"tool","instruction":"wrong approach",
             "depends_on":["n1"],
             "acceptance":{"command":"false","must_exit":0},
             "on_fail":"replan"}
        ]
    })).unwrap();
    bb.save_plan(&plan).unwrap();

    let mut reg = WorkerRegistry::new();
    reg.register(Arc::new(MockWorker::success("gen", "done")) as Arc<dyn Worker>);

    let mut engine = Engine::new(&bb, plan, &reg, opts(3))
        .with_replanner(Arc::new(CannedReplanner));
    let report = engine.run(CancellationToken::new(), None, None).await.unwrap();

    assert!(report.completed, "amended plan should complete: {:?}", report.stop_reason);
    // The DAG on disk was replaced by the amended one.
    let amended = bb.load_plan().unwrap();
    assert_eq!(amended.nodes.len(), 1);
    assert_eq!(amended.nodes[0].id, "fixed");

    let _ = std::fs::remove_dir_all(&ws);
}
