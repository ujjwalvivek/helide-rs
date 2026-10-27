//! Helide git panel: status, hunks, branches, stashes, commits and remotes, docked in the left
//! sidebar.
//!
//! Everything goes through the `git` command line, so hooks, signing, credentials and config
//! behave exactly as in a terminal. Commands run in the background and report back to the
//! editor through jobs; the panel never blocks the UI. Prompts are disabled so git can never
//! take over the terminal the editor is drawing on.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use helix_core::{pos_at_coords, unicode::width::UnicodeWidthStr, Position, Range, Rope};
use helix_view::{
    align_view,
    document::Document,
    editor::Action,
    graphics::{Modifier, Rect},
    input::{KeyEvent, MouseButton, MouseEvent, MouseEventKind},
    keyboard::{KeyCode, KeyModifiers},
    Align, Editor, Theme,
};
use tokio::io::AsyncWriteExt;
use tui::buffer::Buffer as Surface;

use super::panel_style::Roles;

use crate::{
    compositor::{self, Component},
    ui::{completers, overlay::overlaid, EditorView, Picker, PickerColumn, Prompt, PromptEvent},
};

/// While shown, the panel re-reads status at most this often (on redraws).
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const COMMITS_SHOWN: usize = 12;
/// Changed lines shown under an expanded hunk before it is cut short.
const HUNK_LINES_SHOWN: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Section {
    Conflicts,
    Staged,
    Changes,
    Untracked,
}

impl Section {
    fn title(self) -> &'static str {
        match self {
            Section::Conflicts => "Conflicts",
            Section::Staged => "Staged",
            Section::Changes => "Changes",
            Section::Untracked => "Untracked",
        }
    }
}

/// A file in a section, identified by both, since a file can be staged and changed at once.
type FileKey = (Section, String);

#[derive(Debug, Clone)]
struct Entry {
    section: Section,
    /// Relative to the repository root.
    path: String,
    /// The status letter: M, A, D, R, C, T, U or ?.
    code: char,
}

#[derive(Debug, Clone)]
struct Hunk {
    /// The `@@ -a,b +c,d @@ context` line.
    header: String,
    /// Added and removed lines, for the preview under the hunk.
    changes: Vec<String>,
    /// First line of the hunk in the new version of the file, 1-based.
    new_start: usize,
    /// The file header plus this hunk: a patch `git apply` takes on its own.
    patch: String,
}

#[derive(Debug, Clone)]
struct Commit {
    hash: String,
    subject: String,
    when: String,
}

#[derive(Debug, Clone)]
struct Stash {
    /// `stash@{n}`.
    name: String,
    subject: String,
    when: String,
}

/// One read of the repository, produced off the main thread.
#[derive(Debug, Default)]
pub struct Snapshot {
    repo: Option<PathBuf>,
    branch: String,
    upstream: Option<String>,
    ahead: u32,
    behind: u32,
    entries: Vec<Entry>,
    hunks: HashMap<FileKey, Vec<Hunk>>,
    stashes: Vec<Stash>,
    commits: Vec<Commit>,
}

enum Row {
    Header(Section, usize),
    /// A folder of changed files: section, its path, its depth in the tree.
    Dir(Section, String, u16),
    File(usize),
    /// File entry index, hunk index.
    Hunk(usize, usize),
    /// File entry index, hunk index, changed-line index.
    HunkLine(usize, usize, usize),
    /// How many changed lines of a hunk are not shown.
    HunkMore(usize),
    Gap,
    StashesHeader,
    Stash(usize),
    CommitsHeader,
    Commit(usize),
}

/// What the panel asks its host to do after a key or click.
pub enum GitAction {
    None,
    FocusEditor,
    Hide,
    /// Show this layer (a prompt or picker) on top of the editor.
    Layer(Box<dyn Component>),
}

#[derive(Default)]
pub struct GitPanel {
    snapshot: Snapshot,
    rows: Vec<Row>,
    /// Files whose hunks are listed under them.
    expanded: HashSet<FileKey>,
    selected: usize,
    scroll: usize,
    pub visible: bool,
    pub focused: bool,
    /// A running command, shown in the header.
    busy: Option<String>,
    refreshing: bool,
    last_refresh: Option<Instant>,
    rows_area: Rect,
    area: Rect,
}

impl GitPanel {
    /// Hidden: show and focus. Shown but unfocused: focus. Focused: hide.
    pub fn toggle(&mut self) {
        if self.visible && self.focused {
            self.visible = false;
            self.focused = false;
        } else {
            self.visible = true;
            self.focused = true;
            self.refresh();
        }
    }

    /// Re-reads the repository in the background.
    pub fn refresh(&mut self) {
        if self.refreshing {
            return;
        }
        self.refreshing = true;
        self.last_refresh = Some(Instant::now());
        let cwd = helix_stdx::env::current_working_dir();
        let expanded: Vec<FileKey> = self.expanded.iter().cloned().collect();
        tokio::spawn(async move {
            let snapshot = read_snapshot(&cwd, &expanded).await;
            crate::job::dispatch(move |_, compositor| {
                if let Some(editor_view) = compositor.find::<EditorView>() {
                    editor_view.git.apply(snapshot);
                }
            })
            .await;
        });
    }

    /// Refreshes now, even if a refresh is already on its way (its result may be stale).
    fn force_refresh(&mut self) {
        self.refreshing = false;
        self.refresh();
    }

    fn apply(&mut self, snapshot: Snapshot) {
        let selected = self.selected_key();
        self.snapshot = snapshot;
        self.refreshing = false;
        let present: HashSet<FileKey> = self
            .snapshot
            .entries
            .iter()
            .map(|entry| (entry.section, entry.path.clone()))
            .collect();
        self.expanded.retain(|key| present.contains(key));
        self.rebuild_rows();
        // Stay on the same item if it is still listed.
        if let Some(key) = selected {
            if let Some(index) =
                (0..self.rows.len()).find(|&i| self.row_key(i) == Some(key.clone()))
            {
                self.selected = index;
            }
        }
        self.clamp_selection();
    }

