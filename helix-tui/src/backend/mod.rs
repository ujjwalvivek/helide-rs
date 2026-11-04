//! Provides interface for controlling the terminal

use std::{borrow::Cow, io};

use crate::{buffer::Cell, terminal::Config};

use helix_view::{
    graphics::{CursorKind, Rect},
    theme::Color,
};

#[cfg(all(feature = "termina", not(windows)))]
mod termina;
#[cfg(all(feature = "termina", not(windows)))]
pub use self::termina::TerminaBackend;

#[cfg(all(feature = "termina", windows))]
mod crossterm;
#[cfg(all(feature = "termina", windows))]
pub use self::crossterm::CrosstermBackend;

mod test;
pub use self::test::TestBackend;

/// What a cell's symbol looks like on the terminal. Control characters would be obeyed by the
/// terminal instead of drawn (ESC starts a sequence, CR and BS move the cursor), and a symbol
/// with no width would not move the cursor even though it holds a column, so both would shift
/// everything drawn after them. Control characters become their Control Pictures (␀, ␛, ␡) or
/// � and a symbol of zero width becomes �, each one column wide like the cell they sit in.
pub(crate) fn printable(symbol: &str) -> Cow<'_, str> {
    use helix_core::unicode::width::UnicodeWidthStr;

    if symbol.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        return Cow::Borrowed(symbol);
    }
    if symbol.chars().any(char::is_control) {
        return Cow::Owned(
            symbol
                .chars()
                .map(|c| match c as u32 {
                    0x00..=0x1f => char::from_u32(0x2400 + c as u32).unwrap_or('\u{FFFD}'),
                    0x7f => '\u{2421}',
                    _ if c.is_control() => '\u{FFFD}',
                    _ => c,
                })
                .collect(),
        );
    }
    if !symbol.is_empty() && symbol.width() == 0 {
        return Cow::Borrowed("\u{FFFD}");
    }
    Cow::Borrowed(symbol)
}

#[cfg(test)]
mod printable_tests {
    use super::printable;

    #[test]
    fn plain_text_is_untouched() {
        assert_eq!(printable("a"), "a");
        assert_eq!(printable("é"), "é");
        assert_eq!(printable("漢"), "漢");
        assert_eq!(printable(""), "");
    }

    #[test]
    fn control_characters_are_drawn_not_obeyed() {
        assert_eq!(printable("\u{1b}"), "␛");
        assert_eq!(printable("\0"), "␀");
        assert_eq!(printable("\r"), "␍");
        assert_eq!(printable("\u{7f}"), "␡");
        assert_eq!(printable("\u{9b}"), "\u{FFFD}");
    }

    #[test]
    fn zero_width_symbols_keep_their_column() {
        assert_eq!(printable("\u{301}"), "\u{FFFD}");
        assert_eq!(printable("\u{200b}"), "\u{FFFD}");
    }
}

/// Representation of a terminal backend.
pub trait Backend {
    /// Claims the terminal for TUI use.
    fn claim(&mut self) -> Result<(), io::Error>;
    /// Update terminal configuration.
    fn reconfigure(&mut self, config: Config) -> Result<(), io::Error>;
    /// Restores the terminal to a normal state, undoes `claim`
    fn restore(&mut self) -> Result<(), io::Error>;
    /// Draws styled text to the terminal
    fn draw<'a, I>(&mut self, content: I) -> Result<(), io::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>;
    /// Hides the cursor
    fn hide_cursor(&mut self) -> Result<(), io::Error>;
    /// Sets the cursor to the given shape
    fn show_cursor(&mut self, kind: CursorKind) -> Result<(), io::Error>;
    /// Sets the cursor to the given position
    fn set_cursor(&mut self, x: u16, y: u16) -> Result<(), io::Error>;
    /// Clears the terminal
    fn clear(&mut self) -> Result<(), io::Error>;
    /// Begins a synchronized-output frame (if the terminal supports it), so the
    /// draw and cursor updates between `start_sync` and `end_sync` present as one
    /// frame instead of flickering.
    fn start_sync(&mut self) -> Result<(), io::Error>;
    /// Ends the synchronized-output frame opened by `start_sync`.
    fn end_sync(&mut self) -> Result<(), io::Error>;
    /// Gets the size of the terminal in cells
    fn size(&self) -> Result<Rect, io::Error>;
    /// Flushes the terminal buffer
    fn flush(&mut self) -> Result<(), io::Error>;
    fn supports_true_color(&self) -> bool;
    fn get_theme_mode(&self) -> Option<helix_view::theme::Mode>;
    fn set_background_color(&mut self, color: Option<Color>) -> io::Result<()>;
}
