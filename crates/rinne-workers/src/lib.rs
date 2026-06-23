//! `rinne-workers` — concrete implementations of the [`rinne_core::Worker`]
//! contract: the two transports and the per-CLI adapters, plus the mock worker
//! used to test the loop engine (`CONTEXT.md` §8; `PHASE.md` P2).

pub mod adapters;
pub mod mock;
pub mod transport;

pub use mock::{MockScript, MockWorker};
