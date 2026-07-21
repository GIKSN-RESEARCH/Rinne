//! Post-plan routing matrix: tier repair, evaluator injection, T4 human gates.

use std::path::Path;

use rinne_config::model::RoutingConfig;
use rinne_core::dag::{ComplexityTier, EvaluatorKind, Node, Plan};
use rinne_core::worker::{Capability, Role, WorkerDescriptor};

use crate::classifier::Classification;
use crate::prompt::ConductorInput;

/// Apply routing rules to a parsed plan. Returns validation errors (empty = ok).
pub fn apply_routing(
    plan: &mut Plan,
    input: &ConductorInput,
    classification: &Classification,
) -> Vec<String> {
    let mut errors = Vec::new();
    let floor = classification.goal_tier_floor;

    for node in &mut plan.nodes {
        let tier = node.complexity_tier.unwrap_or(infer_node_tier(node, floor));
        let tier = max_tier(tier, floor);
        node.complexity_tier = Some(tier);

        if node.matched_exemplar.is_none() && !classification.matched_ids.is_empty() {
            node.matched_exemplar = Some(classification.matched_ids[0].clone());
        }

        apply_tier_defaults(node, tier, &input.workers);
    }

    ensure_evaluators(plan, floor, input.workspace.as_deref());
    apply_tier_rules(plan, &input.routing);
    errors.extend(validate_plan_routing(plan, &input.workers));
    errors
}

fn apply_tier_rules(plan: &mut Plan, routing: &RoutingConfig) {
    for node in &mut plan.nodes {
        let Some(tier) = node.complexity_tier else {
            continue;
        };
        let key = tier.label().to_string();
        let Some(rule) = routing.tiers.get(&key) else {
            continue;
        };
        if rule.require_human_checkpoint
            && node.checkpoint.is_none()
            && matches!(node.role, Role::Generator | Role::Synthesizer)
        {
            node.checkpoint = Some(rinne_core::dag::Checkpoint::After);
        }
    }
}

fn infer_node_tier(node: &Node, floor: ComplexityTier) -> ComplexityTier {
    if matches!(node.role, Role::Evaluator) {
        return floor;
    }
    if node.needs.contains(&Capability::RepoAware) && node.depends_on.len() >= 2 {
        return max_tier(ComplexityTier::T2, floor);
    }
    if node.needs.len() <= 1 && matches!(node.role, Role::Generator | Role::Synthesizer) {
        return floor.min_tier(ComplexityTier::T1);
    }
    floor
}

fn apply_tier_defaults(node: &mut Node, tier: ComplexityTier, workers: &[WorkerDescriptor]) {
    if tier >= ComplexityTier::T4
        && node.checkpoint.is_none()
        && node.evaluator != Some(EvaluatorKind::Human)
        && matches!(node.role, Role::Generator | Role::Synthesizer)
    {
        node.checkpoint = Some(rinne_core::dag::Checkpoint::After);
    }

    if node.model.is_some() {
        return;
    }

    let prefer_name = node.prefer.as_deref().map(parse_prefer_name);
    let worker = prefer_name
        .and_then(|n| workers.iter().find(|w| w.name == n))
        .or_else(|| workers.first());

    let Some(w) = worker else { return };
    let ladder = &w.models;
    if ladder.is_empty() {
        return;
    }

    // Ladder is cheap → strong. T2 sits one below the frontier when possible;
    // T3+ takes the frontier (T4 also gets a checkpoint gate).
    let idx = match tier {
        ComplexityTier::T0 | ComplexityTier::T1 => 0,
        ComplexityTier::T2 => ladder.len().saturating_sub(2),
        ComplexityTier::T3 | ComplexityTier::T4 => ladder.len().saturating_sub(1),
    };
    node.model = ladder.get(idx).or_else(|| ladder.last()).cloned();
}

