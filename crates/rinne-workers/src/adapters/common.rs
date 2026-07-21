//! Shared machinery for harness adapters (`CONTEXT.md` §8).
//!
//! A harness worker is an autonomous agent: it gets a chunky self-contained
//! prompt and reads/edits the repo itself, so context is passed as a prompt plus
//! *pinned file paths*, never inlined contents (`CONTEXT.md` §8 behavioral
//! split, §12 context assembler).

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use rinne_core::worker::{
    emit, EventSink, ExecStatus, ExecuteRequest, ExecuteResult, McpServerSpec, Role, Usage, Worker,
    WorkerDescriptor, WorkerEvent,
};
use rinne_core::Result;

use crate::transport::subprocess::{self, LineMapper, SubprocessOutput, SubprocessSpec};

/// How a harness invocation is augmented to use a set of MCP servers — the
/// provision path (`MCP_SKILLS.md` §6). A provisioner writes whatever config the
/// CLI needs and returns the extra argv, the subprocess environment (carrying
/// secret tokens, kept out of the file via `${VAR}` expansion), and a file to
/// clean up afterward.
pub struct Provision {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cleanup: Option<PathBuf>,
}

/// Builds the MCP provisioning for a harness given the node's servers and a
/// scratch directory to write any config file into.
pub type McpProvisioner = fn(servers: &[McpServerSpec], scratch: &Path) -> Result<Provision>;

/// What an adapter extracts from a harness CLI's raw output. Parsers should be
/// defensive: on any doubt, fall back to the raw stdout as the result text so a
/// schema change in a beta CLI degrades gracefully (`CONTEXT.md` §21).
pub struct ParsedHarness {
    pub result: String,
    pub session_id: Option<String>,
    pub usage: Usage,
    /// The CLI signalled an error in its structured output, even if it exited 0.
    pub is_error: bool,
}

impl ParsedHarness {
    /// The trivial parse: use raw stdout, no session, no usage.
    pub fn raw(stdout: &str) -> Self {
        Self {
            result: stdout.trim().to_string(),
            session_id: None,
            usage: Usage::default(),
            is_error: false,
        }
    }
}

/// Build the argv for a harness invocation given the composed prompt and an
/// optional model selection.
pub type ArgsBuilder = fn(prompt: &str, model: Option<&str>) -> Vec<String>;

/// Serializes tests that mutate process-global harness env vars. Shared across
/// adapter modules — `cargo test` runs them on one thread pool, so a per-module
/// lock would not actually exclude them from each other.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Whether harness sessions should pass their non-interactive approval flags.
///
/// Set from `[harness_stage].approvals` via `RINNE_HARNESS_APPROVALS`
/// (`crates/rinne-cli/src/runner.rs::apply_harness_stage_env`). Defaults to
/// auto, matching the config default: a Stage session nobody is watching
/// blocks forever on a permission prompt otherwise.
///
/// Unset means auto, but any recognised *negative* spelling means human — this
/// is a permission switch, so `RINNE_HARNESS_APPROVALS=off` must not fail open
/// into auto-approving every tool call. The vocabulary matches the sibling
/// `RINNE_HARNESS_INTERACTIVE_TUI` parse below.
pub fn approvals_are_auto() -> bool {
    match std::env::var("RINNE_HARNESS_APPROVALS") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "human"
                | "manual"
                | "ask"
                | "prompt"
                | "never"
                | "none"
                | "no"
                | "off"
                | "false"
                | "0"
                | ""
        ),
        Err(_) => true,
    }
}

/// Default interactive argv: model flag (if any) + prompt as a positional arg.
/// Used when an adapter has no custom `interactive_args` (opens product TUI).
pub fn default_interactive_args(prompt: &str, model: Option<&str>) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(m) = model {
        args.push("--model".into());
        args.push(m.into());
    }
    args.push(prompt.into());
    args
}

