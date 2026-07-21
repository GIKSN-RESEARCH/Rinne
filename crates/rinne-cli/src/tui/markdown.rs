//! Terminal markdown rendering for the transcript (`CONTEXT.md` §6).
//!
//! Worker output is markdown. Rather than dumping it raw (one token per line),
//! we parse it with `pulldown-cmark` and render styled, word-wrapped lines:
//! headings, bold/italic/inline-code, bullet and numbered lists, block quotes,
//! fenced code blocks, horizontal rules, and box-drawn tables. The result is a
//! `Vec<Line>` inserted into scrollback as one block, so it reads like Claude
//! Code rather than a wall of `⎿` rows.

use pulldown_cmark::{Alignment, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// Render markdown `input` into styled lines fitting `width` columns.
pub fn render(input: &str, width: usize) -> Vec<Line<'static>> {
    let mut r = Renderer::new(width.max(24));
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    for ev in Parser::new_ext(input, opts) {
        r.event(ev);
    }
    r.finish()
}

/// A styled word: its text, draw style, and whether a space precedes it (so
/// `**16**.` renders glued as `16.`, not `16 .`).
struct Word {
    text: String,
    style: Style,
    space_before: bool,
}

struct Renderer {
    width: usize,
    out: Vec<Line<'static>>,
    /// Inline words for the block currently being built (paragraph, heading…).
    words: Vec<Word>,
    // Active inline emphasis (nesting counts).
    bold: u32,
    italic: u32,
    code: u32,
    strike: u32,
    link: u32,
    /// When set, every word in the block uses this base style (headings).
    block_style: Option<Style>,
    /// List marker stack: `Some(counter)` ordered, `None` bulleted.
    lists: Vec<Option<u64>>,
    /// A pending list-item marker for the next flushed line.
    pending_marker: Option<String>,
    quote: u32,
    /// Whether the next inline word should be preceded by a space (tracks source
    /// whitespace across emphasis spans so punctuation stays attached).
    need_space: bool,
    // Fenced code block.
    in_code_block: bool,
    code_buf: String,
    // Table capture.
    in_table: bool,
    in_head: bool,
    aligns: Vec<Alignment>,
    cell: String,
    row: Vec<String>,
    rows: Vec<Vec<String>>,
    head_rows: usize,
}

