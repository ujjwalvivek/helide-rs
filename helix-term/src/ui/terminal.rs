//! Helide terminal panel: shells and agents running in PTYs, docked under the editor.
//!
//! Each tab is a [`Session`]: a child process on a PTY whose output is parsed by
//! `alacritty_terminal` on its own thread. The panel draws the parsed grid into the editor's
//! surface and, while focused, encodes keys, pastes and mouse events back into the PTY.

use std::{
    borrow::Cow,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc,
    },
    time::{Duration, Instant},
};

use alacritty_terminal::{
    event::{Event as TermEvent, EventListener, WindowSize},
    event_loop::{EventLoop, EventLoopSender, Msg},
    grid::{Dimensions, Scroll},
    index::{Column, Line, Point, Side},
    selection::{Selection, SelectionType},
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

use super::{
    agents::{self, AgentKind, AgentSession, TerminalPane, TerminalTab},
    frame::{Label, TabStrip},
    panel_style::Roles,
};

const SCROLLBACK: usize = 10_000;

/// Forwards terminal events to the panel and wakes the editor so it redraws. Events arrive on
/// the PTY reader thread, so the redraw is requested from inside the editor's runtime, where
/// helix-event keeps its state.
#[derive(Clone)]
struct Listener {
    events: Sender<TermEvent>,
    runtime: Option<tokio::runtime::Handle>,
}

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        let _ = self.events.send(event);
        let _guard = self.runtime.as_ref().map(|runtime| runtime.enter());
        helix_event::request_redraw();
    }
}

/// A program is busy while it writes; this much silence makes it idle again.
const QUIET: Duration = Duration::from_millis(1500);
/// Only a busy stretch this long counts as a finished run worth a notification.
const LONG_RUN: Duration = Duration::from_secs(5);
/// How long after input or a resize the program's output counts as its answer to it.
const ECHO: Duration = Duration::from_secs(1);
/// During a redraw, a gap this long ends it.
const REDRAW_GAP: Duration = Duration::from_millis(300);

struct Session {
    term: Arc<FairMutex<Term<Listener>>>,
    pty: EventLoopSender,
    events: Receiver<TermEvent>,
    /// What the tab strip shows: the program's name.
    name: String,
    /// A name the tab was given by double-clicking it, shown instead of the program's.
    title: Option<String>,
    /// The tab this session belongs to; sessions sharing a tab sit side by side as panes.
    group: u32,
    /// The agent session this tab runs, for agent tabs.
    agent: Option<AgentSession>,
    /// For an agent: its turn as its own records last said, and which conversation that was.
    /// While known, it decides busy and finished instead of the terminal's output.
    turn: Option<(String, agents::Turn)>,
    /// The process the pane started, to ask Claude which conversation it is in.
    pid: Option<u32>,
    /// The command it was started with (empty for the shell), to start it again next time.
    command: Vec<String>,
    /// The title the program last set, and whether it changed since an OpenCode tab last
    /// looked at it.
    program_title: Option<String>,
    title_changed: bool,
    /// When the current busy stretch started, and when the program last wrote.
    busy_since: Option<Instant>,
    last_output: Option<Instant>,
    /// Output before this answers input, not work.
    echo_until: std::cell::Cell<Option<Instant>>,
    /// Output before this is a redraw after a resize; each burst of it moves this on.
    redraw_until: Option<Instant>,
    /// Finished a long run while you were elsewhere; cleared once the tab is looked at.
    attention: bool,
    exited: bool,
    size: (u16, u16),
}

