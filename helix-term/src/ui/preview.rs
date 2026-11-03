//! Markdown preview: a pane that shows a markdown file rendered instead of as text. The pane is an
//! ordinary view of the file, so it follows every edit; the EditorView only draws it differently
//! and scrolls it. It has its own renderer, laid out for reading: paragraphs reflow to the pane,
//! tables get borders, code blocks a shaded box. Hover docs keep Helix's popup renderer.

use std::sync::Arc;

use arc_swap::ArcSwap;
use helix_core::{
    syntax,
    unicode::width::{UnicodeWidthChar, UnicodeWidthStr},
};
use helix_view::{
    graphics::{Modifier, Rect, Style, UnderlineStyle},
    theme::Theme,
    Document,
};
use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use tui::{
    buffer::Buffer as Surface,
    text::{Span, Spans},
};

use super::{markdown::highlighted_code_block, panel_style::Roles};

/// Text never runs wider than this, however wide the pane.
const MAX_WIDTH: usize = 100;

/// Whether a document is markdown, by language or, before one is set, by extension.
pub fn is_markdown(doc: &Document) -> bool {
    doc.language_name() == Some("markdown")
        || doc
            .path()
            .and_then(|path| path.extension())
            .is_some_and(|ext| ext == "md" || ext == "markdown")
}

/// Draws `doc` rendered into `area`, from line `scroll`, which is clamped to the content.
/// Returns the source line each rendered line came from, for keeping the preview and the source
/// scrolled together.
pub fn render(
    area: Rect,
    surface: &mut Surface,
    doc: &Document,
    scroll: &mut u16,
    theme: &Theme,
    loader: Arc<ArcSwap<syntax::Loader>>,
    focused: bool,
) -> Vec<usize> {
    let roles = Roles::new(theme);
    surface.set_style(area, theme.get("ui.background"));
    if area.width < 8 || area.height < 3 {
        return Vec::new();
    }
    // A margin around the text, and the last row for a status line like the editor's.
    let body = Rect::new(
        area.x + 2,
        area.y + 1,
        area.width - 4,
        area.height.saturating_sub(2),
    );
    let width = (body.width as usize).min(MAX_WIDTH);
    let loader = loader.load();
    let lines = Renderer::new(theme, &roles, &loader, width).run(&doc.text().to_string());

    let height = lines.len() as u16;
    *scroll = (*scroll).min(height.saturating_sub(body.height / 2));
    for (row, line) in lines
        .iter()
        .skip(*scroll as usize)
        .take(body.height as usize)
        .enumerate()
    {
        let y = body.y + row as u16;
        if let Some(fill) = line.fill {
            let x = body.x + line.fill_from as u16;
            let w = (width - line.fill_from) as u16;
            surface.set_style(Rect::new(x, y, w, 1), fill);
        }
        surface.set_spans(body.x, y, &Spans(line.spans.clone()), body.width);
    }

    let status = area.bottom() - 1;
    let style = if focused {
        theme.get("ui.statusline")
    } else {
        theme.get("ui.statusline.inactive")
    };
    surface.set_style(Rect::new(area.x, status, area.width, 1), style);
    let name = doc.display_name();
    surface.set_stringn(
        area.x + 1,
        status,
        &format!("PREVIEW  {name}"),
        area.width.saturating_sub(2) as usize,
        style,
    );
    let hint = "j/k scroll  q close";
    if focused && (hint.len() + name.len() + 14) < area.width as usize {
        let x = area.right() - hint.len() as u16 - 1;
        surface.set_string(x, status, hint, style.patch(roles.muted));
    }
    lines.into_iter().map(|line| line.src).collect()
}

/// One row of the rendered document. `fill` paints the row's background from `fill_from` to the
/// text width, for code blocks.
struct Line {
    spans: Vec<Span<'static>>,
    fill: Option<Style>,
    fill_from: usize,
    blank: bool,
    /// The source line this line was rendered from.
    src: usize,
}

enum Container {
    Quote,
    /// A list item; its text lines up after the marker.
    Item { indent: usize },
}

