//! `rinne` — the binary entry point and command dispatch.
//!
//! Phase 0 wires the full `clap` command tree (`CONTEXT.md` §17) to stubbed
//! handlers and stands up file-based logging. Real handlers land per-phase.

mod cli;
mod commands;
mod runner;
mod telemetry;
mod tui;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Command};
use rinne_core::BLACKBOARD_DIR;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Cli::parse();

    let blackboard = PathBuf::from(BLACKBOARD_DIR);
    let _log_guard = telemetry::init(&blackboard, args.verbose);

    // A `-p` prompt means one-shot headless mode regardless of subcommand.
    if let Some(task) = args.prompt.as_deref() {
        return run_oneshot(task, args.json).await;
    }

    match args.command {
        None => run_interactive().await,
        Some(Command::Doctor) => run_doctor().await,
        Some(Command::Run { plan }) => commands::run::run(&plan).await,
        Some(Command::Connect { backend, key, models, base_url, add }) => {
            commands::connect::run(&backend, key, models, base_url, add).await
        }
        Some(Command::Forget { provider }) => commands::forget::run(&provider).await,
        Some(Command::Models { provider }) => commands::models::run(&provider).await,
        Some(Command::Status) => run_status().await,
        Some(Command::Resume {
            steer,
            approve,
            reject,
        }) => commands::run::resume(steer, approve, reject).await,
        Some(Command::Config { args }) => commands::config::run(&args).await,
        Some(Command::Logs) => run_logs().await,
    }
}

async fn run_interactive() -> Result<()> {
    tui::run().await
}

async fn run_oneshot(task: &str, json: bool) -> Result<()> {
    commands::run::oneshot(task, json).await
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
