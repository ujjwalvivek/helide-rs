//! Helide git panel: status, staging, commits and remotes, docked in the left sidebar.
//!
//! Everything goes through the `git` command line, so hooks, signing, credentials and config
//! behave exactly as in a terminal. Commands run in the background and report back to the
//! editor through jobs; the panel never blocks the UI. Prompts are disabled so git can never
//! take over the terminal the editor is drawing on.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use helix_core::{unicode::width::UnicodeWidthStr, Rope};
use helix_view::{
    document::Document,
    editor::Action,
    graphics::{Modifier, Rect, Style},
    input::{KeyEvent, MouseButton, MouseEvent, MouseEventKind},
    keyboard::{KeyCode, KeyModifiers},
    Editor, Theme,
};
use tui::buffer::Buffer as Surface;

use crate::{
    compositor::Component,
    ui::{completers, EditorView, Prompt, PromptEvent},
};

/// While shown, the panel re-reads status at most this often (on redraws).
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const COMMITS_SHOWN: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone)]
struct Entry {
    section: Section,
    /// Relative to the repository root.
    path: String,
    /// The status letter: M, A, D, R, C, T, U or ?.
    code: char,
}

#[derive(Debug, Clone)]
struct Commit {
    hash: String,
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
    commits: Vec<Commit>,
}

enum Row {
    Header(Section, usize),
    File(usize),
    Gap,
    CommitsHeader,
    Commit(usize),
}

/// What the panel asks its host to do after a key or click.
pub enum GitAction {
    None,
    FocusEditor,
    Hide,
    /// Show this prompt on top of the editor.
    Prompt(Box<dyn Component>),
}