/// Write a large prompt under the workspace blackboard so we can pass
/// `--prompt-file` instead of blowing ARG_MAX / shell quoting.
fn prompt_file_for(workspace: &Path, prompt: &str) -> std::io::Result<PathBuf> {
    let dir = workspace
        .join(rinne_core::BLACKBOARD_DIR)
        .join("stage-prompts");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!(
        "prompt-{}.txt",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::write(&path, prompt)?;
    Ok(path)
}

/// Grok single-turn via `--prompt-file` (avoids embedding multi-KB plans in argv).
fn grok_prompt_file_args(path: &Path, model: Option<&str>, lean: bool) -> Vec<String> {
    let mut args = vec![
        "--prompt-file".into(),
        path.display().to_string(),
        "--output-format".into(),
        if lean {
            "plain".into()
        } else {
            // Visible Stage plain is human-readable in Terminal; streaming-json
            // is for pure headless parse paths.
            "plain".into()
        },
        "--no-plan".into(),
        "--always-approve".into(),
    ];
    if let Some(m) = model {
        args.push("-m".into());
        args.push(m.into());
    }
    args
}

/// Stage interactive bundle: full task on disk + result path + short kickoff
/// so the product TUI opens cleanly (no multi-KB argv) and Rinne can still
/// recover the deliverable without tee'ing the TTY.
fn stage_task_bundle(
    workspace: &Path,
    full_prompt: &str,
    is_planner: bool,
) -> std::io::Result<(String, PathBuf)> {
    let dir = workspace
        .join(rinne_core::BLACKBOARD_DIR)
        .join("stage-prompts");
    std::fs::create_dir_all(&dir)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let task_path = dir.join(format!("task-{stamp}.md"));
    let result_path = dir.join(format!("result-{stamp}.txt"));
    // Ensure result does not exist yet (poll for create + stable size).
    let _ = std::fs::remove_file(&result_path);
    std::fs::write(&task_path, full_prompt)?;

    let deliverable = if is_planner {
        "the final planning deliverable: a single JSON object (the DAG plan) with no markdown fence unless required"
    } else {
        "your complete final answer / report for this task"
    };

    // Keep kickoff short — long multi-line kicks fight TUI layout on first paint.
    let kickoff = format!(
        "Rinne Stage: complete the task in {task}. \
         Use tools freely. When done, write {deliverable} to {result} (UTF-8, overwrite OK), then exit. \
         No clarifying questions — work autonomously.",
        task = task_path.display(),
        result = result_path.display(),
        deliverable = deliverable,
    );

    Ok((kickoff, result_path))
}

/// Defensive parser for harnesses that emit a single JSON result object: probe
/// common result fields, falling back to raw stdout on any surprise
/// (`CONTEXT.md` §21). Suitable for not-yet-pinned beta CLIs.
pub fn parse_generic_json(out: &SubprocessOutput) -> ParsedHarness {
    let pick = |v: &serde_json::Value| {
        v.get("result")
            .or_else(|| v.get("text"))
            .or_else(|| v.get("message"))
            .or_else(|| v.get("content"))
            .or_else(|| v.get("response"))
            .and_then(|x| x.as_str())
            .map(String::from)
    };
    for line in out.stdout.lines().rev() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(result) = pick(&v) {
                let session_id = v
                    .get("session_id")
                    .or_else(|| v.get("sessionId"))
                    .and_then(|x| x.as_str())
                    .map(String::from);
                let is_error = v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false);
                return ParsedHarness {
                    result,
                    session_id,
                    usage: Usage::default(),
                    is_error,
                };
            }
        }
    }
    ParsedHarness::raw(&out.stdout)
}

/// The trivial parser: the captured stdout is the result (for CLIs that print
/// plain text, e.g. Aider).
pub fn parse_raw(out: &SubprocessOutput) -> ParsedHarness {
    ParsedHarness::raw(&out.stdout)
}
/// Parse a harness CLI's captured output into normalized fields.
pub type OutputParser = fn(out: &SubprocessOutput) -> ParsedHarness;

