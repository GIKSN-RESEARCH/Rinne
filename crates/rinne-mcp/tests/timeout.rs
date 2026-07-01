//! A hung server must not freeze the client: `tools/list` against a server that
//! handshakes but never answers further requests returns a timeout error fast,
//! not an infinite hang. Skips cleanly if `python3` is absent.

use std::time::{Duration, Instant};

use rinne_mcp::McpClient;

/// Responds to `initialize`, then goes silent — every later request hangs.
const HUNG_SERVER: &str = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    if msg.get("method") == "initialize":
        print(json.dumps({"jsonrpc":"2.0","id":msg.get("id"),"result":{
            "protocolVersion":"2024-11-05",
            "serverInfo":{"name":"hung","version":"1"},
            "capabilities":{"tools":{}}}}), flush=True)
    # everything else (initialized notification, tools/list): no reply
"#;

#[tokio::test]
async fn list_tools_times_out_instead_of_hanging() {
    let args = vec!["-c".to_string(), HUNG_SERVER.to_string()];
    let client = match McpClient::connect_stdio("python3", &args, &[]).await {
        Ok(c) => c,
        Err(_) => return, // no python3 — skip
    };
    // Tighten the timeout so the test is fast; the handshake already succeeded.
    let mut client = client.with_request_timeout(Duration::from_millis(400));

    let started = Instant::now();
    let result = client.list_tools().await;
    let elapsed = started.elapsed();

    assert!(result.is_err(), "a silent server must surface an error, not hang");
    assert!(
        result.unwrap_err().to_string().contains("timed out"),
        "the error should name the timeout"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "list_tools returned in {elapsed:?} — the timeout did not bound it"
    );
}
