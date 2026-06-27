//! Rendering for the harness-style TUI (`CONTEXT.md` §6).
//!
//! Two surfaces: [`flush_pending`] commits finished transcript lines into the
//! terminal's normal scrollback (so they're natively scrollable and survive
//! quit), and [`draw_viewport`] paints the small inline live region — status,
//! any in-progress streamed text, the `@`-picker, and the prompt.

use std::io;

use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, Paragraph, Widget};
use ratatui::{Frame, Terminal};

use rinne_core::NodeStatus;

use super::{markdown, App, FeedKind, NodeView};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Commit any queued transcript lines into the terminal's scrollback, above the
/// inline viewport. Each entry is word-wrapped to the terminal width.
pub fn flush_pending(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> io::Result<()> {
    if app.pending.is_empty() {
        return Ok(());
    }
    let width = terminal.get_frame().area().width.max(1) as usize;
    let expand_thinking = app.expand_thinking;
    let entries = std::mem::take(&mut app.pending);
    // Insert one visual line at a time so an entry taller than the terminal is
    // never truncated — each line just scrolls into native scrollback.
    for entry in entries {
        for line in render_entry((&entry).into(), width, expand_thinking) {
            terminal.insert_before(1, move |buf| {
                let area = buf.area;
                Paragraph::new(line).render(area, buf);
            })?;
        }
    }
    Ok(())
}

/// Emit pre-styled lines straight into scrollback (above the inline viewport).
/// Used for the one-time intro banner, whose per-letter gradient can't be
/// expressed through the text-based feed pipeline.
pub fn flush_lines(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    lines: Vec<Line<'static>>,
) -> io::Result<()> {
    for line in lines {
        terminal.insert_before(1, move |buf| {
            Paragraph::new(line).render(buf.area, buf);
        })?;
    }
    Ok(())
}

/// The five identity colors, reused for the wordmark gradient.
const GRADIENT: [Color; 5] = [Color::Cyan, Color::Magenta, Color::Blue, Color::Green, Color::Cyan];

/// The startup intro for scrollback: the gradient wordmark + a resolved workers
/// table. Used when committing the dismissed intro; the spinner is irrelevant by
/// then, so a static glyph is used for any still-checking row.
pub fn intro_lines(intro: &super::IntroState) -> Vec<Line<'static>> {
    intro_render(intro, "·")
}

/// Render the intro: gradient `rinne` wordmark, tagline, a unified workers table
/// (status glyph + model ladder per worker), and the conductor line. `spin` is
/// the spinner frame used for rows still being checked.
pub fn intro_render(intro: &super::IntroState, spin: &str) -> Vec<Line<'static>> {
    use super::WorkerAvail;
    let art = [
        "  ╦═╗ ╦ ╔╗╔ ╔╗╔ ╔═╗",
        "  ╠╦╝ ║ ║║║ ║║║ ╠╣ ",
        "  ╩╚═ ╩ ╝╚╝ ╝╚╝ ╚═╝",
    ];
    let mut out: Vec<Line<'static>> = Vec::new();
    out.push(Line::default());
    for row in art {
        let chars: Vec<char> = row.chars().collect();
        let n = chars.len().max(1);
        let spans: Vec<Span<'static>> = chars
            .into_iter()
            .enumerate()
            .map(|(i, c)| {
                let color = GRADIENT[(i * GRADIENT.len() / n).min(GRADIENT.len() - 1)];
                Span::styled(c.to_string(), Style::default().fg(color).add_modifier(Modifier::BOLD))
            })
            .collect();
        out.push(Line::from(spans));
    }
    out.push(Line::from(Span::styled(
        "  the conductor for your AI coding tools",
        Style::default().fg(Color::DarkGray),
    )));
    out.push(Line::default());

    // Workers table: status glyph · name · model ladder (or a default note).
    // Sorted so available workers come first, then checking, then not-installed.
    out.push(Line::from(Span::styled(
        "  workers",
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    )));
    let mut rows: Vec<&super::WorkerRow> = intro.workers.iter().collect();
    rows.sort_by_key(|w| match w.avail {
        WorkerAvail::Available => 0,
        WorkerAvail::Checking => 1,
        WorkerAvail::NotInstalled => 2,
    });
    for w in rows {
        let (glyph, gcolor) = match w.avail {
            WorkerAvail::Available => ("✔".to_string(), Color::Green),
            WorkerAvail::Checking => (spin.to_string(), Color::Cyan),
            WorkerAvail::NotInstalled => ("·".to_string(), Color::DarkGray),
        };
        let name_color = if w.avail == WorkerAvail::NotInstalled { Color::DarkGray } else { Color::White };
        let detail = match w.avail {
            WorkerAvail::NotInstalled => "not installed".to_string(),
            WorkerAvail::Checking => "checking…".to_string(),
            WorkerAvail::Available if w.ladder.len() > 1 => w.ladder.join(" · "),
            WorkerAvail::Available => "default model".to_string(),
        };
        let detail_color = if w.avail == WorkerAvail::Available { Color::Gray } else { Color::DarkGray };
        out.push(Line::from(vec![
            Span::styled(format!("  {glyph} "), Style::default().fg(gcolor)),
            Span::styled(format!("{:<14}", w.name), Style::default().fg(name_color)),
            Span::styled(detail, Style::default().fg(detail_color)),
        ]));
    }
    out.push(Line::default());

    out.push(Line::from(vec![
        Span::styled("  conductor  ", Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
        Span::styled(intro.conductor.clone(), Style::default().fg(Color::Gray)),
    ]));
    out.push(Line::default());

    out.push(Line::from(Span::styled(
        "  › describe what you want done, then press enter",
        Style::default().fg(Color::White),
    )));
    out.push(Line::from(Span::styled(
        "  @ files · /help · /clear · shift-enter newline · ctrl-q quit",
        Style::default().fg(Color::DarkGray),
    )));
    out.push(Line::default());
    out
}

