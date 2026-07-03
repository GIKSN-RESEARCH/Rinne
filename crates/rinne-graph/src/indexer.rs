use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use rinne_types::skip;

use crate::Graph;

pub struct Indexer {
    graph: Arc<Graph>,
    root: PathBuf,
}

impl Indexer {
    /// Create an indexer without starting a background warm thread.
    pub fn new(graph: Arc<Graph>, root: PathBuf) -> Indexer {
        Indexer { graph, root }
    }

    /// Create an indexer and immediately launch a background thread that walks
    /// `root`, indexing files not yet current. The thread is detached — it runs
    /// as a background citizen with brief sleeps between files.
    pub fn spawn(graph: Arc<Graph>, root: PathBuf) -> Indexer {
        let indexer = Indexer::new(graph, root);

        let graph_clone = Arc::clone(&indexer.graph);
        let root_clone = indexer.root.clone();

        std::thread::spawn(move || {
            warm_background(graph_clone, root_clone);
        });

        indexer
    }

    /// Synchronously re-index a single file from disk.
    ///
    /// Computes the repo-relative path (stripping `root`), gates on
    /// `source_lang` and size cap, reads the file, then calls
    /// `graph.ensure_current`. Errors are logged and swallowed.
    pub fn ensure_now(&self, path: &Path) {
        let rel = match path.strip_prefix(&self.root) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => return,
        };

        if skip::source_lang(&rel).is_none() {
            return;
        }

        let metadata = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("indexer: metadata({:?}): {}", path, e);
                return;
            }
        };

        if metadata.len() > skip::MAX_INDEX_FILE_BYTES {
            tracing::debug!("indexer: skipping oversized file {:?}", path);
            return;
        }

        let mtime = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let source = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("indexer: read({:?}): {}", path, e);
                return;
            }
        };

        if let Err(e) = self.graph.ensure_current(&rel, &source, mtime) {
            tracing::warn!("indexer: ensure_current({:?}): {}", rel, e);
        }
    }

    /// Synchronously walk the whole repo and index every supported, in-size
    /// source file, returning the number of files indexed. Unlike the background
    /// warm, this does not sleep between files — it is the on-demand path used by
    /// `rinne graph index`. Errors on individual files are logged and skipped.
    pub fn index_all(&self) -> usize {
        walk_and_index(&self.graph, &self.root, false)
    }
}

fn warm_background(graph: Arc<Graph>, root: PathBuf) {
    walk_and_index(&graph, &root, true);
}

/// Walk `root` and index each supported source file. When `throttle` is set, a
/// brief sleep between files keeps the background warm a good citizen; the
/// synchronous `index_all` path passes `false`. Returns the count indexed.
fn walk_and_index(graph: &Arc<Graph>, root: &Path, throttle: bool) -> usize {
    let mut indexed = 0;
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!("indexer walk: read_dir({:?}): {}", dir, e);
                continue;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };

            if file_type.is_dir() {
                let dir_name = entry.file_name();
                let name = dir_name.to_string_lossy();
                if !skip::is_skipped_dir(&name) {
                    stack.push(path);
                }
            } else if file_type.is_file() {
                let rel = match path.strip_prefix(root) {
                    Ok(r) => r.to_string_lossy().replace('\\', "/"),
                    Err(_) => continue,
                };

                if skip::source_lang(&rel).is_none() {
                    continue;
                }

                let meta = match std::fs::metadata(&path) {
                    Ok(m) => m,
                    Err(_) => continue,
                };

                if meta.len() > skip::MAX_INDEX_FILE_BYTES {
                    continue;
                }

                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);

                let source = match std::fs::read_to_string(&path) {
                    Ok(s) => s,
                    Err(_) => continue,
                };

                if let Err(e) = graph.ensure_current(&rel, &source, mtime) {
                    tracing::warn!("indexer walk: ensure_current({:?}): {}", rel, e);
                    continue;
                }
                indexed += 1;

                if throttle {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }

    indexed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn ensure_now_indexes_from_disk() {
        let dir = std::env::temp_dir().join(format!("rinne-idx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("m.rs");
        std::fs::write(&file, "fn helper() {}\nfn main() { helper(); }\n").unwrap();

        let graph = Arc::new(crate::Graph::open(&dir.join("state.db"), &dir).unwrap());
        let indexer = Indexer::new(graph.clone(), dir.clone());
        indexer.ensure_now(&file);

        use rinne_types::graph::CodeGraph;
        assert!(CodeGraph::neighborhood(graph.as_ref(), "helper").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn index_all_walks_repo_and_counts_files() {
        let dir = std::env::temp_dir().join(format!("rinne-idxall-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap(); // skipped dir
        std::fs::write(dir.join("src/a.rs"), "fn alpha() { beta(); }\n").unwrap();
        std::fs::write(dir.join("src/b.py"), "def gamma():\n    pass\n").unwrap();
        std::fs::write(dir.join("README.md"), "not source\n").unwrap(); // unsupported
        std::fs::write(dir.join("target/junk.rs"), "fn skipme() {}\n").unwrap();

        let graph = Arc::new(crate::Graph::open(&dir.join("state.db"), &dir).unwrap());
        let indexer = Indexer::new(graph.clone(), dir.clone());
        let count = indexer.index_all();

        // Two supported files under non-skipped dirs; README (unsupported) and
        // target/ (skipped) excluded.
        assert_eq!(count, 2, "should index a.rs and b.py only");

        use rinne_types::graph::CodeGraph;
        assert!(CodeGraph::neighborhood(graph.as_ref(), "alpha").is_some());
        assert!(CodeGraph::neighborhood(graph.as_ref(), "gamma").is_some());
        assert!(
            CodeGraph::neighborhood(graph.as_ref(), "skipme").is_none(),
            "target/ must be skipped"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_now_uses_forward_slash_key() {
        let dir = std::env::temp_dir().join(format!("rinne-fwd-{}", std::process::id()));
        let nested = dir.join("src").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        let file = nested.join("mod.rs");
        std::fs::write(&file, "fn nested_fn() {}\n").unwrap();

        let graph = Arc::new(crate::Graph::open(&dir.join("state.db"), &dir).unwrap());
        let indexer = Indexer::new(graph.clone(), dir.clone());
        indexer.ensure_now(&file);

        use rinne_types::graph::CodeGraph;

        // Symbol must resolve — proves the key the indexer stored is reachable.
        assert!(
            CodeGraph::neighborhood(graph.as_ref(), "nested_fn").is_some(),
            "nested_fn should be indexed and resolvable"
        );

        // Key must not contain backslashes — proves forward-slash normalization.
        let names = CodeGraph::symbol_names(graph.as_ref());
        // The stored path key is part of every symbol's qualified name; none should contain '\'.
        for name in &names {
            assert!(
                !name.contains('\\'),
                "symbol name/key contains backslash: {name}"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
