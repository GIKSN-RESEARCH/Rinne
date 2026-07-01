//! `rinne-mcp` — a thin, framework-free MCP (Model Context Protocol) client
//! (`MCP_SKILLS.md` §10).
//!
//! MCP is JSON-RPC 2.0 over `stdio` (local subprocess servers) or Streamable
//! `http` (remote servers). We hand-write the handful of messages we need —
//! `initialize`, `tools/list`, `tools/call` — rather than pull a framework, in
//! keeping with the locked "stay framework-free" principle (§14). The client
//! gives Rinne's host path the tools it sends a model and dispatches.

mod client;
mod protocol;
mod transport;

pub use client::McpClient;
pub use protocol::{RpcError, Tool, PROTOCOL_VERSION};
