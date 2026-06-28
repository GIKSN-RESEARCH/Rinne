//! The replanner hook (`CONTEXT.md` §12).
//!
//! On repeated failure, a wrong-approach verdict, or genuinely new information,
//! the engine asks the conductor to amend the DAG rather than grinding the same
//! node. Core defines the seam; `rinne-conductor` implements it, so core stays
//! free of any dependency on the conductor.

use async_trait::async_trait;

use crate::dag::Plan;
use crate::Result;

/// Amends a plan in light of new state. Implemented by the conductor.
#[async_trait]
pub trait Replanner: Send + Sync {
    /// Produce an amended plan given the goal, a digest of current state, and
    /// the plan being revised.
    async fn replan(&self, goal: &str, digest: &str, current: &Plan) -> Result<Plan>;
}