/// A generic harness worker driven over the `subprocess-json` transport. Each
/// concrete CLI supplies its program name, argv builder, output parser, and an
/// optional line mapper for richer streaming.
pub struct HarnessAdapter {
    pub descriptor: WorkerDescriptor,
    pub program: String,
    pub build_args: ArgsBuilder,
    /// Lean argv used when the harness runs as the **planner** (`Role::Planner`):
    /// a plain-text completion, not the agentic/streaming work invocation. When
    /// set, planning reads the full stdout as the answer (more robust across CLI
    /// versions than parsing a streaming format). `None` → planning reuses
    /// `build_args` and the normal parser unchanged.
    pub plan_args: Option<ArgsBuilder>,
    /// Args for a **real interactive harness TUI** in an external Terminal window.
    /// Must NOT use headless flags (`-p`, `--output-format json`, `exec --json`, …)
    /// so the full product UI is visible. `None` → default: `[prompt]` (+ model if any).
    pub interactive_args: Option<ArgsBuilder>,
    pub parse: OutputParser,
    pub line_mapper: LineMapper,
    /// Whether the prompt is piped via stdin (vs. passed as an argument).
    pub prompt_via_stdin: bool,
    pub default_timeout: Duration,
    /// How this harness is told to use a node's MCP servers (`MCP_SKILLS.md` §6).
    /// `None` means the harness has no MCP provisioning wired: a node's tools are
    /// simply unavailable to it (the node still runs), so the conductor should
    /// prefer an API worker for tool-heavy nodes on such a harness.
    pub provisioner: Option<McpProvisioner>,
}

impl HarnessAdapter {
    /// Replace the static model ladder with a live (or config-merged) list.
    pub fn with_models(mut self, models: Vec<String>) -> Self {
        self.descriptor.models = models;
        self
    }

    /// Ensure `model` is on the ladder (appended as frontier if missing). Used
    /// so a user pin / `[models].by_worker` entry is never dropped as "unknown".
    pub fn ensure_model(mut self, model: &str) -> Self {
        let model = model.trim();
        if model.is_empty() {
            return self;
        }
        if !self.descriptor.models.iter().any(|m| m == model) {
            self.descriptor.models.push(model.to_string());
        }
        self
    }
}

#[async_trait]
impl Worker for HarnessAdapter {
    fn descriptor(&self) -> &WorkerDescriptor {
        &self.descriptor
    }

    /// A harness serves MCP tools when it has a provisioner wired (the provision
    /// path, `MCP_SKILLS.md` §6).
    fn serves_mcp_tools(&self) -> bool {
        self.provisioner.is_some()
    }

