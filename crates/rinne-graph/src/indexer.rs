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
}

fn warm_background(graph: Arc<Graph>, root: PathBuf) {
    let mut stack = vec![root.clone()];

    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!("indexer warm: read_dir({:?}): {}", dir, e);
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
                let rel = match path.strip_prefix(&root) {
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
                    tracing::warn!("indexer warm: ensure_current({:?}): {}", rel, e);
                }

                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
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

        let graph = Arc::new(crate::Graph::open(&dir.join("state.db")).unwrap());
        let indexer = Indexer::new(graph.clone(), dir.clone());
        indexer.ensure_now(&file);

        use rinne_types::graph::CodeGraph;
        assert!(CodeGraph::neighborhood(graph.as_ref(), "helper").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_now_uses_forward_slash_key() {
        let dir = std::env::temp_dir().join(format!("rinne-fwd-{}", std::process::id()));
        let nested = dir.join("src").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        let file = nested.join("mod.rs");
        std::fs::write(&file, "fn nested_fn() {}\n").unwrap();

        let graph = Arc::new(crate::Graph::open(&dir.join("state.db")).unwrap());
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