/// Draw the inline live region: status, in-progress text or picker, and prompt.
pub fn draw_viewport(f: &mut Frame, app: &App) {
    let area = f.area();

    let prompt_inner_w = (area.width.saturating_sub(5)).max(8) as usize;
    let wrapped_input = wrap_input(app.input_str(), prompt_inner_w);
    let input_h = (wrapped_input.len() as u16 + 2).min(area.height.saturating_sub(2)).max(3);

    // Bottom-anchored: the live region (agents flow / picker / tail) fills the
    // space ABOVE the status+prompt footer. When idle it's empty, so the fill
    // collapses status+prompt to the bottom — no floating status in a tall,
    // otherwise-empty viewport (e.g. right after /clear).
    let chunks = Layout::vertical([
        Constraint::Min(0),          // live tail / picker / agents flow
        Constraint::Length(1),       // status (footer, just above prompt)
        Constraint::Length(input_h), // prompt
    ])
    .split(area);

    let cursor_char = app.cursor_char();
    draw_middle(f, chunks[0], app);
    draw_status(f, chunks[1], app);
    draw_prompt(f, chunks[2], app, &wrapped_input, cursor_char, prompt_inner_w);
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let spin = SPINNER[app.spinner % SPINNER.len()];
    let (status, color) = if app.parked.is_some() {
        ("paused for you".to_string(), Color::Yellow)
    } else if app.running {
        (format!("{spin} working"), Color::Cyan)
    } else {
        ("ready".to_string(), Color::Green)
    };
    let (done, total) = app.progress();
    let mut spans = vec![
        Span::styled(" rinne ", Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(status, Style::default().fg(color)),
    ];
    if total > 0 {
        let active = app.nodes.iter().filter(|n| matches!(n.status, NodeStatus::Running | NodeStatus::Parked)).count();
        let summary = if app.nodes.len() == 1 {
            format!("  · {done}/{total} done")
        } else {
            format!("  · {active} agent{} active · {done}/{total} done", if active == 1 { "" } else { "s" })
        };
        spans.push(Span::styled(summary, Style::default().fg(Color::DarkGray)));
    }
    if let Some(goal) = app.goal.as_deref() {
        spans.push(Span::styled("  · ", Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(truncate(goal, area.width.saturating_sub(28) as usize), Style::default().fg(Color::White)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_middle(f: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 {
        return;
    }
    // The live startup intro takes the whole live region until the first submit.
    if app.picker.is_none() && app.completion.is_none() && !app.running {
        if let Some(intro) = &app.intro {
            let spin = SPINNER[app.spinner % SPINNER.len()];
            let lines = intro_render(intro, spin);
            // Bottom-align within the region so the dashboard sits just above the
            // prompt rather than floating at the top of a tall viewport.
            let h = (lines.len() as u16).min(area.height);
            let slot = bottom_slice(area, h);
            f.render_widget(Paragraph::new(lines), slot);
            return;
        }
    }
    if app.picker.is_none() && app.completion.is_none() && app.running && !app.nodes.is_empty() {
        let spin = SPINNER[app.spinner % SPINNER.len()];
        // Pre-wrap the focused stream (answer preferred, else thinking),
        // preserving hard newlines so a banner line and the answer don't merge.
        let answering = !app.live_tail.is_empty();
        let body = if answering { &app.live_tail } else { &app.live_thinking };
        let avail = area.width.saturating_sub(4).max(8) as usize;
        let tail = tail_lines(body, avail, 2);
        let lines = render_agents_flow(
            &app.nodes,
            &app.live_actions,
            app.live_node.as_deref(),
            spin,
            &tail,
            area.height as usize,
        );
        f.render_widget(Paragraph::new(lines), area);
        return;
    }
    if let Some(picker) = &app.picker {
        let rows = area.height as usize;
        let items: Vec<ListItem> = picker
            .matches
            .iter()
            .take(rows.saturating_sub(1))
            .enumerate()
            .map(|(i, m)| {
                let style = if i == picker.selected {
                    Style::default().fg(Color::Black).bg(Color::Cyan)
                } else {
                    Style::default().fg(Color::White)
                };
                ListItem::new(Line::from(Span::styled(format!(" {m}"), style)))
            })
            .collect();
        let title = format!("@{} · {} match{} · tab", picker.query, picker.matches.len(), if picker.matches.len() == 1 { "" } else { "es" });
        let block = Block::default().borders(Borders::TOP).border_style(Style::default().fg(Color::Cyan)).title(title);
        let slot = bottom_slice(area, items.len() as u16 + 1);
        f.render_widget(List::new(items).block(block), slot);
    } else if let Some(comp) = &app.completion {
        let rows = area.height as usize;
        let items: Vec<ListItem> = comp
            .items
            .iter()
            .take(rows.saturating_sub(1))
            .enumerate()
            .map(|(i, it)| {
                let selected = i == comp.selected;
                let val_style = if selected {
                    Style::default().fg(Color::Black).bg(Color::Cyan)
                } else {
                    Style::default().fg(Color::White)
                };
                let desc_style = if selected {
                    Style::default().fg(Color::Black).bg(Color::Cyan)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                let usage_style = if selected {
                    Style::default().fg(Color::Black).bg(Color::Cyan)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!(" {:<14}", it.value), val_style),
                    Span::styled(format!("{:<24}", it.usage), usage_style),
                    Span::styled(it.desc.clone(), desc_style),
                ]))
            })
            .collect();
        let title = format!("{} · {} · tab to complete", comp.label, comp.items.len());
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::Cyan))
            .title(title);
        let slot = bottom_slice(area, items.len() as u16 + 1);
        f.render_widget(List::new(items).block(block), slot);
    } else if !app.live_tail.is_empty() || !app.live_thinking.is_empty() {
        // The in-progress streamed text, bottom-aligned. Show the answer once it
        // starts; until then show the reasoning (dimmed) so thinking is visible.
        let node = app.live_node.as_deref().unwrap_or("");
        let answering = !app.live_tail.is_empty();
        let body = if answering { &app.live_tail } else { &app.live_thinking };
        let prefix = if answering { "" } else { "✻ " };
        let text = format!("{node}  {prefix}{body}");
        let avail = area.width.saturating_sub(2).max(8) as usize;
        let wrapped = wrap(&text, avail);
        let rows = area.height as usize;
        let start = wrapped.len().saturating_sub(rows);
        let style = if answering {
            Style::default().fg(Color::Gray)
        } else {
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC)
        };
        let lines: Vec<Line> = wrapped[start..]
            .iter()
            .map(|l| Line::from(Span::styled(format!(" {l}"), style)))
            .collect();
        f.render_widget(Paragraph::new(lines), area);
    }
}

fn draw_prompt(f: &mut Frame, area: Rect, app: &App, wrapped: &[String], cursor_char: usize, width: usize) {
    let hint = if app.parked.is_some() {
        "enter to steer · /approve · /reject"
    } else if app.running {
        "esc pause · ctrl-o thinking"
    } else {
        "@ files · /help · /clear · shift-enter newline · ctrl-q quit"
    };

    // Locate the cursor in 2-D using the original text so hard newlines and
    // width-wraps are distinguished correctly.
    let (cline, ccol) = cursor_pos_in_text(app.input_str(), width, cursor_char);
    let lines: Vec<Line> = wrapped
        .iter()
        .enumerate()
        .map(|(i, text)| {
            let lead = if i == 0 {
                Span::styled("› ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
            } else {
                Span::raw("  ")
            };
            let white = Style::default().fg(Color::White);
            let mut spans = vec![lead];
            if i == cline {
                let before: String = text.chars().take(ccol).collect();
                let after: String = text.chars().skip(ccol).collect();
                spans.push(Span::styled(before, white));
                spans.push(Span::styled("▏", Style::default().fg(Color::Cyan)));
                spans.push(Span::styled(after, white));
            } else {
                spans.push(Span::styled(text.clone(), white));
            }
            Line::from(spans)
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .title_bottom(Span::styled(format!(" {hint} "), Style::default().fg(Color::DarkGray)));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// Render one transcript entry into styled, word-wrapped lines for scrollback.
fn render_entry(entry: FeedEntryRef, width: usize, expand_thinking: bool) -> Vec<Line<'static>> {
    // A completed worker message: render it as terminal markdown, indented under
    // a single dim node gutter, instead of one raw line per token.
    if entry.kind == FeedKind::Markdown {
        let indent = 4usize;
        let avail = width.saturating_sub(indent + 1).max(24);
        let mut out: Vec<Line<'static>> = Vec::new();
        if let Some(node) = entry.node {
            out.push(Line::from(Span::styled(
                format!("  ⎿ {node}"),
                Style::default().fg(Color::DarkGray),
            )));
        }
        for line in markdown::render(entry.text, avail) {
            let mut spans = vec![Span::raw(" ".repeat(indent))];
            spans.extend(line.spans);
            out.push(Line::from(spans));
        }
        return out;
    }

    // A reasoning block: dimmed and italic under a "✻ thinking" gutter. Collapsed
    // by default to a single summary line; ctrl+o expands subsequent blocks.
    if entry.kind == FeedKind::Thinking {
        let node = entry.node.map(|n| format!("{n} ")).unwrap_or_default();
        let dim_head = Style::default().fg(Color::DarkGray);
        if !expand_thinking {
            let n = entry.text.lines().filter(|l| !l.trim().is_empty()).count().max(1);
            return vec![Line::from(Span::styled(
                format!("  ✻ {node}thinking · {n} line{} · ctrl+o to show", if n == 1 { "" } else { "s" }),
                dim_head,
            ))];
        }
        let indent = 4usize;
        let avail = width.saturating_sub(indent + 1).max(24);
        let dim = dim_head.add_modifier(Modifier::ITALIC);
        let mut out: Vec<Line<'static>> = Vec::new();
        out.push(Line::from(Span::styled(format!("  ✻ {node}thinking"), dim_head)));
        for seg in entry.text.split('\n') {
            if seg.trim().is_empty() {
                out.push(Line::default());
                continue;
            }
            for chunk in wrap(seg, avail) {
                out.push(Line::from(vec![
                    Span::raw(" ".repeat(indent)),
                    Span::styled(chunk, dim),
                ]));
            }
        }
        return out;
    }

    let (glyph, color, bold) = match entry.kind {
        FeedKind::System => ("", Color::Gray, false),
        FeedKind::Conductor => ("· ", Color::Cyan, false),
        FeedKind::NodeStart => ("▶ ", Color::Blue, true),
        FeedKind::Markdown | FeedKind::Thinking => unreachable!("handled above"),
        FeedKind::NodeOk => ("✔ ", Color::Green, false),
        FeedKind::NodeFail => ("✗ ", Color::Red, false),
        FeedKind::Parked => ("⏸ ", Color::Yellow, false),
    };
    let indent = glyph.chars().count();
    let avail = width.saturating_sub(indent + 1).max(8);

    let mut style = Style::default().fg(color);
    if bold {
        style = style.add_modifier(Modifier::BOLD);
    }

    // Preserve the text's own line breaks. A line that fits is kept verbatim so
    // intentional alignment (e.g. the /help table's column padding) survives;
    // only an over-long line is word-wrapped (which collapses its whitespace).
    let wrapped: Vec<String> = entry
        .text
        .split('\n')
        .flat_map(|seg| {
            if seg.trim().is_empty() {
                vec![String::new()]
            } else if seg.chars().count() <= avail {
                vec![seg.to_string()]
            } else {
                wrap(seg, avail)
            }
        })
        .collect();

    let mut out = Vec::new();
    for (i, chunk) in wrapped.into_iter().enumerate() {
        let prefix = if i == 0 {
            format!(" {glyph}")
        } else {
            format!(" {}", " ".repeat(indent))
        };
        out.push(Line::from(vec![
            Span::styled(prefix, Style::default().fg(color)),
            Span::styled(chunk, style),
        ]));
    }
    out
}

/// A borrow of a feed entry's fields, decoupled from the App's storage type.
struct FeedEntryRef<'a> {
    kind: FeedKind,
    text: &'a str,
    node: Option<&'a str>,
}

impl<'a> From<&'a super::FeedEntry> for FeedEntryRef<'a> {
    fn from(e: &'a super::FeedEntry) -> Self {
        FeedEntryRef { kind: e.kind, text: &e.text, node: e.node.as_deref() }
    }
}

/// Greedy word-wrap to `width` columns, hard-splitting over-long words.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        if word.chars().count() > width {
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            for c in word.chars() {
                if cur.chars().count() == width {
                    lines.push(std::mem::take(&mut cur));
                }
                cur.push(c);
            }
            continue;
        }
        if cur.is_empty() {
            cur.push_str(word);
        } else if cur.chars().count() + 1 + word.chars().count() <= width {
            cur.push(' ');
            cur.push_str(word);
        } else {
            lines.push(std::mem::take(&mut cur));
            cur.push_str(word);
        }
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(cur);
    }
    lines
}

