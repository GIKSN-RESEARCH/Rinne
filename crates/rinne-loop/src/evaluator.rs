//! Evaluator execution (`CONTEXT.md` §10, §12).
//!
//! Tool evaluators run a command and check its exit code — tool-grounded
//! grading, preferred where possible. AI evaluators return a review whose
//! verdict the engine parses. Human evaluators are handled by the engine's
//! parking logic.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tokio::process::Command;

use crate::dag::{Acceptance, Node, OnFail};
use crate::Result;
use rinne_types::{EvalContext, Evaluator, Gate};

/// The result of grading a node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalVerdict {
    pub passed: bool,
    /// On failure, the concrete critique fed back to the generator.
    pub output: String,
}

/// A tool evaluator: run the node's acceptance command, pass iff it exits as
/// required. The combined output is the critique on failure.
pub struct ToolEvaluator;

#[async_trait(?Send)]
impl Evaluator for ToolEvaluator {
    async fn grade(&self, node: &Node, ctx: &dyn EvalContext) -> Result<Gate> {
        let policy = node.parsed_on_fail().unwrap_or(OnFail::Replan);
        let Some(Acceptance { command, must_exit }) = node.acceptance.clone() else {
            return Ok(Gate::Fail {
                critique: "tool evaluator has no acceptance command".into(),
                policy,
            });
        };
        let (passed, output) = ctx.run_tool(node, &command, must_exit).await?;
        if passed {
            Ok(Gate::Pass)
        } else {
            Ok(Gate::Fail { critique: output, policy })
        }
    }
}

/// An AI evaluator: a worker reviews the work and ends with a machine-readable
/// verdict. Absent or `FAIL` fails closed; an explicit `REPLAN` triggers a replan.
pub struct AiEvaluator;

#[async_trait(?Send)]
impl Evaluator for AiEvaluator {
    async fn grade(&self, node: &Node, ctx: &dyn EvalContext) -> Result<Gate> {
        let Some(review) = ctx.run_ai(node, AI_VERDICT_INSTRUCTION).await? else {
            return Ok(Gate::Fail {
                critique: format!("no worker for AI evaluator needs {:?}", node.needs),
                policy: node.parsed_on_fail().unwrap_or(OnFail::Replan),
            });
        };
        // A model can explicitly call for a replan ("wrong approach").
        if review.to_uppercase().contains("VERDICT: REPLAN") {
            return Ok(Gate::Fail { critique: review, policy: OnFail::Replan });
        }
        let verdict = parse_ai_verdict(&review);
        if verdict.passed {
            Ok(Gate::Pass)
        } else {
            Ok(Gate::Fail {
                critique: verdict.output,
                policy: node.parsed_on_fail().unwrap_or(OnFail::Replan),
            })
        }
    }
}

/// A human evaluator: there is no automatic verdict, so it parks for the user.
pub struct HumanEvaluator;

#[async_trait(?Send)]
impl Evaluator for HumanEvaluator {
    async fn grade(&self, node: &Node, _ctx: &dyn EvalContext) -> Result<Gate> {
        Ok(Gate::Park {
            question: node.instruction.clone(),
        })
    }
}

/// Run a tool evaluator's acceptance command in `workspace`, passing iff the
/// process exits with `must_exit`. The combined output becomes the critique on
/// failure (`CONTEXT.md` §10 acceptance).
pub async fn run_tool(
    workspace: &Path,
    command: &str,
    must_exit: i32,
    timeout: Duration,
) -> Result<EvalVerdict> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let output = match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return Ok(EvalVerdict {
                passed: false,
                output: format!("failed to run `{command}`: {e}"),
            });
        }
        Err(_) => {
            return Ok(EvalVerdict {
                passed: false,
                output: format!("acceptance command timed out: `{command}`"),
            });
        }
    };

    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let passed = code == must_exit;

    let combined = format!(
        "$ {command}\nexit: {code} (required {must_exit})\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );

    Ok(EvalVerdict {
        passed,
        output: combined,
    })
}

/// Parse an AI evaluator's review into a verdict. The engine instructs AI
/// evaluators to end with `VERDICT: PASS` or `VERDICT: FAIL`; absent or
/// `FAIL` is treated as a failure (fail-closed), and the full review is the
/// critique.
pub fn parse_ai_verdict(review: &str) -> EvalVerdict {
    let upper = review.to_uppercase();
    let passed = upper.contains("VERDICT: PASS") || upper.contains("VERDICT:PASS");
    EvalVerdict {
        passed,
        output: review.to_string(),
    }
}

/// The instruction suffix appended to AI-evaluator prompts so their verdict is
/// machine-readable.
pub const AI_VERDICT_INSTRUCTION: &str =
    "\n\nEnd your review with a final line that is exactly `VERDICT: PASS` or `VERDICT: FAIL`.";
