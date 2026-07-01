//! The MCP client: connect a transport, run the `initialize` handshake, and
//! call `tools/list` / `tools/call`.
//!
//! Every request is bounded by a timeout so a slow or hung server can never
//! freeze Rinne — a real risk because the catalog scan and host setup connect to
//! every configured server at plan/run startup (`MCP_SKILLS.md` §10).

use std::time::Duration;

use serde_json::{json, Value};
use tokio::time::timeout;

use rinne_types::{Result, RinneError};

use crate::protocol::{Tool, PROTOCOL_VERSION};
use crate::transport::{HttpTransport, StdioTransport, Transport};

/// Default per-request timeout. Generous enough for a tool that does real work
/// (a web search, a DB query), tight enough that a hung server is cut loose.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Handshake timeout — tighter than a tool call, since the catalog scan and host
/// setup connect to every server at startup and a healthy server initializes
/// instantly. Bounds how long one bad server can delay a run starting.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

fn mcp(msg: impl Into<String>) -> RinneError {
    RinneError::Mcp(msg.into())
}

/// A connected, initialized MCP client over one server.
pub struct McpClient {
    transport: Box<dyn Transport>,
    server_name: Option<String>,
    request_timeout: Duration,
}

impl McpClient {
    /// Spawn a local stdio server (`command args`, with extra `env`) and
    /// complete the handshake.
    pub async fn connect_stdio(
        command: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Result<Self> {
        let transport = StdioTransport::spawn(command, args, env)?;
        Self::handshake(Box::new(transport)).await
    }

    /// Connect to a remote Streamable HTTP server and complete the handshake.
    /// `headers` carries auth (e.g. `("authorization", "Bearer …")`).
    pub async fn connect_http(url: &str, headers: Vec<(String, String)>) -> Result<Self> {
        let transport = HttpTransport::new(url, headers);
        Self::handshake(Box::new(transport)).await
    }

    async fn handshake(mut transport: Box<dyn Transport>) -> Result<Self> {
        let init = timeout(
            CONNECT_TIMEOUT,
            transport.request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "rinne", "version": env!("CARGO_PKG_VERSION") },
                }),
            ),
        )
        .await
        .map_err(|_| mcp("server did not complete the handshake in time"))??;
        let server_name = init
            .get("serverInfo")
            .and_then(|s| s.get("name"))
            .and_then(|n| n.as_str())
            .map(String::from);
        // Per spec, tell the server we're ready before any other calls.
        timeout(
            CONNECT_TIMEOUT,
            transport.notify("notifications/initialized", json!({})),
        )
        .await
        .map_err(|_| mcp("sending the initialized notification timed out"))??;
        Ok(Self {
            transport,
            server_name,
            request_timeout: DEFAULT_TIMEOUT,
        })
    }

    /// Override the per-request timeout (the handshake already used the default).
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// The server's self-reported name from the handshake, if any.
    pub fn server_name(&self) -> Option<&str> {
        self.server_name.as_deref()
    }

    /// List the tools this server exposes (`tools/list`).
    pub async fn list_tools(&mut self) -> Result<Vec<Tool>> {
        let res = timeout(self.request_timeout, self.transport.request("tools/list", json!({})))
            .await
            .map_err(|_| mcp("tools/list timed out"))??;
        let tools = res
            .get("tools")
            .and_then(|t| t.as_array())
            .map(|arr| arr.iter().filter_map(Tool::from_value).collect())
            .unwrap_or_default();
        Ok(tools)
    }

    /// Invoke a tool (`tools/call`), returning its raw result. Used by the host
    /// path's agentic loop (Phase B).
    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        timeout(
            self.request_timeout,
            self.transport
                .request("tools/call", json!({ "name": name, "arguments": arguments })),
        )
        .await
        .map_err(|_| mcp(format!("tool `{name}` timed out")))?
    }
}
