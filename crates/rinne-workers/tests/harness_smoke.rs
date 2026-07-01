//! Phase 2 exit-gate: a real harness adapter (Claude Code) executing a one-line
//! task end-to-end.
//!
//! Ignored by default because it drives the live `claude` CLI — it spends real
//! subscription quota and depends on the user being logged in. Run explicitly:
//!
//!     cargo test -p rinne-workers --test harness_smoke -- --ignored --nocapture

use std::path::PathBuf;

use tokio_util::sync::CancellationToken;

use rinne_core::worker::{
    Constraints, ContextPacket, ExecStatus, ExecuteRequest, Role, Worker, WorkerEvent,
};
use rinne_workers::adapters::claude_code;

#[tokio::test]
#[ignore = "drives the live claude CLI; run with --ignored"]
async fn claude_code_executes_one_line_task() {
    let worker = claude_code::worker();

    let request = ExecuteRequest {
        role: Role::Generator,
        instruction: "Reply with exactly the single word: pong. No other text.".into(),
        context: ContextPacket::default(),
        workspace: PathBuf::from("."),
        constraints: Constraints {
            timeout_secs: Some(120),
            ..Default::default()
        },
        tools: Vec::new(),
        mcp_servers: Vec::new(),
    };

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WorkerEvent>();
    let collector = tokio::spawn(async move {
        let mut n = 0;
        while rx.recv().await.is_some() {
            n += 1;
        }
        n
    });

    let res = worker
        .execute(request, tx, CancellationToken::new())
        .await
        .expect("claude adapter should drive the CLI");

    let event_count = collector.await.unwrap();

    eprintln!("status: {:?}", res.status);
    eprintln!("result: {}", res.result);
    eprintln!("usage:  {:?}", res.usage);
    eprintln!("events: {event_count}");

    assert_eq!(res.status, ExecStatus::Success, "expected a successful run");
    assert!(
        res.result.to_lowercase().contains("pong"),
        "expected 'pong' in the result, got: {}",
        res.result
    );
}
