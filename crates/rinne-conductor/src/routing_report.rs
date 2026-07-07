//! `rinne doctor --routing` report assembly.

use rinne_config::model::{ConductorConfig, RoutingConfig};
use rinne_core::worker::WorkerDescriptor;
use rinne_core::pool;

use crate::conductor_eligible::{filter_eligible_ladder, is_conductor_eligible};
use crate::tier_exemplars::{self, LoadedExemplar};

/// Human-readable routing diagnostics.
pub fn format_routing_report(
    conductor: &ConductorConfig,
    routing: &RoutingConfig,
    workers: &[WorkerDescriptor],
    user_exemplars: &[LoadedExemplar],
    api_key_present: bool,
) -> String {
    let mut out = String::new();
    out.push_str("rinne doctor — routing\n\n");

    out.push_str("PLANNER RUNGS (conductor-eligible only)\n");
    let raw = conductor.planner_ladder();
    let filtered = filter_eligible_ladder(&raw, conductor.only_eligible_models, &conductor.allowlist);
    for (i, m) in raw.iter().enumerate() {
        let eligible = is_conductor_eligible(m, &conductor.allowlist);
        let in_ladder = filtered.iter().any(|x| x == m);
        let reach = if !api_key_present && !eligible {
            "no api key"
        } else if !eligible {
            "not CEMR-eligible"
        } else if in_ladder {
            "active"
        } else {
            "filtered out"
        };
        let mark = if in_ladder { "✔" } else { "·" };
        out.push_str(&format!("  {mark} [{i}] {m} — {reach}\n"));
    }
    if filtered.is_empty() {
        out.push_str("  (no reachable planner rungs — configure [conductor] or add API key)\n");
    }
    out.push_str(&format!(
        "  auto_escalate={} max_escalations={}\n",
        conductor.auto_escalate, conductor.max_escalations
    ));

    out.push_str("\nEXECUTION TIERS (worker ladders cheap→strong)\n");
    let profile = pool::profile(workers);
    if profile.workers.is_empty() {
        out.push_str("  (no workers — run `rinne connect` or enable harnesses)\n");
    } else {
        for w in &profile.workers {
            let ladder = if w.ladder.is_empty() {
                "(fixed model)".into()
            } else {
                w.ladder.join(" → ")
            };
            out.push_str(&format!("  {} · {} · {}\n", w.worker, w.family, ladder));
        }
    }

    out.push_str("\nTIER EXEMPLARS (shipped + project)\n");
    let counts = tier_exemplars::tier_counts(user_exemplars);
    for (label, n) in ["T0", "T1", "T2", "T3", "T4"].iter().zip(counts) {
        out.push_str(&format!("  {label}: {n} exemplars\n"));
    }
    if !user_exemplars.is_empty() {
        out.push_str(&format!("  project: {} custom exemplar(s)\n", user_exemplars.len()));
    } else if let Some(p) = &routing.exemplars_file {
        out.push_str(&format!("  project: none (optional {p})\n"));
    }

    if !routing.goal_keywords.is_empty() {
        out.push_str("\nGOAL KEYWORD OVERRIDES\n");
        for (k, t) in &routing.goal_keywords {
            out.push_str(&format!("  {k} → {t}\n"));
        }
    }

    for (tier, rule) in &routing.tiers {
        if rule.require_human_checkpoint || rule.require_tool_eval {
            out.push_str(&format!(
                "\n[{tier}] require_tool_eval={} require_human_checkpoint={}\n",
                rule.require_tool_eval, rule.require_human_checkpoint
            ));
        }
    }

    out
}