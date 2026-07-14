//! Deterministic KT derivations over the graph + resolved cluster. No AI:
//! entry points (where to start reading), blast radius (what a change touches),
//! and ranked files (the few that hold the logic).

use rinne_types::graph::{CodeGraph, SymbolRef};

use crate::learn::Cluster;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryPoint {
    pub name: String,
    pub file: String,
    pub external_callers: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRank {
    pub file: String,
    pub symbols: usize,
    pub inbound: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Impact {
    pub name: String,
    pub caller_count: usize,
    pub caller_files: usize,
}

#[derive(Debug, Clone, Default)]
pub struct FdeFacts {
    pub entry_points: Vec<EntryPoint>,
    pub ranked_files: Vec<FileRank>,
    pub blast_radius: Vec<Impact>,
}

pub fn fde_facts(graph: &dyn CodeGraph, cluster: &Cluster) -> FdeFacts {
    let in_cluster: std::collections::HashSet<&str> =
        cluster.symbols.iter().map(|s| s.name.as_str()).collect();

    let mut entry_points: Vec<EntryPoint> = Vec::new();
    let mut blast_radius: Vec<Impact> = Vec::new();
    // inbound reference count per cluster file.
    let mut inbound_by_file: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    for sym in &cluster.symbols {
        let Some(nb) = graph.neighborhood(&sym.name) else { continue };
        // Callers coming from OUTSIDE the cluster define the change surface.
        let external: Vec<&SymbolRef> = nb
            .callers
            .iter()
            .filter(|c| !in_cluster.contains(c.name.as_str()))
            .collect();
        let caller_files = {
            let mut fs: Vec<&str> = external.iter().map(|c| c.file.as_str()).collect();
            fs.sort();
            fs.dedup();
            fs.len()
        };
        if !external.is_empty() {
            entry_points.push(EntryPoint {
                name: sym.name.clone(),
                file: sym.file.clone(),
                external_callers: external.len(),
            });
        }
        blast_radius.push(Impact {
            name: sym.name.clone(),
            caller_count: nb.callers.len(),
            caller_files,
        });
        *inbound_by_file.entry(sym.file.clone()).or_default() += nb.callers.len();
    }

    entry_points.sort_by(|a, b| b.external_callers.cmp(&a.external_callers).then_with(|| a.name.cmp(&b.name)));
    blast_radius.sort_by(|a, b| b.caller_count.cmp(&a.caller_count).then_with(|| a.name.cmp(&b.name)));

    let mut symbols_by_file: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for s in &cluster.symbols {
        *symbols_by_file.entry(s.file.as_str()).or_default() += 1;
    }
    let mut ranked_files: Vec<FileRank> = cluster
        .files
        .iter()
        .map(|f| FileRank {
            file: f.clone(),
            symbols: symbols_by_file.get(f.as_str()).copied().unwrap_or(0),
            inbound: inbound_by_file.get(f).copied().unwrap_or(0),
        })
        .collect();
    ranked_files.sort_by(|a, b| b.inbound.cmp(&a.inbound).then_with(|| b.symbols.cmp(&a.symbols)).then_with(|| a.file.cmp(&b.file)));

    FdeFacts { entry_points, ranked_files, blast_radius }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::learn::{Cluster, ClusterSymbol};
    use rinne_types::graph::{CodeGraph, Neighborhood, SymbolRef};

    struct G;
    impl CodeGraph for G {
        fn neighborhood(&self, s: &str) -> Option<Neighborhood> {
            match s {
                // `public_api` is called by two symbols OUTSIDE the cluster.
                "public_api" => Some(Neighborhood {
                    definition: SymbolRef { name: "public_api".into(), file: "api.rs".into(), line: 1, end_line: 5 },
                    callers: vec![
                        SymbolRef { name: "route_a".into(), file: "web.rs".into(), line: 2, end_line: 2 },
                        SymbolRef { name: "route_b".into(), file: "web.rs".into(), line: 9, end_line: 9 },
                    ],
                    callees: vec![
                        SymbolRef { name: "helper".into(), file: "api.rs".into(), line: 7, end_line: 7 },
                    ],
                    imports: vec![], stale: false,
                }),
                "helper" => Some(Neighborhood {
                    definition: SymbolRef { name: "helper".into(), file: "api.rs".into(), line: 7, end_line: 7 },
                    callers: vec![SymbolRef { name: "public_api".into(), file: "api.rs".into(), line: 1, end_line: 5 }],
                    callees: vec![], imports: vec![], stale: false,
                }),
                _ => None,
            }
        }
        fn resolve_in_file(&self, _: &str, _: &str) -> Option<SymbolRef> { None }
        fn symbol_names(&self) -> Vec<String> { vec!["public_api".into(), "helper".into()] }
    }

    fn cluster() -> Cluster {
        Cluster {
            topic: "api".into(),
            seeds: vec!["public_api".into()],
            symbols: vec![
                ClusterSymbol { name: "public_api".into(), file: "api.rs".into(), line: 1, end_line: 5, kind: "symbol".into() },
                ClusterSymbol { name: "helper".into(), file: "api.rs".into(), line: 7, end_line: 7, kind: "symbol".into() },
            ],
            files: vec!["api.rs".into()],
        }
    }

    #[test]
    fn entry_point_is_symbol_called_from_outside_cluster() {
        let facts = fde_facts(&G, &cluster());
        // `public_api` has 2 external callers (route_a, route_b in web.rs), not in cluster.
        // `helper` is only called by public_api (inside the cluster) → not an entry point.
        assert_eq!(facts.entry_points.first().map(|e| e.name.as_str()), Some("public_api"));
        assert_eq!(facts.entry_points[0].external_callers, 2);
        assert!(!facts.entry_points.iter().any(|e| e.name == "helper"), "internal helper is not an entry");
    }

    #[test]
    fn blast_radius_counts_external_callers_and_files() {
        let facts = fde_facts(&G, &cluster());
        let api = facts.blast_radius.iter().find(|i| i.name == "public_api").unwrap();
        assert_eq!(api.caller_count, 2);
        assert_eq!(api.caller_files, 1, "both callers live in web.rs");
    }
}
