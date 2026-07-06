//! `rinne-conductor` — prompt assembly, plan parsing, and backend client
//! (`CONTEXT.md` §7; `PHASE.md` P4).
//!
//! The conductor is the brain that plans and routes. It does no work itself and
//! runs prompted on a cheap, decoupled backend. It turns a goal plus blackboard
//! state into a JSON DAG, tolerating messy model output at the boundary and
//! falling back across backends when one is unavailable.

pub mod backend;
pub mod classifier;
pub mod conductor;
pub mod conductor_eligible;
pub mod ladder;
pub mod parse;
pub mod prompt;
pub mod routing;
pub mod tier_exemplars;

pub use backend::{
    conductor_base_url, conductor_credential, resolve_openai, resolve_openai_model, HarnessBackend,
    OpenAiBackend, PlanBackend,
};
pub use classifier::{classify_goal, Classification};
pub use conductor::Conductor;
pub use conductor_eligible::{filter_eligible_ladder, is_conductor_eligible};
pub use ladder::{ConductorLadder, EscalationReason};
pub use parse::parse_plan;
pub use prompt::{ConductorInput, SkillInfo, ToolInfo};
pub use routing::apply_routing;