    fn rebuild_rows(&mut self) {
        let snapshot = &self.snapshot;
        let mut rows = Vec::new();
        for section in [
            Section::Conflicts,
            Section::Staged,
            Section::Changes,
            Section::Untracked,
        ] {
            let files: Vec<usize> = (0..snapshot.entries.len())
                .filter(|&i| snapshot.entries[i].section == section)
                .collect();
            if files.is_empty() {
                continue;
            }
            if !rows.is_empty() {
                rows.push(Row::Gap);
            }
            rows.push(Row::Header(section, files.len()));
            // A tree, like Zed's git panel: files sorted by path, with a row for each folder
            // the first time it is reached.
            let mut files = files;
            files.sort_by(|a, b| snapshot.entries[*a].path.cmp(&snapshot.entries[*b].path));
            let mut open: Vec<&str> = Vec::new();
            for file in files {
                let path = snapshot.entries[file].path.as_str();
                let parts: Vec<&str> = path.split('/').collect();
                let dirs = &parts[..parts.len() - 1];
                let shared = open.iter().zip(dirs).take_while(|(a, b)| a == b).count();
                open.truncate(shared);
                for (depth, dir) in dirs.iter().enumerate().skip(shared) {
                    open.push(dir);
                    rows.push(Row::Dir(section, dirs[..=depth].join("/"), depth as u16));
                }
                rows.push(Row::File(file));
                let entry = &snapshot.entries[file];
                let key = (entry.section, entry.path.clone());
                if !self.expanded.contains(&key) {
                    continue;
                }
                for (h, hunk) in snapshot.hunks.get(&key).into_iter().flatten().enumerate() {
                    rows.push(Row::Hunk(file, h));
                    let shown = hunk.changes.len().min(HUNK_LINES_SHOWN);
                    rows.extend((0..shown).map(|l| Row::HunkLine(file, h, l)));
                    if hunk.changes.len() > shown {
                        rows.push(Row::HunkMore(hunk.changes.len() - shown));
                    }
                }
            }
        }
        if !snapshot.stashes.is_empty() {
            rows.push(Row::Gap);
            rows.push(Row::StashesHeader);
            rows.extend((0..snapshot.stashes.len()).map(Row::Stash));
        }
        if !snapshot.commits.is_empty() {
            rows.push(Row::Gap);
            rows.push(Row::CommitsHeader);
            rows.extend((0..snapshot.commits.len()).map(Row::Commit));
        }
        self.rows = rows;
    }

    /// A stable identity for row `index`, so the selection survives a refresh.
    fn row_key(&self, index: usize) -> Option<String> {
        let snapshot = &self.snapshot;
        match self.rows.get(index)? {
            Row::Dir(section, path, _) => Some(format!("dir {section:?} {path}")),
            Row::File(i) => {
                let entry = &snapshot.entries[*i];
                Some(format!("file {:?} {}", entry.section, entry.path))
            }
            Row::Hunk(i, h) => {
                let entry = &snapshot.entries[*i];
                Some(format!("hunk {:?} {} {h}", entry.section, entry.path))
            }
            Row::Stash(i) => Some(format!("stash {}", snapshot.stashes[*i].name)),
            Row::Commit(i) => Some(format!("commit {}", snapshot.commits[*i].hash)),
            _ => None,
        }
    }

    fn selected_key(&self) -> Option<String> {
        self.row_key(self.selected)
    }

    fn selectable(row: &Row) -> bool {
        matches!(
            row,
            Row::Dir(..) | Row::File(_) | Row::Hunk(..) | Row::Stash(_) | Row::Commit(_)
        )
    }

    /// Keeps the selection on a selectable row.
    fn clamp_selection(&mut self) {
        if self.rows.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected = self.selected.min(self.rows.len() - 1);
        if !Self::selectable(&self.rows[self.selected]) {
            self.step(1);
            if !Self::selectable(&self.rows[self.selected]) {
                self.step(-1);
            }
        }
    }

    /// Moves the selection `delta` selectable rows, stopping at the ends.
    fn step(&mut self, delta: isize) {
        let dir = if delta < 0 { -1 } else { 1 };
        let mut remaining = delta.unsigned_abs();
        let mut index = self.selected as isize;
        let mut last_good = self
            .rows
            .get(self.selected)
            .filter(|row| Self::selectable(row))
            .map(|_| self.selected);
        while remaining > 0 {
            index += dir;
            if index < 0 || index as usize >= self.rows.len() {
                break;
            }
            if Self::selectable(&self.rows[index as usize]) {
                last_good = Some(index as usize);
                remaining -= 1;
            }
        }
        if let Some(index) = last_good {
            self.selected = index;
        } else if let Some(first) = self.rows.iter().position(Self::selectable) {
            self.selected = first;
        }
    }

    fn selected_row(&self) -> Option<&Row> {
        self.rows.get(self.selected)
    }

    fn selected_entry(&self) -> Option<&Entry> {
        match self.selected_row()? {
            Row::File(i) | Row::Hunk(i, _) => self.snapshot.entries.get(*i),
            _ => None,
        }
    }

    fn selected_hunk(&self) -> Option<(&Entry, &Hunk)> {
        let Row::Hunk(i, h) = self.selected_row()? else {
            return None;
        };
        let entry = self.snapshot.entries.get(*i)?;
        let hunk = self
            .snapshot
            .hunks
            .get(&(entry.section, entry.path.clone()))?
            .get(*h)?;
        Some((entry, hunk))
    }

    fn selected_stash(&self) -> Option<&Stash> {
        match self.selected_row()? {
            Row::Stash(i) => self.snapshot.stashes.get(*i),
            _ => None,
        }
    }

    fn selected_commit(&self) -> Option<&Commit> {
        match self.selected_row()? {
            Row::Commit(i) => self.snapshot.commits.get(*i),
            _ => None,
        }
    }

    /// What the frame's top border shows on the right: the running command, or the branch and
    /// its distance from upstream. The flag is true while a command runs.
    pub fn meta(&self) -> (String, bool) {
        if let Some(busy) = &self.busy {
            return (format!("{busy}…"), true);
        }
        if self.snapshot.repo.is_none() {
            return (String::new(), false);
        }
        let mut branch = format!("\u{e725} {}", self.snapshot.branch);
        if self.snapshot.upstream.is_some() {
            if self.snapshot.ahead > 0 {
                branch.push_str(&format!(" ↑{}", self.snapshot.ahead));
            }
            if self.snapshot.behind > 0 {
                branch.push_str(&format!(" ↓{}", self.snapshot.behind));
            }
        }
        (branch, false)
    }

    /// What the frame's bottom border shows: how many files changed.
    pub fn footer(&self) -> Option<String> {
        self.snapshot.repo.as_ref()?;
        let files: HashSet<&str> = self
            .snapshot
            .entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect();
        Some(match files.len() {
            0 => "clean".to_string(),
            1 => "1 file".to_string(),
            n => format!("{n} files"),
        })
    }

    /// (rows, first row shown, rows that fit), for the frame's scrollbar.
    pub fn scroll_state(&self) -> (usize, usize, usize) {
        (self.rows.len(), self.scroll, self.rows_area.height as usize)
    }

