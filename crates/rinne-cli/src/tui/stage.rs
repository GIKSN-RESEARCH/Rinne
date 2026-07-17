//! Harness Stage — multi-pane view of live harness sessions (`plan.md` Phase 2).
//!
//! Each harness node that opens a session (headless or PTY) gets a panel with a
//! scrolling transcript. The Stage sits in the TUI middle region while a run is
//! active so the user can *see* Claude Code / Codex / … work, not only Rinne's
//! summarized agent flow.

use rinne_core::NodeStatus;

/// Cap lines kept per session so a chatty harness can't OOM the TUI.
pub const STAGE_SCROLLBACK: usize = 400;

/// One live (or recently finished) harness session on the Stage.
#[derive(Debug, Clone)]
pub struct StageSession {
    pub node_id: String,
    pub worker: String,
    pub model: Option<String>,
    /// `pty` | `headless` | future backends.
    pub backend: String,
    pub status: StageStatus,
    /// Newest lines at the end.
    pub lines: Vec<String>,
    /// Scroll offset from the bottom (0 = follow tail).
    pub scroll: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageStatus {
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl StageStatus {
    pub fn label(self) -> &'static str {
        match self {
            StageStatus::Running => "running",
            StageStatus::Succeeded => "ok",
            StageStatus::Failed => "fail",
            StageStatus::Cancelled => "cancel",
        }
    }

}

impl StageSession {
    pub fn title(&self) -> String {
        let m = self
            .model
            .as_deref()
            .map(|m| format!(":{m}"))
            .unwrap_or_default();
        format!("{}{} · {} [{}]", self.worker, m, self.node_id, self.backend)
    }

    pub fn open(node_id: String, worker: String, model: Option<String>, backend: String) -> Self {
        Self {
            node_id,
            worker,
            model,
            backend,
            status: StageStatus::Running,
            lines: Vec::new(),
            scroll: 0,
        }
    }

    pub fn push_line(&mut self, line: impl Into<String>) {
        let line = line.into();
        if line.is_empty() {
            return;
        }
        // Skip pure whitespace-only spam
        if line.chars().all(|c| c.is_whitespace()) {
            return;
        }
        self.lines.push(line);
        if self.lines.len() > STAGE_SCROLLBACK {
            let drop = self.lines.len() - STAGE_SCROLLBACK;
            self.lines.drain(0..drop);
            self.scroll = self.scroll.saturating_sub(drop.min(self.scroll));
        }
    }

    /// Append a streaming token fragment onto the last Stage line (or start one).
    pub fn push_token(&mut self, token: &str) {
        if token.is_empty() {
            return;
        }
        if let Some(last) = self.lines.last_mut() {
            // Soft-wrap long token lines so panes stay readable.
            if last.len() > 240 || last.ends_with('\n') {
                self.lines.push(token.to_string());
            } else {
                last.push_str(token);
            }
        } else {
            self.lines.push(token.to_string());
        }
        if self.lines.len() > STAGE_SCROLLBACK {
            let drop = self.lines.len() - STAGE_SCROLLBACK;
            self.lines.drain(0..drop);
        }
    }

    pub fn finish(&mut self, status: NodeStatus) {
        self.status = match status {
            NodeStatus::Succeeded => StageStatus::Succeeded,
            NodeStatus::Failed => StageStatus::Failed,
            _ => StageStatus::Cancelled,
        };
        self.push_line(format!("── {} ──", self.status.label()));
    }

    pub fn scroll_up(&mut self, n: usize) {
        let max = self.lines.len().saturating_sub(1);
        self.scroll = (self.scroll + n).min(max);
    }

    pub fn scroll_down(&mut self, n: usize) {
        self.scroll = self.scroll.saturating_sub(n);
    }

    /// Visible window of lines for a pane of `height` rows (excluding chrome).
    pub fn window(&self, height: usize) -> &[String] {
        if height == 0 || self.lines.is_empty() {
            return &[];
        }
        let end = self.lines.len().saturating_sub(self.scroll);
        let start = end.saturating_sub(height);
        &self.lines[start..end]
    }
}

/// Stage board: ordered sessions + focus index + visibility toggle.
#[derive(Debug, Default, Clone)]
pub struct StageBoard {
    pub sessions: Vec<StageSession>,
    /// Index into `sessions` for keyboard focus.
    pub focus: usize,
    /// User can hide the Stage with `/stage` or ctrl+y while keeping sessions.
    pub visible: bool,
}

impl StageBoard {
    pub fn new() -> Self {
        Self {
            sessions: Vec::new(),
            focus: 0,
            // Show Stage automatically when the first session opens.
            visible: true,
        }
    }

