//! MCP wire types (JSON-RPC 2.0) and the public `Tool` shape.
//!
//! MCP is JSON-RPC 2.0: an `initialize` handshake, then `tools/list`,
//! `tools/call`, etc. We hand-write the few messages we need rather than pull a
//! framework (`MCP_SKILLS.md` §14, stay framework-free).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The MCP protocol revision we speak.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// A JSON-RPC request (has an `id`; expects a response).
#[derive(Serialize)]
pub(crate) struct Request<'a> {
    pub jsonrpc: &'static str,
    pub id: i64,
    pub method: &'a str,
    pub params: Value,
}

/// A JSON-RPC notification (no `id`; no response expected).
#[derive(Serialize)]
pub(crate) struct Notification<'a> {
    pub jsonrpc: &'static str,
    pub method: &'a str,
    pub params: Value,
}

/// A parsed JSON-RPC response off the wire: `id` plus `result` or `error`.
/// Server-initiated notifications (no `id`) are skipped by the transport.
#[derive(Deserialize, Default)]
pub(crate) struct Incoming {
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<RpcError>,
}

/// A JSON-RPC error object.
#[derive(Deserialize, Debug, Clone)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

/// A tool an MCP server advertises (`tools/list`).
#[derive(Debug, Clone, Serialize)]
pub struct Tool {
    /// The tool name, unique within its server (e.g. `query`).
    pub name: String,
    /// A one-line description — the cheap layer the conductor plans over.
    pub description: String,
    /// The JSON Schema for the tool's arguments (sent to a model as a function
    /// definition on the host path).
    pub input_schema: Value,
}

impl Tool {
    /// Parse a tool object from a `tools/list` result entry, tolerating both
    /// `inputSchema` (spec) and `input_schema`.
    pub(crate) fn from_value(v: &Value) -> Option<Tool> {
        let name = v.get("name")?.as_str()?.to_string();
        let description = v
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string();
        let input_schema = v
            .get("inputSchema")
            .or_else(|| v.get("input_schema"))
            .cloned()
            .unwrap_or_else(|| serde_json::json!({"type": "object"}));
        Some(Tool {
            name,
            description,
            input_schema,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Tool;
    use serde_json::json;

    #[test]
    fn parses_tool_from_list_entry() {
        let v = json!({
            "name": "query",
            "description": "run a SQL query",
            "inputSchema": { "type": "object", "properties": { "sql": { "type": "string" } } }
        });
        let t = Tool::from_value(&v).unwrap();
        assert_eq!(t.name, "query");
        assert_eq!(t.description, "run a SQL query");
        assert!(t.input_schema.get("properties").is_some());
    }

    #[test]
    fn tool_defaults_missing_fields() {
        let t = Tool::from_value(&json!({ "name": "ping" })).unwrap();
        assert_eq!(t.description, "");
        assert_eq!(t.input_schema, json!({ "type": "object" }));
        assert!(Tool::from_value(&json!({ "no_name": true })).is_none());
    }
}
