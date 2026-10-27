//! Helide terminal panel: shells and agents running in PTYs, docked under the editor.
//!
//! Each tab is a [`Session`]: a child process on a PTY whose output is parsed by
//! `alacritty_terminal` on its own thread. The panel draws the parsed grid into the editor's
//! surface and, while focused, encodes keys, pastes and mouse events back into the PTY.

use std::{
    borrow::Cow,
    path::PathBuf,
    sync::{
        mpsc::{self, Receiver, Sender},
        Arc,
    },
};

use alacritty_terminal::{
    event::{Event as TermEvent, EventListener, WindowSize},
    event_loop::{EventLoop, EventLoopSender, Msg},
    grid::Scroll,
    index::Point,
    sync::FairMutex,
    term::{
        cell::{Cell as TermCell, Flags},
        test::TermSize,
        Config as TermConfig, TermMode,
    },
    tty::{self, Shell},
    vte::ansi::{Color as AnsiColor, CursorShape, NamedColor},
    Term,
};
use helix_core::unicode::width::UnicodeWidthStr;
use helix_view::{
    graphics::{Color, CursorKind, Modifier, Rect, Style, UnderlineStyle},
    input::{KeyEvent, MouseButton, MouseEvent, MouseEventKind},
    keyboard::{KeyCode, KeyModifiers},
    theme::Theme,
};
use tui::buffer::Buffer as Surface;

use super::panel_style::Roles;

/// Share of the editor area the panel takes when shown.
const HEIGHT_PERCENT: u16 = 35;
/// The panel never gets smaller than this, tab strip included.
const MIN_HEIGHT: u16 = 6;
const SCROLLBACK: usize = 10_000;

/// Forwards terminal events to the panel and wakes the editor so it redraws.
#[derive(Clone)]
struct Listener(Sender<TermEvent>);

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        let _ = self.0.send(event);
        helix_event::request_redraw();
    }
}

struct Session {
    term: Arc<FairMutex<Term<Listener>>>,
    pty: EventLoopSender,
    events: Receiver<TermEvent>,
    /// What the tab strip shows: the program name until the program sets a title.
    name: String,
    title: Option<String>,
    exited: bool,
    size: (u16, u16),
}

impl Session {
    fn spawn(command: &[String], name: Option<String>, size: (u16, u16)) -> anyhow::Result<Self> {
        let (cols, rows) = size;
        let shell = command
            .split_first()
            .map(|(program, args)| Shell::new(program.clone(), args.to_vec()));
        let name = name
            .or_else(|| command.first().cloned())
            .or_else(|| std::env::var("SHELL").ok())
            .map(|program| {
                PathBuf::from(program)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
            .unwrap_or_else(|| "shell".into());

        let options = tty::Options {
            shell,
            working_directory: Some(helix_stdx::env::current_working_dir()),
            drain_on_exit: true,
            env: [
                ("TERM".to_string(), "xterm-256color".to_string()),
                ("COLORTERM".to_string(), "truecolor".to_string()),
            ]
            .into(),
            #[cfg(windows)]
            escape_args: false,
        };
        let pty = tty::new(&options, window_size(size), 0)?;

        let (tx, events) = mpsc::channel();
        let listener = Listener(tx);
        let config = TermConfig {
            scrolling_history: SCROLLBACK,
            ..Default::default()
        };
        let term = Term::new(
            config,
            &TermSize::new(cols as usize, rows as usize),
            listener.clone(),
        );
        let term = Arc::new(FairMutex::new(term));
        let event_loop = EventLoop::new(term.clone(), listener, pty, true, false)?;
        let pty = event_loop.channel();
        event_loop.spawn();

        Ok(Self {
            term,
            pty,
            events,
            name,
            title: None,
            exited: false,
            size,
        })
    }

    fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        let _ = self.pty.send(Msg::Input(bytes.into()));
    }

    fn resize(&mut self, size: (u16, u16)) {
        if size == self.size || size.0 == 0 || size.1 == 0 {
            return;
        }
        self.size = size;
        self.term
            .lock()
            .resize(TermSize::new(size.0 as usize, size.1 as usize));
        let _ = self.pty.send(Msg::Resize(window_size(size)));
    }

    /// Handles what the terminal asked for since the last frame.
    fn drain_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                TermEvent::PtyWrite(text) => self.write(text.into_bytes()),
                TermEvent::TextAreaSizeRequest(format) => {
                    self.write(format(window_size(self.size)).into_bytes())
                }
                TermEvent::Title(title) => self.title = Some(title),
                TermEvent::ResetTitle => self.title = None,
                TermEvent::ChildExit(_) | TermEvent::Exit => self.exited = true,
                _ => {}
            }
        }
    }

    fn label(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.name)
    }

    fn mode(&self) -> TermMode {
        *self.term.lock().mode()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.pty.send(Msg::Shutdown);
    }
}

