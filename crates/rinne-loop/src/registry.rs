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

    /// Resolve a worker for a node, accounting for whether it attaches MCP tools
    /// (`MCP_SKILLS.md` §6 tool-aware routing). Returns the worker and whether it
    /// can actually serve those tools.
    ///
    /// When `needs_tools`, a tool-capable worker (API host loop or a provisioning
    /// harness) is preferred over one that can't serve the tools — even over the
    /// `prefer` hint, since silently dropping a node's tools is worse than
    /// overriding a soft preference. Only when no capable worker satisfies `needs`
    /// does it fall back to the plain resolution, with `false` so the caller can
    /// narrate the degraded landing. With `needs_tools == false` this is exactly
    /// [`resolve`], always `true`.
    pub fn resolve_for(
        &self,
        needs: &[Capability],
        prefer: Option<&str>,
        needs_tools: bool,
    ) -> Option<(Arc<dyn Worker>, bool)> {
        if !needs_tools {
            return self.resolve(needs, prefer).map(|w| (w, true));
        }
        // The preferred worker, if it both satisfies needs and serves tools.
        if let Some(pref) = prefer {
            let want = parse_prefer(pref);
            if let Some(w) = self.workers.iter().find(|w| {
                w.descriptor().name == want
                    && w.descriptor().satisfies(needs)
                    && w.serves_mcp_tools()
            }) {
                return Some((Arc::clone(w), true));
            }
        }
        // Any needs-satisfier that can serve the tools.
        if let Some(w) = self
            .workers
            .iter()
            .find(|w| w.descriptor().satisfies(needs) && w.serves_mcp_tools())
        {
            return Some((Arc::clone(w), true));
        }
        // Degraded: a capable worker for the needs exists, but none can run the
        // tools. Run it anyway (the tools just won't be available) and let the
        // caller surface that.
        self.resolve(needs, prefer).map(|w| (w, false))
    }
}

/// Extract the worker name from a `prefer` string like `harness:claude-code`,
/// `api:gpt-5.5`, or a bare `claude-code`. The family prefix is a hint; the name
/// is what resolution matches on.
pub fn parse_prefer(prefer: &str) -> &str {
    prefer.split_once(':').map(|(_, name)| name).unwrap_or(prefer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker::{
        AuthMode, EventSink, ExecuteRequest, ExecuteResult, LatencyProfile, QuotaModel, Transport,
        WorkerDescriptor,
    };
    use async_trait::async_trait;
    use tokio_util::sync::CancellationToken;

    /// A worker that satisfies `code-edit`, with a configurable tool capability.
    struct Fake {
        desc: WorkerDescriptor,
        tools: bool,
    }

    #[async_trait]
    impl Worker for Fake {
        fn descriptor(&self) -> &WorkerDescriptor {
            &self.desc
        }
        fn serves_mcp_tools(&self) -> bool {
            self.tools
        }
        async fn execute(
            &self,
            _r: ExecuteRequest,
            _e: EventSink,
            _c: CancellationToken,
        ) -> crate::Result<ExecuteResult> {
            unimplemented!("not exercised by routing tests")
        }
    }

    fn fake(name: &str, family: WorkerFamily, tools: bool) -> Arc<dyn Worker> {
        Arc::new(Fake {
            desc: WorkerDescriptor {
                name: name.into(),
                family,
                capabilities: vec![Capability::CodeEdit],
                auth_mode: AuthMode::ApiKey,
                quota: QuotaModel::unlimited(),
                latency: LatencyProfile::Fast,
                transport: Transport::Http,
                models: vec![],
            },
            tools,
        })
    }

    #[test]
    fn tool_node_prefers_a_tool_capable_worker() {
        let mut reg = WorkerRegistry::new();
        reg.register(fake("plain-harness", WorkerFamily::Harness, false)); // first, not tool-capable
        reg.register(fake("api", WorkerFamily::Api, true)); // tool-capable, second
        let needs = vec![Capability::CodeEdit];

        // No tools → first registered wins (unchanged behaviour).
        let (w, ok) = reg.resolve_for(&needs, None, false).unwrap();
        assert_eq!(w.descriptor().name, "plain-harness");
        assert!(ok);

        // Tools → the tool-capable worker wins despite being second.
        let (w, ok) = reg.resolve_for(&needs, None, true).unwrap();
        assert_eq!(w.descriptor().name, "api");
        assert!(ok);
    }

    #[test]
    fn degrades_when_no_worker_serves_tools() {
        let mut reg = WorkerRegistry::new();
        reg.register(fake("plain", WorkerFamily::Harness, false));
        let (w, ok) = reg
            .resolve_for(&[Capability::CodeEdit], None, true)
            .unwrap();
        assert_eq!(w.descriptor().name, "plain");
        assert!(!ok, "tools not servable → degraded landing");
    }

    #[test]
    fn incapable_prefer_is_overridden_for_a_tool_node() {
        let mut reg = WorkerRegistry::new();
        reg.register(fake("codex", WorkerFamily::Harness, false));
        reg.register(fake("claude", WorkerFamily::Harness, true));
        let needs = vec![Capability::CodeEdit];

        // Preferring the incapable worker is overridden to keep the tools.
        let (w, ok) = reg
            .resolve_for(&needs, Some("harness:codex"), true)
            .unwrap();
        assert_eq!(w.descriptor().name, "claude");
        assert!(ok);

        // Preferring the capable worker is honoured.
        let (w, _) = reg
            .resolve_for(&needs, Some("harness:claude"), true)
            .unwrap();
        assert_eq!(w.descriptor().name, "claude");
    }
}
