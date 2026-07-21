//! Command-line surface for `rinne` (`CONTEXT.md` §17).
//!
//! Phase 0 defines the full command tree with `clap` derive. Every command is
//! stubbed; the handlers land in their respective phases.

use clap::{Parser, Subcommand};

/// Rinne — local, terminal-first AI orchestration.
///
/// With no subcommand, `rinne` opens the interactive REPL/TUI. With `-p`, it
/// runs one shot headless with structured output. With `--continue`, it resumes
/// the previous orchestration session from saved state.
///
/// `rinne .` (or `rinne open .`) opens the **macOS app** on that folder — like
/// VS Code's `code .`.
#[derive(Debug, Parser)]
#[command(name = "rinne", version, about, long_about = None)]
pub struct Cli {
    /// Run one shot headless on the given task and emit structured output,
    /// instead of opening the interactive TUI (`shoal -p` in the spec).
    #[arg(short = 'p', long = "prompt", value_name = "TASK", global = true)]
    pub prompt: Option<String>,

    /// Resume the previous orchestration session from `.rinne/` (plan + machine
    /// state) without re-planning. Fails if there is no prior session.
    ///
    /// For a parked human gate, prefer `rinne resume --steer` / `--approve` /
    /// `--reject`. This flag is the short path for "pick up the last run."
    #[arg(short = 'c', long = "continue")]
    pub continue_session: bool,

    /// With `-p`, emit a single JSON result (scriptable) instead of streaming
    /// human-readable progress.
    #[arg(long, global = true)]
    pub json: bool,

    /// Increase log verbosity (repeatable). Logs go to a file in `.rinne/`,
    /// never to the TUI.
    #[arg(short = 'v', long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Disable the code graph for this run (faster startup; skips indexing).
    /// Also useful as a benchmark toggle to measure graph overhead.
    #[arg(long, global = true)]
    pub no_graph: bool,

    /// Skip AI narration for `learn explain` (template-only HTML, no worker needed).
    #[arg(long, global = true)]
    pub no_ai: bool,

    /// Print an 'open in browser' hint with the output path (learn explain).
    #[arg(long, global = true)]
    pub open: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// The `rinne` subcommands (`CONTEXT.md` §17).
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Detect and report backends, auth mode, and quota.
    Doctor {
        /// Show planner rungs, execution tiers, and exemplar counts.
        #[arg(long)]
        routing: bool,
    },

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
        /// Emit machine-readable JSON (`{ "workers": { "name": ["model", …] } }`).
        /// Used by the macOS app so model lists never depend on text parsing.
        #[arg(long)]
        json: bool,
        /// With a provider: include the live `/v1/models` catalog (all available
        /// ids, prices, context). Used by the macOS Models tab "Browse catalog".
        #[arg(long)]
        catalog: bool,
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

