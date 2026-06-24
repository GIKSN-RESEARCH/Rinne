//! Worker transports (`CONTEXT.md` §8, §14).
//!
//! Two transports in v1, behind one conceptual trait: `subprocess-json` for
//! harness CLIs and `http` for API workers and the conductor backend. The `acp`
//! JSON-RPC transport is deferred to V2.

pub mod http;
pub mod subprocess;

pub use http::{
    normalize_base_url, ChatBackend, ChatMessage, ChatRequest, ChatResponse, DiscoveredModel,
    OpenAiClient,
};
pub use subprocess::{raw_lines, run, SubprocessOutput, SubprocessSpec};
