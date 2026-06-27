//! The interactive REPL/TUI (`CONTEXT.md` §6; `PHASE.md` P6).
//!
//! Harness-style rendering: the transcript is written to the terminal's NORMAL
//! scrollback (natively scrollable, never clipped to the window, and preserved
//! when you quit), while a small inline viewport at the bottom holds the live
//! status, any in-progress streamed text, the `@`-picker, and the prompt. This
//! mirrors how Claude Code / Grok behave, rather than a fixed full-screen grid.

mod index;
mod complete;
mod markdown;
mod picker;
mod ui;

use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyboardEnhancementFlags,
    KeyCode, KeyEventKind, KeyModifiers, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, Clear, ClearType};
use crossterm::cursor::MoveTo;
use crossterm::execute;
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::{Terminal, TerminalOptions, Viewport};
use tokio_util::sync::CancellationToken;

use rinne_conductor::ConductorInput;
use rinne_core::{
    Blackboard, Engine, EngineEvent, HumanDecision, NodeStatus, ResumeInput, RunReport, StopReason,
};

use crate::runner;
use complete::Completion;
use index::FileIndex;
use picker::Picker;

/// Guardrail: refuse a glob that expands past this before the conductor sees it.
const GLOB_LIMIT: usize = 400;

/// Cap on persisted history entries kept in memory / loaded from disk.
const HISTORY_MAX: usize = 1000;

/// Recent actions kept per agent in the live view.
const ACTIONS_PER_AGENT: usize = 4;

/// Map a worker action event to a friendly, verb-first label (Claude-Code
/// style). Returns `None` for non-action events. Adapter payloads already carry
/// human text; this only normalises the leading verb and strips redundant
/// adapter prefixes.
fn action_label(ev: &rinne_core::worker::WorkerEvent) -> Option<String> {
    use rinne_core::worker::WorkerEvent::*;
    let strip = |s: &str, p: &str| s.strip_prefix(p).map(str::trim).map(str::to_string);
    match ev {
        Reading(m) => Some(format!("Read {}", m.trim())),
        Editing(m) => {
            if let Some(rest) = strip(m, "writing ") {
                Some(format!("Wrote {rest}"))
            } else if let Some(rest) = strip(m, "editing ") {
                Some(format!("Edited {rest}"))
            } else {
                Some(format!("Edited {}", m.trim()))
            }
        }
        ToolUse(m) => {
            let m = m.trim();
            if let Some(rest) = strip(m, "grep ") {
                Some(format!("Searched {rest}"))
            } else if let Some(rest) = strip(m, "glob ") {
                Some(format!("Listed {rest}"))
            } else if let Some(rest) = strip(m, "subagent: ") {
                Some(format!("Delegated {rest}"))
            } else {
                Some(m.to_string())
            }
        }
        Token(_) | Thinking(_) | Message(_) | Raw(_) | Done => None,
    }
}

/// A node as shown in the (printed) plan listing and progress count.
#[derive(Clone)]
pub struct NodeView {
    pub id: String,
    pub role: String,
    pub status: NodeStatus,
    pub worker: String,
}

/// The kind of transcript entry, which drives its glyph, indent, and color.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum FeedKind {
    System,
    Conductor,
    NodeStart,
    /// A completed worker message, rendered as terminal markdown.
    Markdown,
    /// A reasoning ("thinking") block from a reasoning model, rendered dimmed.
    Thinking,
    NodeOk,
    NodeFail,
    Parked,
}

/// One transcript line, committed to scrollback.
#[derive(Clone)]
pub struct FeedEntry {
    pub kind: FeedKind,
    pub text: String,
    /// The node id this entry belongs to, shown as a gutter for worker output.
    pub node: Option<String>,
}

/// Messages delivered to the app loop from background runs.
pub enum AppMsg {
    Engine(EngineEvent),
    Planned { goal: String, nodes: Vec<NodeView> },
    Finished(RunReport),
    Failed(String),
    Reindex,
    /// A block of informational lines (e.g. `/workers`, `/connect` output).
    Note(String),
}

/// The TUI application state.
pub struct App {
    goal: Option<String>,
    nodes: Vec<NodeView>,
    /// Lines committed and waiting to be flushed into terminal scrollback.
    pending: Vec<FeedEntry>,
    /// In-progress streamed text for the current node (token-level workers),
    /// shown live in the viewport until a boundary commits it to scrollback.
    live_tail: String,
    /// In-progress reasoning text for the live node (reasoning models), shown
    /// dimmed and committed just before the answer block.
    live_thinking: String,
    live_node: Option<String>,
    /// Recent friendly actions per node (newest last), shown under the agent's
    /// heading in the live view. Capped at `ACTIONS_PER_AGENT`.
    live_actions: std::collections::HashMap<String, Vec<String>>,
    input: String,
    /// Cursor position in `input` as a byte index (always on a char boundary).
    cursor: usize,
    /// Submitted prompts, newest last, for up/down recall.
    history: Vec<String>,
    /// Current position while browsing history (`None` = editing a fresh line).
    history_pos: Option<usize>,
    /// The in-progress line saved when history browsing began, restored on exit.
    draft: String,
    /// File backing `history` across sessions (`.rinne/history`); `None` in tests.
    history_path: Option<PathBuf>,
    running: bool,
    parked: Option<String>,
    picker: Option<Picker>,
    completion: Option<Completion>,
    index: FileIndex,
    spinner: usize,
    /// Whether reasoning blocks render expanded (full) or collapsed (a summary
    /// line). Toggled with ctrl+o; governs blocks committed from here on.
    expand_thinking: bool,
    clear_requested: bool,
    should_quit: bool,
    cancel: Option<CancellationToken>,
    tx: tokio::sync::mpsc::UnboundedSender<AppMsg>,
}

impl App {
    fn new(index: FileIndex, tx: tokio::sync::mpsc::UnboundedSender<AppMsg>) -> Self {
        let mut app = Self {
            goal: None,
            nodes: Vec::new(),
            pending: Vec::new(),
            live_tail: String::new(),
            live_thinking: String::new(),
            live_node: None,
            live_actions: std::collections::HashMap::new(),
            input: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_pos: None,
            draft: String::new(),
            history_path: None,
            running: false,
            parked: None,
            picker: None,
            completion: None,
            index,
            spinner: 0,
            expand_thinking: false,
            clear_requested: false,
            should_quit: false,
            cancel: None,
            tx,
        };
        app.push(FeedKind::System, "Welcome to Rinne. Describe what you want done and press Enter.");
        app.push(FeedKind::System, "Type @ to reference files · /help for commands · ctrl-q to quit · scroll the terminal to see history.");
        app
    }

    /// Queue a committed line for scrollback.
    fn push(&mut self, kind: FeedKind, text: impl Into<String>) {
        self.pending.push(FeedEntry { kind, text: text.into(), node: None });
    }

    /// Commit any accumulated reasoning as a dimmed thinking block. Reasoning
    /// always precedes the answer, so this is flushed first.
    fn commit_thinking(&mut self) {
        if !self.live_thinking.trim().is_empty() {
            let node = self.live_node.clone();
            self.pending.push(FeedEntry {
                kind: FeedKind::Thinking,
                text: std::mem::take(&mut self.live_thinking).trim().to_string(),
                node,
            });
        }
        self.live_thinking.clear();
    }

