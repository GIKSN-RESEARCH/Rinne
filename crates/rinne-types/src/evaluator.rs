//! The `Evaluator` trait seam (`MCP_SKILLS.md` §15).
//!
//! An evaluator grades a node and emits a gate decision. Tool, AI, and human
//! evaluators are impls behind this trait; the loop selects one by the node's
//! evaluator kind and grades through the seam, never through a concrete type.
//! The engine-specific services an evaluator needs (run a command, run an AI
//! worker) are provided by an [`EvalContext`] the loop implements, so the
//! evaluators stay free of engine internals.

use std::path::Path;

use async_trait::async_trait;

use crate::dag::{Node, OnFail};
use crate::Result;

/// An evaluator's decision on a node.
pub enum Gate {
    /// The node passes; the graph advances.
    Pass,
    /// The node fails; `on_fail` runs with this critique.
    Fail { critique: String, policy: OnFail },
    /// A human evaluator with no input yet — park for the user.
    Park { question: String },
}

/// Engine services an evaluator can call. The loop implements this; the
/// evaluator impls depend only on it, not on the engine. `?Send` because the
/// engine runs on a single-threaded runtime over a `!Sync` blackboard.
#[async_trait(?Send)]
pub trait EvalContext {
    /// The workspace the run operates on.
    fn workspace(&self) -> &Path;

    /// Run a tool evaluator's acceptance command for `node`; returns
    /// `(passed, combined_output)`.
    async fn run_tool(&self, node: &Node, command: &str, must_exit: i32) -> Result<(bool, String)>;

    /// Run an AI worker on evaluator `node` with `extra_instruction` appended;
    /// returns its review text, or `None` if no capable worker is available.
    async fn run_ai(&self, node: &Node, extra_instruction: &str) -> Result<Option<String>>;
}

/// One evaluation strategy. The loop dispatches grading through this seam.
#[async_trait(?Send)]
pub trait Evaluator {
    /// Grade `node`, using `ctx` for any engine services, into a [`Gate`].
    async fn grade(&self, node: &Node, ctx: &dyn EvalContext) -> Result<Gate>;
}
