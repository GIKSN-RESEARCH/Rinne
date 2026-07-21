//! `rinne run <plan.json>`, `rinne resume`, and `rinne --continue` (`PHASE.md` P3).
//!
//! `run` loads a hand-written plan into the blackboard and executes it; `resume`
//! and `--continue` continue the plan already in the blackboard without
//! re-planning. Both stream progress through the shared runner. In Phase 4 the
//! conductor will generate the plan from a prompt, but a plan-file entry is how
//! Phase 3 is driven and tested live.

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

/// Error when there is no prior orchestration session to continue/resume.
fn no_session_err() -> anyhow::Error {
    anyhow!(
        "nothing to continue — no prior orchestration session in this directory \
         (.rinne/plan.json not found)\n\
         start a run first with `rinne` or `rinne -p \"…\"`"
    )
}

/// Open the prior session for continue/resume: load plan + state without
/// resetting. Returns an error if there is no session to continue.
///
/// Used by both `rinne --continue` and `rinne resume`. Does **not** call
/// [`Blackboard::reset_run`] — completed nodes stay completed.
pub fn load_continue_session(cwd: &Path) -> Result<(Blackboard, Plan)> {
    if !Blackboard::exists(cwd) {
        return Err(no_session_err());
    }
    let bb = Blackboard::open(cwd)?;
    let plan = bb
        .load_plan()
        .with_context(|| "could not load prior session plan from .rinne/plan.json")?;
    Ok((bb, plan))
}

/// `rinne --continue` / `-c`: resume the previous orchestration session from
/// saved state without re-planning or wiping node statuses.
pub async fn continue_session() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let (bb, plan) = load_continue_session(&cwd)?;
    println!("continuing: {}\n", plan.goal);
    runner::run_plan_with(&bb, None).await?;
    Ok(())
}

/// Resume the plan already in the blackboard, optionally applying a human
/// decision to a parked node (`CONTEXT.md` §11).
pub async fn resume(
    steer: Option<String>,
    approve: bool,
    reject: bool,
    no_graph: bool,
) -> Result<()> {
    let cwd = std::env::current_dir()?;
    if !Blackboard::exists(&cwd) {
        return Err(no_session_err());
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use rinne_core::NodeStatus;

    fn temp_ws(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "rinne-continue-{}-{}-{}",
            std::process::id(),
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn sample_plan() -> Plan {
        serde_json::from_value(serde_json::json!({
            "goal": "partial run for continue",
            "nodes": [
                {
                    "id": "n1",
                    "role": "generator",
                    "instruction": "step one",
                    "needs": ["code-edit"],
                    "outputs": ["out1"]
                },
                {
                    "id": "n2",
                    "role": "generator",
                    "instruction": "step two",
                    "needs": ["code-edit"],
                    "depends_on": ["n1"],
                    "outputs": ["out2"]
                }
            ]
        }))
        .unwrap()
    }

    #[test]
    fn load_continue_session_errors_when_missing() {
        let ws = temp_ws("missing");
        let err = match load_continue_session(&ws) {
            Ok(_) => panic!("expected missing-session error"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("nothing to continue"),
            "expected clear missing-session error, got: {msg}"
        );
        assert!(
            msg.contains("plan.json"),
            "expected path hint in error, got: {msg}"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn load_continue_session_preserves_state() {
        let ws = temp_ws("happy");
        let bb = Blackboard::open(&ws).unwrap();
        let plan = sample_plan();
        bb.save_plan(&plan).unwrap();
        bb.ensure_node("n1").unwrap();
        bb.ensure_node("n2").unwrap();
        bb.set_status("n1", NodeStatus::Succeeded).unwrap();
        // Write the named output so engine integrity checks would pass on resume.
        bb.write_artifact("out1", "done").unwrap();

        let (loaded, loaded_plan) = match load_continue_session(&ws) {
            Ok(v) => v,
            Err(e) => panic!("expected session load to succeed: {e:#}"),
        };
        assert_eq!(loaded_plan.goal, "partial run for continue");
        assert_eq!(loaded.status("n1").unwrap(), NodeStatus::Succeeded);
        assert_eq!(loaded.status("n2").unwrap(), NodeStatus::Pending);
        // Must not wipe prior progress (contrast with a fresh `run` / `plan_goal`).
        assert_eq!(loaded.iterations("n1").unwrap(), 0);
        let _ = std::fs::remove_dir_all(&ws);
    }
}