    /// Commit the accumulated worker message as one markdown block (rendered to
    /// scrollback at flush time), tagged with its node for a gutter. Any pending
    /// reasoning block is flushed first so thinking renders above the answer.
    fn commit_tail(&mut self) {
        self.commit_thinking();
        if !self.live_tail.trim().is_empty() {
            let node = self.live_node.clone();
            self.pending.push(FeedEntry {
                kind: FeedKind::Markdown,
                text: std::mem::take(&mut self.live_tail).trim().to_string(),
                node,
            });
        }
        self.live_tail.clear();
        self.live_node = None;
    }

    /// Make `id` the live node, committing any previous node's block first.
    fn ensure_live(&mut self, id: &str) {
        if self.live_node.as_deref() != Some(id) {
            self.commit_tail();
            self.live_node = Some(id.to_string());
        }
    }

    /// Append a reasoning delta. Accumulates verbatim like content; shown dimmed
    /// and committed just before the answer begins.
    fn stream_thinking(&mut self, id: &str, text: &str) {
        self.ensure_live(id);
        self.live_thinking.push_str(text);
    }

    /// Append a raw streaming delta verbatim. Token fragments are often mid-word,
    /// so they are concatenated with no separator; the whole message accumulates
    /// and renders as one markdown block at the next boundary. This is what makes
    /// any streamed model output read like Claude Code, not one token per row.
    fn stream_token(&mut self, id: &str, text: &str) {
        self.ensure_live(id);
        // The answer has started — flush the reasoning block above it.
        if !self.live_thinking.is_empty() {
            self.commit_thinking();
        }
        self.live_tail.push_str(text);
    }

    /// Append a discrete line/block, ensuring it starts on its own line so
    /// successive messages (e.g. Claude's blocks) don't run together.
    fn stream_line(&mut self, id: &str, text: &str) {
        self.ensure_live(id);
        if !self.live_tail.is_empty() && !self.live_tail.ends_with('\n') {
            self.live_tail.push('\n');
        }
        self.live_tail.push_str(text);
    }

    fn progress(&self) -> (usize, usize) {
        let done = self
            .nodes
            .iter()
            .filter(|n| n.status == NodeStatus::Succeeded)
            .count();
        (done, self.nodes.len())
    }

    fn apply(&mut self, msg: AppMsg) {
        match msg {
            AppMsg::Planned { goal, nodes } => {
                self.commit_tail();
                self.goal = Some(goal);
                self.nodes = nodes;
                self.live_actions.clear();
                self.running = true;
                self.parked = None;
                // Print the plan to scrollback as one block.
                let mut listing = String::from("Plan:");
                for n in &self.nodes {
                    listing.push_str(&format!("\n  ○ {:<5} {}", n.id, n.role));
                }
                self.push(FeedKind::Conductor, listing);
            }
            AppMsg::Engine(ev) => self.apply_engine(ev),
            AppMsg::Finished(report) => {
                self.commit_tail();
                self.running = false;
                self.cancel = None;
                for (id, status) in &report.node_statuses {
                    if let Some(n) = self.nodes.iter_mut().find(|n| &n.id == id) {
                        n.status = *status;
                    }
                }
                match report.stop_reason {
                    StopReason::Completed => self.push(
                        FeedKind::NodeOk,
                        format!(
                            "done · {} iteration{} · {} tokens",
                            report.total_iterations,
                            if report.total_iterations == 1 { "" } else { "s" },
                            report.total_usage.total_tokens()
                        ),
                    ),
                    StopReason::NeedsHuman { node, question } => {
                        self.parked = Some(question.clone());
                        self.push(FeedKind::Parked, format!("parked at {node}: {question}"));
                    }
                    other => self.push(FeedKind::NodeFail, format!("stopped: {other:?}")),
                }
            }
            AppMsg::Failed(e) => {
                self.commit_tail();
                self.running = false;
                self.cancel = None;
                self.push(FeedKind::NodeFail, e);
            }
            AppMsg::Reindex => self.index.refresh(),
            AppMsg::Note(text) => {
                self.commit_tail();
                self.push(FeedKind::System, text);
            }
        }
    }

    fn apply_engine(&mut self, ev: EngineEvent) {
        match ev {
            EngineEvent::Narration(line) => {
                self.commit_tail();
                self.push(FeedKind::Conductor, line);
            }
            EngineEvent::NodeStarted { id, worker } => {
                self.commit_tail();
                if let Some(n) = self.nodes.iter_mut().find(|n| n.id == id) {
                    n.worker = worker.clone();
                    n.status = NodeStatus::Running;
                }
                self.push(FeedKind::NodeStart, format!("{id} → {worker}"));
            }
            EngineEvent::NodeStream { id, event } => {
                use rinne_core::worker::WorkerEvent::*;
                let id = id.clone();
                let action = action_label(&event);
                match event {
                    Done => {}
                    Reading(_) | Editing(_) | ToolUse(_) => {
                        self.commit_tail();
                        if let Some(label) = action {
                            let acts = self.live_actions.entry(id.clone()).or_default();
                            acts.push(label);
                            let n = acts.len();
                            if n > ACTIONS_PER_AGENT { acts.drain(0..n - ACTIONS_PER_AGENT); }
                        }
                    }
                    Thinking(t) => self.stream_thinking(&id, &t),
                    Token(t) => self.stream_token(&id, &t),
                    Message(m) | Raw(m) => self.stream_line(&id, &m),
                }
            }
            EngineEvent::NodeFinished { id, status } => {
                self.commit_tail();
                if let Some(n) = self.nodes.iter_mut().find(|n| n.id == id) {
                    n.status = status;
                }
                self.live_actions.remove(&id);
                let kind = if status == NodeStatus::Succeeded { FeedKind::NodeOk } else { FeedKind::NodeFail };
                self.push(kind, format!("{id} {}", status.label()));
            }
            EngineEvent::Parked { id, question } => {
                self.commit_tail();
                if let Some(n) = self.nodes.iter_mut().find(|n| n.id == id) {
                    n.status = NodeStatus::Parked;
                }
                self.parked = Some(question.clone());
                self.push(FeedKind::Parked, question);
            }
        }
    }

    // ----- input handling -----------------------------------------------------

    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        if mods.contains(KeyModifiers::CONTROL)
            && matches!(code, KeyCode::Char('c') | KeyCode::Char('q'))
        {
            self.should_quit = true;
            return;
        }

