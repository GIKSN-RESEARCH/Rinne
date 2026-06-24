//! `rinne logs` — view trajectory logs for the current run, local only
//! (`CONTEXT.md` §17). Reads the usage ledger from `state.db` plus the progress
//! log; nothing leaves the machine.

use anyhow::{anyhow, Result};

use rinne_core::state::State;
use rinne_core::Blackboard;

pub async fn run() -> Result<()> {
    let cwd = std::env::current_dir()?;
    if !Blackboard::exists(&cwd) {
        return Err(anyhow!("no run in this directory (.rinne/plan.json not found)"));
    }
    let bb = Blackboard::open(&cwd)?;
    let state = State::open(&bb.state_db_path())?;

    let rows = state.usage_rows()?;
    println!("TRAJECTORY  ({} invocation{})\n", rows.len(), if rows.len() == 1 { "" } else { "s" });
    if rows.is_empty() {
        println!("  (no recorded worker invocations yet)");
    } else {
        println!("  {:<6} {:<16} {:>8} {:>8} {:>9}", "node", "worker", "in_tok", "out_tok", "wall_ms");
        for r in &rows {
            println!(
                "  {:<6} {:<16} {:>8} {:>8} {:>9}",
                r.node_id, r.worker, r.prompt_tokens, r.completion_tokens, r.wall_ms
            );
        }
        let usage = state.total_usage()?;
        println!(
            "\n  total: {} tokens · {} ms · {} iterations",
            usage.total_tokens(),
            usage.wall_ms,
            state.total_iterations()?
        );
    }

    // Tail the human-readable progress log.
    if let Ok(progress) = std::fs::read_to_string(bb.progress_path()) {
        let tail: Vec<&str> = progress.lines().rev().take(20).collect();
        if !tail.is_empty() {
            println!("\nPROGRESS (recent):");
            for line in tail.into_iter().rev() {
                println!("  {line}");
            }
        }
    }
    println!("\ntranscripts: {}/transcripts/", bb.root().display());
    Ok(())
}
