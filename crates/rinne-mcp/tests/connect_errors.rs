//! Connecting to a server that can't be reached fails cleanly and fast, rather
//! than panicking or hanging — the contract the best-effort catalog/pool rely on.

use rinne_mcp::McpClient;

#[tokio::test]
async fn stdio_connect_to_missing_binary_errors() {
    let r = McpClient::connect_stdio("rinne-no-such-binary-xyz-123", &[], &[]).await;
    assert!(r.is_err(), "spawning a missing binary must surface an error");
}

#[tokio::test]
async fn http_connect_to_dead_endpoint_errors() {
    // Port 1 on localhost: nothing listens, the connection is refused promptly.
    let r = McpClient::connect_http("http://127.0.0.1:1/mcp", vec![]).await;
    assert!(r.is_err(), "an unreachable endpoint must surface an error");
}
