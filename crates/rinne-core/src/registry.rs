//! The worker registry and capability resolution (`CONTEXT.md` §7, §13).
//!
//! The conductor assigns each node a capability requirement plus an optional
//! preferred worker; it does *not* hard-bind a concrete worker. The scheduler
//! resolves the concrete worker here, at dispatch time, from live availability —
//! so a node does not die because its preferred worker is unavailable
//! (`CONTEXT.md` §7 key design decision).

use std::sync::Arc;

use crate::worker::{Capability, Worker, WorkerFamily};

/// A set of available workers, in the user's preference order. The composition
/// root (the CLI) builds this from config + `doctor`; the engine consumes it.
#[derive(Clone, Default)]
pub struct WorkerRegistry {
    workers: Vec<Arc<dyn Worker>>,
}

impl WorkerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a worker. Insertion order is preference order: earlier wins ties.
    pub fn register(&mut self, worker: Arc<dyn Worker>) -> &mut Self {
        self.workers.push(worker);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.workers.is_empty()
    }

    pub fn len(&self) -> usize {
        self.workers.len()
    }

    /// All registered worker names, in preference order.
    pub fn names(&self) -> Vec<String> {
        self.workers
            .iter()
            .map(|w| w.descriptor().name.clone())
            .collect()
    }

    /// Clones of all worker descriptors, for handing the conductor the worker
    /// registry (`CONTEXT.md` §7).
    pub fn descriptors(&self) -> Vec<crate::worker::WorkerDescriptor> {
        self.workers.iter().map(|w| w.descriptor().clone()).collect()
    }

    /// The first registered (most-preferred) worker, if any. Used as the
    /// harness-conductor fallback.
    pub fn first(&self) -> Option<std::sync::Arc<dyn Worker>> {
        self.workers.first().map(std::sync::Arc::clone)
    }

    /// All registered workers of `family`, in preference order. Used to build the
    /// conductor's fallback chain so planning survives one harness failing.
    pub fn by_family(&self, family: WorkerFamily) -> Vec<Arc<dyn Worker>> {
        self.workers
            .iter()
            .filter(|w| w.descriptor().family == family)
            .map(Arc::clone)
            .collect()
    }

    /// Resolve a node's `needs` (and optional `prefer`) to a concrete worker.
    ///
    /// Resolution order (`CONTEXT.md` §13):
    ///   1. the preferred worker, if present *and* it satisfies `needs`;
    ///   2. otherwise the first registered worker that satisfies `needs`.
    pub fn resolve(&self, needs: &[Capability], prefer: Option<&str>) -> Option<Arc<dyn Worker>> {
        if let Some(pref) = prefer {
            let want = parse_prefer(pref);
            if let Some(w) = self.workers.iter().find(|w| {
                w.descriptor().name == want && w.descriptor().satisfies(needs)
            }) {
                return Some(Arc::clone(w));
            }
        }
        self.workers
            .iter()
            .find(|w| w.descriptor().satisfies(needs))
            .map(Arc::clone)
    }
}

/// Extract the worker name from a `prefer` string like `harness:claude-code`,
/// `api:gpt-5.5`, or a bare `claude-code`. The family prefix is a hint; the name
/// is what resolution matches on.
pub fn parse_prefer(prefer: &str) -> &str {
    prefer.split_once(':').map(|(_, name)| name).unwrap_or(prefer)
}
