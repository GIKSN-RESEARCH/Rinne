//! Symbol resolution and clustering logic.

use rinne_types::graph::CodeGraph;

use crate::learn::{Cluster, ClusterSymbol};

/// Stop-words and filler that carry no code meaning, so they never become
/// match terms for a natural-language query like "what is layer 1".
const QUERY_STOP_WORDS: &[&str] = &[
    "the", "a", "an", "is", "are", "was", "were", "how", "does", "do", "what", "why", "when",
    "where", "which", "who", "of", "in", "on", "to", "for", "and", "or", "with", "about", "this",
    "that", "it", "work", "works", "explain", "tell", "me", "show", "code", "here",
];

/// Minimum length for a query term to be used as a substring match. Guards
/// against noise like single letters or "1" matching half the codebase.
const MIN_TERM_LEN: usize = 3;

/// Split a free-form query into distinct, lower-cased match terms: the whole
/// query plus each meaningful word. Stop-words and terms shorter than
/// [`MIN_TERM_LEN`] are dropped. Order is preserved (whole query first) so cap
/// truncation prefers the most specific matches.
fn query_terms(topic: &str) -> Vec<String> {
    let topic_lower = topic.trim().to_lowercase();
    let mut terms: Vec<String> = Vec::new();

    // The whole phrase first — an exact symbol/path fragment should still win.
    if topic_lower.len() >= MIN_TERM_LEN {
        terms.push(topic_lower.clone());
    }

    for word in topic_lower.split(|c: char| !c.is_alphanumeric()) {
        if word.len() >= MIN_TERM_LEN
            && !QUERY_STOP_WORDS.contains(&word)
            && !terms.iter().any(|t| t == word)
        {
            terms.push(word.to_string());
        }
    }
    terms
}

/// Resolves a topic string into a `Cluster` by deterministic term matching.
///
/// The query is broken into terms (the whole phrase plus each meaningful word,
/// see [`query_terms`]) so a natural-language topic like "what is the loop
/// engine" still resolves via its content words. Seeds are symbols whose name
/// or file path contains any term (case-insensitive). One hop of callers and
/// callees is added, then results are deduped by (name, file, line) and
/// truncated to `cap`. Files are sorted and deduplicated. Returns an empty
/// cluster when no term matches — the caller may then fall back to AI.
pub fn resolve_cluster(graph: &dyn CodeGraph, topic: &str, cap: usize) -> Cluster {
    let terms = query_terms(topic);

    // Collect seeds: symbols whose name matches, or whose definition file matches.
    let names = graph.symbol_names();
    let mut seed_names: Vec<String> = Vec::new();

    for name in &names {
        let name_lower = name.to_lowercase();
        let name_matches = terms.iter().any(|t| name_lower.contains(t.as_str()));
        let file_matches = if !name_matches {
            graph
                .neighborhood(name)
                .map(|n| {
                    let f = n.definition.file.to_lowercase();
                    terms.iter().any(|t| f.contains(t.as_str()))
                })
                .unwrap_or(false)
        } else {
            false
        };

        if name_matches || file_matches {
            seed_names.push(name.clone());
        }
    }

    cluster_from_seeds(graph, topic, &seed_names, cap)
}

