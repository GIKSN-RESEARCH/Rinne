//! `rinne status` — show the state of the current run (DAG + progress)
//! (`CONTEXT.md` §17). Plain rendering; the live TUI is Phase 6.

use anyhow::{anyhow, Result};

use rinne_core::state::State;
use rinne_core::{Blackboard, NodeStatus};

pub async fn run() -> Result<()> {
    let cwd = std::env::current_dir()?;
    if !Blackboard::exists(&cwd) {
        return Err(anyhow!("no run in this directory (.rinne/plan.json not found)"));
    }
    let bb = Blackboard::open(&cwd)?;
    let plan = bb.load_plan()?;
    let state = State::open(&bb.state_db_path())?;

    println!("goal: {}\n", plan.goal);
    println!("PLAN");
    for node in &plan.nodes {
        let status = state.status(&node.id).unwrap_or(NodeStatus::Pending);
        let worker = state.worker(&node.id).ok().flatten().unwrap_or_default();
        let deps = if node.depends_on.is_empty() {
            String::new()
        } else {
            format!("  ⟵ {}", node.depends_on.join(", "))
        };
        println!(
            "  {} {:<4} {:<12} {:<14} {}{}",
            mark(status),
            node.id,
            format!("{:?}", node.role).to_lowercase(),
            worker,
            status.label(),
            deps
        );
    }

    let usage = state.total_usage()?;
    println!(
        "\n{} iterations · {} tok (in {} · out {}) · {} ms",
        state.total_iterations()?,
        rinne_core::format_token_count(usage.total_tokens()),
        rinne_core::format_token_count(usage.prompt_tokens),
        rinne_core::format_token_count(usage.completion_tokens),
        usage.wall_ms
    );

    // Per-worker breakdown from the ledger (so Grok/API spend is visible).
    if let Ok(rows) = state.usage_rows() {
        if !rows.is_empty() {
            use std::collections::BTreeMap;
            let mut by_worker: BTreeMap<String, (u64, u64, u64)> = BTreeMap::new();
            for r in &rows {
                let w = if r.worker.is_empty() {
                    "(unknown)".into()
                } else {
                    r.worker.clone()
                };
                let e = by_worker.entry(w).or_default();
                e.0 += r.prompt_tokens;
                e.1 += r.completion_tokens;
                e.2 += r.wall_ms;
            }
            println!("\nTOKENS BY WORKER");
            for (w, (inp, out, ms)) in by_worker {
                let total = inp + out;
                println!(
                    "  {:<14} {:>8} tok  (in {} · out {})  · {} ms",
                    w,
                    rinne_core::format_token_count(total),
                    rinne_core::format_token_count(inp),
                    rinne_core::format_token_count(out),
                    ms
                );
            }
        }
    }

    // Surface a parked run so the user knows it's waiting on them.
    let parked: Vec<&str> = plan
        .nodes
        .iter()
        .filter(|n| matches!(state.status(&n.id), Ok(NodeStatus::Parked)))
        .map(|n| n.id.as_str())
        .collect();
    if !parked.is_empty() {
        println!(
            "\n⏸ parked at {} — resume with `rinne resume --steer \"…\"`, `--approve`, or `--reject`",
            parked.join(", ")
        );
    }

    // Tail the progress log for recent activity.
    if let Ok(progress) = std::fs::read_to_string(bb.progress_path()) {
        let tail: Vec<&str> = progress.lines().rev().take(6).collect();
        if !tail.is_empty() {
            println!("\nrecent:");
            for line in tail.into_iter().rev() {
                println!("  {line}");
            }
        }
    }
    Ok(())
}

fn mark(status: NodeStatus) -> &'static str {
    match status {
        NodeStatus::Succeeded => "✔",
        NodeStatus::Running => "⠋",
        NodeStatus::Failed => "✗",
        NodeStatus::Parked => "⏸",
        NodeStatus::Pending => "○",
    }
}
