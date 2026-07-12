//! The JSON DAG: the plan the loop executes (`CONTEXT.md` §10).
//!
//! Nodes plus dependency edges. A generator-evaluator pair is not a separate
//! structure — it is two nodes with a loop-back policy. Phase 3 parses and
//! executes the full shape; the evaluator/loop-back/checkpoint *semantics* land
//! in Phase 5, but the fields are modeled here so a complete `plan.json` round
//! trips losslessly.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::worker::{Capability, Role};

/// A complete plan (`CONTEXT.md` §10).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub goal: String,
    /// Blackboard directory, conventionally `.rinne/`.
    #[serde(default)]
    pub blackboard: Option<String>,
    /// Pinned context anchors resolved from `@`-mentions (`CONTEXT.md` §6).
    #[serde(default)]
    pub mentioned: Vec<PathBuf>,
    #[serde(default)]
    pub budget: PlanBudget,
    /// Human-readable stop condition (e.g. "n4 passes and n3 passes").
    #[serde(default)]
    pub stop_when: Option<String>,
    pub nodes: Vec<Node>,
}

impl Plan {
    /// Find a node by id.
    pub fn node(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// Validate structural invariants: a non-empty node set, unique ids, every
    /// `depends_on` edge pointing at a real node, and no dependency cycles (which
    /// would otherwise leave the scheduler unable to ever run a node).
    pub fn validate(&self) -> crate::Result<()> {
        use std::collections::HashSet;
        if self.nodes.is_empty() {
            return Err(crate::RinneError::Plan("plan has no nodes".into()));
        }
        let mut seen = HashSet::new();
        for n in &self.nodes {
            if !seen.insert(n.id.as_str()) {
                return Err(crate::RinneError::Plan(format!("duplicate node id `{}`", n.id)));
            }
        }
        for n in &self.nodes {
            for dep in &n.depends_on {
                if dep == &n.id {
                    return Err(crate::RinneError::Plan(format!(
                        "node `{}` depends on itself",
                        n.id
                    )));
                }
                if self.node(dep).is_none() {
                    return Err(crate::RinneError::Plan(format!(
                        "node `{}` depends on unknown node `{dep}`",
                        n.id
                    )));
                }
            }
        }
        self.check_acyclic()?;
        Ok(())
    }

    /// Detect a dependency cycle via DFS, returning a clear error naming a node
    /// on the cycle. A multi-node cycle (`a → b → a`) would otherwise pass the
    /// edge checks and then surface only as a mysterious "blocked" run.
    fn check_acyclic(&self) -> crate::Result<()> {
        use std::collections::HashMap;

        // 0 = unvisited, 1 = on the current DFS stack, 2 = fully explored.
        let mut state: HashMap<&str, u8> = self.nodes.iter().map(|n| (n.id.as_str(), 0u8)).collect();

        // Iterative DFS so a deep chain can't blow the stack.
        for start in &self.nodes {
            if state[start.id.as_str()] != 0 {
                continue;
            }
            let mut stack: Vec<(&str, usize)> = vec![(start.id.as_str(), 0)];
            state.insert(start.id.as_str(), 1);
            while let Some(&mut (id, ref mut idx)) = stack.last_mut() {
                let deps = self.node(id).map(|n| n.depends_on.as_slice()).unwrap_or(&[]);
                if *idx < deps.len() {
                    let dep = deps[*idx].as_str();
                    *idx += 1;
                    match state.get(dep).copied().unwrap_or(2) {
                        0 => {
                            state.insert(dep, 1);
                            stack.push((dep, 0));
                        }
                        1 => {
                            return Err(crate::RinneError::Plan(format!(
                                "plan has a dependency cycle involving node `{dep}`"
                            )));
                        }
                        _ => {}
                    }
                } else {
                    state.insert(id, 2);
                    stack.pop();
                }
            }
        }
        Ok(())
    }
}

/// Run-level budget (`CONTEXT.md` §10).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlanBudget {
    pub minutes: Option<u64>,
    pub max_total_iterations: Option<u32>,
}

