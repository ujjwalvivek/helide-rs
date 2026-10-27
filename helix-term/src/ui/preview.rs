//! Markdown preview: a pane that shows a markdown file rendered, with Helix's own markdown
//! renderer (the one hover docs use), instead of as text. The pane is an ordinary view of the
//! file, so it follows every edit; the EditorView only draws it differently and scrolls it.

use std::sync::Arc;

use arc_swap::ArcSwap;
use helix_core::syntax;
use helix_view::{graphics::Rect, theme::Theme, Document};
use tui::{
    buffer::Buffer as Surface,
    widgets::{Paragraph, Widget, Wrap},
};

use super::{markdown::Markdown, panel_style::Roles, text::required_size};

/// Whether a document is markdown, by language or, before one is set, by extension.
pub fn is_markdown(doc: &Document) -> bool {
    doc.language_name() == Some("markdown")
        || doc
            .path()
            .and_then(|path| path.extension())
            .is_some_and(|ext| ext == "md" || ext == "markdown")
}

/// Draws `doc` rendered into `area`, from line `scroll`, which is clamped to the content.
pub fn render(
    area: Rect,
    surface: &mut Surface,
    doc: &Document,
    scroll: &mut u16,
    theme: &Theme,
    loader: Arc<ArcSwap<syntax::Loader>>,
    focused: bool,
) {
    let roles = Roles::new(theme);
    surface.set_style(area, theme.get("ui.background"));
    if area.width < 4 || area.height < 3 {
        return;
    }
    // A margin around the text, and the last row for a status line like the editor's.
    let body = Rect::new(
        area.x + 2,
        area.y + 1,
        area.width - 4,
        area.height.saturating_sub(2),
    );
    let markdown = Markdown::new(doc.text().to_string(), loader);
    let text = markdown.parse(Some(theme));
    let (_, height) = required_size(&text, body.width);
    *scroll = (*scroll).min(height.saturating_sub(body.height / 2));
    Paragraph::new(&text)
        .wrap(Wrap { trim: false })
        .scroll((*scroll, 0))
        .render(body, surface);

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
}
