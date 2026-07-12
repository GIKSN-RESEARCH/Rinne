//! `rinne limit-usage` / `/limit-usage` — live subscription window usage
//! plus this run's token ledger from `.rinne/state.db`.

use anyhow::Result;

use rinne_config::limits::{probe_limits, LimitReport};
use rinne_config::Config;
use rinne_core::{format_token_count, Blackboard};

/// Probe available workers and return formatted report lines.
pub async fn report_lines(config: &Config) -> Result<Vec<String>> {
    let report = collect(config).await?;
    let mut lines = report.format_lines();
    lines.push(String::new());
    lines.extend(session_token_lines());
    Ok(lines)
}

/// Full structured report (for the TUI chip + alerts).
pub async fn collect(config: &Config) -> Result<LimitReport> {
    let doctor = rinne_config::doctor(config, false).await?;
    Ok(probe_limits(&doctor.workers, &config.limits).await)
}

/// Token consumption for the current project's run (from the usage ledger).
fn session_token_lines() -> Vec<String> {
    let mut out = vec!["  this run (token ledger)".into()];
    let Ok(cwd) = std::env::current_dir() else {
        out.push("    (could not resolve cwd)".into());
        return out;
    };
    if !Blackboard::exists(&cwd) {
        out.push("    (no .rinne/ run in this directory yet)".into());
        return out;
    }
    let Ok(bb) = Blackboard::open(&cwd) else {
        out.push("    (could not open blackboard)".into());
        return out;
    };
    let Ok(state) = rinne_core::State::open(&bb.state_db_path()) else {
        out.push("    (could not open state.db)".into());
        return out;
    };
    let Ok(total) = state.total_usage() else {
        out.push("    (could not read usage)".into());
        return out;
    };
    if total.total_tokens() == 0 {
        out.push(
            "    0 tok recorded — re-run after rebuild; older harness rows often logged 0 \
             because Grok did not report usage"
                .into(),
        );
        return out;
    }
    out.push(format!(
        "    total  {} tok  (in {} · out {})  · {} ms",
        format_token_count(total.total_tokens()),
        format_token_count(total.prompt_tokens),
        format_token_count(total.completion_tokens),
        total.wall_ms
    ));
    if let Ok(rows) = state.usage_rows() {
        use std::collections::BTreeMap;
        let mut by_worker: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        for r in rows {
            let w = if r.worker.is_empty() {
                "(unknown)".into()
            } else {
                r.worker
            };
            let e = by_worker.entry(w).or_default();
            e.0 += r.prompt_tokens;
            e.1 += r.completion_tokens;
        }
        for (w, (inp, out_tok)) in by_worker {
            out.push(format!(
                "    {:<12} {} tok  (in {} · out {})",
                w,
                format_token_count(inp + out_tok),
                format_token_count(inp),
                format_token_count(out_tok)
            ));
        }
    }
    out
}

/// CLI entry: print the report to stdout.
pub async fn run() -> Result<()> {
    let config = rinne_config::load_cwd()?;
    for line in report_lines(&config).await? {
        println!("{line}");
    }
    Ok(())
}
