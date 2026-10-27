use crate::{
    commands::{self, OnKeyCallback, OnKeyCallbackKind},
    compositor::{Component, Context, Event, EventResult},
    events::{OnModeSwitch, PostCommand},
    handlers::completion::CompletionItem,
    key,
    keymap::{KeymapResult, Keymaps},
    ui::{
        document::{render_document, LinePos, TextRenderer},
        statusline,
        text_decorations::{self, Decoration, DecorationManager, InlineDiagnostics},
        Completion, ProgressSpinners,
    },
};

use helix_core::{
    diagnostic::NumberOrString,
    graphemes::{next_grapheme_boundary, prev_grapheme_boundary},
    movement::Direction,
    syntax::{self, OverlayHighlights},
    text_annotations::TextAnnotations,
    unicode::width::UnicodeWidthStr,
    visual_offset_from_block, Change, Position, Range, Selection, Transaction,
};
use helix_view::{
    annotations::diagnostics::DiagnosticFilter,
    document::{Mode, SCRATCH_BUFFER_NAME},
    editor::{Action, CompleteAction, CursorShapeConfig},
    graphics::{Color, CursorKind, Modifier, Rect, Style},
    input::{KeyEvent, MouseButton, MouseEvent, MouseEventKind},
    keyboard::{KeyCode, KeyModifiers},
    Document, DocumentId, Editor, Theme, View, ViewId,
};
use std::{mem::take, num::NonZeroUsize, ops, path::PathBuf, rc::Rc};

use super::{
    agents::{self, AgentKind, AgentSession},
    file_tree::{self, FileTree, TreeAction},
    frame::{self, End, Label},
    git_panel::{GitAction, GitPanel},
    panel_style::Roles,
    terminal::TerminalPanel,
};

/// A panel edge that can be dragged to resize the panel.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Edge {
    Sidebar,
    Terminal,
    Agents,
}

/// What a mouse press took hold of.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Grab {
    /// A panel edge: dragging resizes.
    Edge(Edge),
    /// A tab number on the editor box's top border: dragging along it reorders.
    EditorTab(usize),
    /// A buffer name on the editor box's top border: dropping it on a view splits it there.
    Buffer(DocumentId),
    /// A terminal or agent tab label: dragging along the strip reorders, dropping it in the
    /// panel puts it beside the shown tab as a pane.
    PanelTab(bool, usize),
}

/// A mouse press, until it is released: a press that moves is a drag, one that does not is a
/// click.
struct Press {
    grab: Grab,
    column: u16,
    row: u16,
    moved: bool,
    /// What follows the mouse while dragging.
    label: String,
}

/// The sidebar's default width: a fifth of the window, within readable limits.
const SIDEBAR_DEFAULT: (u16, u16) = (30, 48);
const SIDEBAR_MIN: u16 = 24;
/// The editor always keeps this many columns and rows.
const EDITOR_MIN_WIDTH: u16 = 40;
const EDITOR_MIN_HEIGHT: u16 = 6;
/// The terminal's default share of the main box, and its smallest size, in rows.
const TERMINAL_PERCENT: u16 = 35;
const TERMINAL_MIN: u16 = 4;
/// The agent panel's default width: two fifths of the window, within these limits.
const AGENTS_DEFAULT: (u16, u16) = (52, 110);
const AGENTS_MIN: u16 = 36;

