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
#[derive(Clone)]
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

/// The width a ‹ or › marker takes, room for "‹ 99" with its padding.
const MARKER_WIDTH: u16 = 6;

/// A row of tabs that can be wider than the room for it. The tabs that fit are drawn, with
/// "‹ N" and "N ›" for the ones hidden on either side; the active tab is brought into view
/// whenever it changes, and the wheel or a click on a marker moves the row by a tab.
#[derive(Default)]
pub struct TabStrip {
    /// The first tab drawn.
    offset: usize,
    /// The active tab the row last followed, so scrolling away from it sticks.
    followed: Option<usize>,
    /// Columns of the ‹ and › markers last frame.
    left_marker: Option<(u16, u16)>,
    right_marker: Option<(u16, u16)>,
}

/// How many tabs, from `offset`, fit in `room` columns, leaving room for the markers needed.
fn fitting(widths: &[u16], offset: usize, room: u16) -> usize {
    let count = |room: u16| {
        let mut used = 0;
        widths[offset..]
            .iter()
            .take_while(|&&w| {
                used += w;
                used <= room
            })
            .count()
    };
    let room = if offset > 0 {
        room.saturating_sub(MARKER_WIDTH)
    } else {
        room
    };
    let n = count(room);
    if offset + n < widths.len() {
        count(room.saturating_sub(MARKER_WIDTH))
    } else {
        n
    }
}

/// The first tab to draw and how many: `offset` moved to show `active` if it changed since
/// `followed`, and held back so the last tab ends the row rather than empty space.
fn window(
    widths: &[u16],
    room: u16,
    offset: usize,
    active: Option<usize>,
    followed: Option<usize>,
) -> (usize, usize) {
    let n = widths.len();
    if widths.iter().sum::<u16>() <= room {
        return (0, n);
    }
    let mut offset = offset.min(n.saturating_sub(1));
    if let Some(active) = active.filter(|&a| a < n && Some(a) != followed) {
        if active < offset {
            offset = active;
        }
        while offset < active && active >= offset + fitting(widths, offset, room).max(1) {
            offset += 1;
        }
    }
    let last = (0..n)
        .find(|&o| o + fitting(widths, o, room) >= n)
        .unwrap_or(n - 1);
    let offset = offset.min(last);
    (offset, fitting(widths, offset, room))
}

impl TabStrip {
    /// Sets `lead` (fixed labels such as a title) and then `tabs` into a border row from `x` to
    /// `end`. Returns the columns of each drawn tab, with its index.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        surface: &mut Surface,
        y: u16,
        x: u16,
        end: u16,
        lead: &[Label],
        tabs: &[Label],
        active: Option<usize>,
        marker_style: Style,
    ) -> Vec<(usize, u16, u16)> {
        let lead_hits = labels_left(surface, y, x, end, lead);
        let start = lead_hits.last().map_or(x + 1, |&(_, e)| e);
        let room = end.saturating_sub(start);
        let widths: Vec<u16> = tabs.iter().map(Label::width).collect();
        let (offset, count) = window(&widths, room, self.offset, active, self.followed);
        self.offset = offset;
        self.followed = active;

        let mut labels = Vec::new();
        if offset > 0 {
            labels.push(Label::new(format!("‹ {}", offset.min(99)), marker_style));
        }
        labels.extend(tabs[offset..offset + count].iter().cloned());
        let hidden = tabs.len() - offset - count;
        if hidden > 0 {
            labels.push(Label::new(format!("{} ›", hidden.min(99)), marker_style));
        }
        let mut hits = labels_left(surface, y, start - 1, end, &labels).into_iter();
        self.left_marker = if offset > 0 { hits.next() } else { None };
        let tab_hits: Vec<(usize, u16, u16)> = hits
            .by_ref()
            .take(count)
            .enumerate()
            .map(|(i, (s, e))| (offset + i, s, e))
            .collect();
        self.right_marker = if hidden > 0 { hits.next() } else { None };
        tab_hits
    }

    /// Moves the row by `by` tabs; the next draw keeps it within the tabs.
    pub fn scroll(&mut self, by: isize) {
        self.offset = self.offset.saturating_add_signed(by);
    }

    /// Which way a click at `column` on a marker moves the row, if it is on one.
    pub fn marker_at(&self, column: u16) -> Option<isize> {
        let on = |m: Option<(u16, u16)>| m.is_some_and(|(s, e)| (s..e).contains(&column));
        if on(self.left_marker) {
            Some(-1)
        } else if on(self.right_marker) {
            Some(1)
        } else {
            None
        }
    }
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

#[cfg(test)]
mod tab_strip_tests {
    use super::window;

    #[test]
    fn everything_fits() {
        assert_eq!(window(&[10, 10, 10], 40, 2, Some(2), None), (0, 3));
    }

    #[test]
    fn follows_the_active_tab() {
        // Room 30: markers take 6 each, leaving 18 in the middle, so one 10-wide tab.
        let widths = [10; 8];
        let (offset, count) = window(&widths, 30, 0, Some(5), None);
        assert!(offset <= 5 && 5 < offset + count);
    }

    #[test]
    fn scrolling_away_sticks_until_the_active_tab_changes() {
        let widths = [10; 8];
        assert_eq!(window(&widths, 30, 0, Some(5), Some(5)).0, 0);
    }

    #[test]
    fn never_leaves_empty_room_after_the_last_tab() {
        let widths = [10; 8];
        let (offset, count) = window(&widths, 30, 7, None, None);
        assert_eq!(offset + count, 8);
        assert!(count >= 2);
    }
}