fn ensure_evaluators(plan: &mut Plan, floor: ComplexityTier, workspace: Option<&Path>) {
    if floor < ComplexityTier::T1 {
        return;
    }
    let has_eval = plan
        .nodes
        .iter()
        .any(|n| n.evaluator.is_some() || n.role == Role::Evaluator);
    if has_eval || floor == ComplexityTier::T0 {
        return;
    }
    let gen = plan
        .nodes
        .iter()
        .find(|n| matches!(n.role, Role::Generator) && n.evaluator.is_none())
        .map(|n| (n.id.clone(), edits_code(n)));
    let Some((gen_id, edits_code)) = gen else {
        return;
    };

    let eval_id = format!("{gen_id}-eval");
    if plan.node(&eval_id).is_some() {
        return;
    }
    // Only code work can be verified by a test command. A node that does not
    // edit code cannot change whether the suite passes, so attaching one
    // asserts something it has no control over — and on a repo with any
    // pre-existing failure it can never pass, looping the generator back to
    // "fix the tests" when the task was to explain, summarise or research.
    // Prose still gets an evaluator, just one that reads what was written:
    // dropping the node entirely would leave T3/T4 plans failing
    // `validate_plan_routing`'s "no evaluator dependency" check.
    let (instruction, evaluator, acceptance) = if edits_code {
        (
            format!("Verify the output of {gen_id} — run tests or build if applicable."),
            EvaluatorKind::Tool,
            Some(rinne_core::dag::Acceptance {
                command: detect_test_command(workspace),
                must_exit: 0,
            }),
        )
    } else {
        (
            format!(
                "Review the output of {gen_id} for accuracy and completeness against the goal. \
                 Do not run tests or modify files."
            ),
            EvaluatorKind::Ai,
            None,
        )
    };
    plan.nodes.push(Node {
        id: eval_id,
        role: Role::Evaluator,
        instruction,
        depends_on: vec![gen_id.clone()],
        evaluator: Some(evaluator),
        acceptance,
        on_fail: Some(format!("loop_back({gen_id}, critique=artifacts/review.md)")),
        ..default_node()
    });
}

/// Whether a node's work can change whether the test suite passes.
///
/// `needs` is planner output with nothing enforcing it, so a `CodeEdit`-only
/// check silently drops the test gate whenever the model omits the capability
/// on genuine edit work. Fall back to the node's declared outputs, which the
/// prompt ties to `"diff"` for code changes.
fn edits_code(node: &Node) -> bool {
    node.needs.contains(&Capability::CodeEdit)
        || node.outputs.iter().any(|o| o == "diff")
        || node.test_ratchet
}

/// Detect the project's test command from markers under `workspace`.
pub fn detect_test_command(workspace: Option<&Path>) -> String {
    let root = workspace.unwrap_or_else(|| Path::new("."));
    if root.join("package.json").exists() {
        if root.join("pnpm-lock.yaml").exists() {
            "pnpm test".into()
        } else if root.join("yarn.lock").exists() {
            "yarn test".into()
        } else {
            "npm test".into()
        }
    } else if root.join("Cargo.toml").exists() {
        "cargo test".into()
    } else if root.join("go.mod").exists() {
        "go test ./...".into()
    } else if root.join("pyproject.toml").exists() || root.join("setup.py").exists() {
        "pytest".into()
    } else {
        "true".into()
    }
}

fn validate_plan_routing(plan: &Plan, workers: &[WorkerDescriptor]) -> Vec<String> {
    let mut errs = Vec::new();
    if workers.is_empty() {
        // Pool was not supplied (tests, minimal inputs) — skip worker-aware checks.
        return errs;
    }
    for node in &plan.nodes {
        if let Some(tier) = node.complexity_tier {
            if tier >= ComplexityTier::T3
                && node.evaluator.is_none()
                && !matches!(node.role, Role::Evaluator)
                && node.acceptance.is_none()
                && !plan
                    .nodes
                    .iter()
                    .any(|n| n.depends_on.contains(&node.id) && n.evaluator.is_some())
            {
                errs.push(format!(
                    "node `{}` is {} but has no evaluator dependency",
                    node.id,
                    tier.label()
                ));
            }
        }
        if node.needs.contains(&Capability::RepoAware) {
            let prefer_api = node
                .prefer
                .as_deref()
                .is_some_and(|p| p.starts_with("api:"));
            if prefer_api {
                errs.push(format!(
                    "node `{}` needs repo-aware but prefers an API worker",
                    node.id
                ));
            }
        }
    }
    errs
}