/// Cuts `text` to `width` columns, keeping its start.
fn shorten_end(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(width.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// Helide's panels around the editor, for focus handling.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Panel {
    None,
    Tree,
    Git,
    Terminal,
    Agents,
}

use tui::{buffer::Buffer as Surface, text::Span};

pub struct EditorView {
    pub keymaps: Keymaps,
    on_next_key: Option<(OnKeyCallback, OnKeyCallbackKind)>,
    pseudo_pending: Vec<KeyEvent>,
    pub(crate) last_insert: (commands::MappableCommand, Vec<InsertEvent>),
    pub(crate) completion: Option<Completion>,
    spinners: ProgressSpinners,
    /// Tracks if the terminal window is focused by reaction to terminal focus events
    terminal_focused: bool,
    /// Helide's terminal panel, docked under the editor.
    pub terminal: TerminalPanel,
    /// Helide's agent panel, on the right.
    pub agents: TerminalPanel,
    /// Zen mode: every panel and the frame hidden. Holds which panels to bring back.
    zen: Option<[bool; 4]>,
    /// One line per language server of the current document, set by the Application each
    /// frame: (name, state), shown in the editor box's bottom border.
    pub lsp_summary: Vec<(String, String)>,
    /// Locations found by the last error-location jump, and which one is shown.
    locations: (Vec<super::terminal::Location>, usize),
    /// When the workspace was last saved, and what was saved; it is saved again when it changes.
    saved_at: Option<std::time::Instant>,
    saved: Option<(Option<agents::EditorLayout>, agents::Panels)>,
    /// The project whose saved agent tabs were last restored or saved.
    agents_project: Option<PathBuf>,
    agents_width: Option<u16>,
    agents_box: Option<Rect>,
    /// Helide's file tree, docked left of the editor.
    pub file_tree: FileTree,
    /// Helide's git panel, sharing the left sidebar with the file tree.
    pub git: GitPanel,
    /// Where each tab label was drawn last frame: (row, start column, end column).
    tab_hits: Vec<(u16, u16, u16)>,
    /// Columns each buffer label covered on the editor box's top border last frame.
    buffer_hits: Vec<(u16, u16, DocumentId)>,
    /// Sidebar width and terminal rows the user set by dragging or with Alt+Shift+arrows.
    sidebar_width: Option<u16>,
    terminal_height: Option<u16>,
    /// A mouse press on a label or panel edge, until it is released.
    pressed: Option<Press>,
    /// Where the mouse is while dragging.
    drag_at: (u16, u16),
    /// The frame's boxes, as laid out for the last frame.
    sidebar_box: Option<Rect>,
    main_box: Rect,
    editor_box: Rect,
    terminal_box: Option<Rect>,
}

#[derive(Debug, Clone)]
pub enum InsertEvent {
    Key(KeyEvent),
    CompletionApply {
        trigger_offset: usize,
        changes: Vec<Change>,
    },
    TriggerCompletion,
    RequestCompletion,
}

impl EditorView {
    pub fn new(keymaps: Keymaps) -> Self {
        Self {
            keymaps,
            on_next_key: None,
            pseudo_pending: Vec::new(),
            last_insert: (commands::MappableCommand::normal_mode, Vec::new()),
            completion: None,
            spinners: ProgressSpinners::default(),
            terminal_focused: true,
            terminal: TerminalPanel::new("TERMINAL"),
            agents: TerminalPanel::new("AGENTS"),
            zen: None,
            lsp_summary: Vec::new(),
            locations: (Vec::new(), 0),
            saved_at: None,
            saved: None,
            agents_project: None,
            agents_width: None,
            agents_box: None,
            file_tree: FileTree::default(),
            git: GitPanel::default(),
            tab_hits: Vec::new(),
            buffer_hits: Vec::new(),
            sidebar_width: None,
            terminal_height: None,
            pressed: None,
            drag_at: (0, 0),
            sidebar_box: None,
            main_box: Rect::default(),
            editor_box: Rect::default(),
            terminal_box: None,
        }
    }

    pub fn spinners_mut(&mut self) -> &mut ProgressSpinners {
        &mut self.spinners
    }

    pub fn render_view(
        &self,
        editor: &Editor,
        doc: &Document,
        view: &View,
        viewport: Rect,
        surface: &mut Surface,
        is_focused: bool,
    ) {
        let inner = view.inner_area(doc);
        let area = view.area;
        let theme = &editor.theme;
        let config = editor.config();
        let loader = editor.syn_loader.load();

        let view_offset = doc.view_offset(view.id);

        let text_annotations = view.text_annotations(doc, Some(theme));
        let mut decorations = DecorationManager::default();

        if is_focused && config.cursorline {
            decorations.add_decoration(Self::cursorline(doc, view, theme));
        }

        if is_focused && config.cursorcolumn {
            Self::highlight_cursorcolumn(doc, view, surface, theme, inner, &text_annotations);
        }

        // Set DAP highlights, if needed.
        if let Some(frame) = editor.current_stack_frame() {
            let dap_line = frame.line.saturating_sub(1);
            let style = theme.get("ui.highlight.frameline");
            let line_decoration = move |renderer: &mut TextRenderer, pos: LinePos| {
                if pos.doc_line != dap_line {
                    return;
                }
                renderer.set_style(Rect::new(inner.x, pos.visual_line, inner.width, 1), style);
            };

            decorations.add_decoration(line_decoration);
        }

        let syntax_highlighter =
            Self::doc_syntax_highlighter(doc, view_offset.anchor, inner.height, &loader);
        let mut overlays = Vec::new();

        overlays.push(Self::overlay_syntax_highlights(
            doc,
            view_offset.anchor,
            inner.height,
            &text_annotations,
        ));

        if doc
            .language_config()
            .and_then(|config| config.rainbow_brackets)
            .unwrap_or(config.rainbow_brackets)
        {
            if let Some(overlay) =
                Self::doc_rainbow_highlights(doc, view_offset.anchor, inner.height, theme, &loader)
            {
                overlays.push(overlay);
            }
        }

        if let Some(overlay) = Self::doc_document_link_highlights(doc, theme) {
            overlays.push(overlay);
        }

        Self::doc_diagnostics_highlights_into(doc, theme, &mut overlays);

        if is_focused {
            if config.lsp.auto_document_highlight {
                if let Some(overlay) = Self::doc_document_highlights(doc, view, theme) {
                    overlays.push(overlay);
                }
            }
            if let Some(tabstops) = Self::tabstop_highlights(doc, theme) {
                overlays.push(tabstops);
            }
            overlays.push(Self::doc_selection_highlights(
                editor.mode(),
                doc,
                view,
                theme,
                &config.cursor_shape,
                self.terminal_focused,
            ));
            if let Some(overlay) = Self::highlight_focused_view_elements(view, doc, theme) {
                overlays.push(overlay);
            }
        }

        let gutter_overflow = view.gutter_offset(doc) == 0;
        if !gutter_overflow {
            Self::render_gutter(
                editor,
                doc,
                view,
                view.area,
                theme,
                is_focused & self.terminal_focused,
                &mut decorations,
            );
        }

        Self::render_rulers(editor, doc, view, inner, surface, theme);

        let primary_cursor = doc
            .selection(view.id)
            .primary()
            .cursor(doc.text().slice(..));
        if is_focused {
            decorations.add_decoration(text_decorations::Cursor {
                cache: &editor.cursor_cache,
                primary_cursor,
            });
        }
        let width = view.inner_width(doc);
        let config = doc.config.load();
        let enable_cursor_line = view
            .diagnostics_handler
            .show_cursorline_diagnostics(doc, view.id);
        let inline_diagnostic_config = config.inline_diagnostics.prepare(width, enable_cursor_line);
        decorations.add_decoration(InlineDiagnostics::new(
            doc,
            theme,
            primary_cursor,
            inline_diagnostic_config,
            config.end_of_line_diagnostics,
        ));
        render_document(
            surface,
            inner,
            doc,
            view_offset,
            &text_annotations,
            syntax_highlighter,
            overlays,
            theme,
            decorations,
        );

        // if we're not at the edge of the screen, draw a right border
        if viewport.right() != view.area.right() {
            let x = area.right();
            let border_style = theme.get("ui.window");
            for y in area.top()..area.bottom() {
                surface[(x, y)]
                    .set_symbol(tui::symbols::line::VERTICAL)
                    //.set_symbol(" ")
                    .set_style(border_style);
            }
        }

        if config.inline_diagnostics.disabled()
            && config.end_of_line_diagnostics == DiagnosticFilter::Disable
        {
            Self::render_diagnostics(doc, view, inner, surface, theme);
        }

        let statusline_area = view
            .area
            .clip_top(view.area.height.saturating_sub(1))
            .clip_bottom(1); // -1 from bottom to remove commandline

        let mut context =
            statusline::RenderContext::new(editor, doc, view, is_focused, &self.spinners);

        statusline::render(&mut context, statusline_area, surface);
    }

    pub fn render_rulers(
        editor: &Editor,
        doc: &Document,
        view: &View,
        viewport: Rect,
        surface: &mut Surface,
        theme: &Theme,
    ) {
        let editor_rulers = &editor.config().rulers;
        let ruler_theme = theme
            .try_get("ui.virtual.ruler")
            .unwrap_or_else(|| Style::default().bg(Color::Red));

        let rulers = doc
            .language_config()
            .and_then(|config| config.rulers.as_ref())
            .unwrap_or(editor_rulers);

        let view_offset = doc.view_offset(view.id);

        rulers
            .iter()
            // View might be horizontally scrolled, convert from absolute distance
            // from the 1st column to relative distance from left of viewport
            .filter_map(|ruler| ruler.checked_sub(1 + view_offset.horizontal_offset as u16))
            .filter(|ruler| ruler < &viewport.width)
            .map(|ruler| viewport.clip_left(ruler).with_width(1))
            .for_each(|area| surface.set_style(area, ruler_theme))
    }

    fn viewport_byte_range(
        text: helix_core::RopeSlice,
        row: usize,
        height: u16,
    ) -> std::ops::Range<usize> {
        // Calculate viewport byte ranges:
        // Saturating subs to make it inclusive zero indexing.
        let last_line = text.len_lines().saturating_sub(1);
        let last_visible_line = (row + height as usize).saturating_sub(1).min(last_line);
        let start = text.line_to_byte(row.min(last_line));
        let end = text.line_to_byte(last_visible_line + 1);

        start..end
    }

    /// Get the syntax highlighter for a document in a view represented by the first line
    /// and column (`offset`) and the last line. This is done instead of using a view
    /// directly to enable rendering syntax highlighted docs anywhere (eg. picker preview)
    pub fn doc_syntax_highlighter<'editor>(
        doc: &'editor Document,
        anchor: usize,
        height: u16,
        loader: &'editor syntax::Loader,
    ) -> Option<syntax::Highlighter<'editor>> {
        let syntax = doc.syntax()?;
        let text = doc.text().slice(..);
        let row = text.char_to_line(anchor.min(text.len_chars()));
        let range = Self::viewport_byte_range(text, row, height);
        let range = range.start as u32..range.end as u32;

        let highlighter = syntax.highlighter(text, loader, range);
        Some(highlighter)
    }

    pub fn overlay_syntax_highlights(
        doc: &Document,
        anchor: usize,
        height: u16,
        text_annotations: &TextAnnotations,
    ) -> OverlayHighlights {
        let text = doc.text().slice(..);
        let row = text.char_to_line(anchor.min(text.len_chars()));

        let mut range = Self::viewport_byte_range(text, row, height);
        range = text.byte_to_char(range.start)..text.byte_to_char(range.end);

        text_annotations.collect_overlay_highlights(range)
    }

    pub fn doc_rainbow_highlights(
        doc: &Document,
        anchor: usize,
        height: u16,
        theme: &Theme,
        loader: &syntax::Loader,
    ) -> Option<OverlayHighlights> {
        let syntax = doc.syntax()?;
        let text = doc.text().slice(..);
        let row = text.char_to_line(anchor.min(text.len_chars()));
        let visible_range = Self::viewport_byte_range(text, row, height);
        let start = syntax::child_for_byte_range(
            &syntax.tree().root_node(),
            visible_range.start as u32..visible_range.end as u32,
        )
        .map_or(visible_range.start as u32, |node| node.start_byte());
        let range = start..visible_range.end as u32;

        Some(syntax.rainbow_highlights(text, theme.rainbow_length(), loader, range))
    }

    /// Get highlight spans for document diagnostics
    pub fn doc_diagnostics_highlights_into(
        doc: &Document,
        theme: &Theme,
        overlay_highlights: &mut Vec<OverlayHighlights>,
    ) {
        // Skip redundant work if no diagnostics.
        if doc.diagnostics().is_empty() {
            return;
        }

        use helix_core::diagnostic::{DiagnosticTag, Range, Severity};
        let get_scope_of = |scope| {
            theme
                .find_highlight_exact(scope)
                // get one of the themes below as fallback values
                .or_else(|| theme.find_highlight_exact("diagnostic"))
                .or_else(|| theme.find_highlight_exact("ui.cursor"))
                .or_else(|| theme.find_highlight_exact("ui.selection"))
                .expect(
                    "at least one of the following scopes must be defined in the theme: `diagnostic`, `ui.cursor`, or `ui.selection`",
                )
        };

        // Diagnostic tags
        let unnecessary = theme.find_highlight_exact("diagnostic.unnecessary");
        let deprecated = theme.find_highlight_exact("diagnostic.deprecated");

        let mut default_vec = Vec::new();
        let mut info_vec = Vec::new();
        let mut hint_vec = Vec::new();
        let mut warning_vec = Vec::new();
        let mut error_vec = Vec::new();
        let mut unnecessary_vec = Vec::new();
        let mut deprecated_vec = Vec::new();

        let push_diagnostic = |vec: &mut Vec<ops::Range<usize>>, range: Range| {
            // If any diagnostic overlaps ranges with the prior diagnostic,
            // merge the two together. Otherwise push a new span.
            match vec.last_mut() {
                Some(existing_range) if range.start <= existing_range.end => {
                    // This branch merges overlapping diagnostics, assuming that the current
                    // diagnostic starts on range.start or later. If this assertion fails,
                    // we will discard some part of `diagnostic`. This implies that
                    // `doc.diagnostics()` is not sorted by `diagnostic.range`.
                    debug_assert!(existing_range.start <= range.start);
                    existing_range.end = range.end.max(existing_range.end)
                }
                _ => vec.push(range.start..range.end),
            }
        };

        for diagnostic in doc.diagnostics() {
            // Separate diagnostics into different Vecs by severity.
            let vec = match diagnostic.severity {
                Some(Severity::Info) => &mut info_vec,
                Some(Severity::Hint) => &mut hint_vec,
                Some(Severity::Warning) => &mut warning_vec,
                Some(Severity::Error) => &mut error_vec,
                _ => &mut default_vec,
            };

            // If the diagnostic has tags and a non-warning/error severity, skip rendering
            // the diagnostic as info/hint/default and only render it as unnecessary/deprecated
            // instead. For warning/error diagnostics, render both the severity highlight and
            // the tag highlight.
            if diagnostic.tags.is_empty()
                || matches!(
                    diagnostic.severity,
                    Some(Severity::Warning | Severity::Error)
                )
            {
                push_diagnostic(vec, diagnostic.range);
            }

            for tag in &diagnostic.tags {
                match tag {
                    DiagnosticTag::Unnecessary => {
                        if unnecessary.is_some() {
                            push_diagnostic(&mut unnecessary_vec, diagnostic.range)
                        }
                    }
                    DiagnosticTag::Deprecated => {
                        if deprecated.is_some() {
                            push_diagnostic(&mut deprecated_vec, diagnostic.range)
                        }
                    }
                }
            }
        }

        overlay_highlights.push(OverlayHighlights::Homogeneous {
            highlight: get_scope_of("diagnostic"),
            ranges: default_vec,
        });
        if let Some(highlight) = unnecessary {
            overlay_highlights.push(OverlayHighlights::Homogeneous {
                highlight,
                ranges: unnecessary_vec,
            });
        }
        if let Some(highlight) = deprecated {
            overlay_highlights.push(OverlayHighlights::Homogeneous {
                highlight,
                ranges: deprecated_vec,
            });
        }
        overlay_highlights.extend([
            OverlayHighlights::Homogeneous {
                highlight: get_scope_of("diagnostic.info"),
                ranges: info_vec,
            },
            OverlayHighlights::Homogeneous {
                highlight: get_scope_of("diagnostic.hint"),
                ranges: hint_vec,
            },
            OverlayHighlights::Homogeneous {
                highlight: get_scope_of("diagnostic.warning"),
                ranges: warning_vec,
            },
            OverlayHighlights::Homogeneous {
                highlight: get_scope_of("diagnostic.error"),
                ranges: error_vec,
            },
        ]);
    }

    pub fn doc_document_highlights(
        doc: &Document,
        view: &View,
        theme: &Theme,
    ) -> Option<OverlayHighlights> {
        let ranges = doc.document_highlights(view.id)?;
        if ranges.is_empty() {
            return None;
        }

        let highlight = theme
            .find_highlight_exact("ui.highlight")
            .or_else(|| theme.find_highlight_exact("ui.selection"))
            .or_else(|| theme.find_highlight_exact("ui.cursor"))?;

        Some(OverlayHighlights::Homogeneous {
            highlight,
            ranges: ranges.to_vec(),
        })
    }

    pub fn doc_document_link_highlights(
        doc: &Document,
        theme: &Theme,
    ) -> Option<OverlayHighlights> {
        let highlight = theme
            .find_highlight_exact("markup.link.url")
            .or_else(|| theme.find_highlight_exact("markup.link"))?;

        if doc.document_links.is_empty() {
            return None;
        }

        let mut ranges: Vec<ops::Range<usize>> = Vec::new();
        for link in &doc.document_links {
            if link.start >= link.end {
                continue;
            }

            match ranges.last_mut() {
                Some(existing_range) if link.start <= existing_range.end => {
                    existing_range.end = existing_range.end.max(link.end);
                }
                _ => ranges.push(link.start..link.end),
            }
        }

        if ranges.is_empty() {
            return None;
        }

        Some(OverlayHighlights::Homogeneous { highlight, ranges })
    }

    /// Get highlight spans for selections in a document view.
    pub fn doc_selection_highlights(
        mode: Mode,
        doc: &Document,
        view: &View,
        theme: &Theme,
        cursor_shape_config: &CursorShapeConfig,
        is_terminal_focused: bool,
    ) -> OverlayHighlights {
        let text = doc.text().slice(..);
        let selection = doc.selection(view.id);
        let primary_idx = selection.primary_index();

        let cursorkind = cursor_shape_config.from_mode(mode);
        let cursor_is_block = cursorkind == CursorKind::Block;

        let selection_scope = theme
            .find_highlight_exact("ui.selection")
            .expect("could not find `ui.selection` scope in the theme!");
        let primary_selection_scope = theme
            .find_highlight_exact("ui.selection.primary")
            .unwrap_or(selection_scope);

        let base_cursor_scope = theme
            .find_highlight_exact("ui.cursor")
            .unwrap_or(selection_scope);
        let base_primary_cursor_scope = theme
            .find_highlight("ui.cursor.primary")
            .unwrap_or(base_cursor_scope);

        let cursor_scope = match mode {
            Mode::Insert => theme.find_highlight_exact("ui.cursor.insert"),
            Mode::Select => theme.find_highlight_exact("ui.cursor.select"),
            Mode::Normal => theme.find_highlight_exact("ui.cursor.normal"),
        }
        .unwrap_or(base_cursor_scope);

        let primary_cursor_scope = match mode {
            Mode::Insert => theme.find_highlight_exact("ui.cursor.primary.insert"),
            Mode::Select => theme.find_highlight_exact("ui.cursor.primary.select"),
            Mode::Normal => theme.find_highlight_exact("ui.cursor.primary.normal"),
        }
        .unwrap_or(base_primary_cursor_scope);

        let mut spans = Vec::new();
        for (i, range) in selection.iter().enumerate() {
            let selection_is_primary = i == primary_idx;
            let (cursor_scope, selection_scope) = if selection_is_primary {
                (primary_cursor_scope, primary_selection_scope)
            } else {
                (cursor_scope, selection_scope)
            };

            // Special-case: cursor at end of the rope.
            if range.head == range.anchor && range.head == text.len_chars() {
                if !selection_is_primary || (cursor_is_block && is_terminal_focused) {
                    // Bar and underline cursors are drawn by the terminal
                    // BUG: If the editor area loses focus while having a bar or
                    // underline cursor (eg. when a regex prompt has focus) then
                    // the primary cursor will be invisible. This doesn't happen
                    // with block cursors since we manually draw *all* cursors.
                    spans.push((cursor_scope, range.head..range.head + 1));
                }
                continue;
            }

            let range = range.min_width_1(text);
            if range.head > range.anchor {
                // Standard case.
                let cursor_start = prev_grapheme_boundary(text, range.head);
                // non block cursors look like they exclude the cursor
                let selection_end =
                    if selection_is_primary && !cursor_is_block && mode != Mode::Insert {
                        range.head
                    } else {
                        cursor_start
                    };
                spans.push((selection_scope, range.anchor..selection_end));
                // add block cursors
                // skip primary cursor if terminal is unfocused - terminal cursor is used in that case
                if !selection_is_primary || (cursor_is_block && is_terminal_focused) {
                    spans.push((cursor_scope, cursor_start..range.head));
                }
            } else {
                // Reverse case.
                let cursor_end = next_grapheme_boundary(text, range.head);
                // add block cursors
                // skip primary cursor if terminal is unfocused - terminal cursor is used in that case
                if !selection_is_primary || (cursor_is_block && is_terminal_focused) {
                    spans.push((cursor_scope, range.head..cursor_end));
                }
                // non block cursors look like they exclude the cursor
                let selection_start = if selection_is_primary
                    && !cursor_is_block
                    && !(mode == Mode::Insert && cursor_end == range.anchor)
                {
                    range.head
                } else {
                    cursor_end
                };
                spans.push((selection_scope, selection_start..range.anchor));
            }
        }

        OverlayHighlights::Heterogenous { highlights: spans }
    }

    /// Render brace match, etc (meant for the focused view only)
    pub fn highlight_focused_view_elements(
        view: &View,
        doc: &Document,
        theme: &Theme,
    ) -> Option<OverlayHighlights> {
        // Highlight matching braces
        let syntax = doc.syntax()?;
        let highlight = theme.find_highlight_exact("ui.cursor.match")?;
        let text = doc.text().slice(..);
        let pos = doc.selection(view.id).primary().cursor(text);
        let pos = helix_core::match_brackets::find_matching_bracket(syntax, text, pos)?;
        Some(OverlayHighlights::single(highlight, pos..pos + 1))
    }

    pub fn tabstop_highlights(doc: &Document, theme: &Theme) -> Option<OverlayHighlights> {
        let snippet = doc.active_snippet.as_ref()?;
        let highlight = theme.find_highlight_exact("tabstop")?;
        let mut ranges = Vec::new();
        for tabstop in snippet.tabstops() {
            ranges.extend(tabstop.ranges.iter().map(|range| range.start..range.end));
        }
        Some(OverlayHighlights::Homogeneous { highlight, ranges })
    }

    /// Render bufferline at the top
    pub fn render_bufferline(editor: &Editor, viewport: Rect, surface: &mut Surface) {
        let scratch = PathBuf::from(SCRATCH_BUFFER_NAME); // default filename to use for scratch buffer
        surface.clear_with(
            viewport,
            editor
                .theme
                .try_get("ui.bufferline.background")
                .unwrap_or_else(|| editor.theme.get("ui.statusline")),
        );

        let bufferline_active = editor
            .theme
            .try_get("ui.bufferline.active")
            .unwrap_or_else(|| editor.theme.get("ui.statusline.active"));

        let bufferline_inactive = editor
            .theme
            .try_get("ui.bufferline")
            .unwrap_or_else(|| editor.theme.get("ui.statusline.inactive"));

        let mut x = viewport.x;
        let current_doc = view!(editor).doc;

        for doc in editor.documents() {
            let fname = match (doc.path(), &doc.label) {
                (None, Some(label)) => label.as_str(),
                (path, _) => path
                    .unwrap_or(&scratch)
                    .file_name()
                    .unwrap_or_default()
                    .to_str()
                    .unwrap_or_default(),
            };

            let style = if current_doc == doc.id() {
                bufferline_active
            } else {
                bufferline_inactive
            };

            let text = format!(" {}{} ", fname, if doc.is_modified() { "[+]" } else { "" });
            let used_width = viewport.x.saturating_sub(x);
            let rem_width = surface.area.width.saturating_sub(used_width);

            x = surface
                .set_stringn(x, viewport.y, &text, rem_width as usize, style)
                .0;

            if x >= surface.area.right() {
                break;
            }
        }
    }

    pub fn render_gutter<'d>(
        editor: &'d Editor,
        doc: &'d Document,
        view: &View,
        viewport: Rect,
        theme: &Theme,
        is_focused: bool,
        decoration_manager: &mut DecorationManager<'d>,
    ) {
        let text = doc.text().slice(..);
        let cursors: Rc<[_]> = doc
            .selection(view.id)
            .iter()
            .map(|range| range.cursor_line(text))
            .collect();

        let mut offset = 0;

        let gutter_style = theme.get("ui.gutter");
        let gutter_selected_style = theme.get("ui.gutter.selected");
        let gutter_style_virtual = theme.get("ui.gutter.virtual");
        let gutter_selected_style_virtual = theme.get("ui.gutter.selected.virtual");

        for gutter_type in view.gutters() {
            let mut gutter = gutter_type.style(editor, doc, view, theme, is_focused);
            let width = gutter_type.width(view, doc);
            // avoid lots of small allocations by reusing a text buffer for each line
            let mut text = String::with_capacity(width);
            let cursors = cursors.clone();
            let gutter_decoration = move |renderer: &mut TextRenderer, pos: LinePos| {
                // TODO handle softwrap in gutters
                let selected = cursors.contains(&pos.doc_line);
                let x = viewport.x + offset;
                let y = pos.visual_line;

                let gutter_style = match (selected, pos.first_visual_line) {
                    (false, true) => gutter_style,
                    (true, true) => gutter_selected_style,
                    (false, false) => gutter_style_virtual,
                    (true, false) => gutter_selected_style_virtual,
                };

                if let Some(style) =
                    gutter(pos.doc_line, selected, pos.first_visual_line, &mut text)
                {
                    renderer.set_stringn(x, y, &text, width, gutter_style.patch(style));
                } else {
                    renderer.set_style(
                        Rect {
                            x,
                            y,
                            width: width as u16,
                            height: 1,
                        },
                        gutter_style,
                    );
                }
                text.clear();
            };
            decoration_manager.add_decoration(gutter_decoration);

            offset += width as u16;
        }
    }

    pub fn render_diagnostics(
        doc: &Document,
        view: &View,
        viewport: Rect,
        surface: &mut Surface,
        theme: &Theme,
    ) {
        use helix_core::diagnostic::Severity;
        use tui::{
            layout::Alignment,
            text::Text,
            widgets::{Paragraph, Widget, Wrap},
        };

        let cursor = doc
            .selection(view.id)
            .primary()
            .cursor(doc.text().slice(..));

        let diagnostics = doc.diagnostics().iter().filter(|diagnostic| {
            diagnostic.range.start <= cursor && diagnostic.range.end >= cursor
        });

        let warning = theme.get("warning");
        let error = theme.get("error");
        let info = theme.get("info");
        let hint = theme.get("hint");

        let mut lines = Vec::new();
        let background_style = theme.get("ui.background");
        for diagnostic in diagnostics {
            let style = Style::reset()
                .patch(background_style)
                .patch(match diagnostic.severity {
                    Some(Severity::Error) => error,
                    Some(Severity::Warning) | None => warning,
                    Some(Severity::Info) => info,
                    Some(Severity::Hint) => hint,
                });
            let text = Text::styled(&diagnostic.message, style);
            lines.extend(text.lines);
            let code = diagnostic.code.as_ref().map(|x| match x {
                NumberOrString::Number(n) => format!("({n})"),
                NumberOrString::String(s) => format!("({s})"),
            });
            if let Some(code) = code {
                let span = Span::styled(code, style);
                lines.push(span.into());
            }
        }

        let text = Text::from(lines);
        let paragraph = Paragraph::new(&text)
            .alignment(Alignment::Right)
            .wrap(Wrap { trim: true });
        let width = 100.min(viewport.width);
        let height = 15.min(viewport.height);
        paragraph.render(
            Rect::new(viewport.right() - width, viewport.y + 1, width, height),
            surface,
        );
    }

    /// Apply the highlighting on the lines where a cursor is active
    pub fn cursorline(doc: &Document, view: &View, theme: &Theme) -> impl Decoration {
        let text = doc.text().slice(..);
        // TODO only highlight the visual line that contains the cursor instead of the full visual line
        let primary_line = doc.selection(view.id).primary().cursor_line(text);

        // The secondary_lines do contain the primary_line, it doesn't matter
        // as the else-if clause in the loop later won't test for the
        // secondary_lines if primary_line == line.
        // It's used inside a loop so the collect isn't needless:
        // https://github.com/rust-lang/rust-clippy/issues/6164
        #[allow(clippy::needless_collect)]
        let secondary_lines: Vec<_> = doc
            .selection(view.id)
            .iter()
            .map(|range| range.cursor_line(text))
            .collect();

        let primary_style = theme.get("ui.cursorline.primary");
        let secondary_style = theme.get("ui.cursorline.secondary");
        let viewport = view.area;

        move |renderer: &mut TextRenderer, pos: LinePos| {
            let area = Rect::new(viewport.x, pos.visual_line, viewport.width, 1);
            if primary_line == pos.doc_line {
                renderer.set_style(area, primary_style);
            } else if secondary_lines.binary_search(&pos.doc_line).is_ok() {
                renderer.set_style(area, secondary_style);
            }
        }
    }

    /// Apply the highlighting on the columns where a cursor is active
    pub fn highlight_cursorcolumn(
        doc: &Document,
        view: &View,
        surface: &mut Surface,
        theme: &Theme,
        viewport: Rect,
        text_annotations: &TextAnnotations,
    ) {
        let text = doc.text().slice(..);

        // Manual fallback behaviour:
        // ui.cursorcolumn.{p/s} -> ui.cursorcolumn -> ui.cursorline.{p/s}
        let primary_style = theme
            .try_get_exact("ui.cursorcolumn.primary")
            .or_else(|| theme.try_get_exact("ui.cursorcolumn"))
            .unwrap_or_else(|| theme.get("ui.cursorline.primary"));
        let secondary_style = theme
            .try_get_exact("ui.cursorcolumn.secondary")
            .or_else(|| theme.try_get_exact("ui.cursorcolumn"))
            .unwrap_or_else(|| theme.get("ui.cursorline.secondary"));

        let inner_area = view.inner_area(doc);

        let selection = doc.selection(view.id);
        let view_offset = doc.view_offset(view.id);
        let primary = selection.primary();
        let text_format = doc.text_format(viewport.width, None);
        for range in selection.iter() {
            let is_primary = primary == *range;
            let cursor = range.cursor(text);

            let Position { col, .. } =
                visual_offset_from_block(text, cursor, cursor, &text_format, text_annotations).0;

            // if the cursor is horizontally in the view
            if col >= view_offset.horizontal_offset
                && inner_area.width > (col - view_offset.horizontal_offset) as u16
            {
                let area = Rect::new(
                    inner_area.x + (col - view_offset.horizontal_offset) as u16,
                    view.area.y,
                    1,
                    view.area.height,
                );
                if is_primary {
                    surface.set_style(area, primary_style)
                } else {
                    surface.set_style(area, secondary_style)
                }
            }
        }
    }

    /// Handle events by looking them up in `self.keymaps`. Returns None
    /// if event was handled (a command was executed or a subkeymap was
    /// activated). Only KeymapResult::{NotFound, Cancelled} is returned
    /// otherwise.
    fn handle_keymap_event(
        &mut self,
        mode: Mode,
        cxt: &mut commands::Context,
        event: KeyEvent,
    ) -> Option<KeymapResult> {
        let mut last_mode = mode;
        self.pseudo_pending.extend(self.keymaps.pending());
        let key_result = self.keymaps.get(mode, event);
        cxt.editor.autoinfo = self.keymaps.sticky().map(|node| node.infobox());

        let mut execute_command = |command: &commands::MappableCommand| {
            command.execute(cxt);
            helix_event::dispatch(PostCommand { command, cx: cxt });

            let current_mode = cxt.editor.mode();
            if current_mode != last_mode {
                helix_event::dispatch(OnModeSwitch {
                    old_mode: last_mode,
                    new_mode: current_mode,
                    cx: cxt,
                });

                // HAXX: if we just entered insert mode from normal, clear key buf
                // and record the command that got us into this mode.
                if current_mode == Mode::Insert {
                    // how we entered insert mode is important, and we should track that so
                    // we can repeat the side effect.
                    self.last_insert.0 = command.clone();
                    self.last_insert.1.clear();
                }
            }

            last_mode = current_mode;
        };

        match &key_result {
            KeymapResult::Matched(command) => {
                execute_command(command);
            }
            KeymapResult::Pending(node) => cxt.editor.autoinfo = Some(node.infobox()),
            KeymapResult::MatchedSequence(commands) => {
                for command in commands {
                    execute_command(command);
                }
            }
            KeymapResult::NotFound | KeymapResult::Cancelled(_) => return Some(key_result),
        }
        None
    }

    fn insert_mode(&mut self, cx: &mut commands::Context, event: KeyEvent) {
        if let Some(keyresult) = self.handle_keymap_event(Mode::Insert, cx, event) {
            match keyresult {
                KeymapResult::NotFound => {
                    if !self.on_next_key(OnKeyCallbackKind::Fallback, cx, event) {
                        if let Some(ch) = event.char() {
                            commands::insert::insert_char(cx, ch)
                        }
                    }
                }
                KeymapResult::Cancelled(pending) => {
                    for ev in pending {
                        match ev.char() {
                            Some(ch) => commands::insert::insert_char(cx, ch),
                            None => {
                                if let KeymapResult::Matched(command) =
                                    self.keymaps.get(Mode::Insert, ev)
                                {
                                    command.execute(cx);
                                }
                            }
                        }
                    }
                }
                _ => unreachable!(),
            }
        }
    }

    fn command_mode(&mut self, mode: Mode, cxt: &mut commands::Context, event: KeyEvent) {
        match (event, cxt.editor.count) {
            // If the count is already started and the input is a number, always continue the count.
            (key!(i @ '0'..='9'), Some(count)) => {
                let i = i.to_digit(10).unwrap() as usize;
                let count = count.get() * 10 + i;
                if count > 100_000_000 {
                    return;
                }
                cxt.editor.count = NonZeroUsize::new(count);
            }
            // A non-zero digit will start the count if that number isn't used by a keymap.
            (key!(i @ '1'..='9'), None) if !self.keymaps.contains_key(mode, event) => {
                let i = i.to_digit(10).unwrap() as usize;
                cxt.editor.count = NonZeroUsize::new(i);
            }
            // special handling for repeat operator
            (key!('.'), _) if self.keymaps.pending().is_empty() => {
                for _ in 0..cxt.editor.count.map_or(1, NonZeroUsize::into) {
                    // first execute whatever put us into insert mode
                    self.last_insert.0.execute(cxt);
                    let mut last_savepoint = None;
                    let mut last_request_savepoint = None;
                    // then replay the inputs
                    for key in self.last_insert.1.clone() {
                        match key {
                            InsertEvent::Key(key) => self.insert_mode(cxt, key),
                            InsertEvent::CompletionApply {
                                trigger_offset,
                                changes,
                            } => {
                                let (view, doc) = current!(cxt.editor);

                                if let Some(last_savepoint) = last_savepoint.as_deref() {
                                    doc.restore(view, last_savepoint, true);
                                }

                                let text = doc.text().slice(..);
                                let cursor = doc.selection(view.id).primary().cursor(text);

                                let shift_position = |pos: usize| -> usize {
                                    (pos + cursor).saturating_sub(trigger_offset)
                                };

                                let tx = Transaction::change(
                                    doc.text(),
                                    changes.iter().cloned().map(|(start, end, t)| {
                                        (shift_position(start), shift_position(end), t)
                                    }),
                                );
                                doc.apply(&tx, view.id);
                            }
                            InsertEvent::TriggerCompletion => {
                                last_savepoint = take(&mut last_request_savepoint);
                            }
                            InsertEvent::RequestCompletion => {
                                let (view, doc) = current!(cxt.editor);
                                last_request_savepoint = Some(doc.savepoint(view));
                            }
                        }
                    }
                }
                cxt.editor.count = None;
            }
            _ => {
                // set the count
                cxt.count = cxt.editor.count;
                // TODO: edge case: 0j -> reset to 1
                // if this fails, count was Some(0)
                // debug_assert!(cxt.count != 0);

                // set the register
                cxt.register = cxt.editor.selected_register.take();

                let res = self.handle_keymap_event(mode, cxt, event);
                if matches!(&res, Some(KeymapResult::NotFound)) {
                    self.on_next_key(OnKeyCallbackKind::Fallback, cxt, event);
                }
                if self.keymaps.pending().is_empty() {
                    cxt.editor.count = None
                } else {
                    cxt.editor.selected_register = cxt.register.take();
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn set_completion(
        &mut self,
        editor: &mut Editor,
        items: Vec<CompletionItem>,
        trigger_offset: usize,
        size: Rect,
    ) -> Option<Rect> {
        let mut completion = Completion::new(editor, items, trigger_offset);

        if completion.is_empty() {
            // skip if we got no completion results
            return None;
        }

        let area = completion.area(size, editor);
        editor.last_completion = Some(CompleteAction::Triggered);
        self.last_insert.1.push(InsertEvent::TriggerCompletion);

        // TODO : propagate required size on resize to completion too
        self.completion = Some(completion);
        Some(area)
    }

    pub fn clear_completion(&mut self, editor: &mut Editor) -> Option<OnKeyCallback> {
        self.completion = None;
        let mut on_next_key: Option<OnKeyCallback> = None;
        editor.handlers.completions.request_controller.restart();
        editor.handlers.completions.active_completions.clear();
        if let Some(last_completion) = editor.last_completion.take() {
            match last_completion {
                CompleteAction::Triggered => (),
                CompleteAction::Applied {
                    trigger_offset,
                    changes,
                    placeholder,
                } => {
                    self.last_insert.1.push(InsertEvent::CompletionApply {
                        trigger_offset,
                        changes,
                    });
                    on_next_key = placeholder.then_some(Box::new(|cx, key| {
                        if let Some(c) = key.char() {
                            let (view, doc) = current!(cx.editor);
                            if let Some(snippet) = &doc.active_snippet {
                                doc.apply(&snippet.delete_placeholder(doc.text()), view.id);
                            }
                            commands::insert::insert_char(cx, c);
                        }
                    }))
                }
                CompleteAction::Selected { savepoint } => {
                    let (view, doc) = current!(editor);
                    doc.restore(view, &savepoint, false);
                }
            }
        }
        on_next_key
    }

    pub fn handle_idle_timeout(&mut self, cx: &mut commands::Context) -> EventResult {
        commands::compute_inlay_hints_for_all_views(cx.editor, cx.jobs);

        EventResult::Ignored(None)
    }
}

/// Whether the focused doc's workspace is in restricted mode and running `trust` would
/// change something visible at the workspace level.
fn workspace_trust_indicator_visible(editor: &Editor) -> bool {
    if editor.workspace_trust.implicit_level()
        == helix_loader::workspace_trust::ImplicitTrustLevel::Insecure
    {
        return false;
    }
    let (_, doc) = helix_view::current_ref!(editor);
    editor
        .workspace_trust
        .restricted_for_doc(doc.workspace_root(), doc.servers_to_load())
}

impl EditorView {
    /// must be called whenever the editor processed input that
    /// is not a `KeyEvent`. In these cases any pending keys/on next
    /// key callbacks must be canceled.
    fn handle_non_key_input(&mut self, cxt: &mut commands::Context) {
        cxt.editor.status_msg = None;
        cxt.editor.reset_idle_timer();
        // HACKS: create a fake key event that will never trigger any actual map
        // and therefore simply acts as "dismiss"
        let null_key_event = KeyEvent {
            code: KeyCode::Null,
            modifiers: KeyModifiers::empty(),
        };
        // dismiss any pending keys
        if let Some((on_next_key, _)) = self.on_next_key.take() {
            on_next_key(cxt, null_key_event);
        }
        self.handle_keymap_event(cxt.editor.mode, cxt, null_key_event);
        self.pseudo_pending.clear();
    }

    fn handle_mouse_event(
        &mut self,
        event: &MouseEvent,
        cxt: &mut commands::Context,
    ) -> EventResult {
        if event.kind != MouseEventKind::Moved {
            self.handle_non_key_input(cxt)
        }

        let config = cxt.editor.config();
        let MouseEvent {
            kind,
            row,
            column,
            modifiers,
            ..
        } = *event;

        let pos_and_view = |editor: &Editor, row, column, ignore_virtual_text| {
            editor.tree.views().find_map(|(view, _focus)| {
                view.pos_at_screen_coords(
                    &editor.documents[&view.doc],
                    row,
                    column,
                    ignore_virtual_text,
                )
                .map(|pos| (pos, view.id))
            })
        };

        let gutter_coords_and_view = |editor: &Editor, row, column| {
            editor.tree.views().find_map(|(view, _focus)| {
                view.gutter_coords_at_screen_coords(row, column)
                    .map(|coords| (coords, view.id))
            })
        };

        match kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let editor = &mut cxt.editor;

                if let Some((pos, view_id)) = pos_and_view(editor, row, column, true) {
                    editor.focus(view_id);

                    let prev_view_id = view!(editor).id;
                    let doc = doc_mut!(editor, &view!(editor, view_id).doc);

                    if modifiers == KeyModifiers::ALT {
                        let selection = doc.selection(view_id).clone();
                        doc.set_selection(view_id, selection.push(Range::point(pos)));
                    } else if editor.mode == Mode::Select {
                        // Discards non-primary selections for consistent UX with normal mode
                        let primary = doc.selection(view_id).primary().put_cursor(
                            doc.text().slice(..),
                            pos,
                            true,
                        );
                        editor.mouse_down_range = Some(primary);
                        doc.set_selection(view_id, Selection::single(primary.anchor, primary.head));
                    } else {
                        doc.set_selection(view_id, Selection::point(pos));
                    }

                    if view_id != prev_view_id {
                        self.clear_completion(editor);
                    }

                    editor.ensure_cursor_in_view(view_id);

                    return EventResult::Consumed(None);
                }

                if let Some((coords, view_id)) = gutter_coords_and_view(editor, row, column) {
                    editor.focus(view_id);

                    let (view, doc) = current!(cxt.editor);

                    let Some(path) = doc.path().map(ToOwned::to_owned) else {
                        return EventResult::Ignored(None);
                    };

                    if let Some(char_idx) =
                        view.pos_at_visual_coords(doc, coords.row as u16, coords.col as u16, true)
                    {
                        let line = doc.text().char_to_line(char_idx);
                        commands::dap_toggle_breakpoint_impl(cxt, path, line);
                        return EventResult::Consumed(None);
                    }
                }

                EventResult::Ignored(None)
            }

            MouseEventKind::Drag(MouseButton::Left) => {
                let (view, doc) = current!(cxt.editor);

                let pos = match view.pos_at_screen_coords(doc, row, column, true) {
                    Some(pos) => pos,
                    None => return EventResult::Ignored(None),
                };

                let mut selection = doc.selection(view.id).clone();
                let primary = selection.primary_mut();
                *primary = primary.put_cursor(doc.text().slice(..), pos, true);
                doc.set_selection(view.id, selection);
                let view_id = view.id;
                cxt.editor.ensure_cursor_in_view(view_id);
                EventResult::Consumed(None)
            }

            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let current_view = cxt.editor.tree.focus;

                let direction = match event.kind {
                    MouseEventKind::ScrollUp => Direction::Backward,
                    MouseEventKind::ScrollDown => Direction::Forward,
                    _ => unreachable!(),
                };

                match pos_and_view(cxt.editor, row, column, false) {
                    Some((_, view_id)) => cxt.editor.tree.focus = view_id,
                    None => return EventResult::Ignored(None),
                }

                let offset = config.scroll_lines.unsigned_abs();
                commands::scroll(cxt, offset, direction, false);

                cxt.editor.tree.focus = current_view;
                cxt.editor.ensure_cursor_in_view(current_view);

                EventResult::Consumed(None)
            }

            MouseEventKind::Up(MouseButton::Left) => {
                if !config.middle_click_paste {
                    return EventResult::Ignored(None);
                }

                let (view, doc) = current!(cxt.editor);

                let should_yank = match cxt.editor.mouse_down_range.take() {
                    Some(down_range) => doc.selection(view.id).primary() != down_range,
                    None => {
                        // This should not happen under normal cases. We fall back to the original
                        // behavior of yanking on non-single-char selections.
                        doc.selection(view.id)
                            .primary()
                            .slice(doc.text().slice(..))
                            .len_chars()
                            > 1
                    }
                };

                if should_yank {
                    commands::yank_main_selection_to_register(
                        cxt.editor,
                        config.mouse_yank_register,
                    );
                    EventResult::Consumed(None)
                } else {
                    EventResult::Ignored(None)
                }
            }

            MouseEventKind::Up(MouseButton::Right) => {
                if let Some((pos, view_id)) = gutter_coords_and_view(cxt.editor, row, column) {
                    cxt.editor.focus(view_id);

                    if let Some((pos, _)) = pos_and_view(cxt.editor, row, column, true) {
                        doc_mut!(cxt.editor).set_selection(view_id, Selection::point(pos));
                    } else {
                        let (view, doc) = current!(cxt.editor);

                        if let Some(pos) = view.pos_at_visual_coords(doc, pos.row as u16, 0, true) {
                            doc.set_selection(view_id, Selection::point(pos));
                            match modifiers {
                                KeyModifiers::ALT => {
                                    commands::MappableCommand::dap_edit_log.execute(cxt)
                                }
                                _ => commands::MappableCommand::dap_edit_condition.execute(cxt),
                            };
                        }
                    }

                    cxt.editor.ensure_cursor_in_view(view_id);
                    return EventResult::Consumed(None);
                }
                EventResult::Ignored(None)
            }

            MouseEventKind::Up(MouseButton::Middle) => {
                let editor = &mut cxt.editor;
                if !config.middle_click_paste {
                    return EventResult::Ignored(None);
                }

                if modifiers == KeyModifiers::ALT {
                    commands::replace_selections_with_register(
                        cxt.editor,
                        config.mouse_yank_register,
                        cxt.count(),
                    );

                    return EventResult::Consumed(None);
                }

                if let Some((pos, view_id)) = pos_and_view(editor, row, column, true) {
                    let doc = doc_mut!(editor, &view!(editor, view_id).doc);
                    doc.set_selection(view_id, Selection::point(pos));
                    cxt.editor.focus(view_id);

                    commands::paste(
                        cxt.editor,
                        config.mouse_yank_register,
                        commands::Paste::Before,
                        cxt.count(),
                    );

                    return EventResult::Consumed(None);
                }

                EventResult::Ignored(None)
            }

            _ => EventResult::Ignored(None),
        }
    }
    fn on_next_key(
        &mut self,
        kind: OnKeyCallbackKind,
        ctx: &mut commands::Context,
        event: KeyEvent,
    ) -> bool {
        if let Some((on_next_key, kind_)) = self.on_next_key.take() {
            if kind == kind_ {
                on_next_key(ctx, event);
                true
            } else {
                self.on_next_key = Some((on_next_key, kind_));
                false
            }
        } else {
            false
        }
    }
}