    async fn execute(
        &self,
        request: ExecuteRequest,
        events: EventSink,
        cancel: CancellationToken,
    ) -> Result<ExecuteResult> {
        let prompt = compose_prompt(&request);
        let timeout = request
            .constraints
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(self.default_timeout);

        let model = request.constraints.model.as_deref();
        let has_lean = self.plan_args.is_some();
        // The lean plain-text invocation: used for the planner role, and as an
        // automatic fallback when the rich/streaming invocation fails with no
        // output (e.g. a CLI version that rejects the streaming flags). Planning
        // starts lean so a work flag can't make the planner exit non-zero.
        let mut lean = matches!(request.role, Role::Planner) && has_lean;
        let is_planner = matches!(request.role, Role::Planner);

        // Provision the node's MCP servers into this harness once, up front
        // (`MCP_SKILLS.md` §6). A provisioner failure is non-fatal: narrate it and
        // run without the tools rather than failing the node.
        let provision = self.provision(&request, &events);

        // Retry transient failures (spawn errors, timeouts) once before giving
        // up — beta CLIs are flaky (`CONTEXT.md` §21). A cancelled run is not
        // retried. Visible sessions are NOT retried on timeout (that just opens
        // a second broken Terminal window).
        const MAX_ATTEMPTS: u32 = 2;
        let mut attempt = 0;
        let out = loop {
            attempt += 1;
            // Visible Stage = real Terminal.app with the **product harness UI**
            // (Grok Build, Claude Code, …). Capture goes through a result file
            // so we never tee/script the TTY (that caused mouse-protocol garbage).
            // Opt out of the TUI with RINNE_HARNESS_INTERACTIVE_TUI=0 (single-turn
            // plain text in Terminal instead).
            let mut visible = request.constraints.visible_stage;
            let interactive_env = std::env::var("RINNE_HARNESS_INTERACTIVE_TUI")
                .map(|v| v.to_ascii_lowercase())
                .unwrap_or_default();
            let force_plain = matches!(interactive_env.as_str(), "0" | "false" | "no" | "off");
            let want_interactive_tui = visible && !force_plain && self.interactive_args.is_some();

            let builder = if want_interactive_tui {
                self.interactive_args.unwrap_or(default_interactive_args)
            } else if lean {
                self.plan_args.unwrap()
            } else {
                self.build_args
            };
            let mapper = if want_interactive_tui {
                // TUI has no useful line stream; Stage events come from SessionOpened.
                subprocess::raw_lines
            } else if lean || visible {
                subprocess::raw_lines
            } else {
                self.line_mapper
            };

            // Interactive Stage: short kickoff prompt + full task on disk +
            // result file for the deliverable (plan JSON, report, etc.).
            let mut result_file: Option<PathBuf> = None;
            let (mut args, stdin) = if want_interactive_tui {
                match stage_task_bundle(&request.workspace, &prompt, is_planner) {
                    Ok((kickoff, result_path)) => {
                        result_file = Some(result_path);
                        (builder(&kickoff, model), None)
                    }
                    Err(e) => {
                        emit(
                            &events,
                            WorkerEvent::Message(format!(
                                "could not write Stage task files ({e}) — falling back to plain single-turn"
                            )),
                        );
                        // Fall through to plain builders below.
                        if lean {
                            if self.program == "grok" && prompt.len() > 1500 {
                                match prompt_file_for(&request.workspace, &prompt) {
                                    Ok(path) => (grok_prompt_file_args(&path, model, true), None),
                                    Err(_) => (builder(&prompt, model), None),
                                }
                            } else {
                                (self.plan_args.unwrap()(&prompt, model), None)
                            }
                        } else if self.prompt_via_stdin {
                            ((self.build_args)("", model), Some(prompt.clone()))
                        } else {
                            ((self.build_args)(&prompt, model), None)
                        }
                    }
                }
            } else if self.program == "grok" && prompt.len() > 1500 {
                let pf = prompt_file_for(&request.workspace, &prompt);
                match pf {
                    Ok(path) => (grok_prompt_file_args(&path, model, lean), None),
                    Err(_) if self.prompt_via_stdin => (builder("", model), Some(prompt.clone())),
                    Err(_) => (builder(&prompt, model), None),
                }
            } else if self.prompt_via_stdin {
                (builder("", model), Some(prompt.clone()))
            } else {
                (builder(&prompt, model), None)
            };
            // If interactive setup failed we may have cleared want visually but
            // still set result_file only on success — OK.
            let interactive_active = want_interactive_tui && result_file.is_some();

            // Append the MCP flags on the rich work invocation (not the lean
            // planner path, which neither needs tools nor accepts the flags).
            // Skip for interactive visible sessions — MCP provision flags are
            // often headless-oriented; interactive harnesses use their own MCP.
            if !lean && !visible {
                args.extend(provision.args.iter().cloned());
            }
            let spec = SubprocessSpec {
                program: self.program.clone(),
                args,
                workspace: request.workspace.clone(),
                stdin,
                timeout: Some(timeout),
                env: provision.env.clone(),
                result_file: result_file.clone(),
            };
            // Cap concurrent Terminal windows; overflow → headless.
            let visible_slot = if visible {
                match crate::session_gate::try_acquire_visible() {
                    Some(slot) => Some(slot),
                    None => {
                        emit(
                            &events,
                            WorkerEvent::Message(format!(
                                "Harness Stage: max concurrent terminal windows reached — \
                                 running {} headless",
                                self.descriptor.name
                            )),
                        );
                        visible = false;
                        None
                    }
                }
            } else {
                None
            };
            let role_tag = match request.role {
                Role::Planner => "planner",
                Role::Generator => "generator",
                Role::Evaluator => "evaluator",
                _ => "worker",
            };
            if attempt == 1 {
                let model = model.map(|m| m.to_string());
                emit(
                    &events,
                    WorkerEvent::SessionOpened {
                        worker: self.descriptor.name.clone(),
                        model,
                        backend: if visible {
                            if interactive_active {
                                format!("external-terminal/interactive/{role_tag}")
                            } else {
                                format!("external-terminal/single-turn/{role_tag}")
                            }
                        } else {
                            format!("headless/{role_tag}")
                        },
                    },
                );
                if visible {
                    emit(
                        &events,
                        WorkerEvent::Message(format!(
                            "▶ {} · {} · {}",
                            role_tag,
                            self.descriptor.name,
                            if interactive_active {
                                "product UI"
                            } else {
                                "single-turn"
                            }
                        )),
                    );
                }
            }
            let run_result = if visible {
                use crate::transport::external_terminal::{self, TerminalMode};
                let mode = if interactive_active {
                    TerminalMode::Interactive
                } else {
                    TerminalMode::Capture
                };
                // Real Terminal window. Prefer system Terminal; on failure try
                // embedded PTY, then in-process headless as last resort.
                match external_terminal::run_with_mode(spec.clone(), &events, &cancel, mapper, mode)
                    .await
                {
                    Ok(out) => Ok(out),
                    Err(e) => {
                        emit(
                            &events,
                            WorkerEvent::Message(format!(
                                "could not open system Terminal ({e}) — trying embedded PTY"
                            )),
                        );
                        match crate::transport::pty::run(spec.clone(), &events, &cancel, mapper)
                            .await
                        {
                            Ok(out) => Ok(out),
                            Err(e2) => {
                                emit(
                                    &events,
                                    WorkerEvent::Message(format!(
                                        "PTY session failed ({e2}) — headless fallback"
                                    )),
                                );
                                let hb = if lean {
                                    self.plan_args.unwrap_or(self.build_args)
                                } else {
                                    self.build_args
                                };
                                let (mut hargs, hstdin) = if self.prompt_via_stdin {
                                    (hb("", model), Some(prompt.clone()))
                                } else {
                                    (hb(&prompt, model), None)
                                };
                                if !lean {
                                    hargs.extend(provision.args.iter().cloned());
                                }
                                let hspec = SubprocessSpec {
                                    program: self.program.clone(),
                                    args: hargs,
                                    workspace: request.workspace.clone(),
                                    stdin: hstdin,
                                    timeout: Some(timeout),
                                    env: provision.env.clone(),
                                    result_file: None,
                                };
                                subprocess::run(hspec, &events, &cancel, mapper).await
                            }
                        }
                    }
                }
            } else {
                subprocess::run(spec, &events, &cancel, mapper).await
            };
            drop(visible_slot);
            match run_result {
                Ok(out) => {
                    let timed_out = matches!(out.status, ExecStatus::TimedOut);
                    // Never re-open another Terminal window on timeout — that is
                    // what produced the double grok/claude Stage spam.
                    if timed_out && !visible && attempt < MAX_ATTEMPTS && !cancel.is_cancelled() {
                        emit(
                            &events,
                            WorkerEvent::Message(format!(
                                "{} timed out — retrying ({attempt}/{MAX_ATTEMPTS})",
                                self.program
                            )),
                        );
                        continue;
                    }
                    // Rich invocation failed with nothing usable on stdout, and a
                    // lean plain invocation is available → fall back to it once.
                    let empty_fail =
                        !matches!(out.status, ExecStatus::Success) && out.stdout.trim().is_empty();
                    if empty_fail && !lean && has_lean && !cancel.is_cancelled() {
                        lean = true;
                        emit(
                            &events,
                            WorkerEvent::Message(format!(
                                "{} failed in streaming mode — retrying in plain mode",
                                self.program
                            )),
                        );
                        continue;
                    }
                    break out;
                }
                Err(e) => {
                    if attempt < MAX_ATTEMPTS && !cancel.is_cancelled() {
                        emit(
                            &events,
                            WorkerEvent::Message(format!(
                                "{} failed to start ({e}) — retrying ({attempt}/{MAX_ATTEMPTS})",
                                self.program
                            )),
                        );
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        continue;
                    }
                    return Err(e);
                }
            }
        };
        // Best-effort cleanup of the provisioned config file (it holds no secret,
        // but leaving scratch around is untidy).
        if let Some(path) = &provision.cleanup {
            let _ = std::fs::remove_file(path);
        }
        // Lean planner + interactive Stage deliverables are plain text / JSON
        // written to a result file (not NDJSON streams). Take stdout as-is.
        let parsed = if lean || request.constraints.visible_stage {
            ParsedHarness::raw(&out.stdout)
        } else {
            (self.parse)(&out)
        };

        // Reconcile the transport-level status with the parsed error flag.
        let status = match out.status {
            ExecStatus::Success if parsed.is_error => {
                ExecStatus::Failed("worker reported an error".into())
            }
            other => other,
        };

        let mut usage = parsed.usage;
        if usage.wall_ms == 0 {
            usage.wall_ms = out.wall_ms;
        }
        // Many harness CLIs (Grok streaming-json, Aider, …) never report token
        // counts. Fill zeros from the prompt + result so status / ledger / UI
        // show consumption instead of a permanent `0 tok`.
        usage.fill_estimates(&prompt, &parsed.result);

        Ok(ExecuteResult {
            result: parsed.result,
            // Diff capture from the workspace is handled by the dispatcher in
            // Phase 3 (git-aware); adapters leave it None unless the CLI emits
            // one directly.
            file_diff: None,
            transcript: if out.stderr.is_empty() {
                out.stdout
            } else {
                format!("{}\n--- stderr ---\n{}", out.stdout, out.stderr)
            },
            status,
            usage,
            session_id: parsed.session_id,
        })
    }
}