#[derive(Default)]
pub struct GitPanel {
    snapshot: Snapshot,
    rows: Vec<Row>,
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
        tokio::spawn(async move {
            let snapshot = read_snapshot(&cwd).await;
            crate::job::dispatch(move |_, compositor| {
                if let Some(editor_view) = compositor.find::<EditorView>() {
                    editor_view.git.apply(snapshot);
                }
            })
            .await;
        });
    }

    fn apply(&mut self, snapshot: Snapshot) {
        let selected = self.selected_key();
        self.snapshot = snapshot;
        self.refreshing = false;
        self.rebuild_rows();
        // Stay on the same file or commit if it is still listed.
        if let Some(key) = selected {
            if let Some(index) = self
                .rows
                .iter()
                .position(|row| self.row_key(row) == Some(key.clone()))
            {
                self.selected = index;
            }
        }
        self.clamp_selection();
    }

    fn rebuild_rows(&mut self) {
        let mut rows = Vec::new();
        for section in [
            Section::Conflicts,
            Section::Staged,
            Section::Changes,
            Section::Untracked,
        ] {
            let files: Vec<usize> = (0..self.snapshot.entries.len())
                .filter(|&i| self.snapshot.entries[i].section == section)
                .collect();
            if files.is_empty() {
                continue;
            }
            rows.push(Row::Header(section, files.len()));
            rows.extend(files.into_iter().map(Row::File));
        }
        if !self.snapshot.commits.is_empty() {
            rows.push(Row::Gap);
            rows.push(Row::CommitsHeader);
            rows.extend((0..self.snapshot.commits.len()).map(Row::Commit));
        }
        self.rows = rows;
    }

    /// A stable identity for a row, so the selection survives a refresh.
    fn row_key(&self, row: &Row) -> Option<(Option<Section>, String)> {
        match row {
            Row::File(i) => {
                let entry = &self.snapshot.entries[*i];
                Some((Some(entry.section), entry.path.clone()))
            }
            Row::Commit(i) => Some((None, self.snapshot.commits[*i].hash.clone())),
            _ => None,
        }
    }

    fn selected_key(&self) -> Option<(Option<Section>, String)> {
        self.rows
            .get(self.selected)
            .and_then(|row| self.row_key(row))
    }

    fn selectable(row: &Row) -> bool {
        matches!(row, Row::File(_) | Row::Commit(_))
    }

    /// Keeps the selection on a file or commit row.
    fn clamp_selection(&mut self) {
        if self.rows.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected = self.selected.min(self.rows.len() - 1);
        if !Self::selectable(&self.rows[self.selected]) {
            self.step(1);
        }
    }

    /// Moves the selection `delta` selectable rows, stopping at the ends.
    fn step(&mut self, delta: isize) {
        let mut index = self.selected as isize;
        let mut remaining = delta.abs();
        let dir = delta.signum().max(-1);
        let dir = if dir == 0 { 1 } else { dir };
        let mut last_good = if self.rows.get(self.selected).is_some_and(Self::selectable) {
            Some(self.selected)
        } else {
            None
        };
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

    fn selected_entry(&self) -> Option<&Entry> {
        match self.rows.get(self.selected) {
            Some(Row::File(i)) => self.snapshot.entries.get(*i),
            _ => None,
        }
    }

    fn selected_commit(&self) -> Option<&Commit> {
        match self.rows.get(self.selected) {
            Some(Row::Commit(i)) => self.snapshot.commits.get(*i),
            _ => None,
        }
    }

    /// Gives the panel its slice of `area`, returning what is left for the editor.
    pub fn layout(&self, area: Rect) -> Rect {
        if !self.visible {
            return area;
        }
        let width = (area.width / 5).clamp(30, 48);
        if width + 40 > area.width {
            return area;
        }
        area.clip_left(width)
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
        if area.width < 8 || area.height < 4 {
            return;
        }

        let separator = theme.get("ui.window");
        for y in area.top()..area.bottom() {
            if let Some(cell) = surface.get_mut(area.right() - 1, y) {
                cell.set_symbol(tui::symbols::line::VERTICAL)
                    .set_style(separator);
            }
        }
        let area = area.clip_right(1);
        let inner_width = area.width.saturating_sub(2) as usize;
        let text = theme.get("ui.text");
        let dim = theme.get("ui.text.inactive");

        // Header: branch, upstream distance, and whatever is running.
        let header = match &self.snapshot.repo {
            None if self.refreshing && self.last_refresh.is_some() => "reading…".to_string(),
            None => "not a git repository".to_string(),
            Some(_) => {
                let mut header = format!("\u{e725} {}", self.snapshot.branch);
                if self.snapshot.upstream.is_some() {
                    header.push_str(&format!(
                        "  ↑{} ↓{}",
                        self.snapshot.ahead, self.snapshot.behind
                    ));
                }
                header
            }
        };
        surface.set_stringn(
            area.x + 1,
            area.y,
            &header,
            inner_width,
            theme.get("ui.text.directory").add_modifier(Modifier::BOLD),
        );
        if let Some(busy) = &self.busy {
            let label = format!("{busy}…");
            let x = area.right().saturating_sub(label.width() as u16 + 1);
            surface.set_string(x, area.y, &label, theme.get("warning"));
        }

        // Footer: the keys that matter most.
        let footer_y = area.bottom() - 1;
        let footer = "spc stage  ⏎ diff  c commit  P push  ? keys";
        surface.set_stringn(area.x + 1, footer_y, footer, inner_width, dim);

        let rows_area = Rect::new(
            area.x,
            area.y + 2,
            area.width,
            area.height.saturating_sub(3),
        );
        self.rows_area = rows_area;
        let height = rows_area.height as usize;
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if height > 0 && self.selected >= self.scroll + height {
            self.scroll = self.selected + 1 - height;
        }
        if self.snapshot.repo.is_some() && self.rows.is_empty() {
            surface.set_stringn(
                area.x + 1,
                rows_area.y,
                "nothing to commit",
                inner_width,
                dim,
            );
        }

        let selected_style = if self.focused {
            theme.get("ui.menu.selected")
        } else {
            theme.get("ui.cursorline.primary")
        };
        let section_style = text.add_modifier(Modifier::BOLD);

        for (i, row) in self.rows.iter().enumerate().skip(self.scroll).take(height) {
            let y = rows_area.y + (i - self.scroll) as u16;
            let line = Rect::new(rows_area.x, y, rows_area.width, 1);
            let is_selected = i == self.selected;
            if is_selected {
                surface.set_style(line, selected_style);
            }
            let patch = |style: Style| {
                if is_selected {
                    style.patch(selected_style)
                } else {
                    style
                }
            };
            match row {
                Row::Header(section, count) => {
                    let label = format!("{} {count}", section.title());
                    surface.set_stringn(line.x + 1, y, &label, inner_width, section_style);
                }
                Row::CommitsHeader => {
                    surface.set_stringn(line.x + 1, y, "Commits", inner_width, section_style);
                }
                Row::Gap => {}
                Row::File(index) => {
                    let entry = &self.snapshot.entries[*index];
                    let code_style = match entry.code {
                        'A' | '?' => theme.get("diff.plus"),
                        'D' => theme.get("diff.minus"),
                        'R' | 'C' => theme.get("diff.delta.moved"),
                        'U' => theme.get("error"),
                        _ => theme.get("diff.delta"),
                    };
                    surface.set_string(line.x + 2, y, &entry.code.to_string(), patch(code_style));
                    let width = inner_width.saturating_sub(3);
                    let path = shorten_left(&entry.path, width);
                    surface.set_stringn(line.x + 4, y, &path, width, patch(text));
                }
                Row::Commit(index) => {
                    let commit = &self.snapshot.commits[*index];
                    let hash = &commit.hash[..commit.hash.len().min(7)];
                    surface.set_string(line.x + 2, y, hash, patch(theme.get("constant")));
                    let when_width = commit.when.width();
                    let subject_x = line.x + 2 + hash.width() as u16 + 1;
                    let room =
                        (line.right() as usize).saturating_sub(subject_x as usize + when_width + 2);
                    surface.set_stringn(subject_x, y, &commit.subject, room, patch(text));
                    if room > 0 {
                        let x = line.right().saturating_sub(when_width as u16 + 1);
                        surface.set_string(x, y, &commit.when, patch(dim));
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
            KeyCode::Char(' ') => self.toggle_stage(),
            KeyCode::Char('s') if !ctrl => self.toggle_stage(),
            KeyCode::Char('a') => self.toggle_stage_all(),
            KeyCode::Enter | KeyCode::Char('d') => self.show_diff(Action::NewTab),
            KeyCode::Char('v') => self.show_diff(Action::VerticalSplit),
            KeyCode::Char('s') if ctrl => self.show_diff(Action::HorizontalSplit),
            KeyCode::Char('o') | KeyCode::Char('l') => {
                if let (Some(repo), Some(entry)) = (&self.snapshot.repo, self.selected_entry()) {
                    if entry.code != 'D' {
                        let path = repo.join(&entry.path);
                        return match editor.open(&path, Action::Replace) {
                            Ok(_) => GitAction::FocusEditor,
                            Err(err) => {
                                editor.set_error(format!("cannot open {}: {err}", entry.path));
                                GitAction::None
                            }
                        };
                    }
                }
            }
            KeyCode::Char('x') => return self.discard_prompt(),
            KeyCode::Char('c') => return self.commit_prompt(false, editor),
            KeyCode::Char('A') => return self.commit_prompt(true, editor),
            KeyCode::Char('P') => {
                let args: &[&str] = if self.snapshot.upstream.is_some() {
                    &["push"]
                } else {
                    &["push", "--set-upstream", "origin", "HEAD"]
                };
                self.run(args, "pushing", "pushed", false);
            }
            KeyCode::Char('p') => self.run(&["pull"], "pulling", "pulled", true),
            KeyCode::Char('f') => {
                self.run(&["fetch", "--all", "--prune"], "fetching", "fetched", false)
            }
            KeyCode::Char('R') => {
                self.refreshing = false;
                self.refresh();
            }
            KeyCode::Char('?') => editor.set_status(
                "spc/s stage  a all  ⏎ diff in tab  v/C-s in split  o open  x discard  \
                 c commit  A amend  P push  p pull  f fetch  R refresh  q hide",
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
                    if index == self.selected {
                        // A second click on the selected row opens its diff.
                        self.show_diff(Action::NewTab);
                    }
                    self.selected = index;
                }
            }
            _ => {}
        }
        GitAction::None
    }

    fn toggle_stage(&mut self) {
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

    /// Opens the selected file's diff, or the selected commit, in a new tab or split.
    fn show_diff(&mut self, action: Action) {
        let Some(repo) = self.snapshot.repo.clone() else {
            return;
        };
        let (args, label): (Vec<String>, String) = if let Some(entry) = self.selected_entry() {
            let args = match entry.section {
                Section::Staged => vec!["diff", "--cached", "--", &entry.path],
                Section::Untracked => vec!["diff", "--no-index", "--", "/dev/null", &entry.path],
                _ => vec!["diff", "--", &entry.path],
            };
            (
                args.into_iter().map(String::from).collect(),
                format!("diff {}", entry.path),
            )
        } else if let Some(commit) = self.selected_commit() {
            (
                vec![
                    "show".into(),
                    "--stat".into(),
                    "--patch".into(),
                    commit.hash.clone(),
                ],
                format!("show {}", &commit.hash[..commit.hash.len().min(7)]),
            )
        } else {
            return;
        };

        tokio::spawn(async move {
            // `diff --no-index` exits with 1 when there are differences.
            let output = git(&repo, &args).await;
            crate::job::dispatch(move |editor, _| match output {
                Ok(out) if !out.stdout.is_empty() => {
                    open_scratch(editor, &label, &out.stdout, "diff", action);
                }
                Ok(out) if !out.success && !out.stderr.is_empty() => {
                    editor.set_error(last_line(&out.stderr))
                }
                Ok(_) => editor.set_status(format!("{label}: no changes")),
                Err(err) => editor.set_error(format!("git: {err}")),
            })
            .await;
        });
    }

    fn discard_prompt(&mut self) -> GitAction {
        let Some(entry) = self.selected_entry().cloned() else {
            return GitAction::None;
        };
        let what = if entry.section == Section::Untracked {
            "delete untracked"
        } else {
            "discard changes to"
        };
        let label = format!("{what} {}? [y/N]: ", entry.path);
        let prompt = Prompt::new(
            label.into(),
            None,
            completers::none,
            move |cx, input, event| {
                if event != PromptEvent::Validate || !input.eq_ignore_ascii_case("y") {
                    return;
                }
                let path = entry.path.clone();
                let args: Vec<&str> = match entry.section {
                    Section::Untracked => vec!["clean", "--force", "--", &path],
                    Section::Staged => vec!["restore", "--staged", "--worktree", "--", &path],
                    _ => vec!["restore", "--", &path],
                };
                let args: Vec<String> = args.into_iter().map(String::from).collect();
                run_from_prompt(cx, args, "discarding", "discarded", true);
            },
        );
        GitAction::Prompt(Box::new(prompt))
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
                let mut args = vec!["commit".to_string()];
                if amend {
                    args.push("--amend".into());
                }
                args.extend(["--message".into(), input.to_string()]);
                run_from_prompt(cx, args, "committing", "committed", false);
            },
        );
        if amend {
            if let Some(last) = self.snapshot.commits.first() {
                prompt = prompt.with_line(last.subject.clone(), editor);
            }
        }
        GitAction::Prompt(Box::new(prompt))
    }

    /// Runs git in the background, then reports and refreshes. `reload` re-reads unmodified open
    /// buffers afterwards, for commands that change files on disk.
    fn run(&mut self, args: &[&str], busy: &str, done: &str, reload: bool) {
        let Some(repo) = self.snapshot.repo.clone() else {
            return;
        };
        self.busy = Some(busy.to_string());
        spawn_git(
            repo,
            args.iter().map(|arg| arg.to_string()).collect(),
            done.to_string(),
            reload,
        );
    }
}

fn run_from_prompt(
    cx: &mut crate::compositor::Context,
    args: Vec<String>,
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
                        spawn_git(repo, args, done.to_string(), reload);
                    }
                }
            },
        )))
    });
}