    /// Session-scoped human control: pin roles, show active overrides.
    ///
    /// Subcommands mirror the TUI `/human` slash command (`CONDUCTOR_LOOP_PLAN.md` §4).
    Human {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Show live subscription / rate-limit usage for available workers.
    ///
    /// Claude Code reports 5h + weekly windows when logged in. Other harnesses
    /// show `n/a` until a probe exists. Alias: `usage`.
    #[command(visible_alias = "usage")]
    LimitUsage,

    /// Connect and manage MCP servers (tools available to your workers).
    ///
    /// `add <link> [--name <name>]` where link is an http(s) URL or a launch
    /// command. Auth: `--bearer <token>`, `--api-key <token> [--auth-header
    /// <NAME>]`, or `--oauth` (browser login) for remote; `--secret-env
    /// <VAR>=<token>` for local. Also `list`, `tools <name>`, `test <name>`,
    /// `login <name>`, `remove <name>`.
    Mcp {
        /// The subcommand and its arguments (e.g. `add https://mcp.example.com/x`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Install and manage Agent Skills (instructions your workers can follow).
    ///
    /// Subcommands: `add <path>` (a skill folder or a `SKILL.md` file), `list`,
    /// `show <name>`, `remove <name>`.
    Skill {
        /// The subcommand and its arguments (e.g. `add ./skills/pdf-forms`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Inspect the local code graph (symbols, call edges, file coverage).
    ///
    /// Subcommands: `stats`, `symbols <file>`, `neighborhood <symbol>`.
    /// Reads from `.rinne/state.db`; does not require a running plan.
    Graph {
        #[command(subcommand)]
        cmd: GraphCmd,
    },

    /// Synthesise a code-literacy document for a topic or symbol.
    ///
    /// Subcommands: `explain <topic>` — resolves symbols, assembles snippets,
    /// optionally narrates with an AI worker, and writes `.rinne/learn/<slug>.html`
    /// (topic is slugged: e.g. `accounts module` → `accounts-module.html`).
    Learn {
        #[command(subcommand)]
        cmd: LearnCmd,
    },

    /// Open the Rinne macOS app on a folder (like `code .` for VS Code).
    ///
    /// Examples:
    ///   rinne open
    ///   rinne open .
    ///   rinne open ~/Desktop/my-project
    ///
    /// Shorthand: `rinne .` (no subcommand) does the same.
    Open {
        /// Project directory (default: current directory).
        #[arg(default_value = ".")]
        path: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn continue_long_flag_parses() {
        let cli = Cli::try_parse_from(["rinne", "--continue"]).expect("parse --continue");
        assert!(cli.continue_session);
        assert!(cli.prompt.is_none());
        assert!(cli.command.is_none());
    }

    #[test]
    fn continue_short_flag_parses() {
        let cli = Cli::try_parse_from(["rinne", "-c"]).expect("parse -c");
        assert!(cli.continue_session);
    }

    #[test]
    fn continue_absent_by_default() {
        let cli = Cli::try_parse_from(["rinne"]).expect("parse bare rinne");
        assert!(!cli.continue_session);
    }

    #[test]
    fn help_mentions_continue() {
        let mut cmd = Cli::command();
        let help = cmd.render_long_help().to_string();
        assert!(
            help.contains("--continue"),
            "long help should document --continue:\n{help}"
        );
        assert!(
            help.contains("-c") || help.contains(" -c,"),
            "long help should document short -c:\n{help}"
        );
    }
}

/// Subcommands for `rinne learn`.
#[derive(Debug, Subcommand)]
pub enum LearnCmd {
    /// Explain a topic: resolve related symbols, assemble code snippets,
    /// optionally narrate with an AI worker, and write an HTML document.
    Explain {
        /// The topic, symbol name, or path fragment to explain.
        topic: String,
    },

    /// Serve every generated explainer in a browser, with a sidebar to switch
    /// between topics. Reads `.rinne/learn/*.html`; does not regenerate them.
    ///
    /// One process owns port 7420 at a time. Starting serve in a different
    /// project takes the port over (stops the previous owner) so you don't
    /// browse ghost docs. Same project reuses the live server.
    Serve {
        /// Port to bind on 127.0.0.1 (default: 7420).
        #[arg(long, default_value_t = 7420)]
        port: u16,

        /// Don't open a browser automatically.
        #[arg(long)]
        no_open: bool,

        /// Stop whatever rinne learn serve currently owns `--port` (including a
        /// ghost from another terminal/project) and exit.
        #[arg(long)]
        stop: bool,
    },
}

/// Subcommands for `rinne graph`.
#[derive(Debug, Subcommand)]
pub enum GraphCmd {
    /// Index (or re-index) the whole repo into the code graph now, then report
    /// how many files were indexed. Runs synchronously; useful for populating
    /// and inspecting the graph without starting a full run.
    Index,

    /// Show indexed file, symbol, and edge counts.
    Stats,

    /// List all symbols indexed from a given file.
    Symbols {
        /// Path to the file (relative to workspace root or absolute).
        file: String,
    },

    /// Show the neighborhood (callers, callees, imports) of a symbol.
    Neighborhood {
        /// Exact symbol name to look up.
        symbol: String,
    },
}