        // Ctrl+O toggles reasoning verbosity for blocks committed from here on
        // (scrollback is append-only, so already-printed blocks are unchanged).
        if mods.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('o')) {
            self.expand_thinking = !self.expand_thinking;
            let state = if self.expand_thinking { "expanded" } else { "collapsed" };
            self.push(FeedKind::System, format!("thinking {state} (ctrl+o)"));
            return;
        }

        if mods.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('l')) {
            self.clear_requested = true;
            return;
        }

        if self.picker.is_some() {
            match code {
                KeyCode::Up => {
                    self.picker.as_mut().unwrap().up();
                    return;
                }
                KeyCode::Down => {
                    self.picker.as_mut().unwrap().down();
                    return;
                }
                KeyCode::Tab | KeyCode::Enter => {
                    self.complete_picker();
                    return;
                }
                KeyCode::Esc => {
                    self.picker = None;
                    return;
                }
                _ => {}
            }
        }

        // Command-completion overlay: arrows + Tab steer it; chars/Enter/Backspace
        // fall through to normal handling (which re-computes the overlay).
        if self.completion.is_some() {
            match code {
                KeyCode::Up => {
                    self.completion.as_mut().unwrap().up();
                    return;
                }
                KeyCode::Down => {
                    self.completion.as_mut().unwrap().down();
                    return;
                }
                KeyCode::Tab => {
                    self.complete_command();
                    return;
                }
                KeyCode::Esc => {
                    self.completion = None;
                    return;
                }
                _ => {}
            }
        }

        match code {
            KeyCode::Char('w') if mods.contains(KeyModifiers::CONTROL) => {
                let start = prev_word(&self.input, self.cursor);
                self.input.replace_range(start..self.cursor, "");
                self.cursor = start;
                self.refresh_completions();
            }
            KeyCode::Char('u') if mods.contains(KeyModifiers::CONTROL) => {
                self.input.replace_range(0..self.cursor, "");
                self.cursor = 0;
                self.refresh_completions();
            }
            KeyCode::Char('k') if mods.contains(KeyModifiers::CONTROL) => {
                self.input.truncate(self.cursor);
                self.refresh_completions();
            }
            KeyCode::Char('a') if mods.contains(KeyModifiers::CONTROL) => self.cursor = 0,
            KeyCode::Char('e') if mods.contains(KeyModifiers::CONTROL) => self.cursor = self.input.len(),
            KeyCode::Left if mods.contains(KeyModifiers::ALT) => self.cursor = prev_word(&self.input, self.cursor),
            KeyCode::Right if mods.contains(KeyModifiers::ALT) => self.cursor = next_word(&self.input, self.cursor),
            KeyCode::Char(c) => {
                self.input.insert(self.cursor, c);
                self.cursor += c.len_utf8();
                self.refresh_completions();
            }
            KeyCode::Backspace if mods.contains(KeyModifiers::ALT) => {
                let start = prev_word(&self.input, self.cursor);
                self.input.replace_range(start..self.cursor, "");
                self.cursor = start;
                self.refresh_completions();
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    let prev = prev_boundary(&self.input, self.cursor);
                    self.input.replace_range(prev..self.cursor, "");
                    self.cursor = prev;
                    self.refresh_completions();
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.input.len() {
                    let next = next_boundary(&self.input, self.cursor);
                    self.input.replace_range(self.cursor..next, "");
                    self.refresh_completions();
                }
            }
            KeyCode::Left => self.cursor = prev_boundary(&self.input, self.cursor),
            KeyCode::Right => self.cursor = next_boundary(&self.input, self.cursor),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.len(),
            // Up/Down recall history (overlays consumed these above).
            KeyCode::Up => self.history_prev(),
            KeyCode::Down => self.history_next(),
            // Re-summon completion (e.g. after Esc) on a slash line.
            KeyCode::Tab => self.refresh_completions(),
            KeyCode::Enter if mods.contains(KeyModifiers::SHIFT) || mods.contains(KeyModifiers::ALT) => {
                self.input.insert(self.cursor, '\n');
                self.cursor += 1;
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Esc => {
                if self.running {
                    self.pause();
                }
            }
            _ => {}
        }
    }

    fn paste(&mut self, s: &str) {
        self.picker = None;
        self.completion = None;
        self.input.insert_str(self.cursor, s);
        self.cursor += s.len();
        self.refresh_completions();
    }

    /// Replace the input line and park the cursor at its end. Recalled history is
    /// shown as-is — no completion/picker popup steals the arrow keys; typing
    /// afterward re-triggers completion as usual.
    fn set_input(&mut self, text: String) {
        self.cursor = text.len();
        self.input = text;
        self.picker = None;
        self.completion = None;
    }

    /// Back `history` with a file and load any prior entries (newest last).
    fn attach_history(&mut self, path: PathBuf) {
        if let Ok(contents) = std::fs::read_to_string(&path) {
            self.history = contents
                .lines()
                .map(str::to_string)
                .filter(|l| !l.trim().is_empty())
                .collect();
            if self.history.len() > HISTORY_MAX {
                self.history.drain(0..self.history.len() - HISTORY_MAX);
            }
        }
        self.history_path = Some(path);
    }

    /// Append `entry` to the on-disk history, unless it carries a secret. API
    /// keys and `--key` tokens must never be written to disk, so a line that
    /// `redact_secret` would alter is kept in-session only and not persisted.
    fn persist_history(&self, entry: &str) {
        let Some(path) = &self.history_path else { return };
        if redact_secret(entry) != entry {
            return; // contains a key/token — never write it to disk
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{entry}");
        }
    }

    /// Recall an older history entry (shell-style Up).
    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_pos {
            None => {
                self.draft = self.input.clone(); // save the in-progress line
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(p) => p - 1,
        };
        self.history_pos = Some(next);
        self.set_input(self.history[next].clone());
    }

    /// Move toward newer history, restoring the saved draft past the newest (Down).
    fn history_next(&mut self) {
        let Some(p) = self.history_pos else { return };
        if p + 1 < self.history.len() {
            self.history_pos = Some(p + 1);
            self.set_input(self.history[p + 1].clone());
        } else {
            self.history_pos = None;
            let draft = std::mem::take(&mut self.draft);
            self.set_input(draft);
        }
    }

    fn refresh_completions(&mut self) {
        // A slash-command line drives the command-completion overlay and never
        // the @-file picker (the two are mutually exclusive).
        if self.input.starts_with('/') {
            self.picker = None;
            self.completion = complete::suggest(&self.input);
            return;
        }
        self.completion = None;
        let token = current_token(&self.input);
        if let Some(rest) = token.strip_prefix('@') {
            let mut picker = self.picker.take().unwrap_or_else(Picker::new);
            picker.update(rest, &self.index);
            self.picker = Some(picker);
        } else {
            self.picker = None;
        }
    }

    /// Insert the highlighted command completion, replacing the current token,
    /// then re-compute so the next argument's hints appear immediately.
    fn complete_command(&mut self) {
        let Some(comp) = &self.completion else { return };
        let Some(item) = comp.selected() else {
            self.completion = None;
            return;
        };
        let value = item.value.clone();
        let start = comp.token_start.min(self.input.len());
        self.input.truncate(start);
        self.input.push_str(&value);
        self.input.push(' ');
        self.cursor = self.input.len();
        self.completion = None;
        self.refresh_completions();
    }

    fn complete_picker(&mut self) {
        let Some(picker) = &self.picker else { return };
        let Some(sel) = picker.selected().cloned() else {
            self.picker = None;
            return;
        };
        let start = self
            .input
            .rfind(|c: char| c.is_whitespace())
            .map(|i| i + 1)
            .unwrap_or(0);
        self.input.truncate(start);
        self.input.push('@');
        self.input.push_str(&sel);
        self.input.push(' ');
        self.cursor = self.input.len();
        self.picker = None;
    }

    fn submit(&mut self) {
        let text = self.input.trim().to_string();
        self.input.clear();
        self.cursor = 0;
        self.picker = None;
        self.completion = None;
        self.history_pos = None;
        self.draft.clear();
        if text.is_empty() {
            return;
        }
        // Record for up-arrow recall, skipping consecutive duplicates. Persist
        // across sessions (secret-bearing lines are filtered inside persist).
        if self.history.last() != Some(&text) {
            self.history.push(text.clone());
            self.persist_history(&text);
        }
        // Echo the input, redacting an API key in a `connect` command so it
        // never lands in the transcript.
        self.push(FeedKind::System, format!("› {}", redact_secret(&text)));

        // A slash command, or a `rinne <cmd>` typed out of shell habit — both
        // route to the command handler, never the conductor.
        if let Some(cmd) = text.strip_prefix('/').or_else(|| text.strip_prefix("rinne ")) {
            self.slash(cmd.trim());
            return;
        }
        if self.parked.is_some() {
            self.resume_with(HumanDecision::Steer(text));
            return;
        }
        if self.running {
            self.push(FeedKind::System, "a run is active — /pause first, or wait for it to park");
            return;
        }
        match self.resolve_mentions(&text) {
            Ok((goal, mentioned)) => self.start_goal(goal, mentioned),
            Err(warning) => self.push(FeedKind::NodeFail, warning),
        }
    }

    fn slash(&mut self, cmd: &str) {
        let head = cmd.split_whitespace().next().unwrap_or("");
        let rest = cmd[head.len()..].trim().to_string();
        match head {
            "help" => self.push(FeedKind::System, help_text()),
            "plan" => {
                let ids: Vec<&str> = self.nodes.iter().map(|n| n.id.as_str()).collect();
                self.push(
                    FeedKind::System,
                    format!("plan: {}", if ids.is_empty() { "(none)".into() } else { ids.join(" → ") }),
                );
            }
            "workers" | "doctor" => self.list_workers(),
            "connect" => {
                if rest.is_empty() {
                    self.push(FeedKind::System, "usage: /connect <backend> [api-key] [--base-url <url>] [--model <id>]  (key is stored in your OS keychain)");
                } else {
                    let (backend, key, models, base_url, add) = parse_connect_args(&rest);
                    self.connect(backend, key, models, base_url, add);
                }
            }
            "forget" => {
                if rest.is_empty() {
                    self.push(FeedKind::System, "usage: /forget <provider>  (deletes the stored API key from your OS keychain)");
                } else {
                    let lines = crate::commands::forget::forget_lines(rest.split_whitespace().next().unwrap_or(""));
                    self.push(FeedKind::System, lines.join("\n"));
                }
            }
            "models" => {
                if rest.is_empty() {
                    self.push(FeedKind::System, "usage: /models <provider>  (lists the models an API provider's key can access)");
                } else {
                    self.list_models_for(rest.split_whitespace().next().unwrap_or("").to_string());
                }
            }
            "config" => {
                let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                let lines = crate::commands::config::edit_lines(&rest, &cwd);
                self.push(FeedKind::System, lines.join("\n"));
                // `/config edit` opens the file; non-blocking GUI/opener so the
                // inline TUI keeps running (terminal editors: run from the shell).
                let sub = rest.split_whitespace().next().unwrap_or("");
                if matches!(sub, "edit" | "open") {
                    let scope = if rest.contains("--project") || rest.contains("--proj") {
                        rinne_config::write::Scope::Project
                    } else {
                        rinne_config::write::Scope::Global
                    };
                    if let Ok(path) = rinne_config::write::target_path(scope, &cwd) {
                        crate::commands::config::open_in_editor(&path, false);
                    }
                }
            }
            "steer" if !rest.is_empty() => self.resume_with(HumanDecision::Steer(rest)),
            "approve" => self.resume_with(HumanDecision::Approve),
            "reject" => self.resume_with(HumanDecision::Reject),
            "pause" => self.pause(),
            "resume" => {
                if self.parked.is_some() {
                    self.push(FeedKind::System, "parked — use /approve, /reject, or /steer <text>");
                } else {
                    self.resume_plain();
                }
            }
            "budget" => self.push(FeedKind::System, format!("budget noted: {rest} (applied next run)")),
            "route" => self.push(FeedKind::System, format!("route override noted: {rest}")),
            "logs" => self.push(FeedKind::System, "logs: .rinne/logs/"),
            "quit" | "q" => self.should_quit = true,
            "clear" | "new" => {
                if self.running || self.parked.is_some() {
                    self.push(FeedKind::System, "a run is active — /pause or wait for it to finish, then /clear");
                } else {
                    self.goal = None;
                    self.nodes.clear();
                    self.pending.clear();
                    self.live_tail.clear();
                    self.live_thinking.clear();
                    self.live_node = None;
                    self.parked = None;
                    self.clear_requested = true;
                }
            }
            "" => {}
            other => self.push(
                FeedKind::System,
                format!("unknown command `{other}` — try /help. (To run something as a task, type it without a leading slash or `rinne`.)"),
            ),
        }
    }

    fn list_workers(&mut self) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let text = match crate::commands::connect::list_lines().await {
                Ok(lines) => lines.join("\n"),
                Err(e) => format!("could not list workers: {e}"),
            };
            let _ = tx.send(AppMsg::Note(text));
        });
    }

    fn list_models_for(&mut self, provider: String) {
        self.push(FeedKind::System, format!("fetching models for {provider}…"));
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let text = crate::commands::models::list_lines(&provider).await.join("\n");
            let _ = tx.send(AppMsg::Note(text));
        });
    }

    fn connect(&mut self, backend: String, key: Option<String>, models: Vec<String>, base_url: Option<String>, add: bool) {
        self.push(FeedKind::System, format!("connecting {backend}…"));
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let text = match crate::commands::connect::connect_lines(&backend, key.as_deref(), &models, base_url.as_deref(), add).await {
                Ok(lines) => lines.join("\n"),
                Err(e) => format!("connect failed: {e}"),
            };
            let _ = tx.send(AppMsg::Note(text));
        });
    }

    fn pause(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
            self.push(FeedKind::System, "paused — state saved; type to steer or /resume");
        }
    }

    fn start_goal(&mut self, goal: String, mentioned: Vec<PathBuf>) {
        let cancel = CancellationToken::new();
        self.cancel = Some(cancel.clone());
        self.running = true;
        self.nodes.clear();
        self.push(FeedKind::Conductor, format!("planning: {goal}"));
        spawn_run(self.tx.clone(), goal, mentioned, None, cancel);
    }

    fn resume_with(&mut self, decision: HumanDecision) {
        if self.parked.is_none() {
            self.push(FeedKind::System, "nothing is parked");
            return;
        }
        self.parked = None;
        self.running = true;
        let cancel = CancellationToken::new();
        self.cancel = Some(cancel.clone());
        let resume = ResumeInput { node: None, decision };
        self.push(FeedKind::Conductor, "resuming with your decision");
        spawn_run(self.tx.clone(), String::new(), Vec::new(), Some(resume), cancel);
    }

    fn resume_plain(&mut self) {
        let cancel = CancellationToken::new();
        self.cancel = Some(cancel.clone());
        self.running = true;
        self.push(FeedKind::Conductor, "resuming");
        spawn_run(self.tx.clone(), String::new(), Vec::new(), None, cancel);
    }

    fn resolve_mentions(&self, input: &str) -> std::result::Result<(String, Vec<PathBuf>), String> {
        let mut goal_words = Vec::new();
        let mut mentioned: Vec<PathBuf> = Vec::new();

        for token in input.split_whitespace() {
            let Some(reference) = token.strip_prefix('@') else {
                goal_words.push(token);
                continue;
            };
            let resolved = self.resolve_one(reference);
            if resolved.len() > GLOB_LIMIT {
                return Err(format!(
                    "`@{reference}` expands to {} files (>{GLOB_LIMIT}) — narrow it before running",
                    resolved.len()
                ));
            }
            mentioned.extend(resolved);
        }

        if mentioned.len() > GLOB_LIMIT {
            return Err(format!(
                "{} files mentioned (>{GLOB_LIMIT}) — narrow before running",
                mentioned.len()
            ));
        }
        mentioned.sort();
        mentioned.dedup();
        Ok((goal_words.join(" "), mentioned))
    }

    fn resolve_one(&self, reference: &str) -> Vec<PathBuf> {
        let root = self.index.root();
        if reference.contains('*') || reference.contains('?') {
            let pattern = root.join(reference);
            if let Ok(paths) = glob::glob(&pattern.to_string_lossy()) {
                return paths
                    .flatten()
                    .filter(|p| p.is_file())
                    .filter_map(|p| p.strip_prefix(root).ok().map(|r| r.to_path_buf()))
                    .collect();
            }
            return Vec::new();
        }
        let as_dir = reference.trim_end_matches('/');
        if root.join(as_dir).is_dir() {
            let prefix = format!("{as_dir}/");
            return self
                .index
                .files()
                .iter()
                .filter(|f| f.starts_with(&prefix))
                .map(PathBuf::from)
                .collect();
        }
        vec![PathBuf::from(reference)]
    }
}

