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

/// Runs a real worker from the pool to produce a [`Narration`].
pub struct WorkerTranslator {
    worker: Arc<dyn Worker>,
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

        let result = self
            .worker
            .execute(req, sink, CancellationToken::new())
            .await;

        match result {
            Ok(r) if r.status.is_success() => Some(Narration {
                overview: r.result,
                components: vec![],
                decisions: String::new(),
                concepts: String::new(),
            }),
            _ => None,
        }
    }
}

/// Build the appropriate [`Translator`] based on flags and registry state.
///
/// Returns a [`NullTranslator`] when `no_ai` is true or the registry has no workers.
/// Otherwise wraps the first (most-preferred) worker in a [`WorkerTranslator`].
pub fn build_translator(
    no_ai: bool,
    registry: &WorkerRegistry,
    workspace: &Path,
) -> Box<dyn Translator> {
    if no_ai || registry.is_empty() {
        Box::new(NullTranslator)
    } else {
        let worker = registry.first().expect("registry non-empty");
        Box::new(WorkerTranslator {
            worker,
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

    #[test]
    fn teach_prompt_demands_markdown_and_mermaid() {
        let doc = LearnDoc { topic: "harness".into(), snippets: vec![], flow: vec![], doc_sections: vec![] };
        let p = teach_prompt(&doc);
        assert!(p.contains("harness"), "topic missing");
        assert!(p.to_lowercase().contains("mermaid"), "no mermaid instruction");
        assert!(p.contains("flowchart") || p.contains("sequenceDiagram"), "no diagram type");
    }
}
