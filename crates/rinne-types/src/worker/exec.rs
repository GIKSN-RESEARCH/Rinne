//! The execution contract: what goes into a worker and what comes back
//! (`CONTEXT.md` §8).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The role a node plays in the DAG (`CONTEXT.md` §10). Defined here because
/// `execute` takes a role; the DAG types in Phase 3 reuse it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Planner,
    Generator,
    Evaluator,
    Synthesizer,
    Fixer,
}

/// The assembled context for one node, shaped per worker family by the context
/// assembler (`CONTEXT.md` §12).
///
/// For a harness worker the assembler writes a thin packet and **pins file
/// paths** (the worker reads the repo itself). For an API worker it **inlines
/// file contents** (the model sees only what is sent). Both forms are carried
/// here; an adapter consumes whichever fits its family.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextPacket {
    /// Pinned file paths for harness workers to read themselves.
    #[serde(default)]
    pub pinned_paths: Vec<PathBuf>,
    /// Inlined `(path, contents)` for API workers.
    #[serde(default)]
    pub inlined_files: Vec<InlinedFile>,
    /// Prior node outputs / blackboard digest text relevant to this node.
    #[serde(default)]
    pub prior_context: String,
    /// A critique artifact from a failed evaluator, fed back on loop-back
    /// (`CONTEXT.md` §10, §11).
    #[serde(default)]
    pub critique: Option<String>,
    /// Instruction text from any skills attached to this node (`MCP_SKILLS.md`
    /// §11). Injected verbatim into the worker's prompt, both families.
    #[serde(default)]
    pub skill_text: String,
}

/// A file inlined into an API worker's context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlinedFile {
    pub path: PathBuf,
    pub contents: String,
}

/// An MCP tool offered to a worker for the host agentic loop (`MCP_SKILLS.md`
/// §6). Carries the full argument schema; the cheap catalog layer the planner
/// plans over is only id+description.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    /// Qualified id `server.tool` — also the function name sent to the model.
    pub id: String,
    pub description: String,
    /// JSON Schema for the tool's arguments (the MCP `inputSchema`).
    pub schema: serde_json::Value,
}

/// How an MCP server is reached. Mirrors the config transport but lives here so
/// the worker adapters need no dependency on `rinne-config`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransportKind {
    Stdio,
    Http,
}

/// Everything a harness needs to launch/connect an MCP server itself — the
/// provision path (`MCP_SKILLS.md` §6). A harness with a node's `mcp_servers`
/// runs the tools natively rather than Rinne driving the host loop.
///
/// The `token` is the resolved secret, held in memory only: a provisioner must
/// reference it via environment expansion in the config it writes, never inline
/// it on disk (§9, §12).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerSpec {
    pub name: String,
    pub transport: McpTransportKind,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// Non-secret environment passed to a stdio server.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    #[serde(default)]
    pub url: Option<String>,
    /// Non-secret headers for an http server.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// The environment variable the server's token is referenced through (the
    /// var a stdio server reads, or the one an http `Authorization` header
    /// expands). `None` when the server needs no token.
    #[serde(default)]
    pub token_env: Option<String>,
    /// The resolved token value. In memory only — never serialized to a config
    /// file by a provisioner; injected into the harness subprocess environment
    /// and referenced via `token_env`.
    #[serde(default, skip_serializing)]
    pub token: Option<String>,
}

/// Per-invocation limits and steering (`CONTEXT.md` §10 budgets).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Constraints {
    /// Hard wall-clock timeout for this invocation, if any.
    pub timeout_secs: Option<u64>,
    /// Optional session id to continue cheap intra-worker context where the
    /// underlying tool supports it (`CONTEXT.md` §8).
    pub session_id: Option<String>,
    /// Ambient steering text captured from the user mid-run (`CONTEXT.md` §11).
    pub steer: Option<String>,
    /// Model the harness should run for this node, if selected. Passed to the
    /// CLI as e.g. `--model sonnet` / `-m grok-build`.
    pub model: Option<String>,
}

/// Everything a worker needs to do one unit of work (`CONTEXT.md` §8).
#[derive(Debug, Clone)]
pub struct ExecuteRequest {
    pub role: Role,
    pub instruction: String,
    pub context: ContextPacket,
    /// The repository / working directory the worker operates in.
    pub workspace: PathBuf,
    pub constraints: Constraints,
    /// MCP tools this node may call via the host agentic loop (`MCP_SKILLS.md`
    /// §6). Empty for nodes that attach none; only API workers act on it. Built
    /// by the engine from `node.tools` against the run's tool catalog.
    pub tools: Vec<ToolSpec>,
    /// MCP servers to provision into a harness so it calls the node's tools
    /// natively — the provision path (`MCP_SKILLS.md` §6). The harness sibling of
    /// `tools`: both are filled from `node.tools`; an API worker reads `tools`, a
    /// harness reads `mcp_servers`.
    pub mcp_servers: Vec<McpServerSpec>,
}

/// How an execution ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "status", content = "detail")]
pub enum ExecStatus {
    /// Completed normally.
    Success,
    /// The worker ran but reported failure (non-zero exit, error result).
    Failed(String),
    /// Exceeded its timeout.
    TimedOut,
    /// Cancelled via a cancellation token (`/pause`, budget kill, stuck abort).
    Cancelled,
}

impl ExecStatus {
    pub fn is_success(&self) -> bool {
        matches!(self, ExecStatus::Success)
    }
}

/// Token / time accounting for one invocation (`CONTEXT.md` §8 usage).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Wall-clock duration of the invocation.
    pub wall_ms: u64,
}

impl Usage {
    pub fn total_tokens(&self) -> u64 {
        self.prompt_tokens + self.completion_tokens
    }
}

/// The normalized result every adapter returns (`CONTEXT.md` §8).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteResult {
    /// The worker's primary textual output.
    pub result: String,
    /// A unified diff of file changes, if the worker edited the workspace.
    #[serde(default)]
    pub file_diff: Option<String>,
    /// The raw transcript of the worker's session (for `.rinne/transcripts/`).
    #[serde(default)]
    pub transcript: String,
    pub status: ExecStatus,
    pub usage: Usage,
    /// A session id the worker can be resumed with, if it supports continuation.
    #[serde(default)]
    pub session_id: Option<String>,
}