    pub fn contains(&self, column: u16, row: u16) -> bool {
        self.visible
            && column >= self.area.x
            && column < self.area.right()
            && row >= self.area.y
            && row < self.area.bottom()
    }

    pub fn render(&mut self, area: Rect, surface: &mut Surface, theme: &Theme) {
        self.area = area;
        if self
            .last_refresh
            .is_none_or(|at| at.elapsed() > REFRESH_INTERVAL)
        {
            self.refresh();
        }
        if area.width < 8 || area.height < 1 {
            return;
        }
        let roles = Roles::new(theme);
        let inner_width = area.width.saturating_sub(2) as usize;
        let rows_area = area;
        self.rows_area = rows_area;
        let height = rows_area.height as usize;
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if height > 0 && self.selected >= self.scroll + height {
            self.scroll = self.selected + 1 - height;
        }

        // Empty states say what is going on, in a sentence.
        let empty = match &self.snapshot.repo {
            None if self.refreshing => Some("Reading the repository."),
            None => Some("Not a git repository."),
            Some(_) if self.snapshot.entries.is_empty() => Some("Nothing to commit."),
            Some(_) => None,
        };
        let mut rows_y = rows_area.y;
        if let Some(empty) = empty {
            surface.set_stringn(area.x + 1, rows_area.y, empty, inner_width, roles.muted);
            rows_y += 1;
        }
        let snapshot = &self.snapshot;

        for (i, row) in self.rows.iter().enumerate().skip(self.scroll) {
            let y = rows_y + (i - self.scroll) as u16;
            if y >= rows_area.bottom() {
                break;
            }
            let line = Rect::new(rows_area.x, y, rows_area.width, 1);
            if i == self.selected {
                roles.select_row(surface, line, self.focused);
            }
            match row {
                Row::Header(section, count) => {
                    let label = section.title().to_uppercase();
                    surface.set_string(line.x + 1, y, &label, roles.label());
                    let x = line.x + 2 + label.width() as u16;
                    surface.set_string(x, y, &count.to_string(), roles.faint);
                }
                Row::StashesHeader => {
                    surface.set_string(line.x + 1, y, "STASHES", roles.label());
                }
                Row::CommitsHeader => {
                    surface.set_string(line.x + 1, y, "COMMITS", roles.label());
                }
                Row::Gap => {}
                Row::Dir(_, path, depth) => {
                    let x = line.x + 1 + depth;
                    let name = path.rsplit('/').next().unwrap_or(path);
                    let (glyph, color) = super::file_tree::icon(name, true, true);
                    surface.set_string(x, y, glyph, roles.text.fg(color));
                    let room = (line.right() as usize).saturating_sub(x as usize + 3);
                    surface.set_stringn(x + 2, y, name, room, roles.muted);
                }
                Row::File(index) => {
                    let entry = &snapshot.entries[*index];
                    let code_style = match entry.code {
                        'A' | '?' => roles.plus,
                        'D' => roles.minus,
                        'U' => roles.error,
                        _ => roles.delta,
                    };
                    let depth = entry.path.matches('/').count() as u16;
                    let name = entry.path.rsplit('/').next().unwrap_or(&entry.path);
                    let x = line.x + 1 + depth;
                    let (glyph, color) = super::file_tree::icon(name, false, false);
                    surface.set_string(x, y, glyph, roles.text.fg(color));
                    // The status letter sits at the right edge, in its colour.
                    let code_x = line.right().saturating_sub(2);
                    surface.set_string(code_x, y, &entry.code.to_string(), code_style);
                    let room = (code_x as usize).saturating_sub(x as usize + 3);
                    let name_style = if entry.code == 'D' {
                        roles.muted.add_modifier(Modifier::CROSSED_OUT)
                    } else {
                        roles.text
                    };
                    surface.set_stringn(x + 2, y, name, room, name_style);
                }
                Row::Hunk(file, h) => {
                    let entry = &snapshot.entries[*file];
                    if let Some(hunk) = snapshot
                        .hunks
                        .get(&(entry.section, entry.path.clone()))
                        .and_then(|hunks| hunks.get(*h))
                    {
                        let width = inner_width.saturating_sub(5);
                        surface.set_stringn(line.x + 5, y, &hunk.header, width, roles.muted);
                    }
                }
                Row::HunkLine(file, h, l) => {
                    let entry = &snapshot.entries[*file];
                    if let Some(change) = snapshot
                        .hunks
                        .get(&(entry.section, entry.path.clone()))
                        .and_then(|hunks| hunks.get(*h))
                        .and_then(|hunk| hunk.changes.get(*l))
                    {
                        let style = if change.starts_with('+') {
                            roles.plus
                        } else {
                            roles.minus
                        };
                        let width = inner_width.saturating_sub(6);
                        surface.set_stringn(line.x + 7, y, change, width, style);
                    }
                }
                Row::HunkMore(count) => {
                    let label = format!("{count} more lines");
                    surface.set_stringn(line.x + 7, y, &label, inner_width, roles.faint);
                }
                Row::Stash(index) => {
                    let stash = &snapshot.stashes[*index];
                    let tag = stash.name.trim_start_matches("stash@");
                    surface.set_string(line.x + 3, y, tag, roles.muted);
                    let subject_x = line.x + 3 + tag.width() as u16 + 1;
                    let when_width = stash.when.width();
                    let room =
                        (line.right() as usize).saturating_sub(subject_x as usize + when_width + 2);
                    surface.set_stringn(subject_x, y, &stash.subject, room, roles.text);
                    if room > 0 {
                        let x = line.right().saturating_sub(when_width as u16 + 1);
                        surface.set_string(x, y, &stash.when, roles.faint);
                    }
                }
                Row::Commit(index) => {
                    let commit = &snapshot.commits[*index];
                    let hash = &commit.hash[..commit.hash.len().min(7)];
                    surface.set_string(line.x + 3, y, hash, roles.muted);
                    let when_width = commit.when.width();
                    let subject_x = line.x + 3 + hash.width() as u16 + 1;
                    let room =
                        (line.right() as usize).saturating_sub(subject_x as usize + when_width + 2);
                    surface.set_stringn(subject_x, y, &commit.subject, room, roles.text);
                    if room > 0 {
                        let x = line.right().saturating_sub(when_width as u16 + 1);
                        surface.set_string(x, y, &commit.when, roles.faint);
                    }
                }
            }
        }
    }