struct Table {
    align: Vec<Alignment>,
    rows: Vec<Vec<Vec<(String, Style)>>>,
    srcs: Vec<usize>,
    row: Vec<Vec<(String, Style)>>,
    head_rows: usize,
}

struct Renderer<'a> {
    theme: &'a Theme,
    loader: &'a syntax::Loader,
    width: usize,
    lines: Vec<Line>,
    /// The paragraph, heading or cell being collected, as styled runs.
    inline: Vec<(String, Style)>,
    styles: Vec<Style>,
    containers: Vec<Container>,
    /// Ordered lists keep their next number; bullet lists have none.
    lists: Vec<Option<u64>>,
    marker: Option<(String, Style)>,
    code: Option<(String, String)>,
    table: Option<Table>,
    /// The source line of the block being rendered.
    src: usize,
    text: Style,
    muted: Style,
    faint: Style,
    accent: Style,
    shade: Style,
}

impl<'a> Renderer<'a> {
    fn new(theme: &'a Theme, roles: &Roles, loader: &'a syntax::Loader, width: usize) -> Self {
        let shade = ["ui.popup", "ui.cursorline.primary", "ui.menu"]
            .iter()
            .find_map(|key| theme.get(key).bg)
            .map(|bg| Style::default().bg(bg))
            .unwrap_or_default();
        Self {
            theme,
            loader,
            width,
            lines: Vec::new(),
            inline: Vec::new(),
            styles: Vec::new(),
            containers: Vec::new(),
            lists: Vec::new(),
            marker: None,
            code: None,
            table: None,
            src: 0,
            text: theme.get("ui.text"),
            muted: roles.muted,
            faint: roles.faint,
            accent: roles.accent,
            shade,
        }
    }