impl Session {
    fn spawn(
        command: &[String],
        name: Option<String>,
        agent: Option<AgentSession>,
        size: (u16, u16),
        cwd: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
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

        let mut env: std::collections::HashMap<String, String> = [
            ("TERM".to_string(), "xterm-256color".to_string()),
            ("COLORTERM".to_string(), "truecolor".to_string()),
        ]
        .into();
        if let Some(agent) = &agent {
            env.extend(agent.env());
        }
        let options = tty::Options {
            shell,
            working_directory: Some(
                cwd.filter(|dir| dir.is_dir())
                    .unwrap_or_else(helix_stdx::env::current_working_dir),
            ),
            drain_on_exit: true,
            env,
            #[cfg(windows)]
            escape_args: false,
        };
        let pty = tty::new(&options, window_size(size), 0)?;
        #[cfg(unix)]
        let pid = Some(pty.child().id());
        #[cfg(not(unix))]
        let pid = None;

        let (tx, events) = mpsc::channel();
        let listener = Listener {
            events: tx,
            runtime: tokio::runtime::Handle::try_current().ok(),
        };
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
            group: 0,
            title: agent.as_ref().and_then(|a| a.name.clone()),
            agent,
            turn: None,
            pid,
            command: command.to_vec(),
            program_title: None,
            title_changed: false,
            busy_since: None,
            last_output: None,
            echo_until: Default::default(),
            redraw_until: None,
            attention: false,
            exited: false,
            size,
        })
    }

    /// Sends what you typed, pasted or clicked. The program's answer to it (an echo, a redraw)
    /// is not work, so output in the next second does not start a run.
    fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        self.echo_until.set(Some(Instant::now() + ECHO));
        self.reply(bytes);
    }

    /// Answers the program's own questions (cursor position, size), which are not input.
    fn reply(&self, bytes: impl Into<Cow<'static, [u8]>>) {
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
        // A resize makes a program redraw everything, which for a long chat takes a while; the
        // redraw lasts while output keeps coming in quick succession.
        self.redraw_until = Some(Instant::now() + ECHO);
    }

    /// Handles what the terminal asked for since the last frame.
    fn drain_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                TermEvent::PtyWrite(text) => self.reply(text.into_bytes()),
                TermEvent::TextAreaSizeRequest(format) => {
                    self.reply(format(window_size(self.size)).into_bytes())
                }
                TermEvent::Wakeup => {
                    let now = Instant::now();
                    if self.redraw_until.is_some_and(|until| now < until) {
                        self.redraw_until = Some(now + REDRAW_GAP);
                        continue;
                    }
                    if self.echo_until.get().is_some_and(|until| now < until) {
                        continue;
                    }
                    self.last_output = Some(now);
                    self.busy_since.get_or_insert(now);
                }
                TermEvent::ChildExit(_) | TermEvent::Exit => self.exited = true,
                TermEvent::Title(title) => {
                    self.title_changed = self.program_title.as_deref() != Some(title.as_str());
                    self.program_title = Some(title);
                }
                _ => {}
            }
        }
    }

    fn label(&self) -> &str {
        &self.name
    }

    fn is_busy(&self) -> bool {
        match &self.turn {
            Some((_, turn)) => turn.busy,
            None => self.busy_since.is_some(),
        }
    }

    /// Ends a busy stretch after a quiet spell. Returns whether it was a long run.
    fn settle(&mut self, now: Instant) -> bool {
        let (Some(since), Some(last)) = (self.busy_since, self.last_output) else {
            return false;
        };
        if now.duration_since(last) < QUIET {
            return false;
        }
        self.busy_since = None;
        last.duration_since(since) >= LONG_RUN
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

/// A tab that finished a long run since the last frame.
pub struct Finished {
    pub name: String,
    /// Whether it was the tab you were looking at.
    pub seen: bool,
}

#[derive(Default)]
pub struct TerminalPanel {
    /// Set into the top border: TERMINAL, AGENTS.
    title: &'static str,
    sessions: Vec<Session>,
    active: usize,
    pub visible: bool,
    pub focused: bool,
    /// Where the terminal grid (below the tab strip) was drawn last frame.
    grid_area: Rect,
    /// Where the whole panel was drawn last frame.
    area: Rect,
    /// Columns each tab label covered on the top edge last frame.
    tab_hits: Vec<(usize, u16, u16)>,
    /// The tab row, scrolled when there are more tabs than fit.
    strip: TabStrip,
    /// A redraw is already scheduled to settle busy tabs.
    tick: Arc<AtomicBool>,
    next_group: u32,
    /// The panes of the shown tab as drawn last frame: (session index, area).
    panes: Vec<(usize, Rect)>,
    /// Text selected with the mouse and not yet handed to the clipboard.
    copied: Option<String>,
}

impl TerminalPanel {
    pub fn new(title: &'static str) -> Self {
        Self {
            title,
            ..Default::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Text the mouse just selected, for the clipboard. Taken once.
    pub fn take_copied(&mut self) -> Option<String> {
        self.copied.take()
    }

    /// Opens a new tab running `command`, or the user's shell when it is empty. `name` labels the
    /// tab; it defaults to the program's name.
    pub fn open(&mut self, command: &[String], name: Option<String>) -> anyhow::Result<()> {
        self.open_session(command, name, None, false)
    }

    /// Opens `command` (a shell when empty) as a new pane beside the focused one.
    pub fn split(&mut self, command: &[String], name: Option<String>) -> anyhow::Result<()> {
        self.open_session(command, name, None, true)
    }

    /// Opens an agent tab on `agent`'s session.
    pub fn open_agent(&mut self, agent: AgentSession) -> anyhow::Result<()> {
        let command = agent.command();
        let name = Some(agent.kind.command().to_string());
        self.open_session(&command, name, Some(agent), false)
    }

    fn open_session(
        &mut self,
        command: &[String],
        name: Option<String>,
        agent: Option<AgentSession>,
        split: bool,
    ) -> anyhow::Result<()> {
        self.open_in(command, name, agent, split, None)
    }

    fn open_in(
        &mut self,
        command: &[String],
        name: Option<String>,
        agent: Option<AgentSession>,
        split: bool,
        cwd: Option<PathBuf>,
    ) -> anyhow::Result<()> {
        let size = if self.grid_area.width > 0 {
            (self.grid_area.width, self.grid_area.height)
        } else {
            (80, 24)
        };
        let mut session = Session::spawn(command, name, agent, size, cwd)?;
        if split && !self.sessions.is_empty() {
            session.group = self.sessions[self.active].group;
            self.active += 1;
            self.sessions.insert(self.active, session);
        } else {
            session.group = self.next_group;
            self.next_group += 1;
            self.sessions.push(session);
            self.active = self.sessions.len() - 1;
        }
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

    /// The tabs, in order, by group.
    fn groups(&self) -> Vec<u32> {
        let mut groups: Vec<u32> = Vec::new();
        for session in &self.sessions {
            if !groups.contains(&session.group) {
                groups.push(session.group);
            }
        }
        groups
    }

    /// The tabs as they are now, to save: each pane's command, current folder and name.
    pub fn tabs(&self) -> Vec<TerminalTab> {
        self.groups()
            .into_iter()
            .map(|group| TerminalTab {
                panes: self
                    .sessions
                    .iter()
                    .filter(|s| s.group == group && !s.exited)
                    .map(|s| TerminalPane {
                        command: s.command.clone(),
                        cwd: s
                            .pid
                            .and_then(|pid| std::fs::read_link(format!("/proc/{pid}/cwd")).ok()),
                        title: s.title.clone(),
                    })
                    .collect(),
            })
            .filter(|tab| !tab.panes.is_empty())
            .collect()
    }

    /// Opens saved tabs, each pane in its folder, under its name. The panel stays hidden; the
    /// caller shows it if it was shown.
    pub fn restore(&mut self, tabs: &[TerminalTab]) -> anyhow::Result<()> {
        for tab in tabs {
            for (n, pane) in tab.panes.iter().enumerate() {
                self.open_in(&pane.command, None, None, n > 0, pane.cwd.clone())?;
                if let Some(session) = self.sessions.get_mut(self.active) {
                    session.title = pane.title.clone();
                }
            }
        }
        self.active = 0;
        self.visible = false;
        self.focused = false;
        Ok(())
    }

    /// The name tab `index` shows: its given name, or its programs' names.
    pub fn tab_title(&self, index: usize) -> Option<String> {
        let group = *self.groups().get(index)?;
        let members: Vec<&Session> = self.sessions.iter().filter(|s| s.group == group).collect();
        Some(
            members
                .iter()
                .find_map(|s| s.title.clone())
                .unwrap_or_else(|| {
                    members
                        .iter()
                        .map(|s| s.label())
                        .collect::<Vec<_>>()
                        .join(" · ")
                }),
        )
    }

    /// Gives tab `index` a name, or takes it away when `name` is empty. An agent tab keeps it
    /// in its saved session, so it comes back with the tab.
    pub fn rename_tab(&mut self, index: usize, name: &str) {
        let Some(&group) = self.groups().get(index) else {
            return;
        };
        let name = (!name.trim().is_empty()).then(|| name.trim().to_string());
        for session in self.sessions.iter_mut().filter(|s| s.group == group) {
            session.title = name.clone();
            if let Some(agent) = session.agent.as_mut() {
                agent.name = name.clone();
            }
        }
    }

    /// Shows tab `index`, focusing its first pane.
    pub fn select(&mut self, index: usize) {
        if let Some(group) = self.groups().get(index) {
            if let Some(first) = self.sessions.iter().position(|s| s.group == *group) {
                self.active = first;
            }
        }
    }

    /// Reorders sessions so their tabs follow `order`, keeping each tab's panes in order and
    /// the focused session focused.
    fn arrange(&mut self, order: &[u32]) {
        let active = self.active;
        let mut indexed: Vec<(usize, Session)> = std::mem::take(&mut self.sessions)
            .into_iter()
            .enumerate()
            .collect();
        indexed.sort_by_key(|(_, session)| {
            order
                .iter()
                .position(|group| *group == session.group)
                .unwrap_or(usize::MAX)
        });
        self.active = indexed
            .iter()
            .position(|(index, _)| *index == active)
            .unwrap_or(0);
        self.sessions = indexed.into_iter().map(|(_, session)| session).collect();
    }

    /// The tab whose × is at `column`: labels end in " × ".
    pub fn close_at(&self, column: u16) -> Option<usize> {
        self.tab_hits
            .iter()
            .find(|&&(_, _, end)| end >= 2 && column == end - 2)
            .map(|&(index, _, _)| index)
    }

    /// Closes tab `index`: every pane in it ends.
    pub fn close_tab(&mut self, index: usize) {
        let Some(&group) = self.groups().get(index) else {
            return;
        };
        let shown = self.sessions.get(self.active).map(|s| s.group);
        // Panes before the focused one that go away shift it left.
        let removed_before = self.sessions[..self.active.min(self.sessions.len())]
            .iter()
            .filter(|session| session.group == group)
            .count();
        self.sessions.retain(|session| session.group != group);
        if self.sessions.is_empty() {
            self.active = 0;
            self.visible = false;
            self.focused = false;
        } else if shown == Some(group) {
            self.active = (self.active - removed_before).min(self.sessions.len() - 1);
        } else {
            // The focused pane stays focused.
            self.active -= removed_before;
        }
    }

    /// The index of the tab being shown.
    pub fn shown_tab(&self) -> Option<usize> {
        let group = self.sessions.get(self.active)?.group;
        self.groups().iter().position(|g| *g == group)
    }

    /// Moves tab `from` to position `to`.
    pub fn move_tab(&mut self, from: usize, to: usize) {
        let mut order = self.groups();
        if from >= order.len() || to >= order.len() || from == to {
            return;
        }
        let group = order.remove(from);
        order.insert(to, group);
        self.arrange(&order);
    }

    /// Moves tab `index`'s panes into the shown tab, beside its panes.
    pub fn merge_into_shown(&mut self, index: usize) {
        let order = self.groups();
        let (Some(&source), Some(target)) = (
            order.get(index),
            self.sessions.get(self.active).map(|s| s.group),
        ) else {
            return;
        };
        if source == target {
            return;
        }
        for session in &mut self.sessions {
            if session.group == source {
                session.group = target;
            }
        }
        self.arrange(&order);
    }

    /// The agent sessions of the open tabs, in tab order.
    pub fn agent_sessions(&self) -> Vec<AgentSession> {
        self.sessions
            .iter()
            .filter_map(|session| session.agent.clone())
            .collect()
    }

    /// The OpenCode tabs whose title changed since last asked: (their session id now, the
    /// title). Looking up which session a title names takes OpenCode's own command, so the
    /// caller does it off the main thread and calls `move_agent` with what it found.
    pub fn retitled_opencode(&mut self) -> Vec<(String, String)> {
        self.sessions
            .iter_mut()
            .filter(|s| s.title_changed)
            .filter_map(|s| {
                s.title_changed = false;
                let agent = s.agent.as_ref().filter(|a| a.kind == AgentKind::OpenCode)?;
                Some((agent.id.clone(), s.program_title.clone()?))
            })
            .collect()
    }

    /// Moves the tab on session `from` onto session `to`, unless another tab already holds
    /// `to`. Returns whether it moved.
    pub fn move_agent(&mut self, from: &str, to: &str) -> bool {
        if from == to
            || self
                .sessions
                .iter()
                .any(|s| s.agent.as_ref().is_some_and(|a| a.id == to))
        {
            return false;
        }
        let agent = self
            .sessions
            .iter_mut()
            .filter_map(|s| s.agent.as_mut())
            .find(|a| a.id == from);
        match agent {
            Some(agent) => {
                agent.id = to.to_string();
                true
            }
            None => false,
        }
    }

    /// Moves each Claude tab's saved id onto the conversation its process is in now, so /clear
    /// and /resume are what the tab reopens. Returns whether any changed. A conversation another
    /// tab already holds stays with that tab.
    pub fn follow_agents(&mut self) -> bool {
        let mut changed = false;
        for index in 0..self.sessions.len() {
            let session = &self.sessions[index];
            let (Some(agent), Some(pid)) = (&session.agent, session.pid) else {
                continue;
            };
            if agent.kind != AgentKind::Claude {
                continue;
            }
            let Some(live) = agents::claude_live_session(&agents::self_and_descendants(pid)) else {
                continue;
            };
            let taken = self
                .sessions
                .iter()
                .any(|other| other.agent.as_ref().is_some_and(|a| a.id == live));
            if !taken {
                if let Some(agent) = self.sessions[index].agent.as_mut() {
                    agent.id = live;
                    changed = true;
                }
            }
        }
        changed
    }

    /// The agent session of the tab being shown.
    pub fn active_agent(&self) -> Option<&AgentSession> {
        self.sessions.get(self.active)?.agent.as_ref()
    }

    /// Types `text` into the tab being shown.
    pub fn send_text(&mut self, text: &str) -> bool {
        let Some(session) = self.sessions.get(self.active) else {
            return false;
        };
        session.write(text.as_bytes().to_vec());
        true
    }

    /// Takes in the agents' turns, read from their records, and reports the tabs whose turn
    /// just ended. The first reading for a conversation only sets where it stands, so a tab
    /// restored onto a chat that finished long ago is not reported.
    pub fn apply_turns(&mut self, turns: &[(String, agents::Turn)]) -> Vec<Finished> {
        let mut finished = Vec::new();
        for (index, session) in self.sessions.iter_mut().enumerate() {
            let Some(id) = session.agent.as_ref().map(|agent| agent.id.clone()) else {
                continue;
            };
            let Some((_, turn)) = turns.iter().find(|(t, _)| *t == id) else {
                continue;
            };
            let ended = match &session.turn {
                Some((was, before)) if *was == id => {
                    !turn.busy && turn.ended.is_some() && turn.ended != before.ended
                }
                _ => false,
            };
            session.turn = Some((id, *turn));
            if ended {
                let seen = self.visible && self.focused && index == self.active;
                session.attention = !seen;
                finished.push(Finished {
                    name: session.name.clone(),
                    seen,
                });
            }
        }
        finished
    }

    /// Drains events from every session, drops the ones whose program exited, and reports the
    /// tabs that just finished a long run. Returns (finished, whether any tab closed).
    pub fn update(&mut self) -> (Vec<Finished>, bool) {
        let now = Instant::now();
        let mut finished = Vec::new();
        for (index, session) in self.sessions.iter_mut().enumerate() {
            session.drain_events();
            // An agent whose records can be read is finished when they say so, not when it
            // goes quiet.
            if session.settle(now) && session.turn.is_none() {
                let seen = self.visible && self.focused && index == self.active;
                session.attention = !seen;
                finished.push(Finished {
                    name: session.name.clone(),
                    seen,
                });
            }
        }
        let before = self.sessions.len();
        self.sessions.retain(|session| !session.exited);
        let closed = self.sessions.len() != before;
        if closed {
            self.active = self.active.min(self.sessions.len().saturating_sub(1));
            if self.sessions.is_empty() {
                self.visible = false;
                self.focused = false;
            }
        }
        // A busy tab is settled by a redraw after the quiet spell, even if nothing else draws.
        if self.sessions.iter().any(|s| s.busy_since.is_some())
            && !self.tick.swap(true, Ordering::Relaxed)
        {
            let tick = self.tick.clone();
            tokio::spawn(async move {
                tokio::time::sleep(QUIET).await;
                tick.store(false, Ordering::Relaxed);
                helix_event::request_redraw();
            });
        }
        (finished, closed)
    }

    pub fn render(&mut self, area: Rect, surface: &mut Surface, theme: &Theme) {
        self.area = area;
        if area.height < 2 || self.sessions.is_empty() {
            return;
        }
        if self.focused {
            self.sessions[self.active].attention = false;
        }

        let strip = area.with_height(1);
        self.render_tabs(strip, surface, theme);

        // Inside the box's side borders, below the divider.
        let grid = Rect::new(
            area.x + 1,
            area.y + 1,
            area.width.saturating_sub(2),
            area.height - 1,
        );
        let base = theme.get("ui.background");
        surface.set_style(grid, base);

        // The shown tab's panes, side by side, with a one-column divider between them.
        let group = self.sessions[self.active].group;
        let members: Vec<usize> = (0..self.sessions.len())
            .filter(|&i| self.sessions[i].group == group)
            .collect();
        let count = members.len() as u16;
        let usable = grid.width.saturating_sub(count - 1);
        let width = usable / count.max(1);
        let divider = Roles::new(theme).muted;
        self.panes.clear();
        let mut x = grid.x;
        for (n, &index) in members.iter().enumerate() {
            let w = if n + 1 == members.len() {
                grid.right().saturating_sub(x)
            } else {
                width
            };
            let pane = Rect::new(x, grid.y, w, grid.height);
            self.panes.push((index, pane));
            draw_session(&mut self.sessions[index], pane, surface, base);
            x += w;
            if n + 1 < members.len() {
                for y in grid.top()..grid.bottom() {
                    if let Some(cell) = surface.get_mut(x, y) {
                        cell.set_symbol("│").set_style(divider);
                    }
                }
                x += 1;
            }
        }
        self.grid_area = self
            .panes
            .iter()
            .find(|(index, _)| *index == self.active)
            .map_or(grid, |(_, pane)| *pane);
    }

    /// Sets TERMINAL and one label per tab into the divider row the frame drew above the panel.
    fn render_tabs(&mut self, area: Rect, surface: &mut Surface, theme: &Theme) {
        let roles = Roles::new(theme);
        let title = [Label::new(self.title, roles.label())];
        let shown = self.sessions.get(self.active).map(|s| s.group);
        let groups = self.groups();
        let active = groups.iter().position(|g| Some(*g) == shown);
        let labels: Vec<Label> = groups
            .into_iter()
            .map(|group| {
                let members: Vec<&Session> =
                    self.sessions.iter().filter(|s| s.group == group).collect();
                let names: Vec<String> = match members.iter().find_map(|s| s.title.as_deref()) {
                    Some(title) => vec![short_label(title)],
                    None => members.iter().map(|s| short_label(s.label())).collect(),
                };
                // The shown tab is a filled chip, like the editor's. Status is a glyph, never colour
                // alone: ● working, ✓ finished while away.
                let chip = roles
                    .selected
                    .patch(roles.accent)
                    .add_modifier(Modifier::BOLD);
                let base = if Some(group) == shown {
                    chip
                } else {
                    roles.muted
                };
                let (mark, style) = if members.iter().any(|s| s.is_busy()) {
                    (" ●", base.patch(roles.warning))
                } else if members.iter().any(|s| s.attention) {
                    (" ✓", base.patch(roles.accent).add_modifier(Modifier::BOLD))
                } else {
                    ("", base)
                };
                // The trailing × closes the tab.
                Label::new(format!("{}{mark} ×", names.join(" · ")), style)
            })
            .collect();
        let keys: Vec<u64> = self.groups().into_iter().map(u64::from).collect();
        let hits = self.strip.draw(
            surface,
            area.y,
            area.x,
            area.right() - 1,
            &title,
            &labels,
            &keys,
            active,
            roles.label(),
            Some(roles.label()),
        );
        self.tab_hits = hits;
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
        if line.0 < 0 || line.0 as u16 >= self.grid_area.height || self.grid_area.width == 0 {
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

    /// The tab row, for scrolling it.
    pub fn tab_strip(&mut self) -> &mut TabStrip {
        &mut self.strip
    }

    /// The tab whose label covers `column` on the top edge.
    pub fn tab_at(&self, column: u16) -> Option<usize> {
        self.tab_hits
            .iter()
            .find(|&&(_, start, end)| (start..end).contains(&column))
            .map(|&(index, _, _)| index)
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
            // Typing jumps back to the live screen and drops a mouse selection.
            let mut term = session.term.lock();
            term.selection = None;
            term.scroll_display(Scroll::Bottom);
            drop(term);
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

    /// The text of the tab being shown: up to `history` lines of scrollback, then the screen,
    /// oldest first, with trailing blanks trimmed. Also returns the cursor's index in the list.
    pub fn text(&self, history: usize) -> Option<(Vec<String>, usize)> {
        let session = self.sessions.get(self.active)?;
        let term = session.term.lock();
        let grid = term.grid();
        let first = -(grid.history_size().min(history) as i32);
        let lines: Vec<String> = (first..grid.screen_lines() as i32)
            .map(|line| row_text(&grid[Line(line)], grid.columns()))
            .collect();
        let cursor = (grid.cursor.point.line.0 - first) as usize;
        Some((lines, cursor))
    }

    /// `path:line[:col]` locations in the tab being shown, newest first.
    pub fn locations(&self) -> Vec<Location> {
        let Some((lines, _)) = self.text(500) else {
            return Vec::new();
        };
        let mut found: Vec<Location> = Vec::new();
        for line in lines.iter().rev() {
            for location in parse_locations(line) {
                if !found.contains(&location) {
                    found.push(location);
                }
            }
        }
        found
    }

    /// The command line and output of the last command: from the previous prompt to the
    /// current one, recognised by the prompt's first character (`❯`, `$`, `%`). Falls back to the
    /// visible screen when no earlier prompt is found.
    pub fn last_output(&self) -> Option<String> {
        let (lines, cursor) = self.text(2000)?;
        let cursor = cursor.min(lines.len().saturating_sub(1));
        let marker = lines[cursor].trim_start().chars().next()?;
        let previous = (0..cursor)
            .rev()
            .find(|&i| lines[i].trim_start().starts_with(marker));
        let start =
            previous.unwrap_or_else(|| cursor.saturating_sub(self.grid_area.height as usize));
        let block: Vec<&str> = lines[start..cursor].iter().map(String::as_str).collect();
        let text = block.join("\n").trim_matches('\n').to_string();
        (!text.trim().is_empty()).then_some(text)
    }

    pub fn mouse(&mut self, event: &MouseEvent) -> Option<Location> {
        // Ctrl+click on a `path:line` opens it.
        if let MouseEventKind::Down(MouseButton::Left) = event.kind {
            if event.modifiers.contains(KeyModifiers::CONTROL)
                && event.row > self.area.y
                && !self.sessions.is_empty()
            {
                let (index, pane) = self
                    .pane_at(event.column)
                    .unwrap_or((self.active, self.grid_area));
                let session = &self.sessions[index];
                let term = session.term.lock();
                let grid = term.grid();
                let line = event.row.saturating_sub(pane.y) as i32 - grid.display_offset() as i32;
                let column = event.column.saturating_sub(pane.x) as usize;
                if line < grid.screen_lines() as i32 {
                    let text = row_text(&grid[Line(line)], grid.columns());
                    return location_at(&text, column);
                }
            }
        }
        self.mouse_inner(event);
        None
    }

    /// The pane under `column`: (session index, area).
    fn pane_at(&self, column: u16) -> Option<(usize, Rect)> {
        self.panes
            .iter()
            .find(|(_, pane)| column >= pane.x && column < pane.right())
            .copied()
    }

    fn mouse_inner(&mut self, event: &MouseEvent) {
        // Clicking the tab strip switches tabs.
        if event.row == self.area.y {
            if let MouseEventKind::Down(MouseButton::Left) = event.kind {
                if let Some(i) = self.tab_at(event.column) {
                    self.select(i);
                }
            }
            return;
        }

        // A click focuses the pane under it; events go to that pane.
        let Some((index, grid)) = self.pane_at(event.column) else {
            return;
        };
        if let MouseEventKind::Down(_) = event.kind {
            self.active = index;
        }
        let Some(session) = self.sessions.get(index) else {
            return;
        };
        let col = event.column.saturating_sub(grid.x);
        let row = event.row.saturating_sub(grid.y);
        let mode = session.mode();
        let reporting = mode.intersects(TermMode::MOUSE_MODE) && mode.contains(TermMode::SGR_MOUSE);

        // Dragging selects text, and letting go copies it. Programs that take the mouse
        // themselves (lazygit, OpenCode) still get it; Shift+drag selects over them.
        if !reporting || event.modifiers.contains(KeyModifiers::SHIFT) {
            let mut term = session.term.lock();
            let point = Point::new(
                Line(
                    (row.min(grid.height.saturating_sub(1)) as i32)
                        - term.grid().display_offset() as i32,
                ),
                Column((col as usize).min(term.columns().saturating_sub(1))),
            );
            match event.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    term.selection = Some(Selection::new(SelectionType::Simple, point, Side::Left));
                    return;
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    if let Some(selection) = term.selection.as_mut() {
                        selection.update(point, Side::Right);
                    }
                    return;
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    match term.selection_to_string().filter(|text| !text.is_empty()) {
                        Some(text) => self.copied = Some(text),
                        None => term.selection = None,
                    }
                    return;
                }
                _ => {}
            }
        }

        if reporting {
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

/// Draws a session's screen into `pane`, resizing it to fit.
fn draw_session(session: &mut Session, pane: Rect, surface: &mut Surface, base: Style) {
    session.resize((pane.width, pane.height));
    let term = session.term.lock();
    let content = term.renderable_content();
    let offset = content.display_offset as i32;
    let selection = content.selection;
    for indexed in content.display_iter {
        let line = indexed.point.line.0 + offset;
        let col = indexed.point.column.0 as u16;
        if line < 0 || line as u16 >= pane.height || col >= pane.width {
            continue;
        }
        let Some(cell) = surface.get_mut(pane.x + col, pane.y + line as u16) else {
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
        let mut style = base.patch(cell_style(term_cell));
        if selection.is_some_and(|range| range.contains(indexed.point)) {
            style = style.add_modifier(Modifier::REVERSED);
        }
        cell.set_style(style);
    }
}

/// A place in a file that a program printed, for jumping to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub path: PathBuf,
    pub line: usize,
    pub column: Option<usize>,
}

fn row_text(row: &alacritty_terminal::grid::Row<TermCell>, columns: usize) -> String {
    let mut text = String::with_capacity(columns);
    for column in 0..columns {
        let cell = &row[Column(column)];
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        text.push(cell.c);
    }
    text.trim_end().to_string()
}

/// `path:line[:col]` (rustc, gcc, tsc, eslint, ripgrep...) and Python's `File "path", line N`.
/// Only paths that exist, relative to the working directory or absolute, count.
pub(crate) fn parse_locations(line: &str) -> Vec<Location> {
    let mut found = Vec::new();
    // Python tracebacks.
    if let Some(rest) = line.trim_start().strip_prefix("File \"") {
        if let Some((path, rest)) = rest.split_once('"') {
            let number = rest
                .trim_start_matches(", line ")
                .split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|n| n.parse().ok());
            if let Some(line) = number {
                if let Some(path) = existing(path) {
                    found.push(Location {
                        path,
                        line,
                        column: None,
                    });
                }
            }
        }
    }
    for token in line.split(|c: char| c.is_whitespace() || "()[]<>\"'`,".contains(c)) {
        if let Some(location) = parse_token(token) {
            found.push(location);
        }
    }
    found
}

fn parse_token(token: &str) -> Option<Location> {
    let token = token.trim_end_matches(['.', ':', ';']);
    let mut parts = token.split(':');
    let path = parts.next()?;
    let line: usize = parts.next()?.parse().ok()?;
    let column = parts.next().and_then(|c| c.parse().ok());
    if line == 0 || !(path.contains('.') || path.contains('/')) {
        return None;
    }
    let path = existing(path)?;
    Some(Location { path, line, column })
}

fn existing(path: &str) -> Option<PathBuf> {
    let path = helix_stdx::path::expand_tilde(std::path::Path::new(path));
    let path = helix_stdx::env::current_working_dir().join(path);
    path.is_file().then(|| helix_stdx::path::normalize(path))
}

/// The location under `column` in a line of text, for Ctrl+click.
fn location_at(line: &str, column: usize) -> Option<Location> {
    let mut start = 0;
    for token in line.split_inclusive(|c: char| c.is_whitespace() || "()[]<>\"'`,".contains(c)) {
        let end = start + token.chars().count();
        if (start..end).contains(&column) {
            return parse_token(
                token.trim_end_matches(|c: char| c.is_whitespace() || "()[]<>\"'`,".contains(c)),
            );
        }
        start = end;
    }
    None
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

    /// A tab that writes for several seconds, then goes quiet, is reported finished once; a
    /// short burst is not.
    #[tokio::test(flavor = "multi_thread")]
    async fn reports_long_runs_once_they_go_quiet() {
        let mut panel = TerminalPanel::new("TEST");
        let busy = "for i in 1 2 3 4 5 6 7 8; do echo $i; sleep 1; done; sleep 30".to_string();
        panel
            .open(&["sh".into(), "-c".into(), busy], Some("busy".into()))
            .unwrap();
        panel
            .open(
                &["sh".into(), "-c".into(), "echo hi; sleep 30".into()],
                Some("short".into()),
            )
            .unwrap();
        panel.focused = false;

        let mut finished = Vec::new();
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(12) {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let (done, _) = panel.update();
            finished.extend(done.into_iter().map(|run| (run.name, run.seen)));
        }
        assert_eq!(finished, vec![("busy".to_string(), false)]);
        assert!(panel.sessions[0].attention);
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Vec<u8> {
        encode_key(KeyEvent { code, modifiers }, TermMode::empty()).unwrap()
    }

    /// Closing a tab ends its panes and keeps showing the tab that was shown.
    #[tokio::test(flavor = "multi_thread")]
    async fn closing_tabs_keeps_the_shown_one() {
        let mut panel = TerminalPanel::new("TEST");
        let sleep = |name: &str| {
            (
                vec!["sh".to_string(), "-c".into(), "sleep 30".into()],
                Some(name.to_string()),
            )
        };
        for name in ["a", "b", "c"] {
            let (command, name) = sleep(name);
            panel.open(&command, name).unwrap();
        }
        let (command, name) = sleep("b2");
        panel.select(1);
        panel.split(&command, name).unwrap();
        // Tabs: a | b · b2 | c, showing b2.
        assert_eq!(panel.groups().len(), 3);
        panel.close_tab(0);
        assert_eq!(panel.groups().len(), 2);
        assert_eq!(panel.sessions[panel.active].name, "b2");
        panel.close_tab(0);
        assert_eq!(panel.sessions.len(), 1);
        assert_eq!(panel.sessions[panel.active].name, "c");
        panel.close_tab(0);
        assert!(panel.is_empty() && !panel.visible);
    }

    #[test]
    fn finds_locations_in_compiler_output() {
        // Paths must exist: use this crate's own files, relative to the test's working dir.
        let found = parse_locations("  --> src/ui/terminal.rs:12:5");
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].line, found[0].column), (12, Some(5)));
        let found = parse_locations("error: src/lib.rs:3: something, see Cargo.toml:1");
        assert_eq!(found.len(), 2);
        assert!(parse_locations("version 1.2:3 of nothing.rs:4").is_empty());
        let python = parse_locations(r#"  File "src/lib.rs", line 7, in <module>"#);
        assert_eq!(python[0].line, 7);
        let clicked = location_at("warning at src/lib.rs:9:2 here", 14).unwrap();
        assert_eq!((clicked.line, clicked.column), (9, Some(2)));
        assert!(location_at("warning at src/lib.rs:9:2 here", 2).is_none());
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
