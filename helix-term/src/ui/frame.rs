//! Helide's window frame: square boxes around the sidebar, the editor and the terminal, with
//! titles and meta set into the borders, junctions where boxes meet, and scrollbar thumbs on
//! right borders. Panels draw only their contents, inside the boxes this module draws.

use helix_core::unicode::width::UnicodeWidthStr;
use helix_view::graphics::{Rect, Style};
use tui::buffer::Buffer as Surface;

/// How a box segment ends at the top or bottom: a corner, or a tee where it meets the
/// segment above or below.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum End {
    Corner,
    Tee,
}

fn set(surface: &mut Surface, x: u16, y: u16, symbol: &str, style: Style) {
    if let Some(cell) = surface.get_mut(x, y) {
        cell.set_symbol(symbol).set_style(style);
    }
}

/// Draws the border of `rect`. `top` and `bottom` choose corners or tees on each end, so
/// stacked segments share one horizontal line.
pub fn draw_box(surface: &mut Surface, rect: Rect, style: Style, top: End, bottom: End) {
    if rect.width < 2 || rect.height < 2 {
        return;
    }
    let (left, right) = (rect.left(), rect.right() - 1);
    let (upper, lower) = (rect.top(), rect.bottom() - 1);
    for x in left + 1..right {
        set(surface, x, upper, "─", style);
        set(surface, x, lower, "─", style);
    }
    for y in upper + 1..lower {
        set(surface, left, y, "│", style);
        set(surface, right, y, "│", style);
    }
    let (tl, tr) = match top {
        End::Corner => ("┌", "┐"),
        End::Tee => ("├", "┤"),
    };
    let (bl, br) = match bottom {
        End::Corner => ("└", "┘"),
        End::Tee => ("├", "┤"),
    };
    set(surface, left, upper, tl, style);
    set(surface, right, upper, tr, style);
    set(surface, left, lower, bl, style);
    set(surface, right, lower, br, style);
}

/// Joins vertical lines (split and pane dividers) to the horizontal border on `row`, between
/// columns `left` and `right` (exclusive): `┬` where a line starts below, `┴` where one ends
/// above, `┼` where it crosses. The border keeps its colour.
pub fn join_verticals(surface: &mut Surface, row: u16, left: u16, right: u16) {
    let area = surface.area;
    let inside =
        |x: u16, y: u16| x >= area.x && x < area.right() && y >= area.y && y < area.bottom();
    let is_vertical = |surface: &Surface, x: u16, y: u16| {
        inside(x, y) && matches!(surface[(x, y)].symbol.as_str(), "│" | "┃")
    };
    for x in left..right {
        if !inside(x, row) || surface[(x, row)].symbol.as_str() != "─" {
            continue;
        }
        let up = row > 0 && is_vertical(surface, x, row - 1);
        let down = is_vertical(surface, x, row + 1);
        let symbol = match (up, down) {
            (true, true) => "┼",
            (true, false) => "┴",
            (false, true) => "┬",
            (false, false) => continue,
        };
        surface[(x, row)].set_symbol(symbol);
    }
}

/// One piece of text set into a border, padded by a space each side.
pub struct Label {
    pub text: String,
    pub style: Style,
}

impl Label {
    pub fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }

    fn width(&self) -> u16 {
        self.text.width() as u16 + 2
    }
}

/// Sets `labels` into a border row from the left, starting after the corner. Labels that do not
/// fit before `end` are dropped. Returns the columns each label covers, for clicks.
pub fn labels_left(
    surface: &mut Surface,
    y: u16,
    x: u16,
    end: u16,
    labels: &[Label],
) -> Vec<(u16, u16)> {
    let mut hits = Vec::new();
    let mut x = x + 1;
    for label in labels {
        if x + label.width() > end {
            break;
        }
        surface.set_string(x, y, &format!(" {} ", label.text), label.style);
        hits.push((x, x + label.width()));
        x += label.width();
    }
    hits
}

/// Sets `labels` into a border row so they end just before the corner at `end`. Returns the
/// columns each label covers, in the order given.
pub fn labels_right(
    surface: &mut Surface,
    y: u16,
    start: u16,
    end: u16,
    labels: &[Label],
) -> Vec<(u16, u16)> {
    let total: u16 = labels.iter().map(Label::width).sum();
    if total + 1 > end.saturating_sub(start) {
        return Vec::new();
    }
    labels_left(surface, y, end - total - 1, end, labels)
}

/// A scrollbar thumb drawn over a right border spanning `top..bottom`, for `total` rows of which
/// `visible` are shown starting at `offset`. Nothing is drawn when everything fits.
pub fn scrollbar(
    surface: &mut Surface,
    x: u16,
    top: u16,
    bottom: u16,
    (total, offset, visible): (usize, usize, usize),
    style: Style,
) {
    let track = bottom.saturating_sub(top) as usize;
    if track == 0 || total <= visible || visible == 0 {
        return;
    }
    let thumb = (track * visible / total).clamp(1, track);
    let max_offset = total - visible;
    let start = ((track - thumb) * offset.min(max_offset)) / max_offset.max(1);
    for y in 0..thumb {
        set(surface, x, top + (start + y) as u16, "┃", style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(surface: &Surface, y: u16) -> String {
        (0..surface.area.width)
            .map(|x| surface[(x, y)].symbol.to_string())
            .collect()
    }

    #[test]
    fn stacked_boxes_share_a_tee_line() {
        let mut surface = Surface::empty(Rect::new(0, 0, 8, 5));
        draw_box(
            &mut surface,
            Rect::new(0, 0, 8, 3),
            Style::default(),
            End::Corner,
            End::Tee,
        );
        draw_box(
            &mut surface,
            Rect::new(0, 2, 8, 3),
            Style::default(),
            End::Tee,
            End::Corner,
        );
        assert_eq!(row(&surface, 0), "┌──────┐");
        assert_eq!(row(&surface, 1), "│      │");
        assert_eq!(row(&surface, 2), "├──────┤");
        assert_eq!(row(&surface, 4), "└──────┘");
    }

    #[test]
    fn labels_sit_in_the_border() {
        let mut surface = Surface::empty(Rect::new(0, 0, 20, 2));
        draw_box(
            &mut surface,
            Rect::new(0, 0, 20, 2),
            Style::default(),
            End::Corner,
            End::Corner,
        );
        let left = labels_left(
            &mut surface,
            0,
            0,
            19,
            &[Label::new("GIT", Style::default())],
        );
        let right = labels_right(
            &mut surface,
            0,
            0,
            19,
            &[Label::new("main", Style::default())],
        );
        assert_eq!(row(&surface, 0), "┌ GIT ─────── main ┐");
        assert_eq!(left, vec![(1, 6)]);
        assert_eq!(right, vec![(13, 19)]);
    }

    #[test]
    fn scrollbar_thumb_tracks_the_offset() {
        let mut surface = Surface::empty(Rect::new(0, 0, 1, 10));
        scrollbar(&mut surface, 0, 0, 10, (100, 90, 10), Style::default());
        let column: String = (0..10)
            .map(|y| surface[(0, y)].symbol.to_string())
            .collect();
        assert_eq!(column, "         ┃");
    }
}