    fn run(mut self, source: &str) -> Vec<Line> {
        let mut options = Options::empty();
        options.insert(Options::ENABLE_STRIKETHROUGH);
        options.insert(Options::ENABLE_TABLES);
        options.insert(Options::ENABLE_TASKLISTS);
        let starts: Vec<usize> = std::iter::once(0)
            .chain(source.match_indices('\n').map(|(i, _)| i + 1))
            .collect();
        for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
            if let Event::Start(_) | Event::Rule = event {
                self.src = starts.partition_point(|&s| s <= range.start).saturating_sub(1);
            }
            self.event(event);
        }
        self.flush(Style::default());
        while self.lines.last().is_some_and(|line| line.blank) {
            self.lines.pop();
        }
        self.lines
    }

    fn style(&self) -> Style {
        let base = if self.in_quote() {
            self.theme.get("markup.quote").add_modifier(Modifier::ITALIC)
        } else {
            self.text
        };
        self.styles.iter().fold(base, |acc, s| acc.patch(*s))
    }

    fn in_quote(&self) -> bool {
        self.containers.iter().any(|c| matches!(c, Container::Quote))
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => {
                if let Some((_, code)) = &mut self.code {
                    code.push_str(&text);
                } else {
                    let style = self.style();
                    self.inline.push((text.to_string(), style));
                }
            }
            Event::Code(text) => {
                // Themes often give code the text colour; then it borrows the string colour.
                let code = self
                    .theme
                    .try_get_exact("markup.raw.inline")
                    .unwrap_or_else(|| self.theme.get("string"));
                let style = code
                    .patch(self.styles.iter().fold(Style::default(), |a, s| a.patch(*s)));
                self.inline.push((text.to_string(), style));
            }
            Event::InlineMath(text) | Event::DisplayMath(text) => {
                self.inline.push((text.to_string(), self.theme.get("markup.raw")));
            }
            Event::Html(text) | Event::InlineHtml(text) => {
                self.inline.push((text.trim_end().to_string(), self.muted));
            }
            Event::FootnoteReference(name) => {
                self.inline.push((format!("[{name}]"), self.muted));
            }
            Event::SoftBreak => {
                let style = self.style();
                self.inline.push((" ".into(), style));
            }
            Event::HardBreak => self.inline.push(("\n".into(), Style::default())),
            Event::Rule => {
                self.flush(Style::default());
                self.gap();
                let (_, prefix, used) = self.prefixes();
                let mut spans = prefix;
                spans.push(Span::styled(
                    "─".repeat(self.width.saturating_sub(used)),
                    self.faint,
                ));
                self.push(spans);
            }
            Event::TaskListMarker(done) => {
                self.marker = Some(if done {
                    ("☑ ".into(), self.accent)
                } else {
                    ("☐ ".into(), self.muted)
                });
            }
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => {
                if self.marker.is_none() {
                    self.gap();
                }
            }
            Tag::Heading { .. } => {
                self.flush(Style::default());
                self.gap();
            }
            Tag::BlockQuote(_) => {
                self.flush(Style::default());
                self.gap();
                self.containers.push(Container::Quote);
            }
            Tag::CodeBlock(kind) => {
                self.flush(Style::default());
                self.gap();
                let lang = match kind {
                    CodeBlockKind::Fenced(lang) => {
                        lang.split([',', ' ']).next().unwrap_or("").to_string()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((lang, String::new()));
            }
            Tag::HtmlBlock => {
                self.flush(Style::default());
                self.gap();
            }
            Tag::List(first) => {
                if self.containers.last().is_some_and(|c| matches!(c, Container::Item { .. })) {
                    // A nested list: the item's own text goes first, without a gap.
                    self.flush(Style::default());
                } else {
                    self.flush(Style::default());
                    self.gap();
                }
                self.lists.push(first);
            }
            Tag::Item => {
                self.flush(Style::default());
                let depth = self.lists.len().saturating_sub(1);
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let marker = format!("{n}. ");
                        *n += 1;
                        marker
                    }
                    _ => ["• ", "◦ ", "▪ "][depth % 3].to_string(),
                };
                let indent = marker.width();
                self.marker = Some((marker, self.theme.get("markup.list")));
                self.containers.push(Container::Item { indent });
            }
            Tag::Table(align) => {
                self.flush(Style::default());
                self.gap();
                self.table = Some(Table {
                    align,
                    rows: Vec::new(),
                    srcs: Vec::new(),
                    row: Vec::new(),
                    head_rows: 0,
                });
            }
            Tag::TableHead | Tag::TableRow | Tag::TableCell => {}
            Tag::Emphasis => self.styles.push(Style::default().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.styles.push(
                self.theme
                    .get("markup.bold")
                    .add_modifier(Modifier::BOLD),
            ),
            Tag::Strikethrough => self
                .styles
                .push(Style::default().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { .. } => self.styles.push(
                self.theme
                    .get("markup.link.label")
                    .underline_style(UnderlineStyle::Line),
            ),
            Tag::Image { .. } => {
                self.inline.push(("▣ ".into(), self.muted));
                self.styles.push(self.muted.add_modifier(Modifier::ITALIC));
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::HtmlBlock => self.flush(Style::default()),
            TagEnd::Heading(level) => self.heading(level),
            TagEnd::BlockQuote(_) => {
                self.flush(Style::default());
                self.containers.pop();
            }
            TagEnd::CodeBlock => self.code_block(),
            TagEnd::List(_) => {
                self.lists.pop();
            }
            TagEnd::Item => {
                self.flush(Style::default());
                self.containers.pop();
            }
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.inline);
                if let Some(table) = &mut self.table {
                    table.row.push(cell);
                }
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push(row);
                    table.srcs.push(self.src);
                    if tag == TagEnd::TableHead {
                        table.head_rows = table.rows.len();
                    }
                }
            }
            TagEnd::Table => self.table_block(),
            TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Link
            | TagEnd::Image => {
                self.styles.pop();
            }
            _ => {}
        }
    }

    /// The prefix for a block's first line (with any pending list marker), for its other lines,
    /// and their width.
    fn prefixes(&mut self) -> (Vec<Span<'static>>, Vec<Span<'static>>, usize) {
        let marker = self.marker.take();
        let innermost = self
            .containers
            .iter()
            .rposition(|c| matches!(c, Container::Item { .. }));
        let bar = self.faint;
        let (mut first, mut rest, mut width) = (Vec::new(), Vec::new(), 0);
        for (i, container) in self.containers.iter().enumerate() {
            match container {
                Container::Quote => {
                    first.push(Span::styled("▎ ", bar));
                    rest.push(Span::styled("▎ ", bar));
                    width += 2;
                }
                Container::Item { indent } => {
                    let pad = " ".repeat(*indent);
                    match (&marker, Some(i) == innermost) {
                        (Some((text, style)), true) => {
                            let text = format!("{text:<indent$}", indent = *indent);
                            first.push(Span::styled(text, *style));
                        }
                        _ => first.push(Span::raw(pad.clone())),
                    }
                    rest.push(Span::raw(pad));
                    width += indent;
                }
            }
        }
        (first, rest, width)
    }

    fn push(&mut self, spans: Vec<Span<'static>>) {
        self.lines.push(Line {
            spans,
            fill: None,
            fill_from: 0,
            blank: false,
            src: self.src,
        });
    }

    /// One empty line between blocks, never two, and none at the top.
    fn gap(&mut self) {
        if self.lines.last().is_none_or(|line| line.blank) {
            return;
        }
        let spans = self
            .containers
            .iter()
            .filter(|c| matches!(c, Container::Quote))
            .map(|_| Span::styled("▎ ", self.faint))
            .collect();
        self.lines.push(Line {
            spans,
            fill: None,
            fill_from: 0,
            blank: true,
            src: self.src,
        });
    }

    /// Wraps the collected inline text into lines, with `extra` patched over its styles.
    fn flush(&mut self, extra: Style) {
        if self.table.is_some() {
            return;
        }
        let run = std::mem::take(&mut self.inline);
        if run.iter().all(|(text, _)| text.trim().is_empty()) && self.marker.is_none() {
            return;
        }
        let (first, rest, used) = self.prefixes();
        let run: Vec<_> = run
            .into_iter()
            .map(|(text, style)| (text, style.patch(extra)))
            .collect();
        for (i, line) in wrap(&run, self.width.saturating_sub(used)).into_iter().enumerate() {
            let mut spans = if i == 0 { first.clone() } else { rest.clone() };
            spans.extend(line);
            self.push(spans);
        }
    }

    fn heading(&mut self, level: HeadingLevel) {
        let n = level as usize;
        let style = self
            .theme
            .get(&format!("markup.heading.{n}"))
            .add_modifier(Modifier::BOLD);
        self.flush(style);
        let rule = match level {
            HeadingLevel::H1 => Some(("━", style.remove_modifier(Modifier::BOLD))),
            HeadingLevel::H2 => Some(("─", self.faint)),
            _ => None,
        };
        if let Some((ch, style)) = rule {
            let (_, mut spans, used) = self.prefixes();
            spans.push(Span::styled(ch.repeat(self.width.saturating_sub(used)), style));
            self.push(spans);
        }
    }

    fn code_block(&mut self) {
        let Some((lang, code)) = self.code.take() else {
            return;
        };
        let (_, prefix, used) = self.prefixes();
        let inner = self.width.saturating_sub(used + 4).max(1);
        let text = highlighted_code_block(
            code.trim_end_matches('\n'),
            &lang,
            Some(self.theme),
            self.loader,
            None,
        );
        let shade = self.shade;
        let start = self.src;
        let boxed = |spans: Vec<Span<'static>>, src: usize, lines: &mut Vec<Line>| {
            let mut row = prefix.clone();
            row.push(Span::raw("  "));
            row.extend(spans);
            lines.push(Line {
                spans: row,
                fill: Some(shade),
                fill_from: used,
                blank: false,
                src,
            });
        };
        // The language sits on the top padding row; without one, the row stays empty.
        let label = vec![Span::styled(lang.clone(), self.muted)];
        boxed(label, start, &mut self.lines);
        for (i, line) in text.lines.into_iter().enumerate() {
            for part in char_wrap(line.0, inner) {
                boxed(part, start + 1 + i, &mut self.lines);
            }
        }
        let end = start + 1 + code.lines().count();
        boxed(Vec::new(), end, &mut self.lines);
    }

    fn table_block(&mut self) {
        let Some(table) = self.table.take() else {
            return;
        };
        let cols = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        if cols == 0 {
            return;
        }
        let (_, prefix, used) = self.prefixes();
        // Each column gets its widest cell, then the widest columns give way until the table fits.
        let mut widths = vec![1; cols];
        for row in &table.rows {
            for (i, cell) in row.iter().enumerate() {
                let w: usize = cell.iter().map(|(text, _)| text.width()).sum();
                widths[i] = widths[i].max(w);
            }
        }
        let room = self.width.saturating_sub(used + 3 * cols + 1);
        while widths.iter().sum::<usize>() > room {
            let (i, w) = widths
                .iter()
                .copied()
                .enumerate()
                .max_by_key(|&(_, w)| w)
                .unwrap();
            if w <= 4 {
                break;
            }
            widths[i] -= 1;
        }

        // A heavy muted frame; the lines between columns and rows are light and faint, and stop at
        // the frame without a joint, so no bright stub shows where they meet it.
        let (frame, inner) = (self.muted, self.faint);
        let rule = |l: &str, fill: &str, m: &str, r: &str, mid: Style| -> Vec<Span<'static>> {
            let mut spans = prefix.clone();
            spans.push(Span::styled(l.to_string(), frame));
            for (i, w) in widths.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::styled(m.to_string(), mid));
                }
                spans.push(Span::styled(fill.repeat(w + 2), mid));
            }
            spans.push(Span::styled(r.to_string(), frame));
            spans
        };
        let top = rule("┏", "━", "━", "┓", frame);
        self.push(top);
        let head = self.theme.get("markup.heading").add_modifier(Modifier::BOLD);
        let rows = table.rows.len();
        for (r, row) in table.rows.into_iter().enumerate() {
            self.src = table.srcs[r];
            let is_head = r < table.head_rows;
            let cells: Vec<Vec<Vec<Span<'static>>>> = (0..cols)
                .map(|i| {
                    let cell = row.get(i).cloned().unwrap_or_default();
                    let cell: Vec<_> = if is_head {
                        cell.into_iter().map(|(t, s)| (t, s.patch(head))).collect()
                    } else {
                        cell
                    };
                    wrap(&cell, widths[i])
                })
                .collect();
            let height = cells.iter().map(Vec::len).max().unwrap_or(1);
            for h in 0..height {
                let mut spans = prefix.clone();
                spans.push(Span::styled("┃", frame));
                for (i, cell) in cells.iter().enumerate() {
                    let content = cell.get(h).cloned().unwrap_or_default();
                    let w: usize = content.iter().map(|s| s.content.width()).sum();
                    let space = widths[i].saturating_sub(w);
                    let (left, right) = match table.align.get(i) {
                        Some(Alignment::Right) => (space, 0),
                        Some(Alignment::Center) => (space / 2, space - space / 2),
                        _ => (0, space),
                    };
                    spans.push(Span::raw(" ".repeat(left + 1)));
                    spans.extend(content);
                    spans.push(Span::raw(" ".repeat(right + 1)));
                    if i + 1 == cols {
                        spans.push(Span::styled("┃", frame));
                    } else {
                        spans.push(Span::styled("│", inner));
                    }
                }
                self.push(spans);
            }
            if r + 1 == rows {
                break;
            }
            let line = if is_head && r + 1 == table.head_rows {
                rule("┣", "━", "━", "┫", frame)
            } else {
                rule("┃", "─", "┼", "┃", inner)
            };
            self.push(line);
        }
        let bottom = rule("┗", "━", "━", "┛", frame);
        self.push(bottom);
    }
}

