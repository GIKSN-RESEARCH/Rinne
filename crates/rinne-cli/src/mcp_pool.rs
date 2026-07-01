//! A warm pool of MCP connections (`MCP_SKILLS.md` §6, §10). Connections are
//! opened lazily on first use and reused for the life of the pool, so the
//! plan-time catalog scan and the run-time host tool loop share one live
//! connection per server instead of reconnecting each call.
//!
//! `McpClient` calls take `&mut self`, so each client sits behind its own async
//! mutex; the pool's client map sits behind another. Both are `tokio::Mutex`
//! since the locks are held across `.await`.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::Mutex;

use rinne_config::model::{Config, McpServer};
use rinne_core::ToolExecutor;
use rinne_mcp::{McpClient, Tool};

/// A lazily-connected, reused set of MCP server connections.
pub struct McpPool {
    /// Snapshot of enabled servers at construction (name → config).
    servers: HashMap<String, McpServer>,
    /// Live connections, opened on demand.
    clients: Mutex<HashMap<String, Arc<Mutex<McpClient>>>>,
}

impl McpPool {
    /// Build a pool over the enabled MCP servers in `config`. No connections are
    /// opened yet.
    pub fn from_config(config: &Config) -> Self {
        let servers = config
            .mcp
            .servers
            .iter()
            .filter(|(_, s)| s.enabled)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        Self {
            servers,
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// Whether any enabled servers are configured (cheap; no connect).
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// Get the live client for a server, connecting on first use. The client map
    /// lock is held across the connect so two callers never double-dial the same
    /// server.
    async fn client(&self, name: &str) -> std::result::Result<Arc<Mutex<McpClient>>, String> {
        let mut map = self.clients.lock().await;
        if let Some(c) = map.get(name) {
            return Ok(c.clone());
        }
        let server = self
            .servers
            .get(name)
            .ok_or_else(|| format!("no enabled MCP server named `{name}`"))?;
        let client = crate::commands::mcp::connect_client(name, server).await?;
        let arc = Arc::new(Mutex::new(client));
        map.insert(name.to_string(), arc.clone());
        Ok(arc)
    }

    /// Every tool across all enabled servers, as `(qualified_id, tool)` where the
    /// id is `server.tool`. Respects each server's `tools_allow`. Best-effort: an
    /// unreachable server is logged and skipped, never fatal.
    pub async fn list_all_tools(&self) -> Vec<(String, Tool)> {
        let mut out = Vec::new();
        for (name, server) in &self.servers {
            match self.client(name).await {
                Ok(client) => {
                    let tools = client.lock().await.list_tools().await;
                    match tools {
                        Ok(tools) => {
                            for t in tools {
                                if server.allows_tool(&t.name) {
                                    out.push((format!("{name}.{}", t.name), t));
                                }
                            }
                        }
                        Err(e) => tracing::warn!("listing tools for mcp `{name}` failed: {e}"),
                    }
                }
                Err(e) => tracing::warn!("mcp server `{name}` unreachable: {e}"),
            }
        }
        out
    }

    /// Invoke a tool by qualified id `server.tool`, returning its raw JSON result.
    pub async fn call(&self, id: &str, arguments: Value) -> std::result::Result<Value, String> {
        let (server, tool) = id
            .split_once('.')
            .ok_or_else(|| format!("tool id `{id}` is not `server.tool`"))?;
        let client = self.client(server).await?;
        let result = client.lock().await.call_tool(tool, arguments).await;
        result.map_err(|e| e.to_string())
    }
}

/// A [`ToolExecutor`] over a warm [`McpPool`] — the bridge the host agentic loop
/// calls to run a tool, keeping the worker adapters MCP-agnostic.
pub struct McpToolExecutor {
    pool: Arc<McpPool>,
}

impl McpToolExecutor {
    pub fn new(pool: Arc<McpPool>) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ToolExecutor for McpToolExecutor {
    async fn call(&self, id: &str, arguments: Value) -> std::result::Result<String, String> {
        let result = self.pool.call(id, arguments).await?;
        Ok(render_tool_result(&result))
    }
}

/// Flatten an MCP `tools/call` result into text the model can read. MCP returns
/// `{ content: [{type:"text", text}, …], isError? }`; we join the text parts and
/// fall back to compact JSON for non-text content.
fn render_tool_result(result: &Value) -> String {
    let mut out = String::new();
    if let Some(parts) = result.get("content").and_then(|c| c.as_array()) {
        for part in parts {
            if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            } else {
                out.push_str(&part.to_string());
            }
        }
    }
    if out.is_empty() {
        out = result.to_string();
    }
    if result.get("isError").and_then(|e| e.as_bool()) == Some(true) {
        out = format!("[tool reported an error] {out}");
    }
    out
}
