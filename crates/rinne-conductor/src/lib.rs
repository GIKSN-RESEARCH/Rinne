//! `rinne-conductor` — prompt assembly, plan parsing, and backend client
//! (`CONTEXT.md` §7; `PHASE.md` P4).
//!
//! The conductor is the brain that plans and routes. It does no work itself and
//! runs prompted on a cheap, decoupled backend. It turns a goal plus blackboard
//! state into a JSON DAG, tolerating messy model output at the boundary and
//! falling back across backends when one is unavailable.

pub mod backend;
pub mod conductor;
pub mod parse;
pub mod prompt;

pub use backend::{
    conductor_base_url, conductor_credential, resolve_openai, HarnessBackend, OpenAiBackend,
    PlanBackend,
};
pub use conductor::Conductor;
pub use parse::parse_plan;
pub use prompt::ConductorInput;
