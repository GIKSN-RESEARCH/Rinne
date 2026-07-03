//! `rinne run <plan.json>` and `rinne resume` (`PHASE.md` P3).
//!
//! `run` loads a hand-written plan into the blackboard and executes it; `resume`
//! continues the plan already in the blackboard. Both stream progress through
//! the shared runner. In Phase 4 the conductor will generate the plan from a
//! prompt, but a plan-file entry is how Phase 3 is driven and tested live.

use std::path::Path;

use anyhow::{anyhow, Context, Result};

use rinne_core::dag::Plan;
use rinne_core::Blackboard;

use crate::runner;

/// Load a plan file into the blackboard and run it.
pub async fn run(plan_path: &str, no_graph: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let bb = Blackboard::open_with(&cwd, !no_graph)?;

    let bytes = std::fs::read(plan_path)
        .with_context(|| format!("could not read plan file `{plan_path}`"))?;
    let plan: Plan =
        serde_json::from_slice(&bytes).with_context(|| "plan file is not valid JSON")?;
    plan.validate().map_err(|e| anyhow!(e.to_string()))?;
    bb.save_plan(&plan)?;
    bb.reset_run()?; // a freshly loaded plan starts a fresh run

    println!("loaded plan from {}\n", Path::new(plan_path).display());
    runner::run_plan(&bb).await?;
    Ok(())
}

/// One-shot headless: generate a plan from a prompt with the conductor, then
/// run it (`CONTEXT.md` §6 `shoal -p`). With `json`, emit a single structured
/// JSON result; otherwise stream human-readable progress.
pub async fn oneshot(task: &str, json: bool, no_graph: bool) -> Result<()> {
    if json {
        let result = runner::oneshot_json(task, no_graph).await?;
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }

    let cwd = std::env::current_dir()?;
    let bb = Blackboard::open_with(&cwd, !no_graph)?;
    runner::plan_goal(&bb, task).await?;
    runner::run_plan(&bb).await?;
    Ok(())
}

/// Resume the plan already in the blackboard, optionally applying a human
/// decision to a parked node (`CONTEXT.md` §11).
pub async fn resume(steer: Option<String>, approve: bool, reject: bool, no_graph: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    if !Blackboard::exists(&cwd) {
        return Err(anyhow!(
            "no run to resume in this directory (.rinne/plan.json not found)"
        ));
    }
    let bb = Blackboard::open_with(&cwd, !no_graph)?;

    let decision = match (steer, approve, reject) {
        (Some(text), _, _) => Some(rinne_core::HumanDecision::Steer(text)),
        (_, true, _) => Some(rinne_core::HumanDecision::Approve),
        (_, _, true) => Some(rinne_core::HumanDecision::Reject),
        _ => None,
    };
    let resume = decision.map(|decision| rinne_core::ResumeInput {
        node: None,
        decision,
    });

    runner::run_plan_with(&bb, resume).await?;
    Ok(())
}