impl EditorView {
    /// Shows the terminal panel, or hides it when focused. Only one panel holds focus at a time.
    pub fn toggle_terminal(&mut self, editor: &mut Editor) {
        self.zen = None;
        if let Err(err) = self.terminal.toggle() {
            editor.set_error(format!("cannot start terminal: {err}"));
        }
        if self.terminal.focused {
            self.focus_only(Panel::Terminal);
        }
    }

    /// Shows and focuses the terminal panel.
    pub fn toggle_terminal_focus(&mut self) {
        self.zen = None;
        self.terminal.visible = true;
        self.focus_only(Panel::Terminal);
    }

    /// Opens a terminal tab running `command` (a shell when empty) and focuses it.
    pub fn open_terminal(&mut self, command: &[String], name: Option<String>, editor: &mut Editor) {
        match self.terminal.open(command, name) {
            Ok(()) => self.focus_only(Panel::Terminal),
            Err(err) => editor.set_error(format!("cannot start terminal: {err}")),
        }
    }

    /// Shows the file tree on the current file, or hides it when focused. The tree and the git
    /// panel share the left sidebar.
    pub fn toggle_file_tree(&mut self, editor: &Editor) {
        self.zen = None;
        self.file_tree.toggle(editor);
        if self.file_tree.visible {
            self.git.visible = false;
        }
        if self.file_tree.focused {
            self.focus_only(Panel::Tree);
        }
    }