fn window_size((cols, rows): (u16, u16)) -> WindowSize {
    WindowSize {
        num_cols: cols,
        num_lines: rows,
        cell_width: 1,
        cell_height: 1,
    }
}

#[derive(Default)]
pub struct TerminalPanel {
    sessions: Vec<Session>,
    active: usize,
    pub visible: bool,
    pub focused: bool,
    /// Where the terminal grid (below the tab strip) was drawn last frame.
    grid_area: Rect,
    /// Where the whole panel was drawn last frame.
    area: Rect,
    /// Columns each tab label covered on the top edge last frame.
    tab_hits: Vec<(u16, u16)>,
}

impl TerminalPanel {
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Opens a new tab running `command`, or the user's shell when it is empty. `name` labels the
    /// tab until the program sets a title.
    pub fn open(&mut self, command: &[String], name: Option<String>) -> anyhow::Result<()> {
        let size = if self.grid_area.width > 0 {
            (self.grid_area.width, self.grid_area.height)
        } else {
            (80, 24)
        };
        self.sessions.push(Session::spawn(command, name, size)?);
        self.active = self.sessions.len() - 1;
        self.visible = true;
        self.focused = true;
        Ok(())
    }

    /// Hidden: show and focus, opening a shell if there is none. Shown but unfocused: focus.
    /// Focused: hide.
    pub fn toggle(&mut self) -> anyhow::Result<()> {
        if self.visible && self.focused {
            self.visible = false;
            self.focused = false;
        } else if self.sessions.is_empty() {
            self.open(&[], None)?;
        } else {
            self.visible = true;
            self.focused = true;
        }
        Ok(())
    }

    pub fn cycle(&mut self, forward: bool) {
        let len = self.sessions.len();
        if len > 0 {
            self.active = if forward {
                (self.active + 1) % len
            } else {
                (self.active + len - 1) % len
            };
        }
    }

    /// Gives the panel its slice of `area`, returning what is left for the editor.
    /// `height` overrides the default share when the user has dragged the panel's edge.
    pub fn layout(&self, area: Rect, height: Option<u16>) -> Rect {
        if !self.visible {
            return area;
        }
        let height = height
            .unwrap_or(area.height * HEIGHT_PERCENT / 100)
            .max(MIN_HEIGHT);
        if height + MIN_HEIGHT > area.height {
            return area;
        }
        area.clip_bottom(height)
    }

    /// Drains events from every session and drops the ones whose program exited.
    pub fn update(&mut self) {
        for session in &mut self.sessions {
            session.drain_events();
        }
        let before = self.sessions.len();
        self.sessions.retain(|session| !session.exited);
        if self.sessions.len() != before {
            self.active = self.active.min(self.sessions.len().saturating_sub(1));
            if self.sessions.is_empty() {
                self.visible = false;
                self.focused = false;
            }
        }
    }