/// One node in the DAG (`CONTEXT.md` §10 field reference).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub role: Role,
    pub instruction: String,
    /// Capability requirements the scheduler resolves to a worker.
    #[serde(default)]
    pub needs: Vec<Capability>,
    /// Optional preferred worker, soft, overridable at dispatch
    /// (e.g. `harness:claude-code`, `api:gpt-5.5`, `tool:npm-test`).
    #[serde(default)]
    pub prefer: Option<String>,
    /// Optional model for the chosen harness (e.g. `sonnet`, `opus`,
    /// `grok-composer-2.5-fast`). The conductor picks the cheapest model that
    /// fits the node; resolved against config defaults at dispatch.
    #[serde(default)]
    pub model: Option<String>,
    /// MCP tools this node is scoped to use, as `server.tool` ids (e.g.
    /// `postgres.query`). The host path sends these as function definitions; the
    /// provision path provisions exactly these into the harness
    /// (`MCP_SKILLS.md` §7 per-node scoping). Only this node's tools, never the
    /// global catalog.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Skills attached to this node, by name (e.g. `migration-safety`). The
    /// skill's `SKILL.md` body is injected for this node only; the conductor
    /// sees only names + descriptions until then (`MCP_SKILLS.md` §11 progressive
    /// disclosure).
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Named blackboard artifacts consumed, plus the special `diff`.
    #[serde(default)]
    pub inputs: Vec<String>,
    /// Named blackboard artifacts produced, plus the special `diff`.
    #[serde(default)]
    pub outputs: Vec<String>,
    #[serde(default)]
    pub budget: NodeBudget,
    /// Present on evaluator nodes (`CONTEXT.md` §10, §11). Semantics land in P5.
    #[serde(default)]
    pub evaluator: Option<EvaluatorKind>,
    /// For tool evaluators: a command and required exit code.
    #[serde(default)]
    pub acceptance: Option<Acceptance>,
    /// Block any diff that weakens or deletes tests.
    #[serde(default)]
    pub test_ratchet: bool,
    /// Failure policy. Stored raw in P3 (e.g. "loop_back(n2, critique=...)");
    /// parsed and acted on in P5.
    #[serde(default)]
    pub on_fail: Option<String>,
    /// A human gate before or after this node.
    #[serde(default)]
    pub checkpoint: Option<Checkpoint>,
    /// Routing tier assigned by the conductor (T0–T4). The matrix may bump this up.
    #[serde(default)]
    pub complexity_tier: Option<ComplexityTier>,
    /// Closest tier exemplar id from the library (e.g. `T2-05`).
    #[serde(default)]
    pub matched_exemplar: Option<String>,
    /// Optional phase tag for human checkpoints (`build`, `tests`, …).
    #[serde(default)]
    pub phase: Option<String>,
}

/// Task complexity tier for routing (`CONDUCTOR_LOOP_PLAN.md` §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum ComplexityTier {
    T0,
    T1,
    T2,
    T3,
    T4,
}

impl ComplexityTier {
    pub fn label(self) -> &'static str {
        match self {
            ComplexityTier::T0 => "T0",
            ComplexityTier::T1 => "T1",
            ComplexityTier::T2 => "T2",
            ComplexityTier::T3 => "T3",
            ComplexityTier::T4 => "T4",
        }
    }

    /// Parse `T0`..`T4` (case-insensitive).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_uppercase().as_str() {
            "T0" => Some(ComplexityTier::T0),
            "T1" => Some(ComplexityTier::T1),
            "T2" => Some(ComplexityTier::T2),
            "T3" => Some(ComplexityTier::T3),
            "T4" => Some(ComplexityTier::T4),
            _ => None,
        }
    }
}

/// Per-node loop cap (`CONTEXT.md` §10).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodeBudget {
    pub iterations: Option<u32>,
}

/// The kind of evaluator on a node (`CONTEXT.md` §10, §11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EvaluatorKind {
    Ai,
    Tool,
    Human,
}

/// A tool evaluator's acceptance check (`CONTEXT.md` §10).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Acceptance {
    pub command: String,
    #[serde(default)]
    pub must_exit: i32,
}

/// A human checkpoint position (`CONTEXT.md` §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Checkpoint {
    Before,
    After,
}