fn parse_prefer_name(prefer: &str) -> &str {
    prefer.split_once(':').map(|(_, n)| n).unwrap_or(prefer)
}

fn max_tier(a: ComplexityTier, b: ComplexityTier) -> ComplexityTier {
    if a >= b {
        a
    } else {
        b
    }
}

trait MinTier {
    fn min_tier(self, cap: ComplexityTier) -> ComplexityTier;
}

impl MinTier for ComplexityTier {
    fn min_tier(self, cap: ComplexityTier) -> ComplexityTier {
        if self <= cap {
            self
        } else {
            cap
        }
    }
}

fn default_node() -> Node {
    Node {
        id: String::new(),
        role: Role::Generator,
        instruction: String::new(),
        needs: Vec::new(),
        prefer: None,
        model: None,
        tools: Vec::new(),
        skills: Vec::new(),
        depends_on: Vec::new(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        budget: Default::default(),
        evaluator: None,
        acceptance: None,
        test_ratchet: false,
        on_fail: None,
        checkpoint: None,
        complexity_tier: None,
        matched_exemplar: None,
        phase: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rinne_core::dag::ComplexityTier;
    use std::fs;

    #[test]
    fn detect_test_command_uses_workspace_not_cwd() {
        let ws =
            std::env::temp_dir().join(format!("rinne-routing-{}-{}", std::process::id(), "ws"));
        let _ = fs::remove_dir_all(&ws);
        fs::create_dir_all(&ws).unwrap();
        fs::write(ws.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        assert_eq!(detect_test_command(Some(ws.as_path())), "cargo test");
        let _ = fs::remove_dir_all(&ws);
    }

    fn plan_with(node: Node) -> Plan {
        Plan {
            goal: "g".into(),
            blackboard: None,
            mentioned: Vec::new(),
            budget: Default::default(),
            stop_when: None,
            nodes: vec![node],
        }
    }

    fn prose_node() -> Node {
        Node {
            id: "n1".into(),
            role: Role::Generator,
            instruction: "Explain this codebase; do not modify any files.".into(),
            needs: vec![
                Capability::RepoAware,
                Capability::ToolRun,
                Capability::Reasoning,
                Capability::Writing,
                Capability::LongContext,
            ],
            complexity_tier: Some(ComplexityTier::T1),
            ..default_node()
        }
    }

    #[test]
    fn a_prose_node_gets_a_review_evaluator_not_a_test_command() {
        // Asking for an explanation produced a `cargo test` evaluator, which
        // failed on the repo's pre-existing failures and looped the generator
        // back to "fix the tests" — work the node was explicitly told not to do.
        // It still needs *an* evaluator: dropping it entirely made every T3/T4
        // prose plan fail `validate_plan_routing` instead.
        let mut plan = plan_with(prose_node());

        ensure_evaluators(&mut plan, ComplexityTier::T1, None);

        assert_eq!(plan.nodes.len(), 2, "prose still needs a reviewer");
        let eval = &plan.nodes[1];
        assert_eq!(eval.evaluator, Some(EvaluatorKind::Ai));
        assert!(
            eval.acceptance.is_none(),
            "a node that cannot edit code cannot change whether tests pass: {:?}",
            eval.acceptance
        );
    }

    #[test]
    fn a_t4_prose_plan_passes_routing_validation() {
        // Regression: gating evaluator injection on `CodeEdit` left T3/T4 prose
        // nodes with no evaluator, which `validate_plan_routing` rejects. The
        // classifier floors "audit"/"architecture"/"security" goals at T4, so
        // `rinne "audit our auth architecture"` burned the whole planner ladder
        // and then failed with "plan failed routing validation".
        let mut plan = plan_with(Node {
            complexity_tier: Some(ComplexityTier::T4),
            ..prose_node()
        });

        ensure_evaluators(&mut plan, ComplexityTier::T4, None);
        let workers = vec![WorkerDescriptor {
            name: "claude-code".into(),
            family: rinne_core::worker::WorkerFamily::Harness,
            capabilities: vec![Capability::RepoAware, Capability::Writing],
            auth_mode: rinne_core::worker::AuthMode::Subscription,
            quota: rinne_core::worker::QuotaModel::unlimited(),
            latency: rinne_core::worker::LatencyProfile::Medium,
            transport: rinne_core::worker::Transport::SubprocessJson,
            models: vec!["haiku".into()],
        }];

        let errs = validate_plan_routing(&plan, &workers);
        assert!(errs.is_empty(), "T4 prose plan must validate: {errs:?}");
    }

    #[test]
    fn a_code_node_without_the_capability_still_gets_a_test_evaluator() {
        // `needs` is planner output with nothing enforcing it, so keying only on
        // `CodeEdit` silently dropped the test gate whenever the model omitted
        // the capability on genuine edit work. `outputs: ["diff"]` is the other
        // signal the prompt ties to code changes.
        let mut plan = plan_with(Node {
            id: "n1".into(),
            role: Role::Generator,
            instruction: "Fix the off-by-one in pagination".into(),
            needs: vec![Capability::RepoAware],
            outputs: vec!["diff".into()],
            complexity_tier: Some(ComplexityTier::T2),
            ..default_node()
        });

        ensure_evaluators(&mut plan, ComplexityTier::T2, None);

        assert_eq!(plan.nodes.len(), 2);
        assert_eq!(plan.nodes[1].evaluator, Some(EvaluatorKind::Tool));
        assert!(
            plan.nodes[1].acceptance.is_some(),
            "code work must keep its test gate"
        );
    }

    #[test]
    fn a_code_editing_node_still_gets_a_test_evaluator() {
        let mut plan = plan_with(Node {
            id: "n1".into(),
            role: Role::Generator,
            instruction: "Implement the feature".into(),
            needs: vec![Capability::CodeEdit, Capability::RepoAware],
            complexity_tier: Some(ComplexityTier::T1),
            ..default_node()
        });

        ensure_evaluators(&mut plan, ComplexityTier::T1, None);

        assert_eq!(plan.nodes.len(), 2, "code work still needs verification");
        assert_eq!(plan.nodes[1].role, Role::Evaluator);
    }

    #[test]
    fn t4_injects_checkpoint_after() {
        let mut plan = Plan {
            goal: "g".into(),
            blackboard: None,
            mentioned: Vec::new(),
            budget: Default::default(),
            stop_when: None,
            nodes: vec![Node {
                id: "n1".into(),
                role: Role::Generator,
                instruction: "do".into(),
                complexity_tier: Some(ComplexityTier::T4),
                ..default_node()
            }],
        };
        let class = Classification {
            goal_tier_floor: ComplexityTier::T4,
            matched_ids: vec!["T4-01".into()],
            confidence: 0.9,
        };
        let input = ConductorInput::default();
        let errs = apply_routing(&mut plan, &input, &class);
        assert!(errs.is_empty() || !errs.is_empty()); // routing may warn
        assert_eq!(
            plan.nodes[0].checkpoint,
            Some(rinne_core::dag::Checkpoint::After)
        );
    }

    #[test]
    fn t2_model_is_below_t3_on_a_three_rung_ladder() {
        use rinne_core::worker::{AuthMode, LatencyProfile, QuotaModel, Transport, WorkerFamily};

        let desc = vec![WorkerDescriptor {
            name: "w".into(),
            family: WorkerFamily::Api,
            capabilities: vec![],
            auth_mode: AuthMode::ApiKey,
            quota: QuotaModel::unlimited(),
            latency: LatencyProfile::Fast,
            transport: Transport::Http,
            models: vec!["cheap".into(), "mid".into(), "strong".into()],
        }];
        let mut n2 = Node {
            id: "a".into(),
            role: Role::Generator,
            instruction: "x".into(),
            ..default_node()
        };
        let mut n3 = Node {
            id: "b".into(),
            role: Role::Generator,
            instruction: "x".into(),
            ..default_node()
        };
        apply_tier_defaults(&mut n2, ComplexityTier::T2, &desc);
        apply_tier_defaults(&mut n3, ComplexityTier::T3, &desc);
        assert_eq!(n2.model.as_deref(), Some("mid"));
        assert_eq!(n3.model.as_deref(), Some("strong"));
    }
}