/// Build a cluster from an explicit list of seed symbol names: add each seed's
/// definition, then one hop of callers and callees, dedup by (name, file, line),
/// truncate to `cap`, and derive a sorted, distinct file list.
///
/// Shared by [`resolve_cluster`] (literal seeds) and the AI-fallback path
/// (worker-picked seeds), so both produce identically-shaped clusters.
pub fn cluster_from_seeds(
    graph: &dyn CodeGraph,
    topic: &str,
    seed_names: &[String],
    cap: usize,
) -> Cluster {
    let mut symbols: Vec<ClusterSymbol> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String, u32)> =
        std::collections::HashSet::new();

    // Add seeds first, then expansion — order matters for deterministic cap truncation.
    for seed in seed_names {
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
                    end_line: nbr.definition.end_line,
                    kind: "symbol".into(),
                });
            }
        }
    }

    // Expand one hop: callers + callees of each seed.
    for seed in seed_names {
        if let Some(nbr) = graph.neighborhood(seed) {
            for sym_ref in nbr.callers.into_iter().chain(nbr.callees) {
                let key = (sym_ref.name.clone(), sym_ref.file.clone(), sym_ref.line);
                if seen.insert(key) {
                    symbols.push(ClusterSymbol {
                        name: sym_ref.name,
                        file: sym_ref.file,
                        line: sym_ref.line,
                        end_line: sym_ref.end_line,
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
        seeds: seed_names.to_vec(),
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
                definition: SymbolRef {
                    name: "HarnessAdapter".into(),
                    file: "adapters/common.rs".into(),
                    line: 10,
                    end_line: 10,
                },
                callers: vec![SymbolRef {
                    name: "run_node".into(),
                    file: "engine.rs".into(),
                    line: 5,
                    end_line: 5,
                }],
                callees: vec![],
                imports: vec![],
                stale: false,
            })
        }
        fn resolve_in_file(&self, _: &str, _: &str) -> Option<SymbolRef> {
            None
        }
        fn symbol_names(&self) -> Vec<String> {
            vec!["HarnessAdapter".into(), "unrelated_thing".into()]
        }
    }

    #[test]
    fn resolves_topic_by_name_and_expands_one_hop() {
        let c = resolve_cluster(&G, "harness", 40);
        assert!(c.symbols.iter().any(|s| s.name == "HarnessAdapter"));
        assert!(
            c.symbols.iter().any(|s| s.name == "run_node"),
            "one-hop caller included"
        );
        assert!(!c.symbols.iter().any(|s| s.name == "unrelated_thing"));
    }

    #[test]
    fn empty_when_no_match() {
        let c = resolve_cluster(&G, "zzz-nope", 40);
        assert!(c.symbols.is_empty());
    }

    #[test]
    fn resolves_natural_language_query_via_content_word() {
        // "what is the harness adapter" → stop-words dropped, "harness" matches.
        let c = resolve_cluster(&G, "what is the harness adapter", 40);
        assert!(
            c.symbols.iter().any(|s| s.name == "HarnessAdapter"),
            "content word should resolve NL query, got: {:?}",
            c.symbols
        );
    }

    #[test]
    fn resolves_topic_by_file_path_word() {
        // "common" appears in the definition file `adapters/common.rs`.
        let c = resolve_cluster(&G, "common", 40);
        assert!(c.symbols.iter().any(|s| s.name == "HarnessAdapter"));
    }

    #[test]
    fn stop_words_and_numbers_alone_dont_match_everything() {
        // "what is 1" is all stop-words + a too-short token → no terms → empty.
        let c = resolve_cluster(&G, "what is 1", 40);
        assert!(
            c.symbols.is_empty(),
            "pure filler query must not match, got: {:?}",
            c.symbols
        );
    }

    #[test]
    fn query_terms_splits_and_filters() {
        let t = query_terms("what is the HarnessAdapter layer");
        assert!(t.contains(&"harnessadapter".to_string()));
        assert!(t.contains(&"layer".to_string()));
        assert!(!t.iter().any(|w| w == "the" || w == "is" || w == "what"));
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

    #[test]
    fn cluster_from_seeds_expands_one_hop() {
        // Explicit seed (as the AI fallback would supply) → def + one-hop caller.
        let c = cluster_from_seeds(&G, "layer 1", &["HarnessAdapter".to_string()], 40);
        assert!(
            c.symbols.iter().any(|s| s.name == "HarnessAdapter"),
            "seed included"
        );
        assert!(
            c.symbols.iter().any(|s| s.name == "run_node"),
            "one-hop caller included"
        );
        assert_eq!(c.topic, "layer 1", "topic preserved verbatim");
    }

    #[test]
    fn cluster_from_seeds_ignores_unknown_seed() {
        let c = cluster_from_seeds(&G, "x", &["does_not_exist".to_string()], 40);
        assert!(c.symbols.is_empty());
    }
}