/// A parsed failure policy (`CONTEXT.md` §10 `on_fail`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OnFail {
    /// Re-run `node`, feeding it the critique artifact at `critique` (if given).
    LoopBack {
        node: String,
        critique: Option<String>,
    },
    /// Re-run `node` together with this evaluator.
    LoopWith { node: String },
    /// Route the failure to a fixer step.
    Fixer,
    /// Ask the conductor to amend the DAG.
    Replan,
}

impl Node {
    /// Parse this node's `on_fail` string into a structured policy. Defaults to
    /// looping back to the first dependency when the field is absent or
    /// unparseable but a dependency exists.
    pub fn parsed_on_fail(&self) -> Option<OnFail> {
        match self.on_fail.as_deref() {
            Some(raw) => parse_on_fail(raw).or_else(|| self.default_loop_back()),
            None => self.default_loop_back(),
        }
    }

    fn default_loop_back(&self) -> Option<OnFail> {
        self.depends_on.first().map(|n| OnFail::LoopBack {
            node: n.clone(),
            critique: None,
        })
    }
}

/// Parse an `on_fail` expression such as:
/// `loop_back(n2, critique=artifacts/review.md)`, `loop_back(n2)`,
/// `loop_with(n3)`, `fixer`, or `replan`.
pub fn parse_on_fail(raw: &str) -> Option<OnFail> {
    let s = raw.trim();
    match s {
        "fixer" => return Some(OnFail::Fixer),
        "replan" => return Some(OnFail::Replan),
        _ => {}
    }
    let (head, inner) = s.split_once('(')?;
    let inner = inner.strip_suffix(')')?;
    let mut parts = inner.split(',').map(|p| p.trim());
    let node = parts.next()?.to_string();
    if node.is_empty() {
        return None;
    }
    match head.trim() {
        "loop_back" => {
            let critique = parts.find_map(|p| {
                p.strip_prefix("critique=")
                    .map(|c| c.trim().to_string())
            });
            Some(OnFail::LoopBack { node, critique })
        }
        "loop_with" => Some(OnFail::LoopWith { node }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_loop_back_with_critique() {
        assert_eq!(
            parse_on_fail("loop_back(n2, critique=artifacts/review.md)"),
            Some(OnFail::LoopBack {
                node: "n2".into(),
                critique: Some("artifacts/review.md".into())
            })
        );
    }

    #[test]
    fn parses_bare_forms() {
        assert_eq!(
            parse_on_fail("loop_back(n2)"),
            Some(OnFail::LoopBack { node: "n2".into(), critique: None })
        );
        assert_eq!(parse_on_fail("loop_with(n3)"), Some(OnFail::LoopWith { node: "n3".into() }));
        assert_eq!(parse_on_fail("fixer"), Some(OnFail::Fixer));
        assert_eq!(parse_on_fail("replan"), Some(OnFail::Replan));
    }

    #[test]
    fn unparseable_returns_none() {
        assert_eq!(parse_on_fail("garbage"), None);
    }

    #[test]
    fn rejects_empty_plan() {
        let p: Plan = serde_json::from_str(r#"{"goal":"g","nodes":[]}"#).unwrap();
        let err = p.validate().unwrap_err().to_string();
        assert!(err.contains("no nodes"), "got: {err}");
    }

    #[test]
    fn detects_multi_node_cycle() {
        let p: Plan = serde_json::from_str(
            r#"{"goal":"g","nodes":[
                {"id":"a","role":"generator","instruction":"x","depends_on":["b"]},
                {"id":"b","role":"generator","instruction":"y","depends_on":["a"]}]}"#,
        )
        .unwrap();
        let err = p.validate().unwrap_err().to_string();
        assert!(err.contains("cycle"), "got: {err}");
    }

    #[test]
    fn accepts_a_valid_dag() {
        let p: Plan = serde_json::from_str(
            r#"{"goal":"g","nodes":[
                {"id":"a","role":"generator","instruction":"x"},
                {"id":"b","role":"evaluator","instruction":"y","depends_on":["a"]}]}"#,
        )
        .unwrap();
        assert!(p.validate().is_ok());
    }
}