/// The current whitespace-delimited token at the end of `input`.
fn current_token(input: &str) -> &str {
    input.rsplit(char::is_whitespace).next().unwrap_or("")
}

/// The byte index of the char boundary just before `i` (clamped at 0).
fn prev_boundary(s: &str, i: usize) -> usize {
    s[..i].char_indices().next_back().map(|(b, _)| b).unwrap_or(0)
}

/// The byte index of the char boundary just after `i` (clamped at `s.len()`).
fn next_boundary(s: &str, i: usize) -> usize {
    s[i..].chars().next().map(|c| i + c.len_utf8()).unwrap_or(i)
}

/// Byte index of the start of the word before `i`: skip spaces left, then
/// non-spaces left. If only spaces remain to the left, collapses to 0.
fn prev_word(s: &str, mut i: usize) -> usize {
    let bytes = s.as_bytes();
    // Spaces are word delimiters; a newline is a hard stop word-motion never crosses.
    while i > 0 && bytes[i - 1] == b' ' { i -= 1; }
    while i > 0 && !matches!(bytes[i - 1], b' ' | b'\n') { i = prev_boundary(s, i); }
    // If only spaces remain to the left of this logical line, collapse past them.
    if i > 0 && bytes[i - 1] == b' ' && bytes[..i].iter().all(|&b| b == b' ') { i = 0; }
    i
}