    pub fn key(&mut self, key: KeyEvent, editor: &mut Editor) -> GitAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = (self.rows_area.height as isize / 2).max(1);
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.step(1),
            KeyCode::Char('k') | KeyCode::Up => self.step(-1),
            KeyCode::Char('d') if ctrl => self.step(page),
            KeyCode::Char('u') if ctrl => self.step(-page),
            KeyCode::Char('g') | KeyCode::Home => {
                self.selected = 0;
                self.clamp_selection();
            }
            KeyCode::Char('G') | KeyCode::End => self.step(isize::MAX / 2),
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Tab => self.expand(true),
            KeyCode::Char('h') | KeyCode::Left => self.expand(false),
            KeyCode::Char(' ') => self.primary(),
            KeyCode::Char('s') if !ctrl => self.primary(),
            KeyCode::Char('a') => self.toggle_stage_all(),
            KeyCode::Enter => {
                if let Some(action) = self.jump_to_hunk(editor) {
                    return action;
                }
                self.show(Action::NewTab);
            }
            KeyCode::Char('d') => self.show(Action::NewTab),
            KeyCode::Char('v') => self.show(Action::VerticalSplit),
            KeyCode::Char('s') if ctrl => self.show(Action::HorizontalSplit),
            KeyCode::Char('o') => return self.open_file(editor),
            KeyCode::Char('x') => return self.discard_prompt(),
            KeyCode::Char('c') => return self.commit_prompt(false, editor),
            KeyCode::Char('A') => return self.commit_prompt(true, editor),
            KeyCode::Char('b') => self.branch_picker(),
            KeyCode::Char('B') => return self.new_branch_prompt(),
            KeyCode::Char('z') => return self.stash_prompt(),
            KeyCode::Char('Z') => self.run(&["stash", "pop"], "popping", "Stash popped.", true),
            KeyCode::Char('P') => {
                let args: &[&str] = if self.snapshot.upstream.is_some() {
                    &["push"]
                } else {
                    &["push", "--set-upstream", "origin", "HEAD"]
                };
                self.run(args, "pushing", "Pushed.", false);
            }
            KeyCode::Char('p') => self.run(&["pull"], "pulling", "Pulled.", true),
            KeyCode::Char('f') => self.run(
                &["fetch", "--all", "--prune"],
                "fetching",
                "Fetched.",
                false,
            ),
            KeyCode::Char('R') => self.force_refresh(),
            KeyCode::Char('?') => editor.set_status(
                "files: spc stage  l/h hunks  ⏎ diff  v/C-s split  o open  x discard  a all  \
                 hunks: spc stage  x discard  ⏎ jump  stashes: spc pop  ⏎ show  x drop  \
                 c commit  A amend  b branch  B new branch  z stash  Z pop  P push  p pull  \
                 f fetch  R refresh  q hide",
            ),
            KeyCode::Char('q') => return GitAction::Hide,
            KeyCode::Esc => return GitAction::FocusEditor,
            _ => {}
        }
        GitAction::None
    }

    pub fn mouse(&mut self, event: &MouseEvent) -> GitAction {
        match event.kind {
            MouseEventKind::ScrollDown => self.step(3),
            MouseEventKind::ScrollUp => self.step(-3),
            MouseEventKind::Down(MouseButton::Left)
                if event.row >= self.rows_area.y && event.row < self.rows_area.bottom() =>
            {
                let index = self.scroll + (event.row - self.rows_area.y) as usize;
                if self.rows.get(index).is_some_and(Self::selectable) {
                    let again = index == self.selected;
                    self.selected = index;
                    if again {
                        // A second click on a file expands it; on anything else, shows it.
                        match self.selected_row() {
                            Some(Row::File(_)) => self.expand(true),
                            _ => self.show(Action::NewTab),
                        }
                    }
                }
            }
            _ => {}
        }
        GitAction::None
    }

    /// `l` toggles a file's hunks; `h` collapses, from the file or one of its hunks.
    fn expand(&mut self, toggle: bool) {
        let Some(entry) = self.selected_entry() else {
            return;
        };
        if entry.section == Section::Untracked {
            return;
        }
        let key = (entry.section, entry.path.clone());
        let on_hunk = matches!(self.selected_row(), Some(Row::Hunk(..)));
        if toggle && !on_hunk && !self.expanded.contains(&key) {
            self.expanded.insert(key);
            self.force_refresh();
            return;
        }
        if toggle && on_hunk {
            return;
        }
        if self.expanded.remove(&key) {
            // Move the selection back up to the file.
            if let Some(file) = (0..=self.selected).rev().find(|&i| {
                matches!(&self.rows[i], Row::File(f) if self.snapshot.entries[*f].path == key.1
                    && self.snapshot.entries[*f].section == key.0)
            }) {
                self.selected = file;
            }
            let selected = self.selected_key();
            self.rebuild_rows();
            if let Some(selected) = selected {
                if let Some(index) =
                    (0..self.rows.len()).find(|&i| self.row_key(i) == Some(selected.clone()))
                {
                    self.selected = index;
                }
            }
            self.clamp_selection();
        }
    }

    /// Space: stage or unstage a file or hunk, or pop a stash.
    fn primary(&mut self) {
        if let Some(Row::Dir(section, path, _)) = self.selected_row() {
            let (section, path) = (*section, path.clone());
            match section {
                Section::Staged => self.run(
                    &["restore", "--staged", "--", &path],
                    "unstaging",
                    "",
                    false,
                ),
                _ => self.run(&["add", "--", &path], "staging", "", false),
            }
            return;
        }
        if let Some((entry, hunk)) = self.selected_hunk() {
            let (args, busy): (&[&str], _) = match entry.section {
                Section::Staged => (&["apply", "--cached", "--reverse", "-"], "unstaging hunk"),
                _ => (&["apply", "--cached", "-"], "staging hunk"),
            };
            let patch = hunk.patch.clone();
            self.run_with_input(args, patch, busy, "", false);
            return;
        }
        if let Some(stash) = self.selected_stash() {
            let name = stash.name.clone();
            self.run(&["stash", "pop", &name], "popping", "Stash popped.", true);
            return;
        }
        let Some(entry) = self.selected_entry() else {
            return;
        };
        let path = entry.path.clone();
        match entry.section {
            Section::Staged => self.run(
                &["restore", "--staged", "--", &path],
                "unstaging",
                "",
                false,
            ),
            _ => self.run(&["add", "--", &path], "staging", "", false),
        }
    }

    fn toggle_stage_all(&mut self) {
        let unstaged = self
            .snapshot
            .entries
            .iter()
            .any(|entry| entry.section != Section::Staged);
        if unstaged {
            self.run(&["add", "--all"], "staging", "", false);
        } else {
            self.run(&["reset", "--quiet"], "unstaging", "", false);
        }
    }

    /// Enter on a hunk opens its file at the hunk.
    fn jump_to_hunk(&self, editor: &mut Editor) -> Option<GitAction> {
        let (entry, hunk) = self.selected_hunk()?;
        let repo = self.snapshot.repo.as_ref()?;
        let path = repo.join(&entry.path);
        let line = hunk.new_start.saturating_sub(1);
        Some(match editor.open(&path, Action::Replace) {
            Ok(_) => {
                let (view, doc) = current!(editor);
                let pos = pos_at_coords(doc.text().slice(..), Position::new(line, 0), true);
                doc.set_selection(view.id, Range::point(pos).into());
                align_view(doc, view, Align::Center);
                GitAction::FocusEditor
            }
            Err(err) => {
                editor.set_error(format!("cannot open {}: {err}", entry.path));
                GitAction::None
            }
        })
    }

    fn open_file(&self, editor: &mut Editor) -> GitAction {
        let (Some(repo), Some(entry)) = (&self.snapshot.repo, self.selected_entry()) else {
            return GitAction::None;
        };
        if entry.code == 'D' {
            return GitAction::None;
        }
        match editor.open(&repo.join(&entry.path), Action::Replace) {
            Ok(_) => GitAction::FocusEditor,
            Err(err) => {
                editor.set_error(format!("cannot open {}: {err}", entry.path));
                GitAction::None
            }
        }
    }

    /// Opens the selected file's diff, a stash, or a commit, in a new tab or split.
    fn show(&mut self, action: Action) {
        let Some(repo) = self.snapshot.repo.clone() else {
            return;
        };
        let (args, label): (Vec<&str>, String) = if let Some(entry) = self.selected_entry() {
            let args = match entry.section {
                Section::Staged => vec!["diff", "--cached", "--", &entry.path],
                Section::Untracked => vec!["diff", "--no-index", "--", "/dev/null", &entry.path],
                _ => vec!["diff", "--", &entry.path],
            };
            (args, format!("diff {}", entry.path))
        } else if let Some(stash) = self.selected_stash() {
            (
                vec![
                    "stash",
                    "show",
                    "--patch",
                    "--include-untracked",
                    &stash.name,
                ],
                format!("show {}", stash.name),
            )
        } else if let Some(commit) = self.selected_commit() {
            (
                vec!["show", "--stat", "--patch", &commit.hash],
                format!("show {}", &commit.hash[..commit.hash.len().min(7)]),
            )
        } else {
            return;
        };
        let args: Vec<String> = args.into_iter().map(String::from).collect();

        tokio::spawn(async move {
            // `diff --no-index` exits with 1 when there are differences.
            let output = git(&repo, &args, None).await;
            crate::job::dispatch(move |editor, _| match output {
                Ok(out) if !out.stdout.is_empty() => {
                    open_scratch(editor, &label, &out.stdout, "diff", action);
                }
                Ok(out) if !out.success && !out.stderr.is_empty() => {
                    editor.set_error(last_line(&out.stderr))
                }
                Ok(_) => editor.set_status(format!("{label}: no changes.")),
                Err(err) => editor.set_error(format!("git: {err}")),
            })
            .await;
        });
    }

    fn discard_prompt(&mut self) -> GitAction {
        if let Some((entry, hunk)) = self.selected_hunk() {
            if entry.section != Section::Changes {
                return GitAction::None;
            }
            let label = format!("discard this hunk of {}? [y/N]: ", entry.path);
            let patch = hunk.patch.clone();
            return confirm(label, move |cx| {
                run_from_prompt(
                    cx,
                    args(&["apply", "--reverse", "-"]),
                    Some(patch.clone()),
                    "discarding",
                    "Hunk discarded.",
                    true,
                )
            });
        }
        if let Some(stash) = self.selected_stash() {
            let name = stash.name.clone();
            let label = format!("drop {name} ({})? [y/N]: ", stash.subject);
            return confirm(label, move |cx| {
                run_from_prompt(
                    cx,
                    args(&["stash", "drop", &name]),
                    None,
                    "dropping",
                    "Stash dropped.",
                    false,
                )
            });
        }
        let Some(entry) = self.selected_entry().cloned() else {
            return GitAction::None;
        };
        let what = if entry.section == Section::Untracked {
            "delete untracked"
        } else {
            "discard changes to"
        };
        let label = format!("{what} {}? [y/N]: ", entry.path);
        confirm(label, move |cx| {
            let path = entry.path.as_str();
            let list: &[&str] = match entry.section {
                Section::Untracked => &["clean", "--force", "--", path],
                Section::Staged => &["restore", "--staged", "--worktree", "--", path],
                _ => &["restore", "--", path],
            };
            run_from_prompt(
                cx,
                args(list),
                None,
                "discarding",
                "Changes discarded.",
                true,
            );
        })
    }

    fn commit_prompt(&mut self, amend: bool, editor: &Editor) -> GitAction {
        if self.snapshot.repo.is_none() {
            return GitAction::None;
        }
        let label = if amend { "amend: " } else { "commit: " };
        let mut prompt = Prompt::new(
            label.into(),
            None,
            completers::none,
            move |cx, input, event| {
                if event != PromptEvent::Validate || input.trim().is_empty() {
                    return;
                }
                let mut list = vec!["commit"];
                if amend {
                    list.push("--amend");
                }
                list.extend(["--message", input]);
                run_from_prompt(cx, args(&list), None, "committing", "Committed.", false);
            },
        );
        if amend {
            if let Some(last) = self.snapshot.commits.first() {
                prompt = prompt.with_line(last.subject.clone(), editor);
            }
        }
        GitAction::Layer(Box::new(prompt))
    }

    fn new_branch_prompt(&mut self) -> GitAction {
        if self.snapshot.repo.is_none() {
            return GitAction::None;
        }
        let prompt = Prompt::new(
            "new branch: ".into(),
            None,
            completers::none,
            move |cx, input, event| {
                let name = input.trim();
                if event != PromptEvent::Validate || name.is_empty() {
                    return;
                }
                run_from_prompt(
                    cx,
                    args(&["switch", "--create", name]),
                    None,
                    "creating branch",
                    "Switched to the new branch.",
                    false,
                );
            },
        );
        GitAction::Layer(Box::new(prompt))
    }

    fn stash_prompt(&mut self) -> GitAction {
        if self.snapshot.repo.is_none() {
            return GitAction::None;
        }
        let prompt = Prompt::new(
            "stash message (optional): ".into(),
            None,
            completers::none,
            move |cx, input, event| {
                if event != PromptEvent::Validate {
                    return;
                }
                let mut list = vec!["stash", "push", "--include-untracked"];
                if !input.trim().is_empty() {
                    list.extend(["--message", input.trim()]);
                }
                run_from_prompt(cx, args(&list), None, "stashing", "Stashed.", true);
            },
        );
        GitAction::Layer(Box::new(prompt))
    }

    /// Lists local and remote branches, most recent first, and switches to the one picked.
    fn branch_picker(&mut self) {
        let Some(repo) = self.snapshot.repo.clone() else {
            return;
        };
        tokio::spawn(async move {
            let output = git(
                &repo,
                &args(&[
                    "for-each-ref",
                    "--sort=-committerdate",
                    "--format=%(HEAD)%1f%(refname)%1f%(refname:short)%1f%(committerdate:relative)%1f%(subject)",
                    "refs/heads",
                    "refs/remotes",
                ]),
                None,
            )
            .await;
            crate::job::dispatch(move |editor, compositor| {
                let output = match output {
                    Ok(out) if out.success => out.stdout,
                    Ok(out) => return editor.set_error(last_line(&out.stderr)),
                    Err(err) => return editor.set_error(format!("git: {err}")),
                };
                let branches = parse_branches(&output);
                if branches.is_empty() {
                    return editor.set_status("No branches yet.");
                }
                compositor.push(Box::new(overlaid(branch_picker(branches))));
            })
            .await;
        });
    }

    /// Runs git in the background, then reports and refreshes. `reload` re-reads unmodified open
    /// buffers afterwards, for commands that change files on disk.
    fn run(&mut self, list: &[&str], busy: &str, done: &str, reload: bool) {
        let Some(repo) = self.snapshot.repo.clone() else {
            return;
        };
        self.busy = Some(busy.to_string());
        spawn_git(repo, args(list), None, done.to_string(), reload);
    }

    fn run_with_input(
        &mut self,
        list: &[&str],
        input: String,
        busy: &str,
        done: &str,
        reload: bool,
    ) {
        let Some(repo) = self.snapshot.repo.clone() else {
            return;
        };
        self.busy = Some(busy.to_string());
        spawn_git(repo, args(list), Some(input), done.to_string(), reload);
    }
}