    pub fn render(&mut self, area: Rect, surface: &mut Surface, theme: &Theme) {
        self.area = area;
        if area.height < 2 || self.sessions.is_empty() {
            return;
        }

        let strip = area.with_height(1);
        self.render_tabs(strip, surface, theme);

        let grid = area.clip_top(1);
        self.grid_area = grid;
        let session = &mut self.sessions[self.active];
        session.resize((grid.width, grid.height));

        let base = theme.get("ui.background");
        surface.set_style(grid, base);

        let term = session.term.lock();
        let content = term.renderable_content();
        let offset = content.display_offset as i32;
        for indexed in content.display_iter {
            let line = indexed.point.line.0 + offset;
            let col = indexed.point.column.0 as u16;
            if line < 0 || line as u16 >= grid.height || col >= grid.width {
                continue;
            }
            let (x, y) = (grid.x + col, grid.y + line as u16);
            let Some(cell) = surface.get_mut(x, y) else {
                continue;
            };
            let term_cell: &TermCell = &indexed;
            if term_cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                cell.reset();
                continue;
            }
            let mut symbol = String::from(term_cell.c);
            if let Some(zerowidth) = term_cell.zerowidth() {
                symbol.extend(zerowidth);
            }
            if term_cell.flags.contains(Flags::HIDDEN) || symbol.width() == 0 {
                symbol = " ".into();
            }
            cell.set_symbol(&symbol);
            cell.set_style(base.patch(cell_style(term_cell)));
        }
    }

    /// The top edge doubles as the tab strip: TERMINAL, then one label per tab, set into a
    /// line that is solid and accented while the panel has focus.
    fn render_tabs(&mut self, area: Rect, surface: &mut Surface, theme: &Theme) {
        let roles = Roles::new(theme);
        roles.horizontal_edge(surface, area, self.focused);
        self.tab_hits.clear();

        let mut x = area.x + 1;
        let title = " TERMINAL ";
        surface.set_string(x, area.y, title, roles.label());
        x += title.width() as u16;
        for (i, session) in self.sessions.iter().enumerate() {
            let label = format!(" {} {} ", i + 1, short_label(session.label()));
            let width = area.right().saturating_sub(x + 1) as usize;
            if width < 4 {
                break;
            }
            let style = if i == self.active {
                roles.text.add_modifier(Modifier::BOLD)
            } else {
                roles.muted
            };
            let (next, _) = surface.set_stringn(x, area.y, &label, width, style);
            self.tab_hits.push((x, next));
            x = next;
        }
    }

    pub fn cursor(&self) -> (Option<helix_core::Position>, CursorKind) {
        let Some(session) = self.sessions.get(self.active) else {
            return (None, CursorKind::Hidden);
        };
        let term = session.term.lock();
        let content = term.renderable_content();
        if content.cursor.shape == CursorShape::Hidden || content.display_offset != 0 {
            return (None, CursorKind::Hidden);
        }
        let Point { line, column } = content.cursor.point;
        if line.0 < 0 || line.0 as u16 >= self.grid_area.height {
            return (None, CursorKind::Hidden);
        }
        let kind = match content.cursor.shape {
            CursorShape::Beam => CursorKind::Bar,
            CursorShape::Underline => CursorKind::Underline,
            _ => CursorKind::Block,
        };
        let pos = helix_core::Position::new(
            (self.grid_area.y as i32 + line.0) as usize,
            self.grid_area.x as usize + column.0,
        );
        (Some(pos), kind)
    }

    /// The tab whose label covers `column` on the top edge.
    pub fn tab_at(&self, column: u16) -> Option<usize> {
        self.tab_hits
            .iter()
            .position(|&(start, end)| (start..end).contains(&column))
    }

    pub fn contains(&self, column: u16, row: u16) -> bool {
        self.visible
            && column >= self.area.x
            && column < self.area.right()
            && row >= self.area.y
            && row < self.area.bottom()
    }

    pub fn key(&mut self, key: KeyEvent) {
        let Some(session) = self.sessions.get(self.active) else {
            return;
        };
        let mode = session.mode();
        if let Some(bytes) = encode_key(key, mode) {
            // Typing jumps back to the live screen.
            session.term.lock().scroll_display(Scroll::Bottom);
            session.write(bytes);
        }
    }

    pub fn paste(&mut self, text: &str) {
        let Some(session) = self.sessions.get(self.active) else {
            return;
        };
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        if session.mode().contains(TermMode::BRACKETED_PASTE) {
            session.write(format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', "")).into_bytes());
        } else {
            session.write(text.into_bytes());
        }
    }

    pub fn mouse(&mut self, event: &MouseEvent) {
        // Clicking the tab strip switches tabs.
        if event.row == self.area.y {
            if let MouseEventKind::Down(MouseButton::Left) = event.kind {
                if let Some(i) = self
                    .tab_hits
                    .iter()
                    .position(|&(start, end)| (start..end).contains(&event.column))
                {
                    self.active = i;
                }
            }
            return;
        }

        let Some(session) = self.sessions.get(self.active) else {
            return;
        };
        let grid = self.grid_area;
        let col = event.column.saturating_sub(grid.x);
        let row = event.row.saturating_sub(grid.y);
        let mode = session.mode();

        if mode.intersects(TermMode::MOUSE_MODE) && mode.contains(TermMode::SGR_MOUSE) {
            if let Some(report) = sgr_mouse(event, col, row, mode) {
                session.write(report.into_bytes());
            }
            return;
        }

        let lines: i32 = match event.kind {
            MouseEventKind::ScrollUp => 3,
            MouseEventKind::ScrollDown => -3,
            _ => return,
        };
        if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
            // Full-screen programs without mouse support scroll with arrow keys.
            let arrow = if lines > 0 { b'A' } else { b'B' };
            let prefix: &[u8] = if mode.contains(TermMode::APP_CURSOR) {
                b"\x1bO"
            } else {
                b"\x1b["
            };
            let mut bytes = Vec::new();
            for _ in 0..lines.abs() {
                bytes.extend_from_slice(prefix);
                bytes.push(arrow);
            }
            session.write(bytes);
        } else {
            session.term.lock().scroll_display(Scroll::Delta(lines));
        }
    }
}

