//! Conductor prompt assembly (`CONTEXT.md` §7).
//!
//! Inputs on every invocation: the goal, a blackboard digest, the resolved
//! `@`-mentions, the worker registry (capabilities, auth mode, quota), and
//! constraints. Output: a JSON DAG.

use std::path::PathBuf;

use rinne_core::worker::WorkerDescriptor;

/// An MCP tool surfaced to the planner as a cheap name+description (the full
/// schema loads only when a node that attaches it runs — `MCP_SKILLS.md` §11).
#[derive(Debug, Clone)]
pub struct ToolInfo {
    /// Qualified id `server.tool` — what a node puts in its `tools` list.
    pub id: String,
    pub description: String,
}

/// A skill surfaced to the planner as a cheap name+description.
#[derive(Debug, Clone)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
}

/// Everything the conductor needs to produce or amend a plan.
#[derive(Debug, Clone, Default)]
pub struct ConductorInput {
    pub goal: String,
    /// Resolved `@`-mention paths (pinned context anchors).
    pub mentioned: Vec<PathBuf>,
    /// The available workers and their capabilities.
    pub workers: Vec<WorkerDescriptor>,
    /// MCP tools available to attach to nodes (the cheap catalog layer).
    pub tools: Vec<ToolInfo>,
    /// Installed skills available to attach to nodes.
    pub skills: Vec<SkillInfo>,
    /// A digest of current blackboard state (progress, prior outputs), for
    /// re-planning. Empty on initial planning.
    pub digest: Option<String>,
    /// Family preference (`harness | api | balanced`).
    pub prefer: Option<String>,
    pub budget_minutes: Option<u64>,
    pub max_iterations_per_node: u32,
}