#[derive(Debug, Clone)]
struct Branch {
    /// What the picker shows and matches: the short ref name, marked when checked out.
    label: String,
    when: String,
    subject: String,
    /// The `git switch` arguments that check it out.
    switch: Vec<String>,
}

fn parse_branches(output: &str) -> Vec<Branch> {
    let rows: Vec<Vec<&str>> = output
        .lines()
        .map(|line| line.split('\x1f').collect())
        .filter(|fields: &Vec<&str>| fields.len() == 5)
        .collect();
    let locals: HashSet<&str> = rows
        .iter()
        .filter(|f| f[1].starts_with("refs/heads/"))
        .map(|f| f[2])
        .collect();
    rows.iter()
        .filter(|f| !f[1].ends_with("/HEAD"))
        .map(|f| {
            let (head, refname, short) = (f[0] == "*", f[1], f[2]);
            let switch = if refname.starts_with("refs/heads/") {
                vec!["switch".into(), short.into()]
            } else {
                // A remote branch: switch to the local one if it exists, else start tracking it.
                let local = short.split_once('/').map_or(short, |(_, name)| name);
                if locals.contains(local) {
                    vec!["switch".into(), local.into()]
                } else {
                    vec!["switch".into(), "--track".into(), short.into()]
                }
            };
            Branch {
                label: if head {
                    format!("* {short}")
                } else {
                    format!("  {short}")
                },
                when: short_age(f[3]),
                subject: f[4].to_string(),
                switch,
            }
        })
        .collect()
}

