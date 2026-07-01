//! The two MCP client transports: `stdio` (local subprocess servers) and
//! Streamable `http` (remote servers). Both speak the same JSON-RPC messages;
//! the [`Transport`] trait hides which is in use from the client.

use std::process::Stdio;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use rinne_types::{Result, RinneError};

use crate::protocol::{Incoming, Notification, Request};

fn mcp(msg: impl Into<String>) -> RinneError {
    RinneError::Mcp(msg.into())
}

/// One MCP transport. `request` sends a JSON-RPC request and returns its result;
/// `notify` sends a fire-and-forget notification.
#[async_trait]
pub(crate) trait Transport: Send {
    async fn request(&mut self, method: &str, params: Value) -> Result<Value>;
    async fn notify(&mut self, method: &str, params: Value) -> Result<()>;
}

// ----- stdio --------------------------------------------------------------

/// A local MCP server spawned as a subprocess, spoken to over newline-delimited
/// JSON-RPC on its stdin/stdout.
pub(crate) struct StdioTransport {
    #[allow(dead_code)] // kept alive (kill-on-drop) for the connection's lifetime
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl StdioTransport {
    pub(crate) fn spawn(command: &str, args: &[String], env: &[(String, String)]) -> Result<Self> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| mcp(format!("could not start `{command}`: {e}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| mcp("no stdin pipe to the server"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| mcp("no stdout pipe from the server"))?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        })
    }

    async fn write_line(&mut self, line: &str) -> Result<()> {
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| mcp(format!("write to server failed: {e}")))?;
        self.stdin
            .write_all(b"\n")
            .await
            .map_err(|e| mcp(format!("write to server failed: {e}")))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| mcp(format!("flush to server failed: {e}")))?;
        Ok(())
    }
}

#[async_trait]
impl Transport for StdioTransport {
    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let req = Request { jsonrpc: "2.0", id, method, params };
        let line = serde_json::to_string(&req).map_err(RinneError::Json)?;
        self.write_line(&line).await?;

        // Read messages until the response with our id arrives, skipping any
        // interleaved server notifications.
        loop {
            let mut buf = String::new();
            let n = self
                .stdout
                .read_line(&mut buf)
                .await
                .map_err(|e| mcp(format!("read from server failed: {e}")))?;
            if n == 0 {
                return Err(mcp("server closed the connection"));
            }
            let trimmed = buf.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(msg) = serde_json::from_str::<Incoming>(trimmed) else {
                continue; // not a JSON-RPC message we understand — skip
            };
            if msg.id == Some(id) {
                if let Some(err) = msg.error {
                    return Err(mcp(format!("{} (code {})", err.message, err.code)));
                }
                return Ok(msg.result.unwrap_or(Value::Null));
            }
            // a notification or a different id — ignore and keep reading
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let note = Notification { jsonrpc: "2.0", method, params };
        let line = serde_json::to_string(&note).map_err(RinneError::Json)?;
        self.write_line(&line).await
    }
}

// ----- http (Streamable HTTP) ---------------------------------------------

/// A remote MCP server reached over Streamable HTTP. Each request is a POST; the
/// server may answer with a single JSON body or an SSE stream — both are read
/// back to the one JSON-RPC response.
pub(crate) struct HttpTransport {
    client: reqwest::Client,
    url: String,
    headers: Vec<(String, String)>,
    session_id: Option<String>,
    next_id: i64,
}

impl HttpTransport {
    pub(crate) fn new(url: &str, headers: Vec<(String, String)>) -> Self {
        // A request timeout so a hung remote server can't freeze a request; the
        // client-level timeout in `McpClient` is the tighter, primary bound.
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(90))
            .build()
            .unwrap_or_default();
        Self {
            client,
            url: url.to_string(),
            headers,
            session_id: None,
            next_id: 1,
        }
    }

    async fn post(&mut self, body: &Value) -> Result<reqwest::Response> {
        let mut rb = self
            .client
            .post(&self.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .json(body);
        for (k, v) in &self.headers {
            rb = rb.header(k, v);
        }
        if let Some(sid) = &self.session_id {
            rb = rb.header("mcp-session-id", sid);
        }
        let resp = rb
            .send()
            .await
            .map_err(|e| mcp(format!("HTTP request failed: {e}")))?;
        if let Some(sid) = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            self.session_id = Some(sid.to_string());
        }
        Ok(resp)
    }
}

#[async_trait]
impl Transport for HttpTransport {
    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let body = serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let resp = self.post(&body).await?;
        let status = resp.status();
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let text = resp
            .text()
            .await
            .map_err(|e| mcp(format!("reading response body failed: {e}")))?;
        if !status.is_success() {
            let snippet: String = text.chars().take(200).collect();
            return Err(mcp(format!("HTTP {status}: {snippet}")));
        }
        let msg = if ct.contains("text/event-stream") {
            parse_sse(&text).ok_or_else(|| mcp("no JSON-RPC message in the SSE response"))?
        } else {
            serde_json::from_str::<Incoming>(text.trim())
                .map_err(|e| mcp(format!("bad JSON-RPC response: {e}")))?
        };
        if let Some(err) = msg.error {
            return Err(mcp(format!("{} (code {})", err.message, err.code)));
        }
        Ok(msg.result.unwrap_or(Value::Null))
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let body = serde_json::json!({"jsonrpc": "2.0", "method": method, "params": params});
        let _ = self.post(&body).await?; // notifications get a 202/empty; ignore body
        Ok(())
    }
}

/// Extract the first JSON-RPC message carrying a `result`/`error` from an SSE
/// body (`data:` lines).
fn parse_sse(body: &str) -> Option<Incoming> {
    for line in body.lines() {
        let line = line.trim_start();
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        if let Ok(msg) = serde_json::from_str::<Incoming>(data.trim()) {
            if msg.result.is_some() || msg.error.is_some() {
                return Some(msg);
            }
        }
    }
    None
}
