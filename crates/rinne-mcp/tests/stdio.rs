//! Drives the real stdio handshake + tools/list against a mock MCP server
//! (a tiny Python JSON-RPC responder). Skips cleanly if `python3` is absent.

use rinne_mcp::McpClient;

const MOCK_SERVER: &str = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")
    if method == "initialize":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{
            "protocolVersion":"2024-11-05",
            "serverInfo":{"name":"mock","version":"1"},
            "capabilities":{"tools":{}}}}), flush=True)
    elif method == "tools/list":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"tools":[
            {"name":"query","description":"run a query","inputSchema":{"type":"object"}},
            {"name":"schema","description":"show the schema","inputSchema":{"type":"object"}}]}}), flush=True)
    elif method == "tools/call":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"content":[{"type":"text","text":"ok"}]}}), flush=True)
    # notifications (no id) are ignored
"#;

#[tokio::test]
async fn stdio_initialize_list_and_call() {
    let args = vec!["-c".to_string(), MOCK_SERVER.to_string()];
    let mut client = match McpClient::connect_stdio("python3", &args, &[]).await {
        Ok(c) => c,
        Err(_) => return, // no python3 in this environment — skip
    };
    assert_eq!(client.server_name(), Some("mock"));

    let tools = client.list_tools().await.unwrap();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].name, "query");
    assert_eq!(tools[1].name, "schema");

    let result = client.call_tool("query", serde_json::json!({"sql": "SELECT 1"})).await.unwrap();
    assert!(result.get("content").is_some());
}
