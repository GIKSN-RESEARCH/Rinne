//! `rinne` — the binary entry point and command dispatch.
//!
//! Phase 0 wires the full `clap` command tree (`CONTEXT.md` §17) to stubbed
//! handlers and stands up file-based logging. Real handlers land per-phase.

mod cli;
mod commands;
mod telemetry;

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
        return run_oneshot(task).await;
    }

    match args.command {
        None => run_interactive().await,
        Some(Command::Doctor) => run_doctor().await,
        Some(Command::Connect { backend }) => run_connect(&backend).await,
        Some(Command::Status) => run_status().await,
        Some(Command::Resume) => run_resume().await,
        Some(Command::Config) => run_config().await,
        Some(Command::Logs) => run_logs().await,
    }
}

/// Marker used by every Phase 0 stub so the unimplemented surface is obvious
/// and uniform at runtime.
fn not_implemented(what: &str, phase: &str) -> Result<()> {
    println!("rinne: `{what}` is not implemented yet (lands in {phase}).");
    Ok(())
}

async fn run_interactive() -> Result<()> {
    not_implemented("interactive TUI", "Phase 6")
}

async fn run_oneshot(_task: &str) -> Result<()> {
    not_implemented("one-shot headless (-p)", "Phase 4 / hardened in Phase 7")
}

async fn run_doctor() -> Result<()> {
    commands::doctor::run(false).await
}

async fn run_connect(backend: &str) -> Result<()> {
    commands::connect::run(backend).await
}

async fn run_status() -> Result<()> {
    not_implemented("status", "Phase 3")
}

async fn run_resume() -> Result<()> {
    not_implemented("resume", "Phase 3")
}

async fn run_config() -> Result<()> {
    commands::config::run().await
}

async fn run_logs() -> Result<()> {
    not_implemented("logs", "Phase 7")
}
