//! Phase 2 exit-gate tests: the mock worker through success, failure,
//! streaming, and cancellation cases (`PHASE.md` P2).

use std::path::PathBuf;

use tokio_util::sync::CancellationToken;

use rinne_core::worker::{
    Constraints, ContextPacket, EventSink, ExecStatus, ExecuteRequest, Role, Worker, WorkerEvent,
};
use rinne_workers::mock::{MockScript, MockWorker};

fn request() -> ExecuteRequest {
    ExecuteRequest {
        role: Role::Generator,
        instruction: "do the thing".into(),
        context: ContextPacket::default(),
        workspace: PathBuf::from("."),
        constraints: Constraints::default(),
        tools: Vec::new(),
        mcp_servers: Vec::new(),
    }
}

fn channel() -> (EventSink, tokio::sync::mpsc::UnboundedReceiver<WorkerEvent>) {
    tokio::sync::mpsc::unbounded_channel()
}

fn drain(mut rx: tokio::sync::mpsc::UnboundedReceiver<WorkerEvent>) -> Vec<WorkerEvent> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    out
}

#[tokio::test]
async fn success_returns_result_and_usage() {
    let worker = MockWorker::success("m1", "all done");
    let (tx, rx) = channel();
    let res = worker
        .execute(request(), tx, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(res.status, ExecStatus::Success);
    assert_eq!(res.result, "all done");
    assert_eq!(res.usage.total_tokens(), 150);

    let events = drain(rx);
    assert_eq!(events.last(), Some(&WorkerEvent::Done));
}

#[tokio::test]
async fn failure_status_propagates() {
    let worker = MockWorker::new(MockScript::failure("m2", "tests failed"));
    let (tx, _rx) = channel();
    let res = worker
        .execute(request(), tx, CancellationToken::new())
        .await
        .unwrap();

    match res.status {
        ExecStatus::Failed(reason) => assert!(reason.contains("tests failed")),
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[tokio::test]
async fn streams_events_in_order_then_done() {
    let script = MockScript::success("m3", "ok").with_events(vec![
        WorkerEvent::Reading("a.rs".into()),
        WorkerEvent::Editing("b.rs".into()),
        WorkerEvent::Message("wrapping up".into()),
    ]);
    let worker = MockWorker::new(script);
    let (tx, rx) = channel();
    worker
        .execute(request(), tx, CancellationToken::new())
        .await
        .unwrap();

    let events = drain(rx);
    assert_eq!(
        events,
        vec![
            WorkerEvent::Reading("a.rs".into()),
            WorkerEvent::Editing("b.rs".into()),
            WorkerEvent::Message("wrapping up".into()),
            WorkerEvent::Done,
        ]
    );
}

#[tokio::test]
async fn diff_is_carried_on_result() {
    let diff = "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n";
    let worker = MockWorker::new(MockScript::success("m4", "edited").with_diff(diff));
    let (tx, _rx) = channel();
    let res = worker
        .execute(request(), tx, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(res.file_diff.as_deref(), Some(diff));
}

#[tokio::test]
async fn cancellation_stops_mid_stream() {
    // A slow script: events spaced out so cancellation lands mid-run.
    let mut script = MockScript::success("m5", "should not finish").with_events(vec![
        WorkerEvent::Message("step 1".into()),
        WorkerEvent::Message("step 2".into()),
        WorkerEvent::Message("step 3".into()),
    ]);
    script.per_event_ms = 50;

    let worker = MockWorker::new(script);
    let (tx, _rx) = channel();
    let cancel = CancellationToken::new();

    let cancel2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cancel2.cancel();
    });

    let res = worker.execute(request(), tx, cancel).await.unwrap();
    assert_eq!(res.status, ExecStatus::Cancelled);
}
