//! `rinne graph` — local code-graph inspection subcommand.
//!
//! Subcommands: `stats`, `symbols <file>`, `neighborhood <symbol>`.
//! All read from the blackboard's code graph and print to stdout.
//! Separate from the run path so it works without a plan.

use anyhow::Result;
use rinne_core::Blackboard;
use rinne_types::graph::Neighborhood;

/// Which `rinne graph …` variant to run.
pub enum GraphCmd {
    Index,
    Stats,
    Symbols { file: String },
    Neighborhood { symbol: String },
}

/// Run a `rinne graph` subcommand, opening the blackboard read-only at `cwd`.
pub async fn run(cmd: GraphCmd, cwd: std::path::PathBuf) -> Result<()> {
    let bb = Blackboard::open(&cwd)?;

    match cmd {
        GraphCmd::Index => {
            let count = bb.index_repo();
            let (files, symbols, edges) = bb
                .concrete_graph()
                .as_ref()
                .map(|g| g.stats())
                .unwrap_or((0, 0, 0));
            println!("indexed {count} file(s)");
            println!("{}", format_stats(files, symbols, edges));
        }
        GraphCmd::Stats => {
            let concrete = bb.concrete_graph();
            let (files, symbols, edges) = concrete
                .as_ref()
                .map(|g| g.stats())
                .unwrap_or((0, 0, 0));
            println!("{}", format_stats(files, symbols, edges));
            if files == 0 {
                println!(
                    "\n(graph is empty — run `rinne graph index` to index the repo now, \
                     or it fills in automatically during a run)"
                );
            }
        }
        GraphCmd::Symbols { file } => {
            let concrete = bb.concrete_graph();
            let syms = concrete
                .as_ref()
                .map(|g| g.symbols_in(&file))
                .unwrap_or_default();
            if syms.is_empty() {
                println!("(no symbols indexed for {file})");
            } else {
                for s in &syms {
                    println!("{}  {}:{}", s.name, s.file, s.start_line);
                }
            }
        }
        GraphCmd::Neighborhood { symbol } => {
            use rinne_types::graph::CodeGraph;
            let concrete = bb.concrete_graph();
            let nb = concrete
                .as_ref()
                .and_then(|g| g.neighborhood(&symbol));
            match nb {
                None => println!("(symbol `{symbol}` not found in index)"),
                Some(nb) => println!("{}", format_neighborhood(&nb)),
            }
        }
    }
    Ok(())
}

/// TUI-facing entry point for `/index` and `/graph <sub>`: runs the same
/// inspection as [`run`] but returns feed lines instead of printing to stdout
/// (printing corrupts the inline TUI). `args` is the whitespace-split tail after
/// the slash command, e.g. `["stats"]` or `["neighborhood", "Foo"]`. An empty
/// `args` (from bare `/index`) reindexes the repo.
pub fn run_lines(args: &[String], cwd: &std::path::Path) -> Vec<String> {
    let bb = match Blackboard::open(cwd) {
        Ok(bb) => bb,
        Err(e) => return vec![format!("graph: could not open blackboard: {e}")],
    };

    let sub = args.first().map(|s| s.as_str()).unwrap_or("index");
    match sub {
        "index" => {
            let count = bb.index_repo();
            let (files, symbols, edges) = bb
                .concrete_graph()
                .as_ref()
                .map(|g| g.stats())
                .unwrap_or((0, 0, 0));
            vec![format!("indexed {count} file(s)"), format_stats(files, symbols, edges)]
        }
        "stats" => {
            let (files, symbols, edges) = bb
                .concrete_graph()
                .as_ref()
                .map(|g| g.stats())
                .unwrap_or((0, 0, 0));
            let mut out = vec![format_stats(files, symbols, edges)];
            if files == 0 {
                out.push("(graph is empty — /index to build it now)".into());
            }
            out
        }
        "symbols" => {
            let Some(file) = args.get(1) else {
                return vec!["usage: /graph symbols <file>".into()];
            };
            let syms = bb
                .concrete_graph()
                .as_ref()
                .map(|g| g.symbols_in(file))
                .unwrap_or_default();
            if syms.is_empty() {
                vec![format!("(no symbols indexed for {file})")]
            } else {
                syms.iter()
                    .map(|s| format!("{}  {}:{}", s.name, s.file, s.start_line))
                    .collect()
            }
        }
        "neighborhood" => {
            use rinne_types::graph::CodeGraph;
            let Some(symbol) = args.get(1) else {
                return vec!["usage: /graph neighborhood <symbol>".into()];
            };
            match bb.concrete_graph().as_ref().and_then(|g| g.neighborhood(symbol)) {
                None => vec![format!("(symbol `{symbol}` not found in index)")],
                Some(nb) => format_neighborhood(&nb).lines().map(str::to_string).collect(),
            }
        }
        other => vec![format!(
            "unknown graph subcommand `{other}` — try: index, stats, symbols <file>, neighborhood <symbol>"
        )],
    }
}

/// Format a neighborhood for display.
pub fn format_neighborhood(nb: &Neighborhood) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{}  {}:{}\n",
        nb.definition.name, nb.definition.file, nb.definition.line
    ));
    for caller in &nb.callers {
        out.push_str(&format!(
            "  called by: {}  {}:{}\n",
            caller.name, caller.file, caller.line
        ));
    }
    for callee in &nb.callees {
        out.push_str(&format!(
            "  calls: {}  {}:{}\n",
            callee.name, callee.file, callee.line
        ));
    }
    for import in &nb.imports {
        out.push_str(&format!(
            "  imports: {}  {}:{}\n",
            import.name, import.file, import.line
        ));
    }
    if nb.stale {
        out.push_str("  (stale — file modified since last index)\n");
    }
    out
}

/// Format graph statistics for display.
pub fn format_stats(files: usize, symbols: usize, edges: usize) -> String {
    format!("files: {files}\nsymbols: {symbols}\nedges: {edges}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_neighborhood_lines() {
        use rinne_types::graph::{Neighborhood, SymbolRef};
        let nb = Neighborhood {
            definition: SymbolRef { name: "helper".into(), file: "m.rs".into(), line: 1 },
            callers: vec![SymbolRef { name: "main".into(), file: "m.rs".into(), line: 2 }],
            callees: vec![], imports: vec![], stale: false,
        };
        let out = format_neighborhood(&nb);
        assert!(out.contains("helper  m.rs:1"));
        assert!(out.contains("called by: main  m.rs:2"));
    }
}