fn branch_picker(branches: Vec<Branch>) -> Picker<Branch, ()> {
    let columns = [
        PickerColumn::new("branch", |branch: &Branch, _| branch.label.as_str().into()),
        PickerColumn::new("when", |branch: &Branch, _| branch.when.as_str().into()),
        PickerColumn::new("subject", |branch: &Branch, _| {
            branch.subject.as_str().into()
        }),
    ];
    Picker::new(columns, 0, branches, (), |cx, branch, _action| {
        run_from_prompt(
            cx,
            branch.switch.clone(),
            None,
            "switching",
            "Switched branch.",
            true,
        );
    })
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|arg| arg.to_string()).collect()
}

/// A y/N prompt that runs `then` on "y".
fn confirm(label: String, then: impl Fn(&mut compositor::Context) + 'static) -> GitAction {
    let prompt = Prompt::new(
        label.into(),
        None,
        completers::none,
        move |cx, input, event| {
            if event == PromptEvent::Validate && input.eq_ignore_ascii_case("y") {
                then(cx);
            }
        },
    );
    GitAction::Layer(Box::new(prompt))
}

/// Starts a git command from a prompt or picker callback, which has no access to the panel.
fn run_from_prompt(
    cx: &mut compositor::Context,
    args: Vec<String>,
    input: Option<String>,
    busy: &'static str,
    done: &'static str,
    reload: bool,
) {
    cx.jobs.callback(async move {
        Ok(crate::job::Callback::EditorCompositor(Box::new(
            move |_, compositor| {
                if let Some(editor_view) = compositor.find::<EditorView>() {
                    if let Some(repo) = editor_view.git.snapshot.repo.clone() {
                        editor_view.git.busy = Some(busy.to_string());
                        spawn_git(repo, args, input, done.to_string(), reload);
                    }
                }
            },
        )))
    });
}

fn spawn_git(repo: PathBuf, args: Vec<String>, input: Option<String>, done: String, reload: bool) {
    tokio::spawn(async move {
        let output = git(&repo, &args, input).await;
        crate::job::dispatch(move |editor, compositor| {
            match output {
                Ok(out) if out.success => {
                    if !done.is_empty() {
                        editor.set_status(done);
                    }
                }
                Ok(out) => {
                    let message = if out.stderr.trim().is_empty() {
                        last_line(&out.stdout)
                    } else {
                        last_line(&out.stderr)
                    };
                    editor.set_error(format!("git {} failed: {message}", args[0]));
                }
                Err(err) => editor.set_error(format!("git: {err}")),
            }
            if reload {
                reload_unmodified(editor);
            }
            if let Some(editor_view) = compositor.find::<EditorView>() {
                editor_view.git.busy = None;
                editor_view.git.force_refresh();
            }
        })
        .await;
    });
}

struct GitOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

