//! End-to-end test of the host agentic tool loop (`MCP_SKILLS.md` §6): an API
//! worker whose node attaches a tool drives a model that calls the tool, runs it
//! via the executor, feeds the result back, and returns the model's final text.
//!
//! A raw-TCP mock stands in for the OpenAI endpoint — first POST returns a tool
//! call, second POST (after the tool result is fed back) returns the answer. The
//! executor is a stub; no real MCP server is needed to exercise the loop.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use rinne_core::worker::{
    ContextPacket, Constraints, ExecuteRequest, Role, ToolExecutor, ToolSpec, Worker, WorkerEvent,
};
use rinne_workers::adapters::openai_api::OpenAiWorker;

/// A tool executor that records its calls and echoes the arguments back.
struct StubExecutor {
    calls: Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>,
}

#[async_trait]
impl ToolExecutor for StubExecutor {
    async fn call(
        &self,
        id: &str,
        arguments: serde_json::Value,
    ) -> std::result::Result<String, String> {
        self.calls.lock().unwrap().push((id.to_string(), arguments.clone()));
        Ok(format!("echoed: {arguments}"))
    }
}

/// Read one HTTP request off the socket (headers + Content-Length body).
fn read_request(stream: &mut std::net::TcpStream) -> String {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let n = stream.read(&mut tmp).unwrap();
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        let text = String::from_utf8_lossy(&buf);
        if let Some(header_end) = text.find("\r\n\r\n") {
            let content_len = text
                .lines()
                .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                .unwrap_or(0);
            if buf.len() >= header_end + 4 + content_len {
                break;
            }
        }
    }
    String::from_utf8_lossy(&buf).to_string()
}

fn write_json(stream: &mut std::net::TcpStream, body: &str) {
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(resp.as_bytes()).unwrap();
    stream.flush().unwrap();
}

#[tokio::test]
async fn api_worker_runs_the_tool_loop() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    // Mock server: first call → tool call; second call → final answer. Also
    // capture whether the second request actually carried the tool result back.
    let saw_tool_result = Arc::new(AtomicUsize::new(0));
    let saw = saw_tool_result.clone();
    let server = thread::spawn(move || {
        let mut conns = listener.incoming();
        // Turn 1: the model asks to call the tool.
        let mut s1 = conns.next().unwrap().unwrap();
        let _ = read_request(&mut s1);
        write_json(
            &mut s1,
            r#"{"choices":[{"message":{"role":"assistant","content":null,
                "tool_calls":[{"id":"call_1","type":"function",
                "function":{"name":"srv.echo","arguments":"{\"text\":\"hi\"}"}}]},
                "finish_reason":"tool_calls"}],
                "usage":{"prompt_tokens":10,"completion_tokens":5}}"#,
        );
        // Turn 2: with the tool result fed back, the model answers.
        let mut s2 = conns.next().unwrap().unwrap();
        let req2 = read_request(&mut s2);
        if req2.contains("\"role\":\"tool\"") && req2.contains("echoed:") {
            saw.store(1, Ordering::SeqCst);
        }
        write_json(
            &mut s2,
            r#"{"choices":[{"message":{"role":"assistant","content":"the tool said hi"},
                "finish_reason":"stop"}],
                "usage":{"prompt_tokens":20,"completion_tokens":8}}"#,
        );
    });

    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let executor: Arc<dyn ToolExecutor> = Arc::new(StubExecutor { calls: calls.clone() });
    let worker = OpenAiWorker::new(
        "mock",
        &format!("http://127.0.0.1:{port}/v1"),
        vec!["test-key".into()],
        vec!["mock-model".into()],
        vec![],
        None,
    )
    .with_tool_executor(executor);

    let request = ExecuteRequest {
        role: Role::Generator,
        instruction: "use the echo tool".into(),
        context: ContextPacket::default(),
        workspace: std::path::PathBuf::from("."),
        constraints: Constraints::default(),
        tools: vec![ToolSpec {
            id: "srv.echo".into(),
            description: "echo text".into(),
            schema: serde_json::json!({"type": "object"}),
        }],
        mcp_servers: Vec::new(),
    };

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WorkerEvent>();
    let result = worker.execute(request, tx, CancellationToken::new()).await.unwrap();
    server.join().unwrap();

    // The model's final answer is returned.
    assert_eq!(result.result, "the tool said hi");
    // The tool was executed once with the model's arguments.
    let recorded = calls.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].0, "srv.echo");
    assert_eq!(recorded[0].1, serde_json::json!({"text": "hi"}));
    // The tool result was fed back into the second request.
    assert_eq!(saw_tool_result.load(Ordering::SeqCst), 1);
    // A ToolUse event was emitted for visibility.
    let mut tool_use_seen = false;
    while let Ok(ev) = rx.try_recv() {
        if matches!(ev, WorkerEvent::ToolUse(_)) {
            tool_use_seen = true;
        }
    }
    assert!(tool_use_seen, "loop should emit a ToolUse event");
}