/// Splits text into words, runs of spaces and line breaks.
fn pieces(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut kind = None;
    for (i, ch) in text.char_indices() {
        let k = if ch == '\n' {
            0
        } else if ch.is_whitespace() {
            1
        } else {
            2
        };
        if kind != Some(k) || k == 0 {
            if i > start {
                out.push(&text[start..i]);
            }
            start = i;
            kind = Some(k);
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// Word-wraps styled runs to `avail` columns. Spaces collapse to one; words longer than a line
/// are split.
fn wrap(run: &[(String, Style)], avail: usize) -> Vec<Vec<Span<'static>>> {
    let avail = avail.max(1);
    let mut lines: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut used = 0;
    let mut space: Option<Style> = None;
    for (text, style) in run {
        for piece in pieces(text) {
            if piece == "\n" {
                lines.push(Vec::new());
                used = 0;
                space = None;
                continue;
            }
            if piece.trim().is_empty() {
                if used > 0 {
                    space = Some(*style);
                }
                continue;
            }
            let w = piece.width();
            if used > 0 && used + 1 + w > avail {
                lines.push(Vec::new());
                used = 0;
            } else if let Some(s) = space {
                lines.last_mut().unwrap().push(Span::styled(" ", s));
                used += 1;
            }
            space = None;
            let mut rest = piece;
            while !rest.is_empty() {
                let room = avail - used;
                if rest.width() <= room {
                    lines.last_mut().unwrap().push(Span::styled(rest.to_string(), *style));
                    used += rest.width();
                    break;
                }
                let mut cut = 0;
                let mut cw = 0;
                for (i, ch) in rest.char_indices() {
                    let w = ch.width().unwrap_or(0);
                    if cw + w > room {
                        break;
                    }
                    cw += w;
                    cut = i + ch.len_utf8();
                }
                if cut == 0 {
                    if used > 0 {
                        lines.push(Vec::new());
                        used = 0;
                        continue;
                    }
                    cut = rest.chars().next().map_or(rest.len(), char::len_utf8);
                }
                lines
                    .last_mut()
                    .unwrap()
                    .push(Span::styled(rest[..cut].to_string(), *style));
                rest = &rest[cut..];
                lines.push(Vec::new());
                used = 0;
            }
        }
    }
    while lines.len() > 1 && lines.last().is_some_and(Vec::is_empty) {
        lines.pop();
    }
    lines
}

/// Breaks a line of code at `avail` columns, keeping every character.
fn char_wrap(spans: Vec<Span<'_>>, avail: usize) -> Vec<Vec<Span<'static>>> {
    let mut lines: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut used = 0;
    for span in spans {
        let mut chunk = String::new();
        for ch in span.content.chars() {
            let w = ch.width().unwrap_or(0);
            if used + w > avail && used > 0 {
                if !chunk.is_empty() {
                    let text = std::mem::take(&mut chunk);
                    lines.last_mut().unwrap().push(Span::styled(text, span.style));
                }
                lines.push(Vec::new());
                used = 0;
            }
            chunk.push(ch);
            used += w;
        }
        if !chunk.is_empty() {
            lines.last_mut().unwrap().push(Span::styled(chunk, span.style));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Vec<Span<'static>>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn wraps_words_and_splits_long_ones() {
        let run = vec![("one two three".to_string(), Style::default())];
        assert_eq!(text(&wrap(&run, 7)), ["one two", "three"]);
        let run = vec![("abcdefghij".to_string(), Style::default())];
        assert_eq!(text(&wrap(&run, 4)), ["abcd", "efgh", "ij"]);
    }

    #[test]
    fn soft_breaks_join_and_hard_breaks_split() {
        let run = vec![
            ("a".to_string(), Style::default()),
            (" ".to_string(), Style::default()),
            ("b".to_string(), Style::default()),
            ("\n".to_string(), Style::default()),
            ("c".to_string(), Style::default()),
        ];
        assert_eq!(text(&wrap(&run, 20)), ["a b", "c"]);
    }
}
