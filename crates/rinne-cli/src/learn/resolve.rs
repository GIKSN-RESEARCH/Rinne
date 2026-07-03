//! Symbol resolution and clustering logic.

use rinne_types::graph::CodeGraph;

use crate::learn::{Cluster, ClusterSymbol};

/// Resolves a topic string into a `Cluster` by deterministic substring matching.
///
/// Seeds are symbols whose name or file path contains `topic` (case-insensitive).
/// One hop of callers and callees is added, then results are deduped by
/// (name, file, line) and truncated to `cap`. Files are sorted and deduplicated.
#[allow(dead_code)]
pub fn resolve_cluster(graph: &dyn CodeGraph, topic: &str, cap: usize) -> Cluster {
    let topic_lower = topic.to_lowercase();

    // Collect seeds: symbols whose name matches, or whose definition file matches.
    let names = graph.symbol_names();
    let mut symbols: Vec<ClusterSymbol> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String, u32)> = std::collections::HashSet::new();

    let mut seed_names: Vec<String> = Vec::new();

    for name in &names {
        let name_matches = name.to_lowercase().contains(&topic_lower);
        let file_matches = if !name_matches {
            graph
                .neighborhood(name)
                .map(|n| n.definition.file.to_lowercase().contains(&topic_lower))
                .unwrap_or(false)
        } else {
            false
        };

        if name_matches || file_matches {
            seed_names.push(name.clone());
        }
    }

    // Add seeds first, then expansion — order matters for deterministic cap truncation.
    for seed in &seed_names {
        if let Some(nbr) = graph.neighborhood(seed) {
            let key = (
                nbr.definition.name.clone(),
                nbr.definition.file.clone(),
                nbr.definition.line,
            );
            if seen.insert(key) {
                symbols.push(ClusterSymbol {
                    name: nbr.definition.name,
                    file: nbr.definition.file,
                    line: nbr.definition.line,
                    kind: "symbol".into(),
                });
            }
        }
    }

    // Expand one hop: callers + callees of each seed.
    for seed in &seed_names {
        if let Some(nbr) = graph.neighborhood(seed) {
            for sym_ref in nbr.callers.into_iter().chain(nbr.callees) {
                let key = (sym_ref.name.clone(), sym_ref.file.clone(), sym_ref.line);
                if seen.insert(key) {
                    symbols.push(ClusterSymbol {
                        name: sym_ref.name,
                        file: sym_ref.file,
                        line: sym_ref.line,
                        kind: "symbol".into(),
                    });
                }
            }
        }
    }

    // Truncate to cap.
    symbols.truncate(cap);

    // Build sorted, distinct file list.
    let mut files: Vec<String> = symbols.iter().map(|s| s.file.clone()).collect();
    files.sort();
    files.dedup();

    Cluster {
        topic: topic.to_string(),
        symbols,
        files,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::learn::Cluster;
    use rinne_types::graph::{CodeGraph, Neighborhood, SymbolRef};

    struct G;
    impl CodeGraph for G {
        fn neighborhood(&self, s: &str) -> Option<Neighborhood> {
            (s == "HarnessAdapter").then(|| Neighborhood {
                definition: SymbolRef { name: "HarnessAdapter".into(), file: "adapters/common.rs".into(), line: 10 },
                callers: vec![SymbolRef { name: "run_node".into(), file: "engine.rs".into(), line: 5 }],
                callees: vec![], imports: vec![], stale: false,
            })
        }
        fn resolve_in_file(&self, _: &str, _: &str) -> Option<SymbolRef> { None }
        fn symbol_names(&self) -> Vec<String> {
            vec!["HarnessAdapter".into(), "unrelated_thing".into()]
        }
    }

    #[test]
    fn resolves_topic_by_name_and_expands_one_hop() {
        let c = resolve_cluster(&G, "harness", 40);
        assert!(c.symbols.iter().any(|s| s.name == "HarnessAdapter"));
        assert!(c.symbols.iter().any(|s| s.name == "run_node"), "one-hop caller included");
        assert!(!c.symbols.iter().any(|s| s.name == "unrelated_thing"));
    }

    #[test]
    fn empty_when_no_match() {
        let c = resolve_cluster(&G, "zzz-nope", 40);
        assert!(c.symbols.is_empty());
    }

    #[test]
    fn cap_limits_symbol_count() {
        let c = resolve_cluster(&G, "harness", 1);
        assert_eq!(c.symbols.len(), 1);
        // Seed comes first, so HarnessAdapter should be the one kept.
        assert_eq!(c.symbols[0].name, "HarnessAdapter");
    }

    #[test]
    fn files_are_sorted_and_distinct() {
        let c = resolve_cluster(&G, "harness", 40);
        let mut sorted = c.files.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(c.files, sorted);
    }

    #[test]
    fn cluster_topic_matches_input() {
        let c: Cluster = resolve_cluster(&G, "harness", 40);
        assert_eq!(c.topic, "harness");
    }
}