impl HarnessAdapter {
    /// Run this harness's provisioner over a node's MCP servers, narrating the
    /// outcome. Returns an empty provision (no args/env) when the node attaches
    /// no servers, the harness has no provisioner, or provisioning fails.
    fn provision(&self, request: &ExecuteRequest, events: &EventSink) -> Provision {
        let empty = Provision {
            args: Vec::new(),
            env: Vec::new(),
            cleanup: None,
        };
        if request.mcp_servers.is_empty() {
            return empty;
        }
        let Some(provisioner) = self.provisioner else {
            emit(
                events,
                WorkerEvent::Message(format!(
                    "{} has no MCP provisioning — running without {} tool server(s)",
                    self.program,
                    request.mcp_servers.len()
                )),
            );
            return empty;
        };
        let scratch = request
            .workspace
            .join(rinne_core::BLACKBOARD_DIR)
            .join("mcp");
        match provisioner(&request.mcp_servers, &scratch) {
            Ok(p) => {
                let names: Vec<&str> = request
                    .mcp_servers
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect();
                emit(
                    events,
                    WorkerEvent::Message(format!("provisioned MCP: {}", names.join(", "))),
                );
                p
            }
            Err(e) => {
                emit(
                    events,
                    WorkerEvent::Message(format!(
                        "MCP provisioning failed ({e}) — running without tools"
                    )),
                );
                empty
            }
        }
    }
}

