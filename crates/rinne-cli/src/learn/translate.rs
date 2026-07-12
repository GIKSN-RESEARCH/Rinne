//! Translation of code and documentation into narrative.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use rinne_loop::worker::{
    ContextPacket, Constraints, ExecuteRequest, InlinedFile, Role, Worker,
};
use rinne_loop::WorkerRegistry;

use super::{LearnDoc, Narration};

/// Converts a [`LearnDoc`] into an optional [`Narration`].
///
/// Returns `None` on degradation (no AI, no worker) or any execution failure.
#[async_trait]
pub trait Translator: Send + Sync {
    async fn translate(&self, doc: &LearnDoc) -> Option<Narration>;
}

/// Always returns `None`. Used when `--no-ai` is passed or the registry is empty.
pub struct NullTranslator;

#[async_trait]
impl Translator for NullTranslator {
    async fn translate(&self, _doc: &LearnDoc) -> Option<Narration> {
        None
    }
}

/// Runs real workers from the pool to produce a [`Narration`].
///
/// Learning is an optional enhancement, so it must not make a document fail
/// merely because the user's preferred worker is temporarily unavailable. Keep
/// the registry ordering, but try its whole fallback chain before degrading to
/// the graph-derived document.
pub struct WorkerTranslator {
    workers: Vec<Arc<dyn Worker>>,
    workspace: PathBuf,
}

fn teach_prompt(doc: &LearnDoc) -> String {
    format!(
        "You are a senior engineer writing a concise architectural narration that teaches a reader this codebase area.\n\
         Topic: {topic}\n\n\
         Requirements:\n\
         - Structure the overview with Markdown headings (##), short paragraphs, and lists.\n\
         - Explain the design decisions, key components, and how the pieces fit together.\n\
         - Include AT LEAST ONE Mermaid diagram in a ```mermaid fenced block: a `flowchart` \
         showing how the main components connect, and a `sequenceDiagram` if there is a clear \
         request/response path. Keep node labels short; avoid characters that break Mermaid ids.\n\
         Output Markdown only (it will be rendered to HTML).",
        topic = doc.topic
    )
}

#[async_trait]
impl Translator for WorkerTranslator {
    async fn translate(&self, doc: &LearnDoc) -> Option<Narration> {
        let inlined_files: Vec<InlinedFile> = doc
            .snippets
            .iter()
            .map(|s| InlinedFile {
                path: PathBuf::from(&s.file),
                contents: format!("{}\n{}", s.doc, s.code),
            })
            .collect();

        let prior_context: String = doc
            .doc_sections
            .iter()
            .map(|ds| ds.body.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");

        let context = ContextPacket {
            inlined_files,
            prior_context,
            ..Default::default()
        };

        let req = ExecuteRequest {
            role: Role::Generator,
            instruction: teach_prompt(doc),
            context,
            workspace: self.workspace.clone(),
            constraints: Constraints {
                timeout_secs: Some(120),
                ..Default::default()
            },
            tools: vec![],
            mcp_servers: vec![],
        };

        // No-op sink: drop the receiver immediately; sends become silent no-ops.
        let (sink, _rx) = tokio::sync::mpsc::unbounded_channel();

        for worker in &self.workers {
            let result = worker
                .execute(req.clone(), sink.clone(), CancellationToken::new())
                .await;

            if let Ok(r) = result {
                if r.status.is_success() {
                    return Some(Narration {
                        overview: r.result,
                        components: vec![],
                        decisions: String::new(),
                        concepts: String::new(),
                    });
                }
            }
        }

        None
    }
}

/// Prompt asking a worker to map a free-form query to relevant symbol names,
/// choosing only from `known`. Kept small: names in, names out, one per line.
fn pick_prompt(query: &str, known: &[String]) -> String {
    // Cap the candidate list so the prompt stays within a sane token budget on
    // large repos; the model picks from what it sees.
    const MAX_CANDIDATES: usize = 600;
    let candidates = known
        .iter()
        .take(MAX_CANDIDATES)
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "A user wants to learn about: \"{query}\"\n\n\
         Below is a list of symbol names defined in this codebase. Choose the ones \
         most relevant to the user's request. Output ONLY the chosen names, one per \
         line, exactly as written, with no commentary, numbering, or extra text. \
         Choose at most 8. If none are relevant, output nothing.\n\n\
         SYMBOLS:\n{candidates}"
    )
}

