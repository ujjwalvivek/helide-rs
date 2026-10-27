//! Styles shared by Helide's panels (file tree, git, terminal), taken from standard theme scopes
//! only, so any theme, including Noctalia's generated one, styles them without extra keys.
//!
//! The roles follow the house design system: three ink strengths, one accent, selection as a
//! small surface step, status colours always next to a word or glyph. Panel edges are dashed
//! and faint when the panel is inert, solid and accented when it has focus.

use helix_view::{
    graphics::{Modifier, Rect, Style},
    Theme,
};
use tui::buffer::Buffer as Surface;

pub struct Roles {
    pub text: Style,
    /// Paths, counts, ages and other secondary text.
    pub muted: Style,
    /// Section labels and inert lines.
    pub faint: Style,
    /// The theme's one hue: focus, the active item, the current file.
    pub accent: Style,
    /// The selected row: a small surface step.
    pub selected: Style,
    pub plus: Style,
    pub minus: Style,
    pub delta: Style,
    pub warning: Style,
    pub error: Style,
}

impl Roles {
    pub fn new(theme: &Theme) -> Self {
        // Only the foreground of these scopes is wanted; their backgrounds belong to the
        // gutter and the cursor line.
        let fg = |scope: &str| Style {
            bg: None,
            ..theme.get(scope)
        };
        Self {
            text: fg("ui.text"),
            muted: fg("ui.linenr"),
            faint: fg("ui.virtual"),
            accent: fg("ui.linenr.selected"),
            selected: theme.get("ui.cursorline.primary"),
            plus: fg("diff.plus"),
            minus: fg("diff.minus"),
            delta: fg("diff.delta"),
            warning: fg("warning"),
            error: fg("error"),
        }
    }

    /// An UPPERCASE section label: set apart by case and weight, not by colour.
    pub fn label(&self) -> Style {
        self.muted.add_modifier(Modifier::BOLD)
    }

    /// Paints a selected row: the surface step, and a thin accent bar when the panel is focused.
    pub fn select_row(&self, surface: &mut Surface, row: Rect, focused: bool) {
        if focused {
            surface.set_style(row, self.selected);
            surface.set_string(row.x, row.y, "▎", self.accent);
        } else {
            surface.set_string(row.x, row.y, "▎", self.faint);
        }
    }

    /// A vertical edge at column `x`: solid accent when focused, dashed and faint otherwise.
    pub fn vertical_edge(
        &self,
        surface: &mut Surface,
        x: u16,
        top: u16,
        bottom: u16,
        focused: bool,
    ) {
        let (symbol, style) = if focused {
            ("│", self.accent)
        } else {
            ("╎", self.faint)
        };
        for y in top..bottom {
            if let Some(cell) = surface.get_mut(x, y) {
                cell.set_symbol(symbol).set_style(style);
            }
        }
    }

    /// A horizontal edge across `row`: solid accent when focused, dashed and faint otherwise.
    pub fn horizontal_edge(&self, surface: &mut Surface, row: Rect, focused: bool) {
        let (symbol, style) = if focused {
            ("─", self.accent)
        } else {
            ("╌", self.faint)
        };
        for x in row.left()..row.right() {
            if let Some(cell) = surface.get_mut(x, row.y) {
                cell.set_symbol(symbol).set_style(style);
            }
        }
    }
}
