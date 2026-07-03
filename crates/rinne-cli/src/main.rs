//! `rinne` — the binary entry point and command dispatch.
//!
//! Phase 0 wires the full `clap` command tree (`CONTEXT.md` §17) to stubbed
//! handlers and stands up file-based logging. Real handlers land per-phase.

mod catalog;
mod cli;
mod commands;
mod mcp_pool;
mod runner;
mod telemetry;
mod tui;
mod update;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Command, GraphCmd as CliGraphCmd};
use rinne_core::BLACKBOARD_DIR;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Cli::parse();

    let blackboard = PathBuf::from(BLACKBOARD_DIR);
    let _log_guard = telemetry::init(&blackboard, args.verbose);

    let no_graph = args.no_graph;

    // A `-p` prompt means one-shot headless mode regardless of subcommand.
    if let Some(task) = args.prompt.as_deref() {
        return run_oneshot(task, args.json, no_graph).await;
    }

    // Best-effort new-release banner; never blocks or fails a command. Skipped
    // in `--json` mode and (inside `notify`) when stderr is not a terminal.
    if !args.json {
        if let Ok(config) = rinne_config::load_cwd() {
            update::notify(&config).await;
        }
    }

    match args.command {
        None => run_interactive(no_graph).await,
        Some(Command::Doctor) => run_doctor().await,
        Some(Command::Run { plan }) => commands::run::run(&plan, no_graph).await,
        Some(Command::Connect { backend, key, models, base_url, add }) => {
            commands::connect::run(&backend, key, models, base_url, add).await
        }
        Some(Command::Forget { provider }) => commands::forget::run(&provider).await,
        Some(Command::Models { provider }) => commands::models::run(provider.as_deref()).await,
        Some(Command::Status) => run_status().await,
        Some(Command::Resume {
            steer,
            approve,
            reject,
        }) => commands::run::resume(steer, approve, reject, no_graph).await,
        Some(Command::Config { args }) => commands::config::run(&args).await,
        Some(Command::Mcp { args }) => commands::mcp::run(&args).await,
        Some(Command::Skill { args }) => commands::skill::run(&args).await,
        Some(Command::Logs) => run_logs().await,
        Some(Command::Graph { cmd }) => {
            let cwd = std::env::current_dir()?;
            let graph_cmd = match cmd {
                CliGraphCmd::Stats => commands::graph::GraphCmd::Stats,
                CliGraphCmd::Symbols { file } => commands::graph::GraphCmd::Symbols { file },
                CliGraphCmd::Neighborhood { symbol } => commands::graph::GraphCmd::Neighborhood { symbol },
            };
            commands::graph::run(graph_cmd, cwd).await
        }
    }
}

async fn run_interactive(no_graph: bool) -> Result<()> {
    tui::run(no_graph).await
}

async fn run_oneshot(task: &str, json: bool, no_graph: bool) -> Result<()> {
    commands::run::oneshot(task, json, no_graph).await
}

async fn run_doctor() -> Result<()> {
    commands::doctor::run(false).await
}


async fn run_status() -> Result<()> {
    commands::status::run().await
}

async fn run_logs() -> Result<()> {
    commands::logs::run().await
}
