//! `serves_mcp_tools` reflects whether tool support is actually wired
//! (`MCP_SKILLS.md` §6) — the signal tool-aware routing depends on.

use std::sync::Arc;

use async_trait::async_trait;

use rinne_core::worker::{ToolExecutor, Worker};
use rinne_workers::adapters::{claude_code, codex, openai_api::OpenAiWorker};

struct NoopExecutor;

#[async_trait]
impl ToolExecutor for NoopExecutor {
    async fn call(&self, _id: &str, _args: serde_json::Value) -> Result<String, String> {
        Ok(String::new())
    }
}

fn api(name: &str) -> OpenAiWorker {
    OpenAiWorker::new(
        name,
        "http://localhost/v1",
        vec!["key".into()],
        vec!["model".into()],
        vec![],
        None,
    )
}

#[test]
fn api_worker_serves_tools_only_with_an_executor() {
    assert!(!api("plain").serves_mcp_tools(), "no executor wired → no tools");
    let wired = api("wired").with_tool_executor(Arc::new(NoopExecutor));
    assert!(wired.serves_mcp_tools(), "executor wired → serves tools");
}

#[test]
fn harness_serves_tools_only_with_a_provisioner() {
    assert!(
        claude_code::worker().serves_mcp_tools(),
        "claude-code provisions MCP"
    );
    assert!(
        !codex::worker().serves_mcp_tools(),
        "codex has no provisioner yet"
    );
}