/// Tab labels keep their end: shells put the working directory in the title.
fn short_label(label: &str) -> String {
    const MAX: usize = 24;
    let chars: Vec<char> = label.chars().collect();
    if chars.len() <= MAX {
        return label.to_string();
    }
    let tail: String = chars[chars.len() - (MAX - 1)..].iter().collect();
    format!("…{tail}")
}

fn cell_style(cell: &TermCell) -> Style {
    let mut style = Style::default();
    if let Some(fg) = color(cell.fg) {
        style = style.fg(fg);
    }
    if let Some(bg) = color(cell.bg) {
        style = style.bg(bg);
    }
    let flags = cell.flags;
    let mut modifiers = Modifier::empty();
    for (flag, modifier) in [
        (Flags::BOLD, Modifier::BOLD),
        (Flags::DIM, Modifier::DIM),
        (Flags::ITALIC, Modifier::ITALIC),
        (Flags::INVERSE, Modifier::REVERSED),
        (Flags::STRIKEOUT, Modifier::CROSSED_OUT),
    ] {
        if flags.contains(flag) {
            modifiers |= modifier;
        }
    }
    style = style.add_modifier(modifiers);
    let underline = if flags.contains(Flags::UNDERCURL) {
        Some(UnderlineStyle::Curl)
    } else if flags.contains(Flags::DOUBLE_UNDERLINE) {
        Some(UnderlineStyle::DoubleLine)
    } else if flags.contains(Flags::DOTTED_UNDERLINE) {
        Some(UnderlineStyle::Dotted)
    } else if flags.contains(Flags::DASHED_UNDERLINE) {
        Some(UnderlineStyle::Dashed)
    } else if flags.contains(Flags::UNDERLINE) {
        Some(UnderlineStyle::Line)
    } else {
        None
    };
    if let Some(underline) = underline {
        style = style.underline_style(underline);
    }
    style
}