/// Byte index of the end of the word after `i`: skip spaces right, then
/// non-spaces right. A newline is a hard stop word-motion never crosses.
fn next_word(s: &str, mut i: usize) -> usize {
    let bytes = s.as_bytes();
    while i < s.len() && bytes[i] == b' ' { i += 1; }
    while i < s.len() && !matches!(bytes[i], b' ' | b'\n') { i = next_boundary(s, i); }
    i
}

/// Parse `connect` arguments: `<provider> [key] [--base-url <url>] [--model <id>]`
/// with positional models also allowed after the key. Returns
/// `(provider, key, models, base_url)`.
fn parse_connect_args(rest: &str) -> (String, Option<String>, Vec<String>, Option<String>, bool) {
    let mut toks = rest.split_whitespace();
    let provider = toks.next().unwrap_or("").to_string();
    let mut key = None;
    let mut models = Vec::new();
    let mut base_url = None;
    let mut add = false;
    while let Some(t) = toks.next() {
        match t {
            "--base-url" | "--base_url" => base_url = toks.next().map(String::from),
            "--model" => {
                if let Some(m) = toks.next() {
                    models.push(m.to_string());
                }
            }
            "--add" => add = true,
            _ if t.starts_with("--") => {} // unknown flag: ignore
            _ if key.is_none() => key = Some(t.to_string()),
            _ => models.push(t.to_string()),
        }
    }
    (provider, key, models, base_url, add)
}

/// Redact an API key in a `connect` line so it never appears in the transcript.
fn redact_secret(text: &str) -> String {
    let cmd = text
        .strip_prefix('/')
        .or_else(|| text.strip_prefix("rinne "))
        .unwrap_or(text);
    if let Some(args) = cmd.strip_prefix("connect ") {
        let (_, key, _, _, _) = parse_connect_args(args.trim());
        if let Some(k) = key {
            if !k.is_empty() {
                return text.replace(&k, "***");
            }
        }
    }
    // Redact a `--key <token>` value (e.g. `/config conductor cloudflare --key …`)
    // and the positional token of `/config key <token>`.
    let toks: Vec<&str> = cmd.split_whitespace().collect();
    let mut secret: Option<&str> = None;
    for (i, t) in toks.iter().enumerate() {
        if *t == "--key" {
            secret = toks.get(i + 1).copied();
        }
    }
    if secret.is_none() && toks.first() == Some(&"config") && toks.get(1) == Some(&"key") {
        secret = toks.get(2).copied();
    }
    if let Some(s) = secret {
        if !s.is_empty() {
            return text.replace(s, "***");
        }
    }
    text.to_string()
}

/// The slash-command reference, formatted as command (left) · description
/// (right). Newlines are preserved by the transcript renderer.
fn help_text() -> String {
    let rows = [
        ("/plan", "show the current plan"),
        ("/workers", "list workers + connected APIs and their auth"),
        ("/connect <b>", "connect a harness, or an API provider + key (e.g. /connect deepseek sk-…)"),
        ("/forget <p>", "delete a stored API key from the OS keychain"),
        ("/models <p>", "list the models an API provider's key can access"),
        ("/config", "show/edit config: conductor <b> [--key <t>], set <k> <v>, init, edit"),
        ("/steer <text>", "give guidance to a parked node (or just type while parked)"),
        ("/approve", "accept the current state and continue"),
        ("/reject", "throw out the approach and replan"),
        ("/pause", "pause the running loop (state is saved)"),
        ("/resume", "resume a paused run"),
        ("/budget <min>", "adjust the time budget"),
        ("/route <n> <w>", "pin a node to a worker"),
        ("/clear", "wipe the screen and reset the session (ctrl-l = wipe only)"),
        ("/logs", "where logs are written (.rinne/logs/)"),
        ("/quit", "exit (or ctrl-q)"),
    ];
    let mut s = String::from("commands:");
    for (cmd, desc) in rows {
        s.push_str(&format!("\n  {cmd:<18}{desc}"));
    }
    s
}

fn spawn_run(
    tx: tokio::sync::mpsc::UnboundedSender<AppMsg>,
    goal: String,
    mentioned: Vec<PathBuf>,
    resume: Option<ResumeInput>,
    cancel: CancellationToken,
) {
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                let _ = tx.send(AppMsg::Failed(format!("runtime: {e}")));
                return;
            }
        };
        rt.block_on(async move {
            match do_run(tx.clone(), goal, mentioned, resume, cancel).await {
                Ok(report) => {
                    let _ = tx.send(AppMsg::Finished(report));
                }
                Err(e) => {
                    let _ = tx.send(AppMsg::Failed(e.to_string()));
                }
            }
        });
    });
}

