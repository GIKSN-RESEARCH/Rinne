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

use super::{markdown, App, FeedKind};

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

/// Draw the inline live region: status, in-progress text or picker, and prompt.
pub fn draw_viewport(f: &mut Frame, app: &App) {
    let area = f.area();

    let prompt_inner_w = (area.width.saturating_sub(5)).max(8) as usize;
    let wrapped_input = wrap_input(app.input_str(), prompt_inner_w);
    let input_h = (wrapped_input.len() as u16 + 2).min(area.height.saturating_sub(2)).max(3);

    let chunks = Layout::vertical([
        Constraint::Length(1),       // status
        Constraint::Min(0),          // live tail / picker
        Constraint::Length(input_h), // prompt
    ])
    .split(area);

    let cursor_char = app.cursor_char();
    draw_status(f, chunks[0], app);
    draw_middle(f, chunks[1], app);
    draw_prompt(f, chunks[2], app, &wrapped_input, cursor_char);
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
        spans.push(Span::styled(format!("  · {done}/{total}"), Style::default().fg(Color::DarkGray)));
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
        f.render_widget(List::new(items).block(block), area);
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
                ListItem::new(Line::from(vec![
                    Span::styled(format!(" {:<28}", it.value), val_style),
                    Span::styled(it.desc.clone(), desc_style),
                ]))
            })
            .collect();
        let title = format!("{} · {} · tab to complete", comp.label, comp.items.len());
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::Cyan))
            .title(title);
        f.render_widget(List::new(items).block(block), area);
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

fn draw_prompt(f: &mut Frame, area: Rect, app: &App, wrapped: &[String], cursor_char: usize) {
    let hint = if app.parked.is_some() {
        "enter to steer · /approve · /reject"
    } else if app.running {
        "esc pause · ctrl-o thinking"
    } else {
        "@ files · /help · ctrl-o thinking · ctrl-q quit"
    };

    // Locate the cursor (a char index) within the wrapped lines.
    let (cline, ccol) = cursor_pos(wrapped, cursor_char);
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
        FeedKind::Stream => ("  ⎿ ", Color::DarkGray, false),
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

/// Char-preserving wrap for the input field: keeps every space so the cursor
/// tracks typing.
fn wrap_input(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    let mut cur = String::new();
    for c in text.chars() {
        if cur.chars().count() == width {
            lines.push(std::mem::take(&mut cur));
        }
        cur.push(c);
    }
    lines.push(cur);
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

/// Map a cursor char index to its `(line, col)` within char-wrapped `lines`.
fn cursor_pos(lines: &[String], cursor_char: usize) -> (usize, usize) {
    let mut idx = cursor_char;
    for (i, l) in lines.iter().enumerate() {
        let lc = l.chars().count();
        if idx <= lc {
            return (i, idx);
        }
        idx -= lc;
    }
    // Past the end: park at the end of the last line.
    let last = lines.len().saturating_sub(1);
    (last, lines.last().map(|l| l.chars().count()).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::{FeedEntry, FeedKind};

    #[test]
    fn input_wrap_preserves_spaces() {
        assert_eq!(wrap_input("a b ", 80), vec!["a b ".to_string()]);
        assert_eq!(wrap_input("", 80), vec![String::new()]);
        assert_eq!(wrap_input("abcdef", 3), vec!["abc".to_string(), "def".to_string()]);
    }

    #[test]
    fn feed_entry_preserves_newlines() {
        let entry = FeedEntry {
            kind: FeedKind::Stream,
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
}