    /// Shows the git panel, or hides it when focused.
    pub fn toggle_git(&mut self) {
        self.zen = None;
        self.git.toggle();
        if self.git.visible {
            self.file_tree.visible = false;
        }
        if self.git.focused {
            self.focus_only(Panel::Git);
        }
    }

    /// Saves the project's editor tabs and splits (with cursors) and its panels, keeping the
    /// saved agent tabs, so the project reopens as it is now.
    pub fn save_workspace(&mut self, editor: &Editor) {
        let project = helix_stdx::env::current_working_dir();
        let mut workspace = agents::load_workspace(&project);
        let (layout, panels) = self.capture(editor);
        // Quitting closes the views before the last save; keep the layout they had.
        if layout.is_some() {
            workspace.editor = layout.clone();
        }
        workspace.panels = Some(panels.clone());
        workspace.opened = agents::now();
        agents::save_workspace(&workspace);
        self.saved_at = Some(std::time::Instant::now());
        self.saved = Some((layout, panels));
    }

    /// Saves the workspace if it changed since the last save: right away when `now`, at most
    /// every few seconds otherwise. Nothing is saved before a file is open.
    fn save_if_changed(&mut self, editor: &Editor, now: bool) {
        if cfg!(feature = "integration") || !editor.documents().any(|doc| doc.path().is_some()) {
            return;
        }
        let due = now
            || self
                .saved_at
                .is_none_or(|at| at.elapsed() > std::time::Duration::from_secs(3));
        if due && self.saved.as_ref() != Some(&self.capture(editor)) {
            self.save_workspace(editor);
        }
    }