async fn do_run(
    tx: tokio::sync::mpsc::UnboundedSender<AppMsg>,
    goal: String,
    mentioned: Vec<PathBuf>,
    resume: Option<ResumeInput>,
    cancel: CancellationToken,
) -> Result<RunReport> {
    let config = rinne_config::load_cwd()?;
    let cwd = std::env::current_dir()?;
    let bb = Blackboard::open(&cwd)?;
    let (registry, _) = runner::build_registry(&config).await?;
    if registry.is_empty() {
        return Err(anyhow!("no available workers — run `rinne doctor`"));
    }
    let conductor = runner::build_conductor(&config, &registry, cwd.clone())
        .ok()
        .map(Arc::new);

    if resume.is_none() {
        let conductor = conductor
            .clone()
            .ok_or_else(|| anyhow!("no conductor backend available"))?;
        let input = ConductorInput {
            goal: goal.clone(),
            mentioned,
            workers: registry.descriptors(),
            budget_minutes: Some(config.loop_.global_budget_minutes as u64),
            max_iterations_per_node: config.loop_.max_iterations_per_node,
            ..Default::default()
        };
        let plan = conductor.plan(&input).await?;
        bb.save_plan(&plan)?;
        bb.reset_run()?;
    }

    let plan = bb.load_plan()?;
    let nodes = plan
        .nodes
        .iter()
        .map(|n| NodeView {
            id: n.id.clone(),
            role: format!("{:?}", n.role).to_lowercase(),
            status: NodeStatus::Pending,
            worker: String::new(),
        })
        .collect();
    let goal_label = if goal.is_empty() { plan.goal.clone() } else { goal };
    let _ = tx.send(AppMsg::Planned { goal: goal_label, nodes });

    let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel::<EngineEvent>();
    let ftx = tx.clone();
    tokio::spawn(async move {
        while let Some(ev) = erx.recv().await {
            let _ = ftx.send(AppMsg::Engine(ev));
        }
    });

    let mut engine = Engine::new(&bb, plan, &registry, runner::options_with_pool(&config, &registry));
    if let Some(c) = &conductor {
        engine = engine.with_replanner(c.clone());
    }
    engine.run(cancel, Some(etx), resume).await.map_err(Into::into)
}