/// Ask a worker to resolve a free-form `query` to relevant symbol names, chosen
/// from `known`. Returns the picked names (intersected with `known`, deduped,
/// capped). Returns an empty vec on `--no-ai`, no worker, execution failure, or
/// when the model picks nothing — the caller then reports "no code found".
pub async fn ai_pick_symbols(
    no_ai: bool,
    registry: &WorkerRegistry,
    workspace: &Path,
    query: &str,
    known: &[String],
) -> Vec<String> {
    if no_ai || registry.is_empty() || known.is_empty() {
        return vec![];
    }
    let req = ExecuteRequest {
        role: Role::Generator,
        instruction: pick_prompt(query, known),
        context: ContextPacket::default(),
        workspace: workspace.to_path_buf(),
        constraints: Constraints {
            timeout_secs: Some(60),
            ..Default::default()
        },
        tools: vec![],
        mcp_servers: vec![],
    };

    let (sink, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut text = None;
    for (worker, _) in registry.resolve_candidates(&[], None, false) {
        let result = worker
            .execute(req.clone(), sink.clone(), CancellationToken::new())
            .await;
        if let Ok(r) = result {
            if r.status.is_success() {
                text = Some(r.result);
                break;
            }
        }
    };
    let Some(text) = text else { return vec![] };

    let known_set: std::collections::HashSet<&str> = known.iter().map(|s| s.as_str()).collect();
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let name = line.trim().trim_matches(|c: char| c == '`' || c == '-' || c == '*').trim();
        if !name.is_empty()
            && known_set.contains(name)
            && !out.iter().any(|o| o == name)
        {
            out.push(name.to_string());
        }
        if out.len() >= 8 {
            break;
        }
    }
    out
}

/// Build the appropriate [`Translator`] based on flags and registry state.
///
/// Returns a [`NullTranslator`] when `no_ai` is true or the registry has no workers.
/// Otherwise wraps every compatible worker, in preference order, in a
/// [`WorkerTranslator`].
pub fn build_translator(
    no_ai: bool,
    registry: &WorkerRegistry,
    workspace: &Path,
) -> Box<dyn Translator> {
    if no_ai || registry.is_empty() {
        Box::new(NullTranslator)
    } else {
        Box::new(WorkerTranslator {
            workers: registry
                .resolve_candidates(&[], None, false)
                .into_iter()
                .map(|(worker, _)| worker)
                .collect(),
            workspace: workspace.to_path_buf(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::learn::LearnDoc;

    #[tokio::test]
    async fn null_translator_returns_none() {
        let doc = LearnDoc { topic: "x".into(), snippets: vec![], flow: vec![], doc_sections: vec![] };
        assert!(NullTranslator.translate(&doc).await.is_none());
    }

    #[tokio::test]
    async fn ai_pick_returns_empty_without_worker() {
        let known = vec!["Foo".to_string(), "Bar".to_string()];
        let reg = WorkerRegistry::new();
        let ws = std::path::Path::new(".");
        // no_ai=true short-circuits; empty registry also short-circuits.
        assert!(ai_pick_symbols(true, &reg, ws, "layer 1", &known).await.is_empty());
        assert!(ai_pick_symbols(false, &reg, ws, "layer 1", &known).await.is_empty());
    }

    #[test]
    fn pick_prompt_lists_candidates_and_query() {
        let known = vec!["Blackboard".to_string(), "Engine".to_string()];
        let p = pick_prompt("what is the loop", &known);
        assert!(p.contains("what is the loop"), "query missing");
        assert!(p.contains("Blackboard") && p.contains("Engine"), "candidates missing");
        assert!(p.to_lowercase().contains("one per line"), "output format missing");
    }

    #[test]
    fn teach_prompt_demands_markdown_and_mermaid() {
        let doc = LearnDoc { topic: "harness".into(), snippets: vec![], flow: vec![], doc_sections: vec![] };
        let p = teach_prompt(&doc);
        assert!(p.contains("harness"), "topic missing");
        assert!(p.to_lowercase().contains("mermaid"), "no mermaid instruction");
        assert!(p.contains("flowchart") || p.contains("sequenceDiagram"), "no diagram type");
    }
}