    /// The editor's tabs and splits (with cursors), and the panels, as they are now.
    fn capture(&self, editor: &Editor) -> (Option<agents::EditorLayout>, agents::Panels) {
        let mut tabs = Vec::new();
        let mut active_tab = 0;
        for index in 0..editor.tree.tab_count() {
            let pane = editor
                .tree
                .tab_layout(index)
                .and_then(|node| capture_pane(editor, &node));
            if let Some(pane) = pane {
                if index == editor.tree.active_tab() {
                    active_tab = tabs.len();
                }
                tabs.push(pane);
            }
        }
        let layout = (!tabs.is_empty()).then_some(agents::EditorLayout { tabs, active_tab });
        let [tree, git, terminal, agents_shown] = self.zen.unwrap_or([
            self.file_tree.visible,
            self.git.visible,
            self.terminal.visible,
            self.agents.visible,
        ]);
        let panels = agents::Panels {
            tree,
            git,
            terminal,
            agents: agents_shown,
            sidebar_width: self.sidebar_width,
            terminal_height: self.terminal_height,
            agents_width: self.agents_width,
        };
        (layout, panels)
    }

    /// Reopens the project as it was saved: tabs, splits, cursors, panels and their sizes, and
    /// the agent tabs if the agent panel was open. Returns false when nothing was saved.
    pub fn restore_workspace(&mut self, editor: &mut Editor) -> bool {
        let project = helix_stdx::env::current_working_dir();
        let workspace = agents::load_workspace(&project);
        log::info!(
            "workspace: restoring {} (editor saved: {}, panels saved: {})",
            project.display(),
            workspace.editor.is_some(),
            workspace.panels.is_some()
        );
        if workspace.editor.is_none() && workspace.panels.is_none() {
            return false;
        }
        if let Some(layout) = &workspace.editor {
            let tabs: Vec<agents::Pane> = layout
                .tabs
                .iter()
                .cloned()
                .filter_map(agents::Pane::prune)
                .collect();
            let mut opened = 0;
            for pane in &tabs {
                let Some(first) = pane.first_file() else {
                    continue;
                };
                let action = if opened == 0 {
                    Action::Replace
                } else {
                    Action::NewTab
                };
                if editor.open(first, action).is_ok() {
                    fill_pane(editor, pane);
                    opened += 1;
                }
            }
            if opened > 0 {
                editor.switch_tab(layout.active_tab.min(opened - 1));
            }
        }
        if let Some(panels) = &workspace.panels {
            self.sidebar_width = panels.sidebar_width;
            self.terminal_height = panels.terminal_height;
            self.agents_width = panels.agents_width;
            if panels.git {
                self.git.visible = true;
                self.git.refresh();
            } else if panels.tree {
                self.file_tree.visible = true;
                self.file_tree.refresh(editor);
            }
            if panels.terminal {
                if let Err(err) = self.terminal.open(&[], None) {
                    editor.set_error(format!("cannot start terminal: {err}"));
                }
            }
            if panels.agents {
                for session in workspace.agents.clone() {
                    if let Err(err) = self.agents.open_agent(session) {
                        editor.set_error(format!("cannot start agent: {err}"));
                    }
                }
            }
        }
        self.agents_project = Some(project);
        self.focus_editor();
        true
    }

    /// F11: hides every panel and the frame, leaving the editor alone; again brings them back.
    pub fn toggle_zen(&mut self) {
        match self.zen.take() {
            Some([tree, git, terminal, agents]) => {
                self.file_tree.visible = tree;
                self.git.visible = git;
                self.terminal.visible = terminal;
                self.agents.visible = agents;
            }
            None => {
                self.zen = Some([
                    self.file_tree.visible,
                    self.git.visible,
                    self.terminal.visible,
                    self.agents.visible,
                ]);
                self.file_tree.visible = false;
                self.git.visible = false;
                self.terminal.visible = false;
                self.agents.visible = false;
                self.focus_editor();
            }
        }
    }

    /// Space l: opens the newest `path:line` printed in the terminal (or the agent panel when
    /// it is focused). Pressed again on the same output, it moves to the next older one.
    pub fn goto_location(&mut self, editor: &mut Editor) {
        let panel = if self.agents.focused || (self.terminal.is_empty() && !self.agents.is_empty())
        {
            &self.agents
        } else {
            &self.terminal
        };
        let found = panel.locations();
        if found.is_empty() {
            editor.set_status("No file locations in the terminal.");
            return;
        }
        let index = if found == self.locations.0 {
            (self.locations.1 + 1) % found.len()
        } else {
            0
        };
        let location = found[index].clone();
        self.locations = (found, index);
        self.open_location(&location, editor);
    }

    fn open_location(&mut self, location: &super::terminal::Location, editor: &mut Editor) {
        if let Err(err) = editor.open(&location.path, Action::Replace) {
            editor.set_error(format!("cannot open {}: {err}", location.path.display()));
            return;
        }
        let (view, doc) = current!(editor);
        let column = location.column.unwrap_or(1).saturating_sub(1);
        let pos = helix_core::pos_at_coords(
            doc.text().slice(..),
            helix_core::Position::new(location.line.saturating_sub(1), column),
            true,
        );
        doc.set_selection(view.id, helix_core::Selection::point(pos));
        helix_view::align_view(doc, view, helix_view::Align::Center);
        self.focus_editor();
    }

    /// Space >: sends the last command's output in the terminal to the agent being shown, as a
    /// fenced block, and focuses the agent.
    pub fn send_output_to_agent(&mut self, editor: &mut Editor) {
        if self.agents.active_agent().is_none() {
            editor.set_error("No agent is open. Press Ctrl+4 to start one.");
            return;
        }
        let Some(output) = self.terminal.last_output() else {
            editor.set_error("The terminal has no output to send.");
            return;
        };
        self.agents.paste(&format!("```\n{output}\n```\n"));
        self.agents.visible = true;
        self.focus_only(Panel::Agents);
    }

    /// Focuses `panel` and takes focus from the others.
    fn focus_only(&mut self, panel: Panel) {
        self.file_tree.focused = panel == Panel::Tree;
        self.git.focused = panel == Panel::Git;
        self.terminal.focused = panel == Panel::Terminal;
        self.agents.focused = panel == Panel::Agents;
    }

    /// Ctrl+4. Focused: hide. Hidden or unfocused with tabs: show and focus. Empty: reopen the
    /// project's saved agent tabs, or start an agent, asking which when both are installed.
    pub fn toggle_agents(&mut self, editor: &mut Editor) -> Option<Box<dyn Component>> {
        self.zen = None;
        if self.agents.visible && self.agents.focused {
            self.agents.visible = false;
            self.agents.focused = false;
            return None;
        }
        if !self.agents.is_empty() {
            self.agents.visible = true;
            self.focus_only(Panel::Agents);
            return None;
        }
        let project = helix_stdx::env::current_working_dir();
        if self.agents_project.as_ref() != Some(&project) {
            self.agents_project = Some(project.clone());
            let saved = agents::load(&project);
            if !saved.is_empty() {
                for session in saved {
                    if let Err(err) = self.agents.open_agent(session) {
                        editor.set_error(format!("cannot start agent: {err}"));
                    }
                }
                if !self.agents.is_empty() {
                    self.focus_only(Panel::Agents);
                }
                return None;
            }
        }
        match AgentKind::installed().as_slice() {
            [] => {
                editor.set_error("Neither claude nor opencode is on PATH.");
                None
            }
            [kind] => {
                self.open_agent(*kind, editor);
                None
            }
            _ => {
                // Just inside the editor box's top-right corner, where the panel will open.
                let anchor = self.editor_box;
                let position = Position::new(
                    anchor.y as usize + 1,
                    anchor.right().saturating_sub(16) as usize,
                );
                Some(Box::new(agent_menu(position)))
            }
        }
    }

    /// Opens a new tab running `kind` on a fresh session, and saves the project's agent tabs.
    pub fn open_agent(&mut self, kind: AgentKind, editor: &mut Editor) {
        match self.agents.open_agent(AgentSession::new(kind)) {
            Ok(()) => {
                self.focus_only(Panel::Agents);
                self.save_agents();
            }
            Err(err) => editor.set_error(format!("cannot start {}: {err}", kind.command())),
        }
    }

    fn save_agents(&mut self) {
        let project = helix_stdx::env::current_working_dir();
        agents::save(&project, &self.agents.agent_sessions());
        self.agents_project = Some(project);
    }

    /// Space @: types an @-mention of the current file (and selected lines) into the agent
    /// being shown, then focuses it.
    pub fn mention_in_agent(&mut self, editor: &mut Editor) {
        let Some(agent) = self.agents.active_agent().cloned() else {
            editor.set_error("No agent is open. Press Ctrl+4 to start one.");
            return;
        };
        let (view, doc) = current_ref!(editor);
        let Some(path) = doc.path() else {
            editor.set_error("This buffer has no file to mention.");
            return;
        };
        let cwd = helix_stdx::env::current_working_dir();
        let relative = path
            .strip_prefix(&cwd)
            .unwrap_or(path)
            .display()
            .to_string();
        let text = doc.text().slice(..);
        let range = doc.selection(view.id).primary();
        let lines = (range.len() > 1).then(|| {
            let (start, end) = range.line_range(text);
            (start + 1, end + 1)
        });
        self.agents.send_text(&agent.mention(&relative, lines));
        self.agents.visible = true;
        self.focus_only(Panel::Agents);
    }

    /// Splits `frame` into the sidebar box, the editor box and the terminal box. The editor and
    /// terminal boxes share their divider row.
    fn layout_frame(&mut self, frame: Rect) {
        self.sidebar_box = None;
        self.terminal_box = None;
        self.agents_box = None;
        if self.zen.is_some() {
            self.main_box = frame;
            self.editor_box = frame;
            return;
        }
        let mut main = frame;
        if self.git.visible || self.file_tree.visible {
            let (min, max) = SIDEBAR_DEFAULT;
            let width = self
                .sidebar_width
                .unwrap_or((frame.width / 5).clamp(min, max))
                .min(frame.width.saturating_sub(EDITOR_MIN_WIDTH))
                .max(SIDEBAR_MIN);
            if width + EDITOR_MIN_WIDTH <= frame.width {
                self.sidebar_box = Some(frame.with_width(width));
                main = frame.clip_left(width);
            }
        }
        self.agents_box = None;
        if self.agents.visible && !self.agents.is_empty() {
            let width = self
                .agents_width
                .unwrap_or((frame.width * 2 / 5).clamp(AGENTS_DEFAULT.0, AGENTS_DEFAULT.1))
                .min(main.width.saturating_sub(EDITOR_MIN_WIDTH))
                .max(AGENTS_MIN);
            if width + EDITOR_MIN_WIDTH <= main.width {
                self.agents_box = Some(Rect::new(main.right() - width, main.y, width, main.height));
                main = main.with_width(main.width - width);
            }
        }
        self.main_box = main;
        self.editor_box = main;
        if self.terminal.visible && !self.terminal.is_empty() {
            // Rows inside the main box, less the divider.
            let rows = main.height.saturating_sub(3);
            let height = self
                .terminal_height
                .unwrap_or(rows * TERMINAL_PERCENT / 100)
                .min(rows.saturating_sub(EDITOR_MIN_HEIGHT))
                .max(TERMINAL_MIN);
            if height + EDITOR_MIN_HEIGHT <= rows {
                let divider = main.bottom() - 2 - height;
                self.editor_box = Rect::new(main.x, main.y, main.width, divider - main.y + 1);
                self.terminal_box = Some(Rect::new(
                    main.x,
                    divider,
                    main.width,
                    main.bottom() - divider,
                ));
            }
        }
    }