fn spawn_git(repo: PathBuf, args: Vec<String>, done: String, reload: bool) {
    tokio::spawn(async move {
        let output = git(&repo, &args).await;
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
                    editor.set_error(format!("git {}: {message}", args[0]));
                }
                Err(err) => editor.set_error(format!("git: {err}")),
            }
            if reload {
                reload_unmodified(editor);
            }
            if let Some(editor_view) = compositor.find::<EditorView>() {
                editor_view.git.busy = None;
                editor_view.git.refreshing = false;
                editor_view.git.refresh();
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

async fn git(repo: &Path, args: &[String]) -> std::io::Result<GitOutput> {
    let mut command = tokio::process::Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
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
    let output = command.output().await?;
    Ok(GitOutput {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

async fn read_snapshot(cwd: &Path) -> Snapshot {
    let args = |list: &[&str]| list.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
    let Ok(root) = git(cwd, &args(&["rev-parse", "--show-toplevel"])).await else {
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
    let (status, log) = tokio::join!(git(&repo, &status_args), git(&repo, &log_args));

    let mut snapshot = Snapshot {
        repo: Some(repo),
        ..Default::default()
    };
    if let Ok(status) = status {
        parse_status(&status.stdout, &mut snapshot);
    }
    if let Ok(log) = log {
        snapshot.commits = log
            .stdout
            .lines()
            .filter_map(|line| {
                let mut parts = line.split('\x1f');
                Some(Commit {
                    hash: parts.next()?.to_string(),
                    subject: parts.next()?.to_string(),
                    when: short_age(parts.next()?),
                })
            })
            .collect();
    }
    snapshot
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

/// "3 hours ago" -> "3h", for the commit list.
fn short_age(age: &str) -> String {
    let mut parts = age.split_whitespace();
    let (Some(n), Some(unit)) = (parts.next(), parts.next()) else {
        return age.to_string();
    };
    let unit = match unit.trim_end_matches('s') {
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

/// Keeps the end of a path, which is the part that tells files apart.
fn shorten_left(path: &str, width: usize) -> String {
    if path.width() <= width || width < 2 {
        return path.to_string();
    }
    let chars: Vec<char> = path.chars().collect();
    let keep = width - 1;
    let tail: String = chars[chars.len().saturating_sub(keep)..].iter().collect();
    format!("…{tail}")
}

/// Opens `text` in a new tab as a read-only, unnamed buffer labelled `label`.
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
    fn shortens_ages_and_paths() {
        assert_eq!(short_age("3 hours ago"), "3h");
        assert_eq!(short_age("1 day ago"), "1d");
        assert_eq!(short_age("2 months ago"), "2mo");
        assert_eq!(shorten_left("src/ui/git_panel.rs", 10), "…_panel.rs");
        assert_eq!(shorten_left("a.rs", 10), "a.rs");
    }
}