/// Render the symbol map neighborhoods into a `## Relevant code structure` section.
/// Returns an empty string when the slice is empty so callers can push it unconditionally.
pub(crate) fn render_symbol_map(neighborhoods: &[rinne_types::graph::Neighborhood]) -> String {
    if neighborhoods.is_empty() {
        return String::new();
    }
    let mut s = String::from("\n\n## Relevant code structure\n");
    for nb in neighborhoods {
        let def = &nb.definition;
        s.push_str(&format!("{} ({}:{})\n", def.name, def.file, def.line));
        if !nb.callers.is_empty() {
            let names: Vec<&str> = nb.callers.iter().map(|r| r.name.as_str()).collect();
            s.push_str(&format!("  called by: {}\n", names.join(", ")));
        }
        if !nb.callees.is_empty() {
            let names: Vec<&str> = nb.callees.iter().map(|r| r.name.as_str()).collect();
            s.push_str(&format!("  calls: {}\n", names.join(", ")));
        }
    }
    s
}

/// Compose a harness prompt from the request: the instruction, any critique fed
/// back on loop-back, ambient steering, and the pinned file paths the worker
/// should read itself.
pub fn compose_prompt(request: &ExecuteRequest) -> String {
    let mut out = String::new();
    out.push_str(&request.instruction);

    if !request.context.skill_text.is_empty() {
        out.push_str("\n\n");
        out.push_str(&request.context.skill_text);
    }

    if !request.context.prior_context.is_empty() {
        out.push_str("\n\n## Context\n");
        out.push_str(&request.context.prior_context);
    }

    if let Some(critique) = &request.context.critique {
        out.push_str("\n\n## Address this feedback from the previous attempt\n");
        out.push_str(critique);
    }

    if let Some(steer) = &request.constraints.steer {
        out.push_str("\n\n## Steering\n");
        out.push_str(steer);
    }

    if !request.context.pinned_paths.is_empty() {
        out.push_str("\n\n## Relevant files (read these)\n");
        for p in &request.context.pinned_paths {
            out.push_str("- ");
            out.push_str(&p.display().to_string());
            out.push('\n');
        }
    }

    out.push_str(&render_symbol_map(&request.context.symbol_map));

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rinne_core::worker::{Constraints, ContextPacket};
    use std::path::PathBuf;

    #[test]
    fn approvals_default_to_auto_and_human_opts_out() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("RINNE_HARNESS_APPROVALS");
        assert!(
            approvals_are_auto(),
            "[harness_stage].approvals defaults to auto, so an unset env must match"
        );
        std::env::set_var("RINNE_HARNESS_APPROVALS", "human");
        assert!(!approvals_are_auto());
        std::env::set_var("RINNE_HARNESS_APPROVALS", "auto");
        assert!(approvals_are_auto());
        std::env::remove_var("RINNE_HARNESS_APPROVALS");
    }

    #[test]
    fn approvals_do_not_fail_open_on_negative_spellings() {
        // A permission switch must not auto-approve because the user wrote
        // `off` instead of the one blessed spelling.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for v in [
            "off", "0", "false", "no", "never", "none", "manual", "ASK", " human ",
        ] {
            std::env::set_var("RINNE_HARNESS_APPROVALS", v);
            assert!(!approvals_are_auto(), "`{v}` must not mean auto-approve");
        }
        for v in ["auto", "AUTO", "yes", "1"] {
            std::env::set_var("RINNE_HARNESS_APPROVALS", v);
            assert!(approvals_are_auto(), "`{v}` must mean auto-approve");
        }
        std::env::remove_var("RINNE_HARNESS_APPROVALS");
    }

    fn req(skill_text: &str) -> ExecuteRequest {
        ExecuteRequest {
            role: Role::Generator,
            instruction: "do the task".into(),
            context: ContextPacket {
                skill_text: skill_text.into(),
                ..Default::default()
            },
            workspace: PathBuf::from("/tmp"),
            constraints: Constraints::default(),
            tools: Vec::new(),
            mcp_servers: Vec::new(),
        }
    }

    #[test]
    fn symbol_map_is_rendered_into_harness_prompt() {
        use rinne_types::graph::{Neighborhood, SymbolRef};
        let nb = Neighborhood {
            definition: SymbolRef {
                name: "helper".into(),
                file: "m.rs".into(),
                line: 1,
                end_line: 1,
            },
            callers: vec![SymbolRef {
                name: "main".into(),
                file: "main.rs".into(),
                line: 5,
                end_line: 5,
            }],
            callees: vec![],
            imports: vec![],
            stale: false,
        };
        let mut r = req("");
        r.context.symbol_map = vec![nb];
        let prompt = compose_prompt(&r);
        assert!(
            prompt.contains("## Relevant code structure"),
            "section header missing"
        );
        assert!(prompt.contains("helper"), "definition name missing");
        assert!(prompt.contains("m.rs:1"), "definition file:line missing");
        assert!(prompt.contains("main"), "caller name missing");
    }

    #[test]
    fn skill_text_is_injected_after_instruction() {
        let prompt = compose_prompt(&req("## Skill: pdf-forms\nFlatten the form"));
        assert!(prompt.starts_with("do the task"));
        assert!(prompt.contains("## Skill: pdf-forms"));
        assert!(prompt.contains("Flatten the form"));
    }

    #[test]
    fn absent_skill_text_adds_nothing() {
        assert_eq!(compose_prompt(&req("")), "do the task");
    }

    fn harness(provisioner: Option<McpProvisioner>) -> HarnessAdapter {
        use rinne_core::worker::{
            AuthMode, Capability, LatencyProfile, QuotaModel, Transport, WorkerFamily,
        };
        HarnessAdapter {
            descriptor: WorkerDescriptor {
                name: "test".into(),
                family: WorkerFamily::Harness,
                capabilities: vec![Capability::CodeEdit],
                auth_mode: AuthMode::Subscription,
                quota: QuotaModel::unlimited(),
                latency: LatencyProfile::Medium,
                transport: Transport::SubprocessJson,
                models: vec![],
            },
            program: "test".into(),
            build_args: |_, _| vec![],
            plan_args: None,
            interactive_args: None,
            parse: parse_raw,
            line_mapper: crate::transport::subprocess::raw_lines,
            prompt_via_stdin: false,
            default_timeout: std::time::Duration::from_secs(1),
            provisioner,
        }
    }

    fn tool_request() -> ExecuteRequest {
        use rinne_core::worker::{McpServerSpec, McpTransportKind};
        let mut r = req("");
        r.mcp_servers = vec![McpServerSpec {
            name: "srv".into(),
            transport: McpTransportKind::Stdio,
            command: Some("cmd".into()),
            args: vec![],
            env: vec![],
            url: None,
            headers: vec![],
            token_env: None,
            token: None,
            auth: None,
            auth_header: None,
        }];
        r
    }

    #[test]
    fn provisioner_supplies_args_and_env_for_a_tool_node() {
        fn prov(_s: &[McpServerSpec], _p: &std::path::Path) -> Result<Provision> {
            Ok(Provision {
                args: vec!["--mcp-config".into(), "x.json".into()],
                env: vec![("TOKEN".into(), "secret".into())],
                cleanup: None,
            })
        }
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let p = harness(Some(prov)).provision(&tool_request(), &tx);
        assert_eq!(p.args, vec!["--mcp-config", "x.json"]);
        assert_eq!(p.env, vec![("TOKEN".to_string(), "secret".to_string())]);
    }

    #[test]
    fn no_provisioner_yields_empty_provision_and_narrates() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let p = harness(None).provision(&tool_request(), &tx);
        assert!(
            p.args.is_empty(),
            "no flags when the harness can't provision"
        );
        assert!(p.env.is_empty());
        // The gap is surfaced, not silent.
        let mut narrated = false;
        while let Ok(ev) = rx.try_recv() {
            if let WorkerEvent::Message(m) = ev {
                if m.contains("no MCP provisioning") {
                    narrated = true;
                }
            }
        }
        assert!(narrated, "a missing provisioner should be narrated");
    }

    #[test]
    fn no_servers_means_no_provision_work() {
        fn prov(_s: &[McpServerSpec], _p: &std::path::Path) -> Result<Provision> {
            panic!("must not be called when the node attaches no servers");
        }
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let p = harness(Some(prov)).provision(&req(""), &tx); // req() has no mcp_servers
        assert!(p.args.is_empty());
    }
}
