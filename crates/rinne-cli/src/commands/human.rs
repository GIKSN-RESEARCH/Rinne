//! `/human` — session-scoped role pins and review aliases (`CONDUCTOR_LOOP_PLAN.md` §4).

use std::path::Path;

use rinne_core::{CheckpointTrigger, HumanSession, NamedCheckpoint, BLACKBOARD_DIR};

/// Handle `rinne human …` or `/human …` (same argument shape).
pub fn run_lines(args: &[String], cwd: &Path) -> Vec<String> {
    let bb_root = cwd.join(BLACKBOARD_DIR);
    let mut session = HumanSession::load(&bb_root);
    let rest: Vec<&str> = args.iter().map(String::as_str).collect();

    let out = match rest.as_slice() {
        [] => vec![session.summary()],
        ["on"] => {
            session.active = true;
            save(&bb_root, &session)
        }
        ["off"] => {
            session.active = false;
            save(&bb_root, &session)
        }
        ["conductor", backend] => {
            session.active = true;
            session.pins.conductor_backend = Some((*backend).to_string());
            session.pins.conductor_model = None;
            save(&bb_root, &session)
        }
        ["conductor", backend, model] => {
            session.active = true;
            session.pins.conductor_backend = Some((*backend).to_string());
            session.pins.conductor_model = Some((*model).to_string());
            save(&bb_root, &session)
        }
        ["generator", worker] => {
            session.active = true;
            session.pins.generator_worker = Some((*worker).to_string());
            session.pins.generator_model = None;
            save(&bb_root, &session)
        }
        ["generator", worker, model] => {
            session.active = true;
            session.pins.generator_worker = Some((*worker).to_string());
            session.pins.generator_model = Some((*model).to_string());
            save(&bb_root, &session)
        }
        ["evaluator", mode] => {
            session.active = true;
            session.pins.evaluator = Some((*mode).to_string());
            save(&bb_root, &session)
        }
        ["evaluator", "ai", worker] => {
            session.active = true;
            session.pins.evaluator = Some(format!("ai:{worker}"));
            save(&bb_root, &session)
        }
        ["evaluator", "ai", worker, model] => {
            session.active = true;
            session.pins.evaluator = Some(format!("ai:{worker}:{model}"));
            save(&bb_root, &session)
        }
        ["checkpoint", "list"] => vec![session.summary()],
        ["checkpoint", "add", name, "after", node] => {
            session.active = true;
            push_checkpoint(
                &mut session,
                NamedCheckpoint {
                    name: (*name).to_string(),
                    trigger: CheckpointTrigger::AfterNode {
                        node: (*node).to_string(),
                    },
                },
            );
            save(&bb_root, &session)
        }
        ["checkpoint", "add", name, "before", node] => {
            session.active = true;
            push_checkpoint(
                &mut session,
                NamedCheckpoint {
                    name: (*name).to_string(),
                    trigger: CheckpointTrigger::BeforeNode {
                        node: (*node).to_string(),
                    },
                },
            );
            save(&bb_root, &session)
        }
        ["checkpoint", "add", name, "on-build"] => {
            session.active = true;
            push_checkpoint(
                &mut session,
                NamedCheckpoint {
                    name: (*name).to_string(),
                    trigger: CheckpointTrigger::OnBuildSuccess,
                },
            );
            save(&bb_root, &session)
        }
        ["checkpoint", "remove", name] => {
            session.checkpoints.retain(|c| c.name != *name);
            save(&bb_root, &session)
        }
        ["go"] | ["approve"] => {
            vec!["use `/approve` or `rinne resume --approve` while a run is parked".into()]
        }
        ["fix", text @ ..] => {
            vec![format!(
                "use `/steer {}` or `rinne resume --steer \"{}\"` while parked",
                text.join(" "),
                text.join(" ")
            )]
        }
        ["steer", text @ ..] => {
            vec![format!(
                "use `/steer {}` or `rinne resume --steer \"{}\"` while parked",
                text.join(" "),
                text.join(" ")
            )]
        }
        _ => vec![
            "usage:".into(),
            "  /human              show session".into(),
            "  /human on|off       enable/disable overlay".into(),
            "  /human conductor <backend> [model]".into(),
            "  /human generator <worker> [model]".into(),
            "  /human evaluator tool|human|ai [worker] [model]".into(),
            "  /human checkpoint add <name> after|before <node>".into(),
            "  /human checkpoint add <name> on-build".into(),
            "  /human checkpoint list|remove <name>".into(),
            "  /human go|fix <text>  aliases for approve/steer while parked".into(),
        ],
    };
    out
}

fn push_checkpoint(session: &mut HumanSession, cp: NamedCheckpoint) {
    session.checkpoints.retain(|c| c.name != cp.name);
    session.checkpoints.push(cp);
}

fn save(bb_root: &Path, session: &HumanSession) -> Vec<String> {
    match session.save(bb_root) {
        Ok(()) => vec![
            session.summary(),
            format!("saved → {}", HumanSession::path(bb_root).display()),
        ],
        Err(e) => vec![format!("could not save human session: {e}")],
    }
}