async fn git(repo: &Path, args: &[String], input: Option<String>) -> std::io::Result<GitOutput> {
    let mut command = tokio::process::Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Never let git, ssh or an editor prompt on the terminal the editor is drawing on.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env("GIT_PAGER", "cat")
        .env("GIT_OPTIONAL_LOCKS", "0");
    if std::env::var_os("GIT_SSH_COMMAND").is_none() {
        command.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    let mut child = command.spawn()?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin.write_all(input.as_bytes()).await?;
        drop(stdin);
    }
    let output = child.wait_with_output().await?;
    Ok(GitOutput {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

async fn read_snapshot(cwd: &Path, expanded: &[FileKey]) -> Snapshot {
    let Ok(root) = git(cwd, &args(&["rev-parse", "--show-toplevel"]), None).await else {
        return Snapshot::default();
    };
    if !root.success {
        return Snapshot::default();
    }
    let repo = PathBuf::from(root.stdout.trim());

    let status_args = args(&[
        "status",
        "--porcelain=v2",
        "--branch",
        "-z",
        "--untracked-files=all",
    ]);
    let log_args = args(&[
        "log",
        &format!("-n{COMMITS_SHOWN}"),
        "--format=%H%x1f%s%x1f%cr",
    ]);
    let stash_args = args(&["stash", "list", "--format=%gd%x1f%s%x1f%cr"]);
    let (status, log, stashes) = tokio::join!(
        git(&repo, &status_args, None),
        git(&repo, &log_args, None),
        git(&repo, &stash_args, None),
    );

    let mut snapshot = Snapshot::default();
    if let Ok(status) = status {
        parse_status(&status.stdout, &mut snapshot);
    }
    if let Ok(log) = log {
        snapshot.commits = parse_records(&log.stdout)
            .map(|[hash, subject, when]| Commit {
                hash,
                subject,
                when: short_age(&when),
            })
            .collect();
    }
    if let Ok(stashes) = stashes {
        snapshot.stashes = parse_records(&stashes.stdout)
            .map(|[name, subject, when]| Stash {
                name,
                subject,
                when: short_age(&when),
            })
            .collect();
    }
    for key in expanded {
        let (section, path) = key;
        let diff_args = match section {
            Section::Staged => args(&["diff", "--cached", "--", path]),
            Section::Untracked => continue,
            _ => args(&["diff", "--", path]),
        };
        if let Ok(diff) = git(&repo, &diff_args, None).await {
            snapshot
                .hunks
                .insert(key.clone(), parse_hunks(&diff.stdout));
        }
    }
    snapshot.repo = Some(repo);
    snapshot
}

/// Lines of three `\x1f`-separated fields.
fn parse_records(output: &str) -> impl Iterator<Item = [String; 3]> + '_ {
    output.lines().filter_map(|line| {
        let mut parts = line.split('\x1f');
        Some([
            parts.next()?.to_string(),
            parts.next()?.to_string(),
            parts.next()?.to_string(),
        ])
    })
}

/// Parses `git status --porcelain=v2 --branch -z` output.
fn parse_status(output: &str, snapshot: &mut Snapshot) {
    let mut fields = output.split('\0');
    while let Some(record) = fields.next() {
        if let Some(header) = record.strip_prefix("# ") {
            if let Some(head) = header.strip_prefix("branch.head ") {
                snapshot.branch = head.to_string();
            } else if let Some(upstream) = header.strip_prefix("branch.upstream ") {
                snapshot.upstream = Some(upstream.to_string());
            } else if let Some(ab) = header.strip_prefix("branch.ab ") {
                let mut parts = ab.split(' ');
                snapshot.ahead = parts.next().and_then(|a| a[1..].parse().ok()).unwrap_or(0);
                snapshot.behind = parts.next().and_then(|b| b[1..].parse().ok()).unwrap_or(0);
            }
            continue;
        }
        let mut parts = record.splitn(2, ' ');
        let kind = parts.next().unwrap_or_default();
        let rest = parts.next().unwrap_or_default();
        let push = |snapshot: &mut Snapshot, section, path: &str, code| {
            snapshot.entries.push(Entry {
                section,
                path: path.to_string(),
                code,
            })
        };
        match kind {
            // 1 XY sub mH mI mW hH hI path
            // 2 XY sub mH mI mW hH hI Xscore path \0 origPath
            "1" | "2" => {
                let field_count = if kind == "1" { 8 } else { 9 };
                let mut parts = rest.splitn(field_count, ' ');
                let xy: Vec<char> = parts.next().unwrap_or("..").chars().collect();
                let path = parts.last().unwrap_or_default();
                if kind == "2" {
                    // The original path follows as its own NUL-separated field.
                    fields.next();
                }
                let (x, y) = (
                    xy.first().copied().unwrap_or('.'),
                    xy.get(1).copied().unwrap_or('.'),
                );
                if x != '.' {
                    push(snapshot, Section::Staged, path, x);
                }
                if y != '.' {
                    push(snapshot, Section::Changes, path, y);
                }
            }
            // u XY sub m1 m2 m3 mW h1 h2 h3 path
            "u" => {
                let path = rest.splitn(10, ' ').last().unwrap_or_default();
                push(snapshot, Section::Conflicts, path, 'U');
            }
            "?" => push(snapshot, Section::Untracked, rest, '?'),
            _ => {}
        }
    }
}

/// Splits one file's `git diff` output into hunks, each a patch `git apply` takes on its own.
fn parse_hunks(diff: &str) -> Vec<Hunk> {
    let mut lines = diff.split_inclusive('\n').peekable();
    let mut file_header = String::new();
    while let Some(line) = lines.peek() {
        if line.starts_with("@@") {
            break;
        }
        file_header.push_str(line);
        lines.next();
    }
    let mut hunks = Vec::new();
    while let Some(header) = lines.next() {
        if !header.starts_with("@@") {
            continue;
        }
        let mut body = String::new();
        let mut changes = Vec::new();
        while let Some(line) = lines.peek() {
            if line.starts_with("@@") || line.starts_with("diff --git") {
                break;
            }
            if (line.starts_with('+') || line.starts_with('-'))
                && !line.starts_with("+++")
                && !line.starts_with("---")
            {
                // The sign, then the code without its indentation: the panel is narrow.
                let (sign, code) = line.split_at(1);
                changes.push(format!("{sign} {}", code.trim()));
            }
            body.push_str(line);
            lines.next();
        }
        // "@@ -a,b +c,d @@": c is where the hunk starts in the new version.
        let new_start = header
            .split_whitespace()
            .find_map(|part| part.strip_prefix('+'))
            .and_then(|range| range.split(',').next())
            .and_then(|start| start.parse().ok())
            .unwrap_or(1);
        hunks.push(Hunk {
            header: header.trim_end().to_string(),
            changes,
            new_start,
            patch: format!("{file_header}{header}{body}"),
        });
    }
    hunks
}

/// "3 hours ago" -> "3h", for the commit and stash lists.
fn short_age(age: &str) -> String {
    let mut parts = age.split_whitespace();
    let (Some(n), Some(unit)) = (parts.next(), parts.next()) else {
        return age.to_string();
    };
    let unit = match unit.trim_end_matches('s').trim_end_matches(',') {
        "second" => "s",
        "minute" => "m",
        "hour" => "h",
        "day" => "d",
        "week" => "w",
        "month" => "mo",
        "year" => "y",
        _ => return age.to_string(),
    };
    format!("{n}{unit}")
}

fn last_line(text: &str) -> String {
    text.lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Opens `text` as a read-only, unnamed buffer labelled `label`.
fn open_scratch(editor: &mut Editor, label: &str, text: &str, language: &str, action: Action) {
    let mut doc = Document::from(
        Rope::from(text),
        None,
        editor.config.clone(),
        editor.syn_loader.clone(),
    );
    let loader = editor.syn_loader.load();
    let _ = doc.set_language_by_language_id(language, &loader);
    doc.readonly = true;
    doc.label = Some(label.to_string());
    editor.new_file_from_document(action, doc);
}

/// Re-reads open buffers that have no unsaved changes, after git changed files on disk.
fn reload_unmodified(editor: &mut Editor) {
    let focus = editor.tree.focus;
    let docs: Vec<_> = editor
        .documents()
        .filter(|doc| doc.path().is_some() && !doc.is_modified())
        .map(|doc| {
            let view = doc.selections().keys().next().copied().unwrap_or(focus);
            (doc.id(), view)
        })
        .collect();
    for (doc_id, view_id) in docs {
        if !editor.tree.contains(view_id) {
            continue;
        }
        let trust_full = {
            let doc = doc!(editor, &doc_id);
            editor
                .workspace_trust
                .query(
                    doc.workspace_root(),
                    helix_loader::workspace_trust::TrustQuery::Git,
                )
                .is_trusted()
        };
        let doc = doc_mut!(editor, &doc_id);
        let view = view_mut!(editor, view_id);
        view.sync_changes(doc);
        if let Err(err) = doc.reload(view, &editor.diff_providers, trust_full) {
            log::warn!("git panel: cannot reload {}: {err}", doc.display_name());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain_v2() {
        let output = [
            "# branch.oid 1234",
            "# branch.head helide",
            "# branch.upstream origin/helide",
            "# branch.ab +2 -1",
            "1 M. N... 100644 100644 100644 aaa bbb src/staged.rs",
            "1 .M N... 100644 100644 100644 aaa bbb src/changed file.rs",
            "1 MM N... 100644 100644 100644 aaa bbb both.rs",
            "2 R. N... 100644 100644 100644 aaa bbb R100 new.rs",
            "old.rs",
            "u UU N... 100644 100644 100644 100644 aaa bbb ccc conflict.rs",
            "? untracked.txt",
            "",
        ]
        .join("\0");
        let mut snapshot = Snapshot::default();
        parse_status(&output, &mut snapshot);

        assert_eq!(snapshot.branch, "helide");
        assert_eq!(snapshot.upstream.as_deref(), Some("origin/helide"));
        assert_eq!((snapshot.ahead, snapshot.behind), (2, 1));
        let entries: Vec<(Section, &str, char)> = snapshot
            .entries
            .iter()
            .map(|e| (e.section, e.path.as_str(), e.code))
            .collect();
        assert_eq!(
            entries,
            vec![
                (Section::Staged, "src/staged.rs", 'M'),
                (Section::Changes, "src/changed file.rs", 'M'),
                (Section::Staged, "both.rs", 'M'),
                (Section::Changes, "both.rs", 'M'),
                (Section::Staged, "new.rs", 'R'),
                (Section::Conflicts, "conflict.rs", 'U'),
                (Section::Untracked, "untracked.txt", '?'),
            ]
        );
    }

    #[test]
    fn splits_a_diff_into_standalone_hunks() {
        let diff = "\
diff --git a/f.rs b/f.rs
index 111..222 100644
--- a/f.rs
+++ b/f.rs
@@ -1,3 +1,3 @@ fn a()
 one
-two
+TWO
 three
@@ -10,2 +10,3 @@ fn b()
 ten
+ten and a half
 eleven
";
        let hunks = parse_hunks(diff);
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].header, "@@ -1,3 +1,3 @@ fn a()");
        assert_eq!(hunks[0].changes, vec!["- two", "+ TWO"]);
        assert_eq!(hunks[1].new_start, 10);
        assert_eq!(hunks[1].changes, vec!["+ ten and a half"]);
        // Each patch carries the file header and only its own hunk.
        assert!(hunks[1].patch.starts_with("diff --git a/f.rs b/f.rs\n"));
        assert!(hunks[1].patch.contains("+++ b/f.rs\n@@ -10,2 +10,3 @@"));
        assert!(!hunks[1].patch.contains("TWO"));
    }

    #[test]
    fn parses_branches_and_how_to_switch() {
        let output = [
            "*\x1frefs/heads/main\x1fmain\x1f2 hours ago\x1ftip",
            " \x1frefs/heads/feature\x1ffeature\x1f3 days ago\x1fwip",
            " \x1frefs/remotes/origin/HEAD\x1forigin\x1f2 hours ago\x1ftip",
            " \x1frefs/remotes/origin/feature\x1forigin/feature\x1f3 days ago\x1fwip",
            " \x1frefs/remotes/origin/other\x1forigin/other\x1f1 week ago\x1fnew",
        ]
        .join("\n");
        let branches = parse_branches(&output);
        let summary: Vec<(&str, Vec<&str>)> = branches
            .iter()
            .map(|b| {
                (
                    b.label.as_str(),
                    b.switch.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                ("* main", vec!["switch", "main"]),
                ("  feature", vec!["switch", "feature"]),
                ("  origin/feature", vec!["switch", "feature"]),
                ("  origin/other", vec!["switch", "--track", "origin/other"]),
            ]
        );
        assert_eq!(branches[1].when, "3d");
    }

    #[test]
    fn shortens_ages() {
        assert_eq!(short_age("3 hours ago"), "3h");
        assert_eq!(short_age("1 day ago"), "1d");
        assert_eq!(short_age("2 months ago"), "2mo");
    }
}