/// Entry point: set up an inline viewport and run the event loop.
pub async fn run() -> Result<()> {
    if !io::stdout().is_terminal() {
        println!("rinne: the interactive TUI needs a terminal. Use `rinne -p \"<task>\"` for headless runs.");
        return Ok(());
    }

    let cwd = std::env::current_dir()?;
    let index = FileIndex::build(&cwd);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AppMsg>();

    let (watch_tx, mut watch_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let _watcher = index::watch(&cwd, watch_tx);
    let reindex_tx = tx.clone();
    tokio::spawn(async move {
        while watch_rx.recv().await.is_some() {
            while watch_rx.try_recv().is_ok() {}
            let _ = reindex_tx.send(AppMsg::Reindex);
        }
    });

    let mut app = App::new(index, tx);
    // Persist prompt history across sessions under the project blackboard.
    app.attach_history(cwd.join(rinne_core::BLACKBOARD_DIR).join("history"));

    // No alternate screen: the transcript stays in normal scrollback. A small
    // inline viewport at the bottom holds the live region.
    enable_raw_mode()?;
    // Build the terminal BEFORE emitting any escape sequences, so a construction
    // failure here returns early with only raw mode to undo — never a terminal
    // left in bracketed-paste / keyboard-enhancement state.
    let rows = crossterm::terminal::size().map(|(_, h)| h).unwrap_or(24);
    let viewport_height = rows.saturating_sub(1).min(10).max(4);
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = match Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(viewport_height),
        },
    ) {
        Ok(t) => t,
        Err(e) => {
            let _ = disable_raw_mode();
            return Err(e.into());
        }
    };
    let mut out = io::stdout();
    let _ = execute!(out, EnableBracketedPaste);
    let kbd_enhanced = matches!(crossterm::terminal::supports_keyboard_enhancement(), Ok(true));
    if kbd_enhanced {
        let _ = execute!(out, PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES));
    }

    let result = event_loop(&mut terminal, &mut app, &mut rx).await;

    // Leave the transcript intact; just drop the live region and free the line.
    let _ = terminal.clear();
    let mut out = io::stdout();
    if kbd_enhanced { let _ = execute!(out, PopKeyboardEnhancementFlags); }
    let _ = execute!(out, DisableBracketedPaste);
    disable_raw_mode()?;
    println!();
    result
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppMsg>,
) -> Result<()> {
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(120));

    loop {
        if app.clear_requested {
            app.clear_requested = false;
            // `terminal.clear()` on an inline viewport only wipes the viewport
            // region, leaving the scrollback transcript on screen. Emit the full
            // screen + scrollback clear (like the shell `clear`), then let the
            // draw below repaint the viewport. ratatui must re-sync afterwards so
            // it doesn't assume the old buffer is still on screen.
            let _ = execute!(io::stdout(), MoveTo(0, 0), Clear(ClearType::All), Clear(ClearType::Purge));
            let _ = terminal.clear();
        }
        ui::flush_pending(terminal, app)?;
        terminal.draw(|f| ui::draw_viewport(f, app))?;
        if app.should_quit {
            return Ok(());
        }

        tokio::select! {
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => {
                        app.on_key(key.code, key.modifiers);
                    }
                    Some(Ok(Event::Paste(text))) => app.paste(&text),
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => return Ok(()),
                }
            }
            Some(msg) = rx.recv() => app.apply(msg),
            _ = tick.tick() => app.spinner = app.spinner.wrapping_add(1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("rinne-tui-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn app_for(dir: &std::path::Path) -> App {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(FileIndex::build(dir), tx)
    }

    #[test]
    fn current_token_picks_trailing_word() {
        assert_eq!(current_token("fix @src/li"), "@src/li");
        assert_eq!(current_token("hello world"), "world");
        assert_eq!(current_token(""), "");
    }

    #[test]
    fn connect_args_parse_flags() {
        let (p, k, models, base, add) = parse_connect_args(
            "deepseek my-key --base-url https://integrate.api.nvidia.com/v1 --model deepseek-ai/deepseek-v4-pro --add",
        );
        assert_eq!(p, "deepseek");
        assert_eq!(k.as_deref(), Some("my-key"));
        assert_eq!(models, vec!["deepseek-ai/deepseek-v4-pro"]);
        assert_eq!(base.as_deref(), Some("https://integrate.api.nvidia.com/v1"));
        assert!(add);
    }

    #[test]
    fn connect_args_positional_models() {
        let (p, k, models, base, add) = parse_connect_args("nvidia mykey model-a model-b");
        assert_eq!(p, "nvidia");
        assert_eq!(k.as_deref(), Some("mykey"));
        assert_eq!(models, vec!["model-a", "model-b"]);
        assert_eq!(base, None);
        assert!(!add);
    }

    #[test]
    fn token_fragments_accumulate_into_one_markdown_block() {
        let dir = temp_dir("stream");
        let mut app = app_for(&dir);
        let before = app.pending.len(); // welcome lines
        // Token deltas (often mid-word) concatenate verbatim and commit as ONE
        // markdown block tagged with its node — not one fragment per row.
        app.stream_token("n1", "Hel");
        app.stream_token("n1", "lo wor");
        app.stream_token("n1", "ld\nsecond line");
        assert_eq!(app.pending.len(), before, "nothing should flush mid-stream");
        assert_eq!(app.live_tail, "Hello world\nsecond line");
        app.commit_tail();
        assert_eq!(app.pending.len(), before + 1);
        let entry = app.pending.last().unwrap();
        assert_eq!(entry.kind, FeedKind::Markdown);
        assert_eq!(entry.node.as_deref(), Some("n1"));
        assert_eq!(entry.text, "Hello world\nsecond line");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cursor_moves_and_edits_midline() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let dir = temp_dir("cursor");
        let mut app = app_for(&dir);
        for c in "helo".chars() {
            app.on_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert_eq!(app.cursor, 4);
        // Move left to between 'e' and 'l', insert the missing 'l'.
        app.on_key(KeyCode::Left, KeyModifiers::NONE);
        app.on_key(KeyCode::Left, KeyModifiers::NONE);
        app.on_key(KeyCode::Char('l'), KeyModifiers::NONE);
        assert_eq!(app.input, "hello");
        assert_eq!(app.cursor, 3);
        // Backspace deletes before the cursor, not at the end.
        app.on_key(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(app.input, "helo");
        // Home/End jump.
        app.on_key(KeyCode::Home, KeyModifiers::NONE);
        assert_eq!(app.cursor, 0);
        app.on_key(KeyCode::End, KeyModifiers::NONE);
        assert_eq!(app.cursor, app.input.len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn up_down_recall_history() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let dir = temp_dir("hist");
        let mut app = app_for(&dir);
        // Submit two harmless slash commands (no run spawned).
        for cmd in ["/help", "/plan"] {
            app.set_input(cmd.to_string());
            app.submit();
        }
        assert_eq!(app.history, vec!["/help".to_string(), "/plan".to_string()]);
        app.on_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.input, "/plan");
        app.on_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.input, "/help");
        app.on_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.input, "/plan");
        app.on_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.input, "", "Down past newest restores the (empty) draft");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn history_persists_and_never_writes_secrets() {
        let dir = temp_dir("histfile");
        let path = dir.join(".rinne").join("history");
        let mut app = app_for(&dir);
        app.attach_history(path.clone());
        app.persist_history("summarize the repo");
        app.persist_history("/connect deepseek sk-SECRETKEY");
        app.persist_history("/config conductor cloudflare --key TOKENABC");
        app.persist_history("/help");

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("summarize the repo") && contents.contains("/help"), "{contents:?}");
        assert!(!contents.contains("sk-SECRETKEY"), "API key leaked to history file");
        assert!(!contents.contains("TOKENABC"), "token leaked to history file");

        // A fresh session loads the persisted (secret-free) entries.
        let mut app2 = app_for(&dir);
        app2.attach_history(path);
        assert!(app2.history.contains(&"summarize the repo".to_string()));
        assert!(app2.history.iter().all(|h| !h.contains("SECRETKEY") && !h.contains("TOKENABC")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reasoning_commits_before_the_answer() {
        let dir = temp_dir("think");
        let mut app = app_for(&dir);
        let before = app.pending.len();
        // Reasoning streams first…
        app.stream_thinking("n1", "let me consider ");
        app.stream_thinking("n1", "the options");
        assert_eq!(app.pending.len(), before, "thinking shouldn't flush until the answer starts");
        // …then the answer begins, flushing the thinking block above it.
        app.stream_token("n1", "# Answer");
        assert_eq!(app.pending.len(), before + 1);
        let think = app.pending.last().unwrap();
        assert_eq!(think.kind, FeedKind::Thinking);
        assert_eq!(think.text, "let me consider the options");
        assert_eq!(app.live_thinking, "");
        // Finishing commits the answer as markdown after the thinking block.
        app.commit_tail();
        let answer = app.pending.last().unwrap();
        assert_eq!(answer.kind, FeedKind::Markdown);
        assert_eq!(answer.text, "# Answer");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn block_messages_are_newline_separated() {
        let dir = temp_dir("blocks");
        let mut app = app_for(&dir);
        // Discrete line/block events (Claude blocks) must not run together.
        app.stream_line("n2", "model: sonnet");
        app.stream_line("n2", "# Heading");
        app.stream_line("n2", "body text");
        assert_eq!(app.live_tail, "model: sonnet\n# Heading\nbody text");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn redact_hides_key_keeps_flags() {
        let r = redact_secret("/connect deepseek SECRETKEY --base-url https://x --model m");
        assert!(!r.contains("SECRETKEY"), "key leaked: {r}");
        assert!(r.contains("--base-url") && r.contains("https://x") && r.contains("***"));
    }

    #[test]
    fn redact_hides_conductor_token() {
        let r = redact_secret("/config conductor cloudflare @cf/llama --key TOKEN123 --project");
        assert!(!r.contains("TOKEN123"), "token leaked: {r}");
        assert!(r.contains("cloudflare") && r.contains("--project") && r.contains("***"), "{r}");
        // positional form: /config key <token>
        let r2 = redact_secret("/config key TOKEN456");
        assert!(!r2.contains("TOKEN456"), "token leaked: {r2}");
    }

    #[test]
    fn resolves_file_reference_and_strips_at_from_goal() {
        let dir = temp_dir("file");
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        let app = app_for(&dir);

        let (goal, mentioned) = app.resolve_mentions("@a.txt fix the bug").unwrap();
        assert_eq!(goal, "fix the bug");
        assert_eq!(mentioned, vec![PathBuf::from("a.txt")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expands_a_directory_mention() {
        let dir = temp_dir("dir");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "x").unwrap();
        std::fs::write(dir.join("src/b.rs"), "y").unwrap();
        let app = app_for(&dir);

        let (_goal, mentioned) = app.resolve_mentions("@src/ refactor").unwrap();
        assert_eq!(mentioned.len(), 2);
        assert!(mentioned.contains(&PathBuf::from("src/a.rs")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn glob_guardrail_trips_past_the_limit() {
        let dir = temp_dir("glob");
        for i in 0..(GLOB_LIMIT + 5) {
            std::fs::write(dir.join(format!("f{i}.txt")), "x").unwrap();
        }
        let app = app_for(&dir);

        let err = app.resolve_mentions("@*.txt do it").unwrap_err();
        assert!(err.contains("narrow"), "should warn to narrow: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn word_boundaries() {
        assert_eq!(prev_word("foo bar", 7), 4);     // from end → start of "bar"
        assert_eq!(prev_word("foo bar", 4), 0);     // skips trailing space → start of "foo"
        assert_eq!(prev_word("  foo", 5), 0);       // leading spaces collapse to 0
        assert_eq!(next_word("foo bar", 0), 3);     // end of "foo"
        assert_eq!(next_word("foo bar", 3), 7);     // skips space → end of "bar"
        assert_eq!(next_word("foo", 3), 3);         // at end stays
        // Word motion must NOT cross a hard newline (multi-line input).
        assert_eq!(prev_word("foo\nbar", 7), 4);    // deletes only "bar", stops at the '\n'
        assert_eq!(prev_word("foo\nbar", 4), 4);    // already at line start → no move
        assert_eq!(next_word("foo\nbar", 0), 3);    // stops before the '\n'
        assert_eq!(next_word("foo\nbar", 3), 3);    // sitting on the '\n' → no move
    }

    #[test]
    fn ctrl_w_deletes_word_back() {
        let dir = temp_dir("ctrlw");
        let mut app = app_for(&dir);
        for c in "hello world".chars() { app.on_key(KeyCode::Char(c), KeyModifiers::NONE); }
        app.on_key(KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(app.input, "hello ");
        assert_eq!(app.cursor, app.input.len());
    }

    #[test]
    fn ctrl_u_deletes_to_line_start_and_ctrl_a_e_move() {
        let dir = temp_dir("ctrlu");
        let mut app = app_for(&dir);
        for c in "abc def".chars() { app.on_key(KeyCode::Char(c), KeyModifiers::NONE); }
        app.on_key(KeyCode::Char('a'), KeyModifiers::CONTROL); // line start
        assert_eq!(app.cursor, 0);
        app.on_key(KeyCode::Char('e'), KeyModifiers::CONTROL); // line end
        assert_eq!(app.cursor, app.input.len());
        app.on_key(KeyCode::Char('u'), KeyModifiers::CONTROL); // delete to start
        assert_eq!(app.input, "");
    }

    #[test]
    fn ctrl_k_truncates_forward() {
        let dir = temp_dir("ctrlk");
        let mut app = app_for(&dir);
        for c in "hello world".chars() { app.on_key(KeyCode::Char(c), KeyModifiers::NONE); }
        app.on_key(KeyCode::Char('a'), KeyModifiers::CONTROL); // move to start
        app.on_key(KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert_eq!(app.input, "");
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn alt_arrows_move_by_word() {
        let dir = temp_dir("altarrow");
        let mut app = app_for(&dir);
        for c in "foo bar".chars() { app.on_key(KeyCode::Char(c), KeyModifiers::NONE); }
        app.on_key(KeyCode::Left, KeyModifiers::ALT);
        assert_eq!(app.cursor, 4); // start of "bar" (space before it is preserved)
        app.on_key(KeyCode::Left, KeyModifiers::ALT);
        assert_eq!(app.cursor, 0); // start of "foo"
        app.on_key(KeyCode::Right, KeyModifiers::ALT);
        assert_eq!(app.cursor, 3); // end of "foo"
    }

    #[test]
    fn clear_resets_session_when_idle_but_keeps_history() {
        let mut app = app_for(&temp_dir("clear"));
        app.history.push("prior goal".to_string());
        app.goal = Some("a goal".to_string());
        app.nodes.push(NodeView { id: "n1".into(), role: "generator".into(), status: NodeStatus::Pending, worker: "x".into() });
        app.pending.push(FeedEntry { kind: FeedKind::System, text: "x".into(), node: None });
        app.live_node = Some("n1".to_string());
        app.slash("clear");
        assert!(app.goal.is_none());
        assert!(app.nodes.is_empty());
        assert!(app.clear_requested);
        assert!(app.pending.is_empty());
        assert!(app.live_node.is_none());
        assert_eq!(app.history, vec!["prior goal".to_string()]); // history preserved
    }

    #[test]
    fn clear_refused_while_running() {
        let mut app = app_for(&temp_dir("clearrun"));
        app.running = true;
        app.goal = Some("live".to_string());
        app.slash("clear");
        assert_eq!(app.goal, Some("live".to_string())); // untouched
        assert!(!app.clear_requested);
    }

    #[test]
    fn clear_refused_while_parked() {
        let mut app = app_for(&temp_dir("clearparked"));
        app.parked = Some("decide".to_string());
        app.goal = Some("live".to_string());
        app.slash("clear");
        assert_eq!(app.goal, Some("live".to_string()));
        assert!(!app.clear_requested);
    }

    #[test]
    fn ctrl_l_sets_flag_without_reset() {
        let mut app = app_for(&temp_dir("ctrll"));
        app.goal = Some("keep".to_string());
        app.on_key(KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert!(app.clear_requested);
        assert_eq!(app.goal, Some("keep".to_string())); // ctrl-l does NOT reset state
    }

    #[test]
    fn paste_inserts_verbatim_including_newlines() {
        let mut app = app_for(&temp_dir("paste"));
        for c in "ab".chars() { app.on_key(KeyCode::Char(c), KeyModifiers::NONE); }
        app.cursor = 1; // between a and b
        app.paste("X\nY");
        assert_eq!(app.input, "aX\nYb");
        assert_eq!(app.cursor, 4); // after the pasted run
    }

    #[test]
    fn alt_backspace_deletes_word_back() {
        let mut app = app_for(&temp_dir("altbs"));
        for c in "one two".chars() { app.on_key(KeyCode::Char(c), KeyModifiers::NONE); }
        app.on_key(KeyCode::Backspace, KeyModifiers::ALT);
        assert_eq!(app.input, "one ");
    }

    #[test]
    fn shift_enter_inserts_newline_plain_enter_submits() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_for(&temp_dir("shiftenter"));
        for c in "line1".chars() { app.on_key(KeyCode::Char(c), KeyModifiers::NONE); }
        app.on_key(KeyCode::Enter, KeyModifiers::SHIFT);
        for c in "line2".chars() { app.on_key(KeyCode::Char(c), KeyModifiers::NONE); }
        assert_eq!(app.input, "line1\nline2");
        app.on_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.input, "");
    }

    #[test]
    fn tool_calls_fill_live_actions_not_scrollback() {
        use rinne_core::worker::WorkerEvent as W;
        let mut app = app_for(&temp_dir("liveact"));
        app.nodes = vec![NodeView { id: "n1".into(), role: "generator".into(), status: NodeStatus::Running, worker: "claude-code".into() }];
        let before = app.pending.len();
        app.apply_engine(EngineEvent::NodeStream { id: "n1".into(), event: W::Reading("Cargo.toml".into()) });
        app.apply_engine(EngineEvent::NodeStream { id: "n1".into(), event: W::ToolUse("grep needle".into()) });
        assert_eq!(app.live_actions.get("n1").map(Vec::as_slice), Some(["Read Cargo.toml".to_string(), "Searched needle".to_string()].as_slice()));
        assert_eq!(app.pending.len(), before, "tool calls must not be pushed to scrollback");
    }

    #[test]
    fn live_actions_capped_per_agent() {
        use rinne_core::worker::WorkerEvent as W;
        let mut app = app_for(&temp_dir("liveactcap"));
        app.nodes = vec![NodeView { id: "n1".into(), role: "r".into(), status: NodeStatus::Running, worker: "w".into() }];
        for i in 0..(ACTIONS_PER_AGENT + 3) {
            app.apply_engine(EngineEvent::NodeStream { id: "n1".into(), event: W::Reading(format!("f{i}.rs")) });
        }
        let acts = app.live_actions.get("n1").unwrap();
        assert_eq!(acts.len(), ACTIONS_PER_AGENT);
        assert_eq!(acts.last().unwrap(), &format!("Read f{}.rs", ACTIONS_PER_AGENT + 2));
    }

    #[test]
    fn action_label_maps_friendly_verbs() {
        use rinne_core::worker::WorkerEvent as W;
        let cases = [
            (W::Reading("Cargo.toml".into()), Some("Read Cargo.toml")),
            (W::Editing("editing src/foo.rs".into()), Some("Edited src/foo.rs")),
            (W::Editing("writing src/foo.rs".into()), Some("Wrote src/foo.rs")),
            (W::ToolUse("grep needle".into()), Some("Searched needle")),
            (W::ToolUse("glob **/*.rs".into()), Some("Listed **/*.rs")),
            (W::ToolUse("subagent: review the diff".into()), Some("Delegated review the diff")),
            (W::ToolUse("cargo build".into()), Some("cargo build")),
            (W::Token("hi".into()), None),
            (W::Done, None),
        ];
        for (ev, want) in cases {
            assert_eq!(super::action_label(&ev).as_deref(), want, "for {ev:?}");
        }
    }
}