    /// Draws the boxes, their titles and meta, and the editor's scrollbar. The focused box is
    /// drawn last, in the accent, so the divider it shares takes its colour.
    fn render_frame(&mut self, editor: &Editor, surface: &mut Surface, editor_focused: bool) {
        let roles = Roles::new(&editor.theme);
        let border = |focused: bool| if focused { roles.accent } else { roles.muted };

        if let Some(sidebar) = self.sidebar_box {
            let focused = self.git.focused || self.file_tree.focused;
            frame::draw_box(surface, sidebar, border(focused), End::Corner, End::Corner);
            let (title, meta, meta_style) = if self.git.visible {
                let (meta, busy) = self.git.meta();
                ("GIT", meta, if busy { roles.warning } else { roles.muted })
            } else {
                ("FILES", self.file_tree.root_name(), roles.muted)
            };
            let (left, right) = (sidebar.x, sidebar.right() - 1);
            let title_hits = frame::labels_left(
                surface,
                sidebar.y,
                left,
                right,
                &[Label::new(title, roles.label())],
            );
            let title_end = title_hits.first().map_or(left, |&(_, end)| end);
            if !meta.is_empty() {
                let room = right.saturating_sub(title_end + 3) as usize;
                let meta = shorten_end(&meta, room);
                frame::labels_right(
                    surface,
                    sidebar.y,
                    title_end,
                    right,
                    &[Label::new(meta, meta_style)],
                );
            }
            if let Some(footer) = self.git.visible.then(|| self.git.footer()).flatten() {
                frame::labels_right(
                    surface,
                    sidebar.bottom() - 1,
                    left,
                    right,
                    &[Label::new(footer, roles.faint)],
                );
            }
        }

        let editor_bottom = if self.terminal_box.is_some() {
            End::Tee
        } else {
            End::Corner
        };
        let draw_editor = |surface: &mut Surface| {
            frame::draw_box(
                surface,
                self.editor_box,
                border(editor_focused),
                End::Corner,
                editor_bottom,
            )
        };
        let draw_terminal = |surface: &mut Surface| {
            if let Some(terminal_box) = self.terminal_box {
                let focused = self.terminal.focused;
                frame::draw_box(
                    surface,
                    terminal_box,
                    border(focused),
                    End::Tee,
                    End::Corner,
                );
            }
        };
        if self.terminal.focused {
            draw_editor(surface);
            draw_terminal(surface);
        } else {
            draw_terminal(surface);
            draw_editor(surface);
        }
        if let Some(agents_box) = self.agents_box {
            frame::draw_box(
                surface,
                agents_box,
                border(self.agents.focused),
                End::Corner,
                End::Corner,
            );
        }
        self.render_editor_labels(editor, surface, &roles);
        self.render_lsp_status(surface, &roles);

        // The focused view's position in its document, on the editor box's right border.
        let (view, doc) = current_ref!(editor);
        let text = doc.text();
        let total = text.len_lines();
        let offset = text.char_to_line(doc.view_offset(view.id).anchor.min(text.len_chars()));
        let visible = view.inner_height();
        let editor_box = self.editor_box;
        frame::scrollbar(
            surface,
            editor_box.right() - 1,
            editor_box.y + 1,
            editor_box.bottom() - 1,
            (total, offset, visible),
            border(editor_focused),
        );
    }

    /// Language servers of the current document, on the editor box's bottom border:
    /// `rust-analyzer ✓`, `rust-analyzer ⟳ indexing 40%`, `starting…`.
    fn render_lsp_status(&self, surface: &mut Surface, roles: &Roles) {
        if self.lsp_summary.is_empty() {
            return;
        }
        let bottom = self.editor_box.bottom() - 1;
        let labels: Vec<Label> = self
            .lsp_summary
            .iter()
            .map(|(name, state)| {
                let style = match state.chars().next() {
                    Some('✓') => roles.muted,
                    Some('✗') => roles.error,
                    _ => roles.warning,
                };
                Label::new(format!("{name} {state}"), style)
            })
            .collect();
        frame::labels_right(
            surface,
            bottom,
            self.editor_box.x,
            self.editor_box.right() - 1,
            &labels,
        );
    }

    /// Buffers on the left of the editor box's top border, tabs on the right. Both are clickable.
    fn render_editor_labels(&mut self, editor: &Editor, surface: &mut Surface, roles: &Roles) {
        self.tab_hits.clear();
        self.buffer_hits.clear();
        let row = self.editor_box;
        let (left, right) = (row.x, row.right() - 1);

        let tab_count = editor.tree.tab_count();
        // The active file and layout are filled chips, so they stand out on a focused (accent)
        // border as well as on a muted one.
        let chip = roles
            .selected
            .patch(roles.accent)
            .add_modifier(Modifier::BOLD);
        // Layouts: whole arrangements of splits, numbered on the right after HELIDE when there are
        // several. Files are the tabs on the left.
        let tab_labels: Vec<Label> = if tab_count > 1 {
            std::iter::once(Label::new("HELIDE", roles.label()))
                .chain((0..tab_count).map(|index| {
                    let style = if index == editor.tree.active_tab() {
                        chip
                    } else {
                        roles.muted
                    };
                    Label::new((index + 1).to_string(), style)
                }))
                .collect()
        } else {
            Vec::new()
        };
        let tabs_width: u16 = tab_labels
            .iter()
            .map(|label| label.text.width() as u16 + 2)
            .sum();
        // The first label is HELIDE; the rest are the layouts.
        for (start, end) in frame::labels_right(surface, row.y, left, right, &tab_labels)
            .into_iter()
            .skip(1)
        {
            self.tab_hits.push((row.y, start, end));
        }

        let current = view!(editor).doc;
        let docs: Vec<(DocumentId, Label)> = editor
            .documents()
            .map(|doc| {
                let mut name = doc
                    .path()
                    .and_then(|path| path.file_name())
                    .map(|name| name.to_string_lossy().into_owned())
                    .or_else(|| doc.label.clone())
                    .unwrap_or_else(|| SCRATCH_BUFFER_NAME.to_string());
                if doc.is_modified() {
                    name.push_str(" ●");
                }
                // The trailing × closes the file.
                name.push_str(" ×");
                let style = if doc.id() == current {
                    chip
                } else {
                    roles.muted
                };
                (doc.id(), Label::new(name, style))
            })
            .collect();
        let end = right.saturating_sub(tabs_width + 1);
        let (ids, labels): (Vec<DocumentId>, Vec<Label>) = docs.into_iter().unzip();
        let hits = frame::labels_left(surface, row.y, left, end, &labels);
        self.buffer_hits = hits
            .into_iter()
            .zip(ids)
            .map(|((start, end), id)| (start, end, id))
            .collect();
    }

    /// A click on a tab's ×: closes the file, terminal tab or agent tab. Returns whether it was
    /// one.
    fn close_click(&mut self, mouse: &MouseEvent, editor: &mut Editor) -> bool {
        let (column, row) = (mouse.column, mouse.row);
        if self.zen.is_none() && row == self.editor_box.y {
            if let Some(&(_, _, doc)) = self
                .buffer_hits
                .iter()
                .find(|&&(_, end, _)| end >= 2 && column == end - 2)
            {
                if editor.close_document(doc, false).is_err() {
                    editor
                        .set_error("This file has unsaved changes. Save it, or close it with :bc!");
                }
                return true;
            }
        }
        if self.terminal_box.is_some_and(|b| row == b.y) {
            if let Some(index) = self.terminal.close_at(column) {
                self.terminal.close_tab(index);
                return true;
            }
        }
        if self.agents_box.is_some_and(|b| row == b.y) {
            if let Some(index) = self.agents.close_at(column) {
                self.agents.close_tab(index);
                self.save_agents();
                return true;
            }
        }
        false
    }

    /// What a left press at (column, row) takes hold of, if anything.
    fn grab_at(&self, column: u16, row: u16, editor: &Editor) -> Option<(Grab, String)> {
        let within = |rect: Rect| column >= rect.x && column < rect.right();
        if self.zen.is_none() && row == self.editor_box.y && within(self.editor_box) {
            if let Some(&(_, _, doc)) = self
                .buffer_hits
                .iter()
                .find(|&&(start, end, _)| (start..end).contains(&column))
            {
                let name = editor
                    .document(doc)
                    .map(|doc| doc.display_name().into_owned())
                    .unwrap_or_default();
                return Some((Grab::Buffer(doc), name));
            }
            if let Some(index) = self
                .tab_hits
                .iter()
                .position(|&(y, start, end)| y == row && (start..end).contains(&column))
            {
                return Some((Grab::EditorTab(index), format!("tab {}", index + 1)));
            }
        }
        if let Some(terminal) = self.terminal_box {
            if row == terminal.y && within(terminal) {
                return Some(match self.terminal.tab_at(column) {
                    Some(index) => (
                        Grab::PanelTab(false, index),
                        format!("terminal {}", index + 1),
                    ),
                    None => (Grab::Edge(Edge::Terminal), String::new()),
                });
            }
        }
        if let Some(agents) = self.agents_box {
            if row == agents.y && within(agents) {
                if let Some(index) = self.agents.tab_at(column) {
                    return Some((Grab::PanelTab(true, index), format!("agent {}", index + 1)));
                }
            }
            if (column == agents.x || column + 1 == agents.x)
                && row >= agents.y
                && row < agents.bottom()
            {
                return Some((Grab::Edge(Edge::Agents), String::new()));
            }
        }
        if let Some(sidebar) = self.sidebar_box {
            if (column == sidebar.right() - 1 || column == sidebar.right())
                && row >= sidebar.y
                && row < sidebar.bottom()
            {
                return Some((Grab::Edge(Edge::Sidebar), String::new()));
            }
        }
        None
    }

    /// Presses, drags and releases on labels and panel edges. Returns whether the event was used.
    fn drag(&mut self, mouse: &MouseEvent, editor: &mut Editor) -> bool {
        match mouse.kind {
            // Middle-click closes a file tab.
            MouseEventKind::Down(MouseButton::Middle) => {
                let Some((Grab::Buffer(doc), _)) = self.grab_at(mouse.column, mouse.row, editor)
                else {
                    return false;
                };
                if editor.close_document(doc, false).is_err() {
                    editor
                        .set_error("This file has unsaved changes. Save it, or close it with :bc!");
                }
                true
            }
            MouseEventKind::Down(MouseButton::Left) if self.close_click(mouse, editor) => true,
            MouseEventKind::Down(MouseButton::Left) => {
                let Some((grab, label)) = self.grab_at(mouse.column, mouse.row, editor) else {
                    return false;
                };
                self.pressed = Some(Press {
                    grab,
                    column: mouse.column,
                    row: mouse.row,
                    moved: false,
                    label,
                });
                self.drag_at = (mouse.column, mouse.row);
                true
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some(press) = self.pressed.as_mut() else {
                    return false;
                };
                if (mouse.column, mouse.row) != (press.column, press.row) {
                    press.moved = true;
                }
                self.drag_at = (mouse.column, mouse.row);
                match press.grab {
                    Grab::Edge(Edge::Sidebar) => {
                        let left = self.sidebar_box.map_or(0, |sidebar| sidebar.x);
                        self.sidebar_width = Some(mouse.column.saturating_sub(left) + 1);
                    }
                    Grab::Edge(Edge::Agents) => {
                        let right = self.agents_box.map_or(0, |agents| agents.right());
                        self.agents_width = Some(right.saturating_sub(mouse.column));
                    }
                    Grab::Edge(Edge::Terminal) => {
                        // The divider follows the mouse: the rows below it, less the border.
                        let bottom = self.main_box.bottom();
                        self.terminal_height = Some(bottom.saturating_sub(mouse.row + 2));
                    }
                    _ => {}
                }
                true
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some(press) = self.pressed.take() else {
                    return false;
                };
                self.release(press, mouse.column, mouse.row, editor);
                true
            }
            _ => false,
        }
    }

    fn release(&mut self, press: Press, column: u16, row: u16, editor: &mut Editor) {
        match press.grab {
            Grab::Edge(_) => {}
            Grab::Buffer(doc) if !press.moved => {
                editor.switch(doc, Action::Replace);
                self.focus_editor();
            }
            Grab::Buffer(doc) => {
                if let Some((view, action, _, swap)) = self.drop_target(column, row, editor) {
                    editor.focus(view);
                    editor.switch(doc, action);
                    // Helix puts a new split to the right or below; a drop on the left or
                    // top half moves it there.
                    if let Some(direction) = swap {
                        editor.tree.swap_split_in_direction(direction);
                    }
                    self.focus_editor();
                }
            }
            Grab::EditorTab(index) if !press.moved => {
                editor.switch_tab(index);
                self.focus_editor();
            }
            Grab::EditorTab(from) => {
                if let Some(to) = self
                    .tab_hits
                    .iter()
                    .position(|&(y, start, end)| y == row && (start..end).contains(&column))
                {
                    editor.tree.move_tab(from, to);
                }
            }
            Grab::PanelTab(agents, index) => {
                let (panel, strip) = if agents {
                    (&mut self.agents, self.agents_box.map(|b| b.y))
                } else {
                    (&mut self.terminal, self.terminal_box.map(|b| b.y))
                };
                if !press.moved {
                    panel.select(index);
                } else if Some(row) == strip {
                    if let Some(to) = panel.tab_at(column) {
                        panel.move_tab(index, to);
                    }
                } else if panel.contains(column, row) {
                    if panel.shown_tab() == Some(index) && !agents {
                        // The shown tab dropped on itself: a new shell beside it.
                        if let Err(err) = panel.split(&[], None) {
                            editor.set_error(format!("cannot start terminal: {err}"));
                        }
                    } else {
                        panel.merge_into_shown(index);
                    }
                }
                self.focus_only(if agents {
                    Panel::Agents
                } else {
                    Panel::Terminal
                });
                if agents {
                    self.save_agents();
                }
            }
        }
    }

