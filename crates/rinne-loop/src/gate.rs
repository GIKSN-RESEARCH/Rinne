//! Named checkpoint gate logic (`CONDUCTOR_LOOP_PLAN.md` §4.3).

use rinne_types::dag::{Node, Plan};
use rinne_types::human::{gate_iter_key, gate_ok_key, CheckpointTrigger, NamedCheckpoint, GATE_ACTIVE_KEY};
use rinne_types::Blackboard;

/// Whether a named gate should fire before `node_id` runs.
pub fn before_gate_for_node<'a>(
    gates: &'a [NamedCheckpoint],
    node_id: &str,
    state: &dyn Blackboard,
) -> Option<&'a NamedCheckpoint> {
    for gate in gates {
        if gate_ok_set(state, &gate.name).unwrap_or(false) {
            continue;
        }
        if matches!(&gate.trigger, CheckpointTrigger::BeforeNode { node } if node == node_id) {
            return Some(gate);
        }
    }
    None
}

/// Whether a gate should fire after `node_id` completes successfully.
pub fn gate_for_node<'a>(
    gates: &'a [NamedCheckpoint],
    plan: &Plan,
    node_id: &str,
    state: &dyn Blackboard,
) -> Option<&'a NamedCheckpoint> {
    for gate in gates {
        if gate_ok_set(state, &gate.name).unwrap_or(false) {
            continue;
        }
        let fires = match &gate.trigger {
            CheckpointTrigger::AfterNode { node } => node == node_id,
            CheckpointTrigger::BeforeNode { .. } => false,
            CheckpointTrigger::OnBuildSuccess => plan.node(node_id).is_some_and(|n| {
                n.acceptance
                    .as_ref()
                    .is_some_and(|a| looks_like_build(&a.command))
            }),
        };
        if fires {
            return Some(gate);
        }
    }
    None
}

fn looks_like_build(cmd: &str) -> bool {
    let c = cmd.to_lowercase();
    c.contains("build") || c.contains("cargo build") || c.contains("npm run build")
}

fn gate_ok_set(state: &dyn Blackboard, name: &str) -> rinne_types::Result<bool> {
    Ok(state.meta(&gate_ok_key(name))?.is_some())
}

/// Write a review artifact for a named gate.
pub fn write_review(
    bb: &dyn Blackboard,
    gate: &NamedCheckpoint,
    node: &Node,
    iter: u32,
) -> rinne_types::Result<()> {
    let dir = format!("checkpoints/{}", gate.name);
    let path_name = format!("{dir}/review-v{iter}.md");
    let body = format!(
        "# Gate: {}\n\nTrigger: {:?}\n\nNode: {} ({:?})\n\nInstruction:\n{}\n\nApprove with `/human go` or `/approve`. \
         Send back with `/human fix <feedback>` or `/steer <text>`.\n",
        gate.name, gate.trigger, node.id, node.role, node.instruction
    );
    bb.write_artifact(&path_name, &body)?;
    Ok(())
}

/// Record gate park metadata on the blackboard.
pub fn mark_active(state: &dyn Blackboard, gate: &NamedCheckpoint) -> rinne_types::Result<u32> {
    state.set_meta(GATE_ACTIVE_KEY, &gate.name)?;
    let key = gate_iter_key(&gate.name);
    let iter = state
        .meta(&key)?
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0)
        .saturating_add(1);
    state.set_meta(&key, &iter.to_string())?;
    Ok(iter)
}

/// Clear gate approval so the run can continue.
pub fn approve_gate(state: &dyn Blackboard, name: &str) -> rinne_types::Result<()> {
    state.set_meta(&gate_ok_key(name), "ok")?;
    state.set_meta(GATE_ACTIVE_KEY, "")?;
    Ok(())
}