/// Palette colours stay palette colours, so programs in the panel use the host terminal's
/// (wallpaper-driven) palette. Default foreground and background are left unset, so the editor
/// theme and the window's translucency show through.
fn color(color: AnsiColor) -> Option<Color> {
    match color {
        AnsiColor::Spec(rgb) => Some(Color::Rgb(rgb.r, rgb.g, rgb.b)),
        AnsiColor::Indexed(i) => Some(Color::Indexed(i)),
        AnsiColor::Named(named) => {
            let index = named as usize;
            if index < 16 {
                Some(Color::Indexed(index as u8))
            } else {
                dim_to_normal(named).map(|normal| Color::Indexed(normal as u8))
            }
        }
    }
}

fn dim_to_normal(named: NamedColor) -> Option<NamedColor> {
    Some(match named {
        NamedColor::DimBlack => NamedColor::Black,
        NamedColor::DimRed => NamedColor::Red,
        NamedColor::DimGreen => NamedColor::Green,
        NamedColor::DimYellow => NamedColor::Yellow,
        NamedColor::DimBlue => NamedColor::Blue,
        NamedColor::DimMagenta => NamedColor::Magenta,
        NamedColor::DimCyan => NamedColor::Cyan,
        NamedColor::DimWhite => NamedColor::White,
        _ => return None,
    })
}

/// xterm modifier parameter: 1 + shift + 2 * alt + 4 * ctrl.
fn modifier_param(modifiers: KeyModifiers) -> u8 {
    1 + modifiers.contains(KeyModifiers::SHIFT) as u8
        + 2 * modifiers.contains(KeyModifiers::ALT) as u8
        + 4 * modifiers.contains(KeyModifiers::CONTROL) as u8
}

/// Encodes a key the way xterm does in its default (legacy) mode.
pub(crate) fn encode_key(key: KeyEvent, mode: TermMode) -> Option<Vec<u8>> {
    let mods = key.modifiers;
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    let alt = mods.contains(KeyModifiers::ALT);
    let shift = mods.contains(KeyModifiers::SHIFT);
    let has_mods = ctrl || alt || shift;
    let param = modifier_param(mods);
    let app_cursor = mode.contains(TermMode::APP_CURSOR);

    let with_alt = |mut bytes: Vec<u8>| {
        if alt {
            bytes.insert(0, 0x1b);
        }
        bytes
    };
    // Arrows, Home and End: `CSI 1;m X` with modifiers, `SS3 X` in application cursor mode.
    let cursor_key = |letter: u8| {
        if has_mods {
            format!("\x1b[1;{param}{}", letter as char).into_bytes()
        } else if app_cursor {
            vec![0x1b, b'O', letter]
        } else {
            vec![0x1b, b'[', letter]
        }
    };
    // PageUp, Delete and friends: `CSI n ~` or `CSI n;m ~`.
    let tilde_key = |n: u8| {
        if has_mods {
            format!("\x1b[{n};{param}~").into_bytes()
        } else {
            format!("\x1b[{n}~").into_bytes()
        }
    };

    let bytes = match key.code {
        KeyCode::Char(c) if ctrl => {
            let byte = match c.to_ascii_lowercase() {
                c @ 'a'..='z' => c as u8 & 0x1f,
                '@' | ' ' | '2' => 0x00,
                '[' | '3' => 0x1b,
                '\\' | '4' => 0x1c,
                ']' | '5' => 0x1d,
                '^' | '6' => 0x1e,
                '_' | '/' | '7' => 0x1f,
                '?' | '8' => 0x7f,
                _ => return None,
            };
            with_alt(vec![byte])
        }
        KeyCode::Char(c) => {
            let mut buf = [0; 4];
            with_alt(c.encode_utf8(&mut buf).as_bytes().to_vec())
        }
        KeyCode::Enter => with_alt(vec![b'\r']),
        KeyCode::Tab if shift => b"\x1b[Z".to_vec(),
        KeyCode::Tab => with_alt(vec![b'\t']),
        KeyCode::Backspace if ctrl => with_alt(vec![0x08]),
        KeyCode::Backspace => with_alt(vec![0x7f]),
        KeyCode::Esc => with_alt(vec![0x1b]),
        KeyCode::Up => cursor_key(b'A'),
        KeyCode::Down => cursor_key(b'B'),
        KeyCode::Right => cursor_key(b'C'),
        KeyCode::Left => cursor_key(b'D'),
        KeyCode::Home => cursor_key(b'H'),
        KeyCode::End => cursor_key(b'F'),
        KeyCode::Insert => tilde_key(2),
        KeyCode::Delete => tilde_key(3),
        KeyCode::PageUp => tilde_key(5),
        KeyCode::PageDown => tilde_key(6),
        KeyCode::F(n @ 1..=4) => {
            let letter = b"PQRS"[n as usize - 1];
            if has_mods {
                format!("\x1b[1;{param}{}", letter as char).into_bytes()
            } else {
                vec![0x1b, b'O', letter]
            }
        }
        KeyCode::F(n @ 5..=12) => tilde_key([15, 17, 18, 19, 20, 21, 23, 24][n as usize - 5]),
        _ => return None,
    };
    Some(bytes)
}