    /// Where a buffer dropped at (column, row) goes: the view under it, and a vertical split
    /// near its left or right edge, a horizontal split near its top or bottom, or a
    /// replacement in the middle. Also the area that will change, for the drop hint.
    fn drop_target(
        &self,
        column: u16,
        row: u16,
        editor: &Editor,
    ) -> Option<(ViewId, Action, Rect, Option<helix_view::tree::Direction>)> {
        use helix_view::tree::Direction;
        let (view, _) = editor.tree.views().find(|(view, _)| {
            let area = view.area;
            column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
        })?;
        let area = view.area;
        let (dx, dy) = (column - area.x, row - area.y);
        let (w, h) = (area.width.max(1), area.height.max(1));
        let (half_w, half_h) = (area.width / 2, area.height / 2);
        let left = Rect::new(area.x, area.y, half_w, area.height);
        let right = Rect::new(area.x + half_w, area.y, area.width - half_w, area.height);
        let top = Rect::new(area.x, area.y, area.width, half_h);
        let bottom = Rect::new(area.x, area.y + half_h, area.width, area.height - half_h);
        // The quarter of the view the mouse is in decides the side.
        Some(if dx < w / 4 {
            (view.id, Action::VerticalSplit, left, Some(Direction::Left))
        } else if dx >= w * 3 / 4 {
            (view.id, Action::VerticalSplit, right, None)
        } else if dy < h / 4 {
            (view.id, Action::HorizontalSplit, top, Some(Direction::Up))
        } else if dy >= h * 3 / 4 {
            (view.id, Action::HorizontalSplit, bottom, None)
        } else {
            (view.id, Action::Replace, area, None)
        })
    }

    /// While dragging a label: the label follows the mouse, and a buffer shows where it will land.
    fn render_drag(&self, editor: &Editor, surface: &mut Surface) {
        let Some(press) = self.pressed.as_ref().filter(|press| press.moved) else {
            return;
        };
        if matches!(press.grab, Grab::Edge(_)) {
            return;
        }
        let roles = Roles::new(&editor.theme);
        let (column, row) = self.drag_at;
        match press.grab {
            Grab::Buffer(_) => {
                if let Some((_, _, area, _)) = self.drop_target(column, row, editor) {
                    frame::draw_box(surface, area, roles.accent, End::Corner, End::Corner);
                }
            }
            // A tab over its panel's contents joins it as a pane: outline the panel.
            Grab::PanelTab(agents, _) => {
                let (panel, area) = if agents {
                    (&self.agents, self.agents_box)
                } else {
                    (&self.terminal, self.terminal_box)
                };
                if let Some(area) = area.filter(|area| row > area.y && panel.contains(column, row))
                {
                    let inside = Rect::new(
                        area.x + 1,
                        area.y + 1,
                        area.width.saturating_sub(2),
                        area.height.saturating_sub(2),
                    );
                    frame::draw_box(surface, inside, roles.accent, End::Corner, End::Corner);
                }
            }
            _ => {}
        }
        let ghost = roles
            .selected
            .patch(roles.accent)
            .add_modifier(Modifier::BOLD);
        surface.set_stringn(
            column.saturating_add(1),
            row,
            &format!(" {} ", press.label),
            surface.area.right().saturating_sub(column + 1) as usize,
            ghost,
        );
    }

    /// Alt+Shift+arrows: left and right resize the sidebar, up and down the terminal.
    fn resize_with_key(&mut self, key: &KeyEvent) -> bool {
        if key.modifiers != KeyModifiers::ALT | KeyModifiers::SHIFT {
            return false;
        }
        // With the agents focused, left and right resize the agent panel.
        if self.agents.focused {
            if let Some(width) = self.agents_box.map(|agents| agents.width) {
                match key.code {
                    KeyCode::Left => self.agents_width = Some(width + 2),
                    KeyCode::Right => self.agents_width = Some(width.saturating_sub(2)),
                    _ => {}
                }
            }
        }
        let sidebar = self
            .sidebar_box
            .filter(|_| !self.agents.focused)
            .map(|sidebar| sidebar.width);
        let terminal = self
            .terminal_box
            .map(|terminal| terminal.height.saturating_sub(2));
        match (key.code, sidebar, terminal) {
            (KeyCode::Left, Some(width), _) => self.sidebar_width = Some(width.saturating_sub(2)),
            (KeyCode::Right, Some(width), _) => self.sidebar_width = Some(width + 2),
            (KeyCode::Up, _, Some(height)) => self.terminal_height = Some(height + 1),
            (KeyCode::Down, _, Some(height)) => {
                self.terminal_height = Some(height.saturating_sub(1))
            }
            _ => return false,
        }
        true
    }

    fn focus_editor(&mut self) {
        self.focus_only(Panel::None);
    }

    fn git_action(&mut self, action: GitAction) -> EventResult {
        match action {
            GitAction::None => {}
            GitAction::FocusEditor => self.focus_editor(),
            GitAction::Hide => {
                self.git.visible = false;
                self.git.focused = false;
            }
            GitAction::Layer(layer) => {
                return EventResult::Consumed(Some(Box::new(move |compositor, _| {
                    compositor.push(layer)
                })));
            }
        }
        EventResult::Consumed(None)
    }

    fn tree_action(&mut self, action: TreeAction, editor: &Editor) -> EventResult {
        match action {
            TreeAction::None => {}
            TreeAction::FocusEditor => self.focus_editor(),
            TreeAction::Hide => {
                self.file_tree.visible = false;
                self.file_tree.focused = false;
            }
            action => {
                if let Some(prompt) = file_tree::prompt(&action, editor) {
                    return EventResult::Consumed(Some(Box::new(move |compositor, _| {
                        compositor.push(prompt)
                    })));
                }
            }
        }
        EventResult::Consumed(None)
    }

    /// Routes input to a focused panel, and mouse input to whichever panel it lands on.
    /// Returns `None` when the event belongs to the editor.
    fn handle_panel_event(
        &mut self,
        event: &Event,
        cx: &mut crate::compositor::Context,
    ) -> Option<EventResult> {
        match event {
            Event::Key(key) if self.resize_with_key(key) => Some(EventResult::Consumed(None)),
            Event::Mouse(mouse) if self.drag(mouse, cx.editor) => Some(EventResult::Consumed(None)),
            Event::Mouse(mouse) if self.file_tree.contains(mouse.column, mouse.row) => {
                if let MouseEventKind::Down(_) = mouse.kind {
                    self.focus_only(Panel::Tree);
                }
                let action = self.file_tree.mouse(mouse, cx.editor);
                Some(self.tree_action(action, cx.editor))
            }
            Event::Mouse(mouse) if self.git.contains(mouse.column, mouse.row) => {
                if let MouseEventKind::Down(_) = mouse.kind {
                    self.focus_only(Panel::Git);
                }
                let action = self.git.mouse(mouse);
                Some(self.git_action(action))
            }
            Event::Mouse(mouse) if self.terminal.contains(mouse.column, mouse.row) => {
                if let MouseEventKind::Down(_) = mouse.kind {
                    self.focus_only(Panel::Terminal);
                }
                if let Some(location) = self.terminal.mouse(mouse) {
                    self.open_location(&location, cx.editor);
                }
                Some(EventResult::Consumed(None))
            }
            Event::Mouse(mouse) if self.agents.contains(mouse.column, mouse.row) => {
                if let MouseEventKind::Down(_) = mouse.kind {
                    self.focus_only(Panel::Agents);
                }
                if let Some(location) = self.agents.mouse(mouse) {
                    self.open_location(&location, cx.editor);
                }
                Some(EventResult::Consumed(None))
            }
            Event::Mouse(mouse) => {
                if let MouseEventKind::Down(_) = mouse.kind {
                    self.focus_editor();
                }
                None
            }
            Event::Key(key)
                if self.terminal.focused
                    || self.file_tree.focused
                    || self.git.focused
                    || self.agents.focused =>
            {
                cx.editor.reset_idle_timer();
                let ctrl = key.modifiers == KeyModifiers::CONTROL;
                match key.code {
                    KeyCode::Char('1') if ctrl => self.toggle_git(),
                    KeyCode::Char('2') if ctrl => self.focus_editor(),
                    KeyCode::Char('3') if ctrl => self.toggle_terminal(cx.editor),
                    KeyCode::Char('4') if ctrl => {
                        if let Some(picker) = self.toggle_agents(cx.editor) {
                            return Some(EventResult::Consumed(Some(Box::new(
                                move |compositor, _| compositor.push(picker),
                            ))));
                        }
                    }
                    KeyCode::Char('5') if ctrl => self.toggle_file_tree(cx.editor),
                    _ if self.file_tree.focused => {
                        let action = self.file_tree.key(*key, cx.editor);
                        return Some(self.tree_action(action, cx.editor));
                    }
                    _ if self.git.focused => {
                        let action = self.git.key(*key, cx.editor);
                        return Some(self.git_action(action));
                    }
                    KeyCode::PageUp if ctrl && self.agents.focused => self.agents.cycle(false),
                    KeyCode::PageDown if ctrl && self.agents.focused => self.agents.cycle(true),
                    _ if self.agents.focused => self.agents.key(*key),
                    KeyCode::Char('\\')
                        if key.modifiers == KeyModifiers::ALT && self.terminal.focused =>
                    {
                        if let Err(err) = self.terminal.split(&[], None) {
                            cx.editor.set_error(format!("cannot start terminal: {err}"));
                        }
                    }
                    KeyCode::PageUp if ctrl => self.terminal.cycle(false),
                    KeyCode::PageDown if ctrl => self.terminal.cycle(true),
                    _ => self.terminal.key(*key),
                }
                Some(EventResult::Consumed(None))
            }
            Event::Paste(text) if self.agents.focused => {
                self.agents.paste(text);
                Some(EventResult::Consumed(None))
            }
            Event::Paste(text) if self.terminal.focused => {
                self.terminal.paste(text);
                Some(EventResult::Consumed(None))
            }
            _ => None,
        }
    }
}