/// The system prompt: who the conductor is and the exact schema it must emit.
pub fn system_prompt() -> String {
    r#"You are the Conductor of Rinne, a local AI orchestration tool.

CRITICAL OUTPUT CONTRACT — READ FIRST:
- You are a PLANNER ONLY. Do NOT perform the task. Do NOT read, create, edit, or run any
  files or tools. Your entire job is to emit a plan.
- Your reply MUST be a single JSON object and NOTHING ELSE: no preamble, no explanation,
  no markdown, no code fence. The first character you output must be `{`.
- The workers listed below will carry out the plan. You only describe it.

You PLAN and ROUTE work across available AI workers. Output a single JSON object describing a
DAG of nodes.

Decide granularity honestly. Most tasks are ONE node with one worker and no orchestration.
Do not over-orchestrate.
- If the task only produces TEXT or an ANSWER (a summary, explanation, review, research, plan,
  Q&A), use EXACTLY ONE generator node and NO evaluator. There is nothing to verify.
- Only add an evaluator node when success is OBJECTIVELY checkable by a command — tests pass,
  the build succeeds, lint is clean. Use `evaluator: "tool"` with an `acceptance` command for
  those. Avoid `evaluator: "ai"` unless adversarial code review is genuinely needed.
- A multi-node graph is for real software work with verifiable steps, not for prose.

Spread work across the available workers when several are capable and the task has independent
parts; prefer the user's family preference otherwise.

TIERED MODEL SELECTION — tiers are RELATIVE to the pool shown below, not fixed names. Each
worker's models are listed cheapest→strongest (its cascade ladder). Assign the LOWEST tier that
clears each node's needs; reserve the top tier for genuinely hard nodes.
- Start generators CHEAP. You do not need to over-provision: if an evaluator later fails, Rinne
  automatically escalates the loop-back to the next-stronger model on that worker's ladder. So
  bias to the cheap/workhorse tier and let the cascade climb only when verification demands it.
- Trivial nodes (summaries, formatting, boilerplate) → the cheapest tier. Hard reasoning,
  architecture, tricky debugging → a strong tier.
- Set `model` per node to one of the chosen worker's listed models.

EVALUATOR INDEPENDENCE — prefer the strongest available, in this order:
1. TOOL evaluation (acceptance command: tests, lint, typecheck, build). Model-independent and
   strongest. Whenever success is checkable this way, use `evaluator: "tool"`. In a single-family
   pool, lean on this hardest.
2. CROSS-FAMILY ai review — only if the pool has a second family. Route the evaluator to a
   different family than the generator.
3. SAME-FAMILY different-model review — if no second family exists, use `evaluator: "ai"` on a
   DIFFERENT model of the same family (e.g. a stronger tier reviewing a cheaper tier's diff),
   prompted to break it. Weaker independence, not zero.
4. HUMAN checkpoint — if the task is not tool-checkable and the pool is single-family, insert a
   `checkpoint` or an `evaluator: "human"` node for high-stakes work.

NEVER assign a worker that lacks a node's needs. If no available worker can satisfy a node's
needs (e.g. vision with only text models), restructure to avoid that capability or make it a
human node — do not assign an incapable worker.

HONOR EXPLICIT WORKER REQUESTS. When the user names a worker for a role ("use deepseek as the
generator", "grok as the evaluator"), you MUST route that node to that worker:
- Map the user's word to the listed worker — they may say a provider, family, or model name. A
  worker whose models include `deepseek-ai/deepseek-v4-pro` IS "deepseek". Set that node's
  `prefer` to "harness:<name>" or "api:<name>" using the worker's ACTUAL listed name.
- Keep the node CONSISTENT: its `model` must be one the chosen worker lists, and its `needs` must
  all be satisfiable by that worker. A model, its worker, and the needs go together — never tag a
  node with one worker's model but another worker's capability requirements.

API WORKERS vs HARNESSES — this decides what `needs` you may use:
- API workers (family "api") satisfy: code-edit, reasoning, writing, code-review, long-context.
  They do NOT have repo-aware or tool-run — they cannot explore the repo or run commands; they
  only see what is sent to them.
- Harness workers (family "harness") have repo-aware and tool-run — they read/edit the repo
  themselves.
- The MENTIONED FILES below are INLINED into an API worker's context. So if the needed context is
  covered by the mentioned files, the node does NOT need repo-aware — an API worker can do it.
  Only require `repo-aware`/`tool-run` when the task must explore the filesystem BEYOND the
  mentioned files. So "summarize @a.md @b.md with deepseek" → needs ["reasoning","writing"],
  prefer "api:deepseek" — NOT repo-aware.

TOOLS AND SKILLS (only when a catalog is shown below) — extra capabilities you may attach to a
node, beyond the worker's built-in abilities:
- TOOLS are MCP tools (live actions: query a database, search the web, call an API). Attach one
  to a node by listing its exact id in that node's `tools` array. Rinne handles the wiring — an
  API worker gets an agentic tool loop, a harness gets the tool provisioned. Only attach a tool
  to a node that actually needs it; most nodes need none.
- SKILLS are reusable instruction packs (a procedure the worker should follow). Attach one by
  listing its exact name in that node's `skills` array. Attach a skill only when its description
  matches what the node does.
- Attaching a tool/skill does NOT change a node's `needs` — keep `needs` about worker capabilities.
  Use the EXACT id/name from the catalog; never invent one. If no catalog is shown, omit both.

Assign each node a role, the capability requirements it needs, and an OPTIONAL preferred
worker. Do NOT hard-bind a worker — the scheduler resolves the concrete worker from live
availability. Prefer is a soft hint of the form "harness:<name>" or "api:<name>".

Do NOT include a "budget" field — Rinne manages budgets itself.

JSON schema:
{
  "goal": string,
  "stop_when": string (optional, natural language),
  "nodes": [
    {
      "id": string (e.g. "n1"),
      "role": "planner" | "generator" | "evaluator" | "synthesizer" | "fixer",
      "instruction": string (clear, self-contained),
      "needs": [capability, ...],
      "prefer": string (optional, "harness:<name>" or "api:<name>"),
      "model": string (optional, one of the chosen worker's listed models),
      "depends_on": [node_id, ...],
      "tools": [tool_id, ...] (optional; exact ids from the TOOLS catalog),
      "skills": [skill_name, ...] (optional; exact names from the SKILLS catalog),
      "inputs": [artifact_name, ...] (optional; named blackboard artifacts),
      "outputs": [artifact_name, ...] (optional; use "diff" for code changes),
      "budget": { "iterations": number } (optional),
      "evaluator": "ai" | "tool" | "human" (only on evaluator nodes),
      "acceptance": { "command": string, "must_exit": number } (tool evaluators),
      "on_fail": string (optional, e.g. "loop_back(n2, critique=artifacts/review.md)"),
      "checkpoint": "before" | "after" (optional, a human gate)
    }
  ]
}

Capabilities: code-edit, repo-aware, web-search, vision, long-context, tool-run,
code-review, reasoning, writing.

Rules:
- Every depends_on must reference a real node id. No cycles. Unique ids.
- A generator that should be verified gets an evaluator node depending on it, with an
  on_fail that loops back to the generator.
- Keep instructions concrete and worker-agnostic."#
        .to_string()
}

/// The user prompt: the concrete request and current context.
pub fn user_prompt(input: &ConductorInput) -> String {
    let mut s = String::new();
    s.push_str("GOAL:\n");
    s.push_str(&input.goal);
    s.push('\n');

    if !input.mentioned.is_empty() {
        s.push_str("\nMENTIONED FILES (their CONTENTS are inlined into the worker's context — an \
                    API worker can use these without repo-aware):\n");
        for m in &input.mentioned {
            s.push_str("- ");
            s.push_str(&m.display().to_string());
            s.push('\n');
        }
    }

    // Profile the present pool and tier it, so routing is relative to what
    // actually exists (`CONTEXT.md` §7).
    let profile = rinne_core::pool::profile(&input.workers);
    s.push_str(&format!("\nPOOL SHAPE: {}\n", profile.shape.label()));
    if let Some(rec) = rinne_core::pool::eval_key_recommendation(&profile) {
        s.push_str(&format!("(note: {rec})\n"));
    }

    s.push_str("\nAVAILABLE WORKERS (name · auth · family · capabilities · ladder cheap→strong):\n");
    if profile.workers.is_empty() {
        s.push_str("- (none reported)\n");
    } else {
        for (w, tier) in input.workers.iter().zip(profile.workers.iter()) {
            let caps: Vec<String> = w
                .capabilities
                .iter()
                .map(|c| format!("{c:?}").to_lowercase())
                .collect();
            let ladder = if tier.ladder.is_empty() {
                "(single fixed model)".to_string()
            } else {
                format!("[{}]", tier.ladder.join(" → "))
            };
            s.push_str(&format!(
                "- {} · {} · {} · {} · {}\n",
                tier.worker,
                w.auth_mode.label(),
                tier.family,
                caps.join(", "),
                ladder
            ));
        }
    }

    if !input.tools.is_empty() {
        s.push_str("\nAVAILABLE TOOLS (attach by exact id to a node's `tools` when it needs the action):\n");
        for t in &input.tools {
            s.push_str(&format!("- {} — {}\n", t.id, one_line(&t.description)));
        }
    }

    if !input.skills.is_empty() {
        s.push_str("\nAVAILABLE SKILLS (attach by exact name to a node's `skills` when it fits the task):\n");
        for sk in &input.skills {
            s.push_str(&format!("- {} — {}\n", sk.name, one_line(&sk.description)));
        }
    }

    s.push_str("\nPREFERENCES:\n");
    if let Some(p) = &input.prefer {
        s.push_str(&format!("- prefer family: {p}\n"));
    } else {
        s.push_str("- no strong preference; spread across capable workers where it helps\n");
    }

    if let Some(digest) = &input.digest {
        s.push_str("\nCURRENT STATE (for re-planning):\n");
        s.push_str(digest);
        s.push('\n');
    }

    s.push_str("\nReturn the JSON DAG now.");
    s
}

/// Collapse a (possibly multi-line) description to a single trimmed line so the
/// catalog stays one entry per line.
fn one_line(s: &str) -> &str {
    s.lines().next().unwrap_or("").trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogs_render_only_when_present() {
        let bare = ConductorInput {
            goal: "do a thing".into(),
            ..Default::default()
        };
        let out = user_prompt(&bare);
        assert!(!out.contains("AVAILABLE TOOLS"));
        assert!(!out.contains("AVAILABLE SKILLS"));

        let with = ConductorInput {
            goal: "do a thing".into(),
            tools: vec![ToolInfo {
                id: "github.search_issues".into(),
                description: "Search issues\nsecond line ignored".into(),
            }],
            skills: vec![SkillInfo {
                name: "pdf-forms".into(),
                description: "Fill PDF forms".into(),
            }],
            ..Default::default()
        };
        let out = user_prompt(&with);
        assert!(out.contains("AVAILABLE TOOLS"));
        assert!(out.contains("- github.search_issues — Search issues"));
        assert!(!out.contains("second line ignored"), "description collapsed to one line");
        assert!(out.contains("AVAILABLE SKILLS"));
        assert!(out.contains("- pdf-forms — Fill PDF forms"));
    }
}