/// An SGR (1006) mouse report, for programs that turned mouse reporting on.
fn sgr_mouse(event: &MouseEvent, col: u16, row: u16, mode: TermMode) -> Option<String> {
    let button = |button: MouseButton| match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    let (code, pressed) = match event.kind {
        MouseEventKind::Down(b) => (button(b), true),
        MouseEventKind::Up(b) => (button(b), false),
        MouseEventKind::Drag(b)
            if mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION) =>
        {
            (button(b) + 32, true)
        }
        MouseEventKind::Moved if mode.contains(TermMode::MOUSE_MOTION) => (35, true),
        MouseEventKind::ScrollUp => (64, true),
        MouseEventKind::ScrollDown => (65, true),
        _ => return None,
    };
    let mods = event.modifiers;
    let code = code
        + 4 * mods.contains(KeyModifiers::SHIFT) as u8
        + 8 * mods.contains(KeyModifiers::ALT) as u8
        + 16 * mods.contains(KeyModifiers::CONTROL) as u8;
    let end = if pressed { 'M' } else { 'm' };
    Some(format!("\x1b[<{code};{};{}{end}", col + 1, row + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Vec<u8> {
        encode_key(KeyEvent { code, modifiers }, TermMode::empty()).unwrap()
    }

    #[test]
    fn encodes_keys() {
        let none = KeyModifiers::NONE;
        assert_eq!(key(KeyCode::Char('a'), none), b"a");
        assert_eq!(key(KeyCode::Char('é'), none), "é".as_bytes());
        assert_eq!(key(KeyCode::Char('c'), KeyModifiers::CONTROL), [0x03]);
        assert_eq!(key(KeyCode::Char('b'), KeyModifiers::ALT), b"\x1bb");
        assert_eq!(key(KeyCode::Enter, none), b"\r");
        assert_eq!(key(KeyCode::Backspace, none), [0x7f]);
        assert_eq!(key(KeyCode::Tab, KeyModifiers::SHIFT), b"\x1b[Z");
        assert_eq!(key(KeyCode::Up, none), b"\x1b[A");
        assert_eq!(key(KeyCode::Left, KeyModifiers::CONTROL), b"\x1b[1;5D");
        assert_eq!(key(KeyCode::PageDown, none), b"\x1b[6~");
        assert_eq!(key(KeyCode::Delete, KeyModifiers::SHIFT), b"\x1b[3;2~");
        assert_eq!(key(KeyCode::F(1), none), b"\x1bOP");
        assert_eq!(key(KeyCode::F(5), none), b"\x1b[15~");
        let app = encode_key(
            KeyEvent {
                code: KeyCode::Up,
                modifiers: none,
            },
            TermMode::APP_CURSOR,
        );
        assert_eq!(app.unwrap(), b"\x1bOA");
    }
}