impl Component for EditorView {
    fn handle_event(
        &mut self,
        event: &Event,
        context: &mut crate::compositor::Context,
    ) -> EventResult {
        if let Some(result) = self.handle_panel_event(event, context) {
            return result;
        }

        let mut cx = commands::Context {
            editor: context.editor,
            count: None,
            register: None,
            callback: Vec::new(),
            on_next_key_callback: None,
            jobs: context.jobs,
        };

        match event {
            Event::Paste(contents) => {
                self.handle_non_key_input(&mut cx);
                cx.count = cx.editor.count;
                commands::paste_bracketed_value(&mut cx, contents.clone());
                cx.editor.count = None;

                let config = cx.editor.config();
                let mode = cx.editor.mode();
                let (view, doc) = current!(cx.editor);
                view.ensure_cursor_in_view(doc, config.scrolloff);

                // Store a history state if not in insert mode. Otherwise wait till we exit insert
                // to include any edits to the paste in the history state.
                if mode != Mode::Insert {
                    doc.append_changes_to_history(view);
                }

                EventResult::Consumed(None)
            }
            Event::Resize(_width, _height) => {
                // Ignore this event, we handle resizing just before rendering to screen.
                // Handling it here but not re-rendering will cause flashing
                EventResult::Consumed(None)
            }
            Event::Key(mut key) => {
                cx.editor.reset_idle_timer();
                canonicalize_key(&mut key);

                // clear status
                cx.editor.status_msg = None;

                let mode = cx.editor.mode();

                if !self.on_next_key(OnKeyCallbackKind::PseudoPending, &mut cx, key) {
                    match mode {
                        Mode::Insert => {
                            // let completion swallow the event if necessary
                            let mut consumed = false;
                            if let Some(completion) = &mut self.completion {
                                let res = {
                                    // use a fake context here
                                    let mut cx = Context {
                                        editor: cx.editor,
                                        jobs: cx.jobs,
                                        scroll: None,
                                    };

                                    if let EventResult::Consumed(callback) =
                                        completion.handle_event(event, &mut cx)
                                    {
                                        consumed = true;
                                        Some(callback)
                                    } else if let EventResult::Consumed(callback) =
                                        completion.handle_event(&Event::Key(key!(Enter)), &mut cx)
                                    {
                                        Some(callback)
                                    } else {
                                        None
                                    }
                                };

                                if let Some(callback) = res {
                                    if callback.is_some() {
                                        // assume close_fn
                                        if let Some(cb) = self.clear_completion(cx.editor) {
                                            if consumed {
                                                cx.on_next_key_callback =
                                                    Some((cb, OnKeyCallbackKind::Fallback))
                                            } else {
                                                self.on_next_key =
                                                    Some((cb, OnKeyCallbackKind::Fallback));
                                            }
                                        }
                                    }
                                }
                            }

                            // if completion didn't take the event, we pass it onto commands
                            if !consumed {
                                self.insert_mode(&mut cx, key);

                                // record last_insert key
                                self.last_insert.1.push(InsertEvent::Key(key));
                            }
                        }
                        mode => self.command_mode(mode, &mut cx, key),
                    }
                }

                self.on_next_key = cx.on_next_key_callback.take();
                match self.on_next_key {
                    Some((_, OnKeyCallbackKind::PseudoPending)) => self.pseudo_pending.push(key),
                    _ => self.pseudo_pending.clear(),
                }

                // appease borrowck
                let callbacks = take(&mut cx.callback);

                // if the command consumed the last view, skip the render.
                // on the next loop cycle the Application will then terminate.
                if cx.editor.should_close() {
                    return EventResult::Ignored(None);
                }

                let config = cx.editor.config();
                let mode = cx.editor.mode();
                let (view, doc) = current!(cx.editor);

                view.ensure_cursor_in_view(doc, config.scrolloff);

                // Store a history state if not in insert mode. This also takes care of
                // committing changes when leaving insert mode.
                if mode != Mode::Insert {
                    doc.append_changes_to_history(view);
                }
                let callback = if callbacks.is_empty() {
                    None
                } else {
                    let callback: crate::compositor::Callback = Box::new(move |compositor, cx| {
                        for callback in callbacks {
                            callback(compositor, cx)
                        }
                    });
                    Some(callback)
                };

                EventResult::Consumed(callback)
            }

            Event::Mouse(event) => self.handle_mouse_event(event, &mut cx),
            Event::IdleTimeout => {
                // Whatever changed since the last save is written once the editor goes idle.
                self.save_if_changed(cx.editor, true);
                self.handle_idle_timeout(&mut cx)
            }
            Event::FocusGained => {
                self.terminal_focused = true;
                EventResult::Consumed(None)
            }
            Event::FocusLost => {
                if context.editor.config().auto_save.focus_lost {
                    let options = commands::WriteAllOptions {
                        force: false,
                        write_scratch: false,
                        auto_format: false,
                        code_actions: false,
                    };
                    if let Err(e) = commands::typed::write_all_impl(context, options) {
                        context.editor.set_error(format!("{}", e));
                    }
                }
                self.terminal_focused = false;
                EventResult::Consumed(None)
            }
        }
    }

    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        // clear with background color
        surface.set_style(area, cx.editor.theme.get("ui.background"));
        let config = cx.editor.config();

        // Helide's frame: the sidebar box on the left, then the main box holding the editor
        // and, below a shared divider, the terminal. The last row stays for the command line.
        let frame = area.clip_bottom(1);
        self.terminal.update();
        // Keep the workspace saved while a project is open, so even a killed window loses
        // little. Integration tests leave the user's state alone.
        self.save_if_changed(cx.editor, false);
        let (finished, closed) = self.agents.update();
        if closed {
            self.save_agents();
        }
        for run in finished.iter().filter(|run| !run.seen) {
            let project = helix_stdx::env::current_working_dir();
            let project = project
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            agents::notify(&format!("{} finished", run.name), &project);
        }
        self.layout_frame(frame);
        let editor_box = self.editor_box;
        let zen = self.zen.is_some();
        let inner = |rect: Rect| {
            if zen {
                return rect;
            }
            Rect::new(
                rect.x + 1,
                rect.y + 1,
                rect.width.saturating_sub(2),
                rect.height.saturating_sub(2),
            )
        };

        // if the terminal size suddenly changed, we need to trigger a resize
        cx.editor.resize(inner(editor_box));

        let editor_focused = !self.terminal.focused
            && !self.file_tree.focused
            && !self.git.focused
            && !self.agents.focused;
        for (view, is_focused) in cx.editor.tree.views() {
            let doc = cx.editor.document(view.doc).unwrap();
            self.render_view(
                cx.editor,
                doc,
                view,
                area,
                surface,
                is_focused && editor_focused,
            );
        }

        if !zen {
            self.render_frame(cx.editor, surface, editor_focused);
        }

        if let Some(terminal_box) = self.terminal_box {
            // The divider row and the inside, without the bottom border.
            let panel = terminal_box.clip_bottom(1);
            self.terminal.render(panel, surface, &cx.editor.theme);
        }
        if let Some(agents_box) = self.agents_box {
            // The top border holds the tab labels; the agent runs inside the box.
            self.agents
                .render(agents_box.clip_bottom(1), surface, &cx.editor.theme);
        }
        if let Some(sidebar) = self.sidebar_box {
            if self.git.visible {
                self.git.render(inner(sidebar), surface, &cx.editor.theme);
            } else {
                self.file_tree.render(inner(sidebar), surface, cx.editor);
            }
            // Draw the scrollbar after the rows, which may have just scrolled.
            let roles = Roles::new(&cx.editor.theme);
            let state = if self.git.visible {
                self.git.scroll_state()
            } else {
                self.file_tree.scroll_state()
            };
            let focused = self.git.focused || self.file_tree.focused;
            let style = if focused { roles.accent } else { roles.muted };
            frame::scrollbar(
                surface,
                sidebar.right() - 1,
                sidebar.y + 1,
                sidebar.bottom() - 1,
                state,
                style,
            );
        }

        // Split and pane dividers meet the borders with junctions, not gaps.
        if self.zen.is_none() {
            let editor_box = self.editor_box;
            let (left, right) = (editor_box.x + 1, editor_box.right().saturating_sub(1));
            frame::join_verticals(surface, editor_box.y, left, right);
            frame::join_verticals(surface, editor_box.bottom().saturating_sub(1), left, right);
            if let Some(terminal) = self.terminal_box {
                let (left, right) = (terminal.x + 1, terminal.right().saturating_sub(1));
                frame::join_verticals(surface, terminal.bottom().saturating_sub(1), left, right);
            }
        }

        self.render_drag(cx.editor, surface);

        if config.auto_info {
            if let Some(mut info) = cx.editor.autoinfo.take() {
                info.render(area, surface, cx);
                cx.editor.autoinfo = Some(info)
            }
        }

        let key_width = 15u16; // for showing pending keys
        let mut status_msg_width = 0;

        // render status msg
        if let Some((status_msg, severity)) = &cx.editor.status_msg {
            status_msg_width = status_msg.width();
            use helix_view::editor::Severity;
            let style = if *severity == Severity::Error {
                cx.editor.theme.get("error")
            } else {
                cx.editor.theme.get("ui.text")
            };

            surface.set_string(
                area.x,
                area.y + area.height.saturating_sub(1),
                status_msg,
                style,
            );
        }

        if area.width.saturating_sub(status_msg_width as u16) > key_width {
            let mut disp = String::new();
            if let Some(count) = cx.editor.count {
                disp.push_str(&count.to_string())
            }
            for key in self.keymaps.pending() {
                disp.push_str(&key.key_sequence_format());
            }
            for key in &self.pseudo_pending {
                disp.push_str(&key.key_sequence_format());
            }
            let style = cx.editor.theme.get("ui.text");
            let macro_width = if cx.editor.macro_recording.is_some() {
                3
            } else {
                0
            };
            let restricted = workspace_trust_indicator_visible(cx.editor);
            let trust_width = if restricted { 3 } else { 0 };
            surface.set_string(
                area.x
                    + area
                        .width
                        .saturating_sub(key_width + macro_width + trust_width),
                area.y + area.height.saturating_sub(1),
                disp.get(disp.len().saturating_sub(key_width as usize)..)
                    .unwrap_or(&disp),
                style,
            );
            if restricted {
                let style = style
                    .fg(helix_view::graphics::Color::Yellow)
                    .add_modifier(Modifier::BOLD);
                surface.set_string(
                    area.x
                        .saturating_add(area.width.saturating_sub(3 + macro_width)),
                    area.y + area.height.saturating_sub(1),
                    "[⚠]",
                    style,
                );
            }
            if let Some((reg, _)) = cx.editor.macro_recording {
                let disp = format!("[{}]", reg);
                let style = style
                    .fg(helix_view::graphics::Color::Yellow)
                    .add_modifier(Modifier::BOLD);
                surface.set_string(
                    area.x + area.width.saturating_sub(3),
                    area.y + area.height.saturating_sub(1),
                    &disp,
                    style,
                );
            }
        }

        if let Some(completion) = self.completion.as_mut() {
            completion.render(area, surface, cx);
        }
    }

    fn cursor(&self, _area: Rect, editor: &Editor) -> (Option<Position>, CursorKind) {
        if self.terminal.visible && self.terminal.focused {
            return self.terminal.cursor();
        }
        if self.agents.visible && self.agents.focused {
            return self.agents.cursor();
        }
        if (self.file_tree.visible && self.file_tree.focused)
            || (self.git.visible && self.git.focused)
        {
            return (None, CursorKind::Hidden);
        }
        match editor.cursor() {
            // all block cursors are drawn manually
            (pos, CursorKind::Block) => {
                if self.terminal_focused {
                    (pos, CursorKind::Hidden)
                } else {
                    // use terminal cursor when terminal loses focus
                    (pos, CursorKind::Underline)
                }
            }
            cursor => cursor,
        }
    }
}

fn canonicalize_key(key: &mut KeyEvent) {
    if let KeyEvent {
        code: KeyCode::Char(_),
        modifiers: _,
    } = key
    {
        key.modifiers.remove(KeyModifiers::SHIFT)
    }
}

/// A saved pane from a tab's split structure: files with their cursors, splits with their
/// direction. Views without a file (scratch, diffs, proposals) are left out.
fn capture_pane(editor: &Editor, node: &helix_view::tree::LayoutNode) -> Option<agents::Pane> {
    use helix_view::tree::{Layout, LayoutNode};
    match node {
        LayoutNode::View(id) => {
            let view = editor.tree.try_get(*id)?;
            let doc = editor.document(view.doc)?;
            let path = doc.path()?.to_path_buf();
            let text = doc.text().slice(..);
            let cursor = doc.selection(view.id).primary().cursor(text);
            let position = helix_core::coords_at_pos(text, cursor);
            Some(agents::Pane::File {
                path,
                line: position.row + 1,
                column: position.col + 1,
            })
        }
        LayoutNode::Split(layout, children) => {
            let mut panes: Vec<agents::Pane> = children
                .iter()
                .filter_map(|child| capture_pane(editor, child))
                .collect();
            match panes.len() {
                0 => None,
                1 => panes.pop(),
                _ => Some(agents::Pane::Split {
                    vertical: *layout == Layout::Vertical,
                    panes,
                }),
            }
        }
    }
}

/// Fills the focused view with `pane`: a file at its cursor, or a split whose panes are opened
/// beside (or below) one another, then filled in turn.
fn fill_pane(editor: &mut Editor, pane: &agents::Pane) {
    match pane {
        agents::Pane::File { path, line, column } => {
            if editor.open(path, Action::Replace).is_ok() {
                let (view, doc) = current!(editor);
                let pos = helix_core::pos_at_coords(
                    doc.text().slice(..),
                    helix_core::Position::new(line.saturating_sub(1), column.saturating_sub(1)),
                    true,
                );
                doc.set_selection(view.id, helix_core::Selection::point(pos));
                helix_view::align_view(doc, view, helix_view::Align::Center);
            }
        }
        agents::Pane::Split { vertical, panes } => {
            let action = if *vertical {
                Action::VerticalSplit
            } else {
                Action::HorizontalSplit
            };
            // Make the sibling views first, in order, then fill each one.
            let mut views = vec![editor.tree.focus];
            for pane in panes.iter().skip(1) {
                let Some(first) = pane.first_file() else {
                    continue;
                };
                if let Some(&last) = views.last() {
                    editor.focus(last);
                }
                if editor.open(first, action).is_ok() {
                    views.push(editor.tree.focus);
                }
            }
            for (pane, view) in panes.iter().zip(views) {
                editor.focus(view);
                fill_pane(editor, pane);
            }
        }
    }
}

/// Which agent to start, when both are installed: a small menu by the editor's top-right.
fn agent_menu(position: Position) -> super::Popup<super::Menu<AgentKind>> {
    let mut menu = super::Menu::new(AgentKind::installed(), (), |_editor, kind, event| {
        if event != super::PromptEvent::Validate {
            return;
        }
        let Some(kind) = kind.copied() else {
            return;
        };
        crate::job::dispatch_blocking(move |editor, compositor| {
            if let Some(editor_view) = compositor.find::<EditorView>() {
                editor_view.open_agent(kind, editor);
            }
        });
    });
    menu.move_down(); // pre-select the first agent
    super::Popup::new("agent-menu", menu)
        .with_scrollbar(false)
        .position(Some(position))
        .auto_close(true)
}

impl super::menu::Item for AgentKind {
    type Data = ();

    fn format(&self, _data: &Self::Data) -> tui::widgets::Row<'_> {
        tui::widgets::Row::new([self.command()])
    }
}