impl Renderer {
    fn new(width: usize) -> Self {
        Self {
            width,
            out: Vec::new(),
            words: Vec::new(),
            bold: 0,
            italic: 0,
            code: 0,
            strike: 0,
            link: 0,
            block_style: None,
            lists: Vec::new(),
            pending_marker: None,
            quote: 0,
            need_space: false,
            in_code_block: false,
            code_buf: String::new(),
            in_table: false,
            in_head: false,
            aligns: Vec::new(),
            cell: String::new(),
            row: Vec::new(),
            rows: Vec::new(),
            head_rows: 0,
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_block(0);
        // Trim trailing blank lines.
        while matches!(self.out.last(), Some(l) if line_is_blank(l)) {
            self.out.pop();
        }
        self.out
    }

    fn inline_style(&self) -> Style {
        let mut s = self.block_style.unwrap_or_default();
        if self.bold > 0 {
            s = s.add_modifier(Modifier::BOLD);
        }
        if self.italic > 0 {
            s = s.add_modifier(Modifier::ITALIC);
        }
        if self.strike > 0 {
            s = s.add_modifier(Modifier::CROSSED_OUT);
        }
        if self.code > 0 {
            s = s.fg(Color::LightCyan);
        }
        if self.link > 0 {
            s = s.fg(Color::Blue).add_modifier(Modifier::UNDERLINED);
        }
        s
    }

    fn push_text(&mut self, text: &str) {
        if self.in_table {
            self.cell.push_str(text);
            return;
        }
        if self.in_code_block {
            self.code_buf.push_str(text);
            return;
        }
        let style = self.inline_style();
        let lead_ws = text.starts_with(char::is_whitespace);
        let mut first = true;
        for piece in text.split_whitespace() {
            let space_before = if first {
                !self.words.is_empty() && (self.need_space || lead_ws)
            } else {
                true
            };
            self.words.push(Word {
                text: piece.to_string(),
                style,
                space_before,
            });
            first = false;
        }
        // A trailing space (or an all-whitespace event) means the next word needs
        // a leading space; otherwise the next word glues to this one.
        self.need_space = text.ends_with(char::is_whitespace);
    }

    fn blank(&mut self) {
        if !self.out.is_empty() && !matches!(self.out.last(), Some(l) if line_is_blank(l)) {
            self.out.push(Line::default());
        }
    }

    fn event(&mut self, ev: Event<'_>) {
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => self.push_text(&t),
            Event::Code(t) => {
                // Inline code: one styled token, preserving internal spaces.
                if self.in_table {
                    self.cell.push_str(&t);
                } else {
                    let style = self.inline_style().fg(Color::LightCyan);
                    let space_before = !self.words.is_empty() && self.need_space;
                    self.words.push(Word {
                        text: t.to_string(),
                        style,
                        space_before,
                    });
                    self.need_space = false;
                }
            }
            Event::SoftBreak => {
                if self.in_table {
                    self.cell.push(' ');
                } else {
                    self.need_space = true; // a source newline is a word break
                }
            }
            Event::HardBreak => {
                if !self.in_table {
                    self.flush_block(self.base_indent());
                }
            }
            Event::Rule => {
                self.flush_block(0);
                self.blank();
                let w = self.width.min(72);
                self.out.push(Line::from(Span::styled(
                    "─".repeat(w),
                    Style::default().fg(Color::DarkGray),
                )));
                self.blank();
            }
            Event::TaskListMarker(done) => {
                let mark = if done { "[x]" } else { "[ ]" };
                let space_before = !self.words.is_empty() && self.need_space;
                self.words.push(Word {
                    text: mark.to_string(),
                    style: Style::default().fg(Color::DarkGray),
                    space_before,
                });
                self.need_space = true;
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                if self.lists.is_empty() {
                    self.blank();
                }
            }
            Tag::Heading { level, .. } => {
                self.flush_block(0);
                self.blank();
                self.block_style = Some(heading_style(level));
                if self.bold == 0 {
                    self.bold = 1; // headings render bold
                }
            }
            Tag::Strong => self.bold += 1,
            Tag::Emphasis => self.italic += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { .. } => self.link += 1,
            Tag::BlockQuote(_) => {
                self.flush_block(self.base_indent());
                self.quote += 1;
            }
            Tag::CodeBlock(_) => {
                self.flush_block(self.base_indent());
                self.blank();
                self.in_code_block = true;
                self.code_buf.clear();
            }
            Tag::List(start) => {
                self.flush_block(self.base_indent());
                if self.lists.is_empty() {
                    self.blank();
                }
                self.lists.push(start);
            }
            Tag::Item => {
                self.flush_block(self.base_indent());
                let depth = self.lists.len().saturating_sub(1);
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        m
                    }
                    _ => "• ".to_string(),
                };
                self.pending_marker = Some(format!("{}{}", "  ".repeat(depth), marker));
            }
            Tag::Table(aligns) => {
                self.flush_block(0);
                self.blank();
                self.in_table = true;
                self.aligns = aligns;
                self.rows.clear();
                self.head_rows = 0;
            }
            Tag::TableHead => {
                self.in_head = true;
                self.row.clear();
            }
            Tag::TableRow => self.row.clear(),
            Tag::TableCell => self.cell.clear(),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush_block(self.base_indent()),
            TagEnd::Heading(_) => {
                self.flush_block(0);
                self.block_style = None;
                self.bold = self.bold.saturating_sub(1);
                self.blank();
            }
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link => self.link = self.link.saturating_sub(1),
            TagEnd::BlockQuote(_) => {
                self.flush_block(self.base_indent());
                self.quote = self.quote.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                self.emit_code_block();
                self.in_code_block = false;
                self.blank();
            }
            TagEnd::List(_) => {
                self.lists.pop();
                if self.lists.is_empty() {
                    self.blank();
                }
            }
            TagEnd::Item => self.flush_block(self.base_indent()),
            TagEnd::Table => {
                self.render_table();
                self.in_table = false;
                self.blank();
            }
            TagEnd::TableHead => {
                self.rows.push(std::mem::take(&mut self.row));
                self.head_rows = self.rows.len();
                self.in_head = false;
            }
            TagEnd::TableRow => self.rows.push(std::mem::take(&mut self.row)),
            TagEnd::TableCell => {
                let c = std::mem::take(&mut self.cell);
                self.row.push(c.trim().to_string());
            }
            _ => {}
        }
    }

    fn base_indent(&self) -> usize {
        self.quote as usize * 2 + self.lists.len().saturating_sub(1) * 2
    }

    /// Flush the accumulated inline words as wrapped, styled lines.
    fn flush_block(&mut self, left: usize) {
        let marker = self.pending_marker.take();
        if self.words.is_empty() && marker.is_none() {
            return;
        }
        let quote_prefix = self.quote > 0;
        let lead_w = left + marker.as_deref().map(UnicodeWidthStr::width).unwrap_or(0);
        let avail = self
            .width
            .saturating_sub(lead_w + if quote_prefix { 2 } else { 0 })
            .max(8);

        // Greedy wrap words to `avail`, honoring each word's leading space.
        let mut lines: Vec<Vec<Word>> = vec![Vec::new()];
        let mut cur_w = 0usize;
        for w in self.words.drain(..) {
            let ww = UnicodeWidthStr::width(w.text.as_str());
            let at_start = lines.last().unwrap().is_empty();
            let sep = if at_start || !w.space_before { 0 } else { 1 };
            if cur_w + sep + ww > avail && !at_start {
                lines.push(Vec::new());
                cur_w = ww;
            } else {
                cur_w += sep + ww;
            }
            lines.last_mut().unwrap().push(w);
        }

        for (i, words) in lines.into_iter().enumerate() {
            let mut spans: Vec<Span<'static>> = Vec::new();
            // Quote bar.
            if quote_prefix {
                spans.push(Span::styled("▌ ", Style::default().fg(Color::DarkGray)));
            }
            // Leading indent + marker (marker only on the first line).
            let pad = if i == 0 {
                " ".repeat(left)
            } else {
                " ".repeat(lead_w)
            };
            if !pad.is_empty() {
                spans.push(Span::raw(pad));
            }
            if i == 0 {
                if let Some(m) = &marker {
                    spans.push(Span::styled(
                        m.clone(),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
            }
            for (j, w) in words.into_iter().enumerate() {
                if j > 0 && w.space_before {
                    spans.push(Span::raw(" "));
                }
                spans.push(Span::styled(w.text, w.style));
            }
            self.out.push(Line::from(spans));
        }
        // The next block starts fresh.
        self.need_space = false;
    }

    fn emit_code_block(&mut self) {
        let body = std::mem::take(&mut self.code_buf);
        let style = Style::default().fg(Color::Gray);
        for raw in body.trim_end_matches('\n').split('\n') {
            self.out.push(Line::from(vec![
                Span::styled("  │ ", Style::default().fg(Color::DarkGray)),
                Span::styled(raw.to_string(), style),
            ]));
        }
    }

    fn render_table(&mut self) {
        let rows = std::mem::take(&mut self.rows);
        let ncols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        if ncols == 0 {
            return;
        }
        // Natural column widths from content.
        let mut natural = vec![0usize; ncols];
        for r in &rows {
            for (i, c) in r.iter().enumerate() {
                natural[i] = natural[i].max(UnicodeWidthStr::width(c.as_str()));
            }
        }
        // Fit to terminal: borders (ncols+1) + padding (2 per col).
        let overhead = (ncols + 1) + 2 * ncols;
        let avail = self.width.saturating_sub(overhead).max(ncols * 4);
        let total: usize = natural.iter().sum();
        let widths: Vec<usize> = if total <= avail {
            natural
        } else {
            // Proportional shrink with a floor, then trim rounding overflow.
            let mut w: Vec<usize> = natural
                .iter()
                .map(|n| ((n * avail) / total.max(1)).max(4))
                .collect();
            let mut over = w.iter().sum::<usize>().saturating_sub(avail);
            while over > 0 {
                if let Some(idx) = (0..ncols).max_by_key(|&i| w[i]) {
                    if w[idx] > 4 {
                        w[idx] -= 1;
                        over -= 1;
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }
            w
        };

        let border = Style::default().fg(Color::DarkGray);
        let rule = |left: &str, mid: &str, right: &str, widths: &[usize]| -> Line<'static> {
            let mut s = String::from(left);
            for (i, w) in widths.iter().enumerate() {
                s.push_str(&"─".repeat(w + 2));
                s.push_str(if i + 1 == widths.len() { right } else { mid });
            }
            Line::from(Span::styled(s, border))
        };

        self.out.push(rule("┌", "┬", "┐", &widths));
        for (ri, row) in rows.iter().enumerate() {
            let is_head = ri < self.head_rows;
            // Wrap each cell to its column width; the row spans max(sub-lines).
            let wrapped: Vec<Vec<String>> = (0..ncols)
                .map(|ci| {
                    let cell = row.get(ci).map(String::as_str).unwrap_or("");
                    wrap_plain(cell, widths[ci])
                })
                .collect();
            let height = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
            for li in 0..height {
                let mut spans: Vec<Span<'static>> = Vec::new();
                for ci in 0..ncols {
                    spans.push(Span::styled("│ ", border));
                    let text = wrapped[ci].get(li).cloned().unwrap_or_default();
                    let cw = UnicodeWidthStr::width(text.as_str());
                    let align = self.aligns.get(ci).copied().unwrap_or(Alignment::None);
                    let padn = widths[ci].saturating_sub(cw);
                    let (lp, rp) = match align {
                        Alignment::Right => (padn, 0),
                        Alignment::Center => (padn / 2, padn - padn / 2),
                        _ => (0, padn),
                    };
                    let cell_style = if is_head {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    spans.push(Span::raw(" ".repeat(lp)));
                    spans.push(Span::styled(text, cell_style));
                    spans.push(Span::raw(" ".repeat(rp + 1)));
                }
                spans.push(Span::styled("│", border));
                self.out.push(Line::from(spans));
            }
            if is_head && ri + 1 == self.head_rows {
                self.out.push(rule("├", "┼", "┤", &widths));
            }
        }
        self.out.push(rule("└", "┴", "┘", &widths));
    }
}

fn heading_style(level: HeadingLevel) -> Style {
    match level {
        HeadingLevel::H1 | HeadingLevel::H2 => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        _ => Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    }
}

fn line_is_blank(l: &Line) -> bool {
    l.spans.iter().all(|s| s.content.trim().is_empty())
}

/// Greedy word-wrap to `width` display columns, hard-splitting over-long words.
fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0;
    for word in text.split_whitespace() {
        let ww = UnicodeWidthStr::width(word);
        if ww > width {
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            // Hard-split the long word by characters.
            for ch in word.chars() {
                let cwch = UnicodeWidthStr::width(ch.to_string().as_str());
                if cur_w + cwch > width && !cur.is_empty() {
                    lines.push(std::mem::take(&mut cur));
                    cur_w = 0;
                }
                cur.push(ch);
                cur_w += cwch;
            }
            continue;
        }
        let sep = if cur.is_empty() { 0 } else { 1 };
        if cur_w + sep + ww > width {
            lines.push(std::mem::take(&mut cur));
            cur.push_str(word);
            cur_w = ww;
        } else {
            if sep == 1 {
                cur.push(' ');
            }
            cur.push_str(word);
            cur_w += sep + ww;
        }
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(cur);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_heading_and_bold_without_markup() {
        let out = render("## Hello **world**", 80);
        let t = text_of(&out);
        assert!(t.contains("Hello world"), "{t:?}");
        assert!(
            !t.contains('#') && !t.contains('*'),
            "raw markup leaked: {t:?}"
        );
    }

    #[test]
    fn renders_bullets() {
        let out = render("- one\n- two\n- three", 80);
        let t = text_of(&out);
        assert!(t.contains("• one") && t.contains("• two"), "{t:?}");
    }

    #[test]
    fn renders_table_as_box() {
        let md = "| Aspect | Detail |\n|---|---|\n| Purpose | Host content |\n| Tech | Next.js |";
        let out = render(md, 80);
        let t = text_of(&out);
        assert!(
            t.contains('┌') && t.contains('│') && t.contains('└'),
            "no box: {t:?}"
        );
        assert!(
            t.contains("Aspect") && t.contains("Purpose") && t.contains("Next.js"),
            "{t:?}"
        );
        assert!(!t.contains("---"), "raw separator leaked: {t:?}");
    }

    #[test]
    fn wide_table_fits_width() {
        let md = "| A | B |\n|---|---|\n| this is a fairly long cell of text | another long-ish cell here |";
        let width = 40;
        let out = render(md, width);
        for l in &out {
            let w: usize = l
                .spans
                .iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            assert!(w <= width, "line exceeds width {width}: {w}");
        }
    }
}