/// A bottom-aligned sub-rect of `area` that is at most `want` rows tall. Used so
/// the `@`-picker and slash-completion lists hug the prompt at the bottom of the
/// (bottom-anchored) live region instead of floating at its top.
fn bottom_slice(area: Rect, want: u16) -> Rect {
    let h = want.min(area.height);
    Rect { x: area.x, y: area.y + area.height - h, width: area.width, height: h }
}

/// The last `max` visual lines of `body` for the focused-stream tail. Splits on
/// hard newlines first so logical lines (e.g. a `model:` banner and the answer
/// that follows it) stay separate, then word-wraps each segment to `width`.
/// Without the newline split, `wrap`'s `split_whitespace` would collapse the
/// break and run the lines together.
fn tail_lines(body: &str, width: usize, max: usize) -> Vec<String> {
    if body.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    for seg in body.split('\n') {
        if seg.trim().is_empty() {
            continue;
        }
        out.extend(wrap(seg, width));
    }
    let start = out.len().saturating_sub(max);
    out[start..].to_vec()
}

/// Char-preserving wrap for the input field: splits on hard `\n` first, then
/// width-wraps each logical line so spaces and cursor positions are preserved.
fn wrap_input(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    for logical in text.split('\n') {
        let mut cur = String::new();
        for c in logical.chars() {
            if cur.chars().count() == width {
                lines.push(std::mem::take(&mut cur));
            }
            cur.push(c);
        }
        lines.push(cur);
    }
    lines
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

// Bridge the App's private input to the renderer.
impl App {
    fn input_str(&self) -> &str {
        &self.input
    }
    /// The cursor as a char index into the input (for prompt rendering).
    fn cursor_char(&self) -> usize {
        self.input[..self.cursor].chars().count()
    }
}

/// Map a cursor char index to its `(line, col)` in the 2-D rendered space,
/// accounting for both hard `\n` breaks and width-wrap boundaries.
fn cursor_pos_in_text(text: &str, width: usize, cursor_char: usize) -> (usize, usize) {
    let mut line = 0usize;
    let mut col = 0usize;
    for (n, c) in text.chars().enumerate() {
        if n == cursor_char {
            return (line, col);
        }
        if c == '\n' {
            line += 1;
            col = 0;
        } else if col == width {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

fn render_agents_flow(
    nodes: &[NodeView],
    actions: &std::collections::HashMap<String, Vec<String>>,
    focused: Option<&str>,
    spinner: &str,
    tail: &[String],
    height: usize,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    if height == 0 { return out; }

    // Order: active (running/parked) first, then finished — so truncation drops
    // finished/overflow last.
    let mut order: Vec<usize> = (0..nodes.len()).collect();
    order.sort_by_key(|&i| match nodes[i].status {
        NodeStatus::Running | NodeStatus::Parked => 0,
        _ => 1,
    });

    // Reserve room for the focused tail (at most 2 lines) when present.
    let tail_budget = if focused.is_some() && !tail.is_empty() { tail.len().min(2) } else { 0 };
    let body_budget = height.saturating_sub(tail_budget);

    let mut shown = 0usize;
    for (rank, &i) in order.iter().enumerate() {
        let n = &nodes[i];
        let identity = agent_color(i);
        let is_focused = focused == Some(n.id.as_str());
        let (glyph, gcolor) = status_glyph(n.status, identity, spinner, is_focused);

        // Heading line: "● Role  worker [· done]"
        let role_color = match n.status {
            NodeStatus::Running | NodeStatus::Parked => identity,
            _ => Color::DarkGray,
        };
        let mut heading = vec![
            Span::styled(format!("{glyph} "), Style::default().fg(gcolor)),
            Span::styled(n.role.clone(), Style::default().fg(role_color).add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::styled(n.worker.clone(), Style::default().fg(Color::DarkGray)),
        ];
        if matches!(n.status, NodeStatus::Succeeded) {
            heading.push(Span::styled("  · done", Style::default().fg(Color::DarkGray)));
        } else if matches!(n.status, NodeStatus::Failed) {
            heading.push(Span::styled("  · failed", Style::default().fg(Color::Red)));
        }

        // Will this heading (+ at least nothing) fit? If only room for a summary, emit it.
        let remaining = body_budget.saturating_sub(shown);
        let left = order.len() - rank;
        if remaining <= 1 && left > 1 {
            out.push(Line::from(Span::styled(
                format!("  … +{} more agents", left),
                Style::default().fg(Color::DarkGray),
            )));
            break;
        }
        if shown >= body_budget { break; }
        out.push(Line::from(heading));
        shown += 1;

        // Action lines for running/parked agents only, budget permitting.
        if matches!(n.status, NodeStatus::Running | NodeStatus::Parked) {
            if let Some(acts) = actions.get(&n.id) {
                for a in acts.iter() {
                    if shown >= body_budget { break; }
                    out.push(Line::from(vec![
                        Span::styled("  ⏺ ", Style::default().fg(Color::DarkGray)),
                        Span::styled(a.clone(), Style::default().fg(Color::DarkGray)),
                    ]));
                    shown += 1;
                }
            }
        }
    }

    // Focused stream tail under a ┊ gutter.
    if tail_budget > 0 {
        for l in tail.iter().take(tail_budget) {
            out.push(Line::from(vec![
                Span::styled("  ┊ ", Style::default().fg(Color::DarkGray)),
                Span::styled(l.clone(), Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC)),
            ]));
        }
    }
    out
}

fn agent_color(index: usize) -> Color {
    const PALETTE: [Color; 5] = [Color::Cyan, Color::Magenta, Color::Green, Color::Yellow, Color::Blue];
    PALETTE[index % PALETTE.len()]
}

fn status_glyph(status: NodeStatus, identity: Color, spinner: &str, focused: bool) -> (String, Color) {
    match status {
        NodeStatus::Running if focused => (spinner.to_string(), Color::Cyan),
        NodeStatus::Running => ("●".to_string(), identity),
        NodeStatus::Parked => ("⏸".to_string(), Color::Yellow),
        NodeStatus::Succeeded => ("✔".to_string(), Color::Green),
        NodeStatus::Failed => ("✗".to_string(), Color::Red),
        _ => ("○".to_string(), Color::DarkGray),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::{FeedEntry, FeedKind};

    #[test]
    fn intro_render_shows_wordmark_and_workers_table() {
        use crate::tui::{IntroState, WorkerAvail, WorkerRow};
        let intro = IntroState {
            workers: vec![
                WorkerRow { name: "claude-code".into(), avail: WorkerAvail::Available, ladder: vec!["haiku".into(), "sonnet".into(), "opus".into()] },
                WorkerRow { name: "codex".into(), avail: WorkerAvail::Available, ladder: vec![] },
                WorkerRow { name: "grok".into(), avail: WorkerAvail::NotInstalled, ladder: vec![] },
            ],
            conductor: "groq · openai/gpt-oss-120b".into(),
            resolved: true,
        };
        let lines = intro_render(&intro, "⠹");
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("conductor for your AI coding tools"), "tagline missing");
        assert!(text.contains("workers"), "workers header missing");
        // Available harness with a ladder shows its full ladder.
        assert!(text.contains("claude-code") && text.contains("haiku") && text.contains("opus"), "ladder missing: {text}");
        // Available harness without a ladder shows the default-model note.
        assert!(text.contains("codex") && text.contains("default model"), "codex row missing: {text}");
        // Not-installed harness is shown as such, not hidden.
        assert!(text.contains("grok") && text.contains("not installed"), "grok row missing: {text}");
        assert!(text.contains("conductor"), "conductor line missing");
        // The wordmark rows use the gradient: more than one fg color appears.
        let colors: std::collections::HashSet<_> = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter_map(|s| s.style.fg)
            .collect();
        assert!(colors.len() >= 3, "expected a multi-color gradient, got {colors:?}");
    }

    #[test]
    fn agent_color_cycles_palette() {
        assert_eq!(agent_color(0), Color::Cyan);
        assert_eq!(agent_color(5), Color::Cyan); // wraps
        assert_eq!(agent_color(1), Color::Magenta);
    }

    #[test]
    fn status_glyph_per_state() {
        let id = Color::Magenta;
        assert_eq!(status_glyph(NodeStatus::Succeeded, id, "⠹", false), ("✔".into(), Color::Green));
        assert_eq!(status_glyph(NodeStatus::Failed, id, "⠹", false), ("✗".into(), Color::Red));
        assert_eq!(status_glyph(NodeStatus::Running, id, "⠹", false).0, "●".to_string());
        assert_eq!(status_glyph(NodeStatus::Running, id, "⠹", true).0, "⠹".to_string());
        assert_eq!(status_glyph(NodeStatus::Parked, id, "⠹", false), ("⏸".into(), Color::Yellow));
        assert_eq!(status_glyph(NodeStatus::Pending, id, "⠹", false), ("○".into(), Color::DarkGray));
        assert_eq!(status_glyph(NodeStatus::Running, id, "⠹", false).1, id); // identity color on unfocused running dot
    }

    #[test]
    fn input_wrap_preserves_spaces() {
        assert_eq!(wrap_input("a b ", 80), vec!["a b ".to_string()]);
        assert_eq!(wrap_input("", 80), vec![String::new()]);
        assert_eq!(wrap_input("abcdef", 3), vec!["abc".to_string(), "def".to_string()]);
    }

    #[test]
    fn bottom_slice_hugs_bottom() {
        let area = Rect { x: 2, y: 0, width: 40, height: 10 };
        let s = bottom_slice(area, 3);
        assert_eq!((s.x, s.y, s.width, s.height), (2, 7, 40, 3)); // last 3 rows
        // Requesting more than available clamps to the area height.
        let s2 = bottom_slice(area, 99);
        assert_eq!((s2.y, s2.height), (0, 10));
    }

    #[test]
    fn tail_lines_keeps_banner_and_answer_separate() {
        // The `model:` banner and the answer arrive as separate lines in
        // live_tail; the tail must not merge them onto one row.
        let body = "model: sonnet-4-6\nI'll start by reviewing the recent commits.";
        let lines = tail_lines(body, 80, 2);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "model: sonnet-4-6");
        assert!(lines[1].starts_with("I'll start"), "{:?}", lines[1]);
    }

    #[test]
    fn tail_lines_takes_last_max_and_handles_empty() {
        assert!(tail_lines("", 80, 2).is_empty());
        let body = "one\ntwo\nthree\nfour";
        let lines = tail_lines(body, 80, 2);
        assert_eq!(lines, vec!["three".to_string(), "four".to_string()]);
    }

    #[test]
    fn feed_entry_preserves_newlines() {
        let entry = FeedEntry {
            kind: FeedKind::System,
            text: "- tip one\n- tip two\n- tip three".to_string(),
            node: None,
        };
        let lines = render_entry((&entry).into(), 80, false);
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn thinking_collapses_to_summary_then_expands() {
        let entry = FeedEntry {
            kind: FeedKind::Thinking,
            text: "first thought\nsecond thought\nthird".to_string(),
            node: Some("n3".to_string()),
        };
        // Collapsed: a single summary line with the line count.
        let collapsed = render_entry((&entry).into(), 80, false);
        assert_eq!(collapsed.len(), 1);
        let t: String = collapsed[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(t.contains("n3") && t.contains("thinking") && t.contains("3 lines") && t.contains("ctrl+o"), "{t:?}");
        // Expanded: a header plus the body lines.
        let expanded = render_entry((&entry).into(), 80, true);
        assert!(expanded.len() > 3, "expected full body, got {}", expanded.len());
        let all: String = expanded.iter().flat_map(|l| l.spans.iter()).map(|s| s.content.as_ref().to_string()).collect();
        assert!(all.contains("first thought") && all.contains("third"), "{all:?}");
    }

    #[test]
    fn fitting_line_preserves_column_padding() {
        // A /help-style row that fits must keep its alignment spaces (the prose
        // word-wrapper would collapse them).
        let entry = FeedEntry {
            kind: FeedKind::System,
            text: "  /plan             show the current plan".to_string(),
            node: None,
        };
        let lines = render_entry((&entry).into(), 80, false);
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("/plan             show"),
            "column padding was collapsed: {text:?}"
        );
    }

    #[test]
    fn wrap_input_splits_on_newline() {
        assert_eq!(wrap_input("a\nb", 80), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(wrap_input("a\n", 80), vec!["a".to_string(), String::new()]);
        assert_eq!(
            wrap_input("abcdef\nx", 3),
            vec!["abc".to_string(), "def".to_string(), "x".to_string()]
        );
    }

    #[test]
    fn cursor_pos_tracks_hard_newlines_and_width_wrap() {
        // Hard newline: positions across "ab\ncd" at a wide width.
        assert_eq!(cursor_pos_in_text("ab\ncd", 80, 0), (0, 0));
        assert_eq!(cursor_pos_in_text("ab\ncd", 80, 2), (0, 2)); // on the '\n'
        assert_eq!(cursor_pos_in_text("ab\ncd", 80, 3), (1, 0)); // first char after '\n'
        assert_eq!(cursor_pos_in_text("ab\ncd", 80, 5), (1, 2)); // end
        // Width wrap and the trailing-empty-line case must agree with wrap_input.
        assert_eq!(cursor_pos_in_text("abcdef", 3, 3), (0, 3)); // at the wrap boundary
        assert_eq!(cursor_pos_in_text("a\n", 80, 2), (1, 0));   // cursor on the new empty line
    }

    #[test]
    fn agents_flow_renders_role_headings_and_actions() {
        use std::collections::HashMap;
        let nodes = vec![
            NodeView { id: "n1".into(), role: "generator".into(), status: NodeStatus::Running, worker: "claude-code".into() },
            NodeView { id: "n2".into(), role: "scanner".into(), status: NodeStatus::Succeeded, worker: "haiku".into() },
        ];
        let mut actions = HashMap::new();
        actions.insert("n1".to_string(), vec!["Read Cargo.toml".to_string(), "Searched needle".to_string()]);
        let lines = render_agents_flow(&nodes, &actions, Some("n1"), "⠹", &[], 20);
        let text: String = lines.iter().flat_map(|l| l.spans.iter()).map(|s| s.content.as_ref().to_string()).collect();
        assert!(text.contains("generator") && text.contains("claude-code"), "{text}");
        assert!(text.contains("Read Cargo.toml") && text.contains("Searched needle"), "{text}");
        assert!(text.contains("scanner"), "finished agent heading still shown: {text}");
        // No node-ids leak into the user view.
        assert!(!text.contains("n1") && !text.contains("n2"), "ids must not appear: {text}");
    }

    #[test]
    fn agents_flow_truncates_under_height_pressure() {
        use std::collections::HashMap;
        let nodes: Vec<NodeView> = (0..10).map(|i| NodeView {
            id: format!("n{i}"), role: format!("role{i}"), status: NodeStatus::Running, worker: "w".into(),
        }).collect();
        let lines = render_agents_flow(&nodes, &HashMap::new(), None, "⠹", &[], 4);
        assert!(lines.len() <= 4, "must fit height: {}", lines.len());
        let text: String = lines.iter().flat_map(|l| l.spans.iter()).map(|s| s.content.as_ref().to_string()).collect();
        assert!(text.contains("more agents"), "overflow summary expected: {text}");
    }

    #[test]
    fn agents_flow_shows_focused_tail() {
        use std::collections::HashMap;
        let nodes = vec![NodeView { id: "n1".into(), role: "gen".into(), status: NodeStatus::Running, worker: "cc".into() }];
        let tail = vec!["…scanning the workspace now".to_string()];
        let lines = render_agents_flow(&nodes, &HashMap::new(), Some("n1"), "⠹", &tail, 20);
        let text: String = lines.iter().flat_map(|l| l.spans.iter()).map(|s| s.content.as_ref().to_string()).collect();
        assert!(text.contains("┊") && text.contains("scanning the workspace"), "{text}");
    }
}