    pub fn is_active(&self) -> bool {
        self.visible && !self.sessions.is_empty()
    }

    pub fn open_session(
        &mut self,
        node_id: String,
        worker: String,
        model: Option<String>,
        backend: String,
    ) {
        if let Some(s) = self.sessions.iter_mut().find(|s| s.node_id == node_id) {
            s.worker = worker;
            s.model = model;
            s.backend = backend;
            s.status = StageStatus::Running;
            return;
        }
        self.sessions
            .push(StageSession::open(node_id, worker, model, backend));
        self.focus = self.sessions.len() - 1;
        self.visible = true;
    }

    pub fn append(&mut self, node_id: &str, line: impl Into<String>) {
        if let Some(s) = self.sessions.iter_mut().find(|s| s.node_id == node_id) {
            s.push_line(line);
        }
    }

    pub fn append_token(&mut self, node_id: &str, token: &str) {
        if let Some(s) = self.sessions.iter_mut().find(|s| s.node_id == node_id) {
            s.push_token(token);
        }
    }

    pub fn finish(&mut self, node_id: &str, status: NodeStatus) {
        if let Some(s) = self.sessions.iter_mut().find(|s| s.node_id == node_id) {
            s.finish(status);
        }
    }

    pub fn cycle_focus(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        self.focus = (self.focus + 1) % self.sessions.len();
    }

    pub fn focused_mut(&mut self) -> Option<&mut StageSession> {
        if self.sessions.is_empty() {
            return None;
        }
        let i = self.focus.min(self.sessions.len() - 1);
        self.focus = i;
        self.sessions.get_mut(i)
    }

    pub fn clear(&mut self) {
        self.sessions.clear();
        self.focus = 0;
    }

    pub fn toggle_visible(&mut self) -> bool {
        self.visible = !self.visible;
        self.visible
    }

    /// Drop finished sessions older than keep_running; keep last N finished for glanceback.
    pub fn prune_finished(&mut self, keep_finished: usize) {
        let finished: Vec<usize> = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| s.status != StageStatus::Running)
            .map(|(i, _)| i)
            .collect();
        if finished.len() <= keep_finished {
            return;
        }
        let drop_n = finished.len() - keep_finished;
        let drop_idx: Vec<usize> = finished.into_iter().take(drop_n).collect();
        for i in drop_idx.into_iter().rev() {
            self.sessions.remove(i);
        }
        if self.focus >= self.sessions.len() {
            self.focus = self.sessions.len().saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_append_finish_and_scroll() {
        let mut board = StageBoard::new();
        board.open_session(
            "n1".into(),
            "claude-code".into(),
            Some("sonnet".into()),
            "pty".into(),
        );
        assert_eq!(board.sessions.len(), 1);
        assert!(board.is_active());
        board.append("n1", "reading src/main.rs");
        board.append("n1", "editing src/main.rs");
        assert_eq!(board.sessions[0].lines.len(), 2);
        board.sessions[0].scroll_up(1);
        assert_eq!(board.sessions[0].scroll, 1);
        board.sessions[0].scroll_down(5);
        assert_eq!(board.sessions[0].scroll, 0);
        board.finish("n1", NodeStatus::Succeeded);
        assert_eq!(board.sessions[0].status, StageStatus::Succeeded);
        assert!(board.sessions[0].title().contains("claude-code:sonnet"));
    }

    #[test]
    fn scrollback_cap() {
        let mut s = StageSession::open("n".into(), "codex".into(), None, "headless".into());
        for i in 0..STAGE_SCROLLBACK + 50 {
            s.push_line(format!("line {i}"));
        }
        assert_eq!(s.lines.len(), STAGE_SCROLLBACK);
    }

    #[test]
    fn cycle_focus_and_toggle() {
        let mut board = StageBoard::new();
        board.open_session("a".into(), "claude-code".into(), None, "pty".into());
        board.open_session("b".into(), "codex".into(), None, "headless".into());
        assert_eq!(board.focus, 1);
        board.cycle_focus();
        assert_eq!(board.focus, 0);
        // starts visible=true → first toggle hides
        assert!(!board.toggle_visible());
        assert!(!board.is_active());
        assert!(board.toggle_visible());
        assert!(board.is_active());
    }
}
