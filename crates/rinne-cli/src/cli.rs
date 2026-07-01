//! Command-line surface for `rinne` (`CONTEXT.md` §17).
//!
//! Phase 0 defines the full command tree with `clap` derive. Every command is
//! stubbed; the handlers land in their respective phases.

use clap::{Parser, Subcommand};

/// Rinne — local, terminal-first AI orchestration.
///
/// With no subcommand, `rinne` opens the interactive REPL/TUI. With `-p`, it
/// runs one shot headless with structured output.
#[derive(Debug, Parser)]
#[command(name = "rinne", version, about, long_about = None)]
pub struct Cli {
    /// Run one shot headless on the given task and emit structured output,
    /// instead of opening the interactive TUI (`shoal -p` in the spec).
    #[arg(short = 'p', long = "prompt", value_name = "TASK", global = true)]
    pub prompt: Option<String>,

    /// With `-p`, emit a single JSON result (scriptable) instead of streaming
    /// human-readable progress.
    #[arg(long, global = true)]
    pub json: bool,

    /// Increase log verbosity (repeatable). Logs go to a file in `.rinne/`,
    /// never to the TUI.
    #[arg(short = 'v', long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// The `rinne` subcommands (`CONTEXT.md` §17).
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Detect and report backends, auth mode, and quota.
    Doctor,

    /// Load a plan file into the blackboard and run it to completion.
    ///
    /// A Phase 3 entry point: in Phase 4 the conductor generates plans from a
    /// prompt, but this drives a hand-written `plan.json` directly.
    Run {
        /// Path to a `plan.json` describing the DAG.
        plan: String,
    },

    /// Run a native login or set a key for a backend, then re-check.
    Connect {
        /// The backend to connect (e.g. `claude-code`, `deepseek`, `openai`).
        backend: String,
        /// For an API provider: the API key, stored securely in the OS keychain
        /// (set once and forget). Omit to be told how to provide it.
        key: Option<String>,
        /// For an API provider: model id(s) to use (cheap→strong), e.g.
        /// `--model deepseek-ai/deepseek-v4-pro`. Repeatable.
        #[arg(long = "model", value_name = "ID")]
        models: Vec<String>,
        /// Override the API endpoint, so a custom provider name can point at any
        /// OpenAI-compatible host (e.g. NVIDIA: https://integrate.api.nvidia.com/v1).
        #[arg(long = "base-url", value_name = "URL")]
        base_url: Option<String>,
        /// Add the key to the provider's rotation pool instead of replacing it
        /// (multiple keys are rotated across rate limits).
        #[arg(long)]
        add: bool,
    },

    /// Delete a stored API key from the OS keychain (undo `connect <p> <key>`).
    Forget {
        /// The API provider whose stored key to remove (e.g. `deepseek`).
        provider: String,
    },

    /// List models. With a provider, its key's live catalog; with no provider,
    /// every available worker and its model ladder (like the startup intro).
    Models {
        /// The configured API provider to query (e.g. `openrouter`). Omit to
        /// list all available workers and their model ladders.
        provider: Option<String>,
    },

    /// Show the state of the current run (DAG, progress).
    Status,

    /// Resume an interrupted or parked run.
    ///
    /// When a run is parked at a checkpoint or human evaluator, supply your
    /// decision: `--steer` gives the missing guidance (it becomes the critique),
    /// `--approve` accepts the current state, `--reject` replans from scratch.
    Resume {
        /// Inject guidance into the parked node; flows into the loop as critique.
        #[arg(long, value_name = "TEXT")]
        steer: Option<String>,
        /// Accept the current state and move on.
        #[arg(long)]
        approve: bool,
        /// Throw out this approach and replan.
        #[arg(long)]
        reject: bool,
    },

    /// View or edit configuration (conductor, loop, preferences, models).
    ///
    /// No args shows the resolved config. Subcommands edit a file in place,
    /// defaulting to global (`--project` scopes to this repo). Examples:
    ///   rinne config conductor groq llama-3.3-70b
    ///   rinne config prefer api
    ///   rinne config set loop.max_iterations_per_node 5
    Config {
        /// The subcommand and its arguments (e.g. `conductor groq`). Empty shows
        /// the resolved config.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// View trajectory logs (local only).
    Logs,

    /// Connect and manage MCP servers (tools available to your workers).
    ///
    /// Subcommands: `add <name> --stdio "<cmd>" | --http <url>`, `list`,
    /// `tools <name>`, `test <name>`, `remove <name>`.
    Mcp {
        /// The subcommand and its arguments (e.g. `add fs --stdio "npx …"`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Install and manage Agent Skills (instructions your workers can follow).
    ///
    /// Subcommands: `install <path> [--project]`, `list`, `show <name>`,
    /// `remove <name>`. A skill is a folder with a `SKILL.md`.
    Skill {
        /// The subcommand and its arguments (e.g. `install ./skills/pdf-forms`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}
