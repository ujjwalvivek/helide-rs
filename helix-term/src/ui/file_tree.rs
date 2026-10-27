//! Helide file tree: the working directory as an expandable tree, docked left of the editor.
//!
//! Directories are listed lazily, one level at a time, with the same ignore rules as the file
//! picker. The tree is rebuilt from the set of expanded directories whenever it may be stale:
//! when it gains focus, after a file operation, or on `R`.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use helix_core::unicode::width::UnicodeWidthStr;
use helix_view::{
    editor::Action,
    graphics::{Modifier, Rect},
    input::{KeyEvent, MouseButton, MouseEvent, MouseEventKind},
    keyboard::{KeyCode, KeyModifiers},
    Editor,
};
use tui::buffer::Buffer as Surface;

use super::panel_style::Roles;

use crate::{
    compositor::{self, Component},
    job::Callback,
    ui::{completers, EditorView, Prompt, PromptEvent},
};

struct Row {
    path: PathBuf,
    name: String,
    is_dir: bool,
    depth: usize,
}

/// What the tree asks its host to do after a key or click.
pub enum TreeAction {
    None,
    /// Hand focus back to the editor.
    FocusEditor,
    Hide,
    /// Ask for a name and create a file, or a directory when the name ends in `/`, in this dir.
    Create(PathBuf),
    Rename(PathBuf),
    Delete(PathBuf),
}

#[derive(Default)]
pub struct FileTree {
    root: PathBuf,
    rows: Vec<Row>,
    expanded: HashSet<PathBuf>,
    selected: usize,
    scroll: usize,
    /// Shows hidden files regardless of the file picker setting.
    show_hidden: bool,
    pub visible: bool,
    pub focused: bool,
    /// Where the rows were drawn last frame (below the header).
    rows_area: Rect,
    /// Where the whole tree was drawn last frame.
    area: Rect,
}

impl FileTree {
    /// Hidden: show and focus it on the current file. Shown but unfocused: focus. Focused: hide.
    pub fn toggle(&mut self, editor: &Editor) {
        if self.visible && self.focused {
            self.visible = false;
            self.focused = false;
        } else {
            self.visible = true;
            self.focused = true;
            self.refresh(editor);
            self.reveal_current(editor);
        }
    }

    /// Re-reads every expanded directory, keeping the selection on the same path.
    pub fn refresh(&mut self, editor: &Editor) {
        let root = helix_stdx::env::current_working_dir();
        if root != self.root {
            self.expanded.clear();
            self.selected = 0;
            self.scroll = 0;
            self.root = root;
        }
        let selected = self.selected_path().map(Path::to_path_buf);
        self.expanded.retain(|dir| dir.is_dir());

        let mut rows = Vec::new();
        self.list(editor, &self.root.clone(), 0, &mut rows);
        self.rows = rows;

        // Follow the selected path; if it is gone (deleted or renamed outside the tree), select
        // the row before it, which is its previous sibling or its parent.
        match selected {
            Some(path) if self.rows.iter().any(|row| row.path == path) => self.select_path(&path),
            Some(_) => self.selected = self.selected.saturating_sub(1),
            None => {}
        }
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
    }

    fn list(&self, editor: &Editor, dir: &Path, depth: usize, rows: &mut Vec<Row>) {
        for (path, is_dir) in read_dir(editor, dir, self.show_hidden) {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let expanded = is_dir && self.expanded.contains(&path);
            rows.push(Row {
                path: path.clone(),
                name,
                is_dir,
                depth,
            });
            if expanded {
                self.list(editor, &path, depth + 1, rows);
            }
        }
    }

    /// Expands the parents of the focused document and selects it.
    pub fn reveal_current(&mut self, editor: &Editor) {
        let (_, doc) = current_ref!(editor);
        let Some(path) = doc.path() else { return };
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return;
        };
        let mut dir = self.root.clone();
        let mut changed = false;
        if let Some(parent) = relative.parent() {
            for part in parent.components() {
                dir.push(part);
                changed |= self.expanded.insert(dir.clone());
            }
        }
        if changed {
            self.refresh(editor);
        }
        self.select_path(path);
    }

    fn select_path(&mut self, path: &Path) {
        if let Some(index) = self.rows.iter().position(|row| row.path == path) {
            self.selected = index;
        }
    }

    fn selected_path(&self) -> Option<&Path> {
        self.rows.get(self.selected).map(|row| row.path.as_path())
    }

    /// The directory new files go into: the selected directory, or the selected file's parent.
    fn target_dir(&self) -> PathBuf {
        match self.rows.get(self.selected) {
            Some(row) if row.is_dir => row.path.clone(),
            Some(row) => row
                .path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| self.root.clone()),
            None => self.root.clone(),
        }
    }

    pub fn contains(&self, column: u16, row: u16) -> bool {
        self.visible
            && column >= self.area.x
            && column < self.area.right()
            && row >= self.area.y
            && row < self.area.bottom()
    }

    /// The project's folder name, shown in the frame's top border.
    pub fn root_name(&self) -> String {
        self.root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.root.display().to_string())
    }

    /// (rows, first row shown, rows that fit), for the frame's scrollbar.
    pub fn scroll_state(&self) -> (usize, usize, usize) {
        (self.rows.len(), self.scroll, self.rows_area.height as usize)
    }

    /// Draws the rows inside `area`, the inside of the frame's box.
    pub fn render(&mut self, area: Rect, surface: &mut Surface, editor: &Editor) {
        self.area = area;
        if area.width < 6 || area.height < 1 {
            return;
        }
        let roles = Roles::new(&editor.theme);
        let rows_area = area;
        self.rows_area = rows_area;
        let height = rows_area.height as usize;
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if height > 0 && self.selected >= self.scroll + height {
            self.scroll = self.selected + 1 - height;
        }

        let current = {
            let (_, doc) = current_ref!(editor);
            doc.path().map(Path::to_path_buf)
        };

        for (i, row) in self.rows.iter().enumerate().skip(self.scroll).take(height) {
            let y = rows_area.y + (i - self.scroll) as u16;
            let line = Rect::new(rows_area.x, y, rows_area.width, 1);
            if i == self.selected {
                roles.select_row(surface, line, self.focused);
            }
            let is_current = current.as_deref() == Some(row.path.as_path());
            let expanded = row.is_dir && self.expanded.contains(&row.path);
            let chevron = match (row.is_dir, expanded) {
                (false, _) => " ",
                (true, true) => "▾",
                (true, false) => "▸",
            };
            let indent = 2 * row.depth as u16;
            let mut x = line.x + 1 + indent;
            if x + 4 >= line.right() {
                continue;
            }
            surface.set_string(x, y, chevron, roles.muted);
            x += 2;
            let icon_style = if is_current {
                roles.accent
            } else {
                roles.muted
            };
            surface.set_string(x, y, icon(&row.name, row.is_dir, expanded), icon_style);
            x += 2;
            let name_style = if is_current {
                roles.accent.add_modifier(Modifier::BOLD)
            } else {
                roles.text
            };
            let room = line.right().saturating_sub(x + 1) as usize;
            surface.set_string(x, y, &shorten(&row.name, room), name_style);
        }
    }

    pub fn key(&mut self, key: KeyEvent, editor: &mut Editor) -> TreeAction {
        let page = (self.rows_area.height as usize / 2).max(1);
        let last = self.rows.len().saturating_sub(1);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.selected = (self.selected + 1).min(last),
            KeyCode::Char('k') | KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Char('d') if ctrl => self.selected = (self.selected + page).min(last),
            KeyCode::Char('u') if ctrl => self.selected = self.selected.saturating_sub(page),
            KeyCode::PageDown => self.selected = (self.selected + page * 2).min(last),
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(page * 2),
            KeyCode::Char('g') | KeyCode::Home => self.selected = 0,
            KeyCode::Char('G') | KeyCode::End => self.selected = last,
            KeyCode::Enter | KeyCode::Char('o') => return self.activate(editor, Action::Replace),
            KeyCode::Char('l') | KeyCode::Right => {
                let is_dir = self.rows.get(self.selected).is_some_and(|row| row.is_dir);
                if is_dir {
                    self.expand(editor, true);
                } else {
                    return self.activate(editor, Action::Replace);
                }
            }
            KeyCode::Char('v') => return self.activate(editor, Action::VerticalSplit),
            KeyCode::Char('s') => return self.activate(editor, Action::HorizontalSplit),
            KeyCode::Char('t') => return self.activate(editor, Action::NewTab),
            KeyCode::Char('h') | KeyCode::Left => self.collapse_or_parent(editor),
            KeyCode::Char('a') => return TreeAction::Create(self.target_dir()),
            KeyCode::Char('r') => {
                if let Some(path) = self.selected_path() {
                    return TreeAction::Rename(path.to_path_buf());
                }
            }
            KeyCode::Char('d') => {
                if let Some(path) = self.selected_path() {
                    return TreeAction::Delete(path.to_path_buf());
                }
            }
            KeyCode::Char('R') => self.refresh(editor),
            KeyCode::Char('.') => {
                self.show_hidden = !self.show_hidden;
                self.refresh(editor);
            }
            KeyCode::Char('q') => return TreeAction::Hide,
            KeyCode::Esc => return TreeAction::FocusEditor,
            _ => {}
        }
        TreeAction::None
    }

    pub fn mouse(&mut self, event: &MouseEvent, editor: &mut Editor) -> TreeAction {
        match event.kind {
            MouseEventKind::ScrollDown => {
                let max = self
                    .rows
                    .len()
                    .saturating_sub(self.rows_area.height as usize);
                self.scroll = (self.scroll + 3).min(max);
                self.selected = self.selected.max(self.scroll);
            }
            MouseEventKind::ScrollUp => {
                self.scroll = self.scroll.saturating_sub(3);
                let bottom = self.scroll + self.rows_area.height as usize;
                self.selected = self.selected.min(bottom.saturating_sub(1));
            }
            MouseEventKind::Down(MouseButton::Left) if event.row >= self.rows_area.y => {
                let index = self.scroll + (event.row - self.rows_area.y) as usize;
                if index < self.rows.len() {
                    self.selected = index;
                    return self.activate(editor, Action::Replace);
                }
            }
            _ => {}
        }
        TreeAction::None
    }

    /// Opens the selected file, or toggles the selected directory.
    fn activate(&mut self, editor: &mut Editor, action: Action) -> TreeAction {
        let Some(row) = self.rows.get(self.selected) else {
            return TreeAction::None;
        };
        if row.is_dir {
            let expand = !self.expanded.contains(&row.path);
            self.expand(editor, expand);
            return TreeAction::None;
        }
        let path = row.path.clone();
        match editor.open(&path, action) {
            Ok(_) => TreeAction::FocusEditor,
            Err(err) => {
                editor.set_error(format!("cannot open {}: {err}", path.display()));
                TreeAction::None
            }
        }
    }

    fn expand(&mut self, editor: &Editor, expand: bool) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        if !row.is_dir {
            return;
        }
        let path = row.path.clone();
        if expand {
            self.expanded.insert(path);
        } else {
            self.expanded.remove(&path);
        }
        self.refresh(editor);
    }

    fn collapse_or_parent(&mut self, editor: &Editor) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        if row.is_dir && self.expanded.contains(&row.path) {
            self.expand(editor, false);
        } else if let Some(parent) = row.path.parent().map(Path::to_path_buf) {
            self.select_path(&parent);
        }
    }
}

/// One level of `dir`: directories first, then files, each sorted case-insensitively.
fn read_dir(editor: &Editor, dir: &Path, show_hidden: bool) -> Vec<(PathBuf, bool)> {
    let config = editor.config();
    let picker = &config.file_picker;
    let mut entries: Vec<(PathBuf, bool)> = ignore::WalkBuilder::new(dir)
        .max_depth(Some(1))
        .hidden(picker.hidden && !show_hidden)
        .parents(picker.parents)
        .ignore(picker.ignore)
        .follow_links(picker.follow_symlinks)
        .git_ignore(picker.git_ignore)
        .git_global(picker.git_global)
        .git_exclude(picker.git_exclude)
        .add_custom_ignore_filename(helix_loader::config_dir().join("ignore"))
        .add_custom_ignore_filename(".helix/ignore")
        .filter_entry(|entry| entry.file_name() != ".git")
        .build()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            if entry.depth() == 0 {
                return None;
            }
            let is_dir = entry.path().is_dir();
            Some((entry.into_path(), is_dir))
        })
        .collect();
    entries.sort_by(|(a, a_dir), (b, b_dir)| {
        b_dir.cmp(a_dir).then_with(|| {
            let a = a.file_name().map(|n| n.to_string_lossy().to_lowercase());
            let b = b.file_name().map(|n| n.to_string_lossy().to_lowercase());
            a.cmp(&b)
        })
    });
    entries
}

/// The prompt a file operation needs, or `None` for actions the host handles itself.
pub fn prompt(action: &TreeAction, editor: &Editor) -> Option<Box<dyn Component>> {
    let root = helix_stdx::env::current_working_dir();
    let relative = |path: &Path| {
        path.strip_prefix(&root)
            .unwrap_or(path)
            .display()
            .to_string()
    };
    let prompt: Prompt = match action {
        TreeAction::Create(dir) => {
            let dir = dir.clone();
            let label = format!("new in {}/ (end with / for a dir): ", relative(&dir));
            Prompt::new(
                label.into(),
                None,
                completers::none,
                move |cx, input, event| {
                    if event != PromptEvent::Validate || input.is_empty() {
                        return;
                    }
                    let path = dir.join(input);
                    let result = if input.ends_with('/') {
                        std::fs::create_dir_all(&path)
                    } else {
                        path.parent()
                            .map_or(Ok(()), std::fs::create_dir_all)
                            .and_then(|_| std::fs::File::create_new(&path).map(drop))
                    };
                    match result {
                        Err(err) => cx.editor.set_error(format!("cannot create {input}: {err}")),
                        Ok(()) if path.is_file() => {
                            match cx.editor.open(&path, Action::Replace) {
                                // A new file is for writing in, so the editor takes focus.
                                Ok(_) => return refresh_later(cx, true),
                                Err(err) => cx.editor.set_error(err.to_string()),
                            }
                        }
                        Ok(()) => {}
                    }
                    refresh_later(cx, false);
                },
            )
        }
        TreeAction::Rename(path) => {
            let old = path.clone();
            let name = old
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let label = format!("rename {}: ", relative(&old));
            Prompt::new(
                label.into(),
                None,
                completers::none,
                move |cx, input, event| {
                    if event != PromptEvent::Validate || input.is_empty() {
                        return;
                    }
                    let new = old.with_file_name(input);
                    if new.exists() {
                        cx.editor
                            .set_error(format!("{} already exists", new.display()));
                        return;
                    }
                    if let Err(err) = cx.editor.move_path(&old, &new) {
                        cx.editor.set_error(format!("cannot rename: {err}"));
                    }
                    refresh_later(cx, false);
                },
            )
            .with_line(name, editor)
        }
        TreeAction::Delete(path) => {
            let path = path.clone();
            let kind = if path.is_dir() { "directory" } else { "file" };
            let label = format!("delete {kind} {}? [y/N]: ", relative(&path));
            Prompt::new(
                label.into(),
                None,
                completers::none,
                move |cx, input, event| {
                    if event != PromptEvent::Validate || !input.eq_ignore_ascii_case("y") {
                        return;
                    }
                    let result = if path.is_dir() {
                        std::fs::remove_dir_all(&path)
                    } else {
                        std::fs::remove_file(&path)
                    };
                    if let Err(err) = result {
                        cx.editor.set_error(format!("cannot delete: {err}"));
                    }
                    refresh_later(cx, false);
                },
            )
        }
        _ => return None,
    };
    Some(Box::new(prompt))
}

/// Prompt callbacks have no access to the compositor, so the tree refresh runs as a job.
fn refresh_later(cx: &mut compositor::Context, focus_editor: bool) {
    cx.jobs.callback(async move {
        Ok(Callback::EditorCompositor(Box::new(
            move |editor, compositor| {
                if let Some(editor_view) = compositor.find::<EditorView>() {
                    editor_view.file_tree.refresh(editor);
                    if focus_editor {
                        editor_view.file_tree.focused = false;
                    }
                }
            },
        )))
    });
}

/// A Nerd Font icon for a tree row, picked by directory state, file name, then extension.
fn icon(name: &str, is_dir: bool, expanded: bool) -> &'static str {
    if is_dir {
        return match (name, expanded) {
            (".git" | ".github", _) => "\u{e5fb}",
            ("node_modules", _) => "\u{e5fa}",
            (_, true) => "\u{f07c}",
            (_, false) => "\u{f07b}",
        };
    }
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "cargo.toml" | "cargo.lock" => return "\u{e7a8}",
        "dockerfile" | "containerfile" => return "\u{f308}",
        "makefile" | "justfile" => return "\u{e779}",
        "license" | "license.md" | "license.txt" | "copying" => return "\u{f0219}",
        ".gitignore" | ".gitattributes" | ".gitmodules" => return "\u{e702}",
        "flake.nix" | "flake.lock" => return "\u{f313}",
        _ => {}
    }
    let extension = lower.rsplit_once('.').map_or("", |(_, ext)| ext);
    match extension {
        "rs" => "\u{e7a8}",
        "md" | "markdown" => "\u{e73e}",
        "toml" => "\u{e6b2}",
        "json" | "jsonc" | "json5" => "\u{e60b}",
        "yml" | "yaml" => "\u{e6a8}",
        "lock" => "\u{f023}",
        "js" | "mjs" | "cjs" => "\u{e74e}",
        "ts" | "mts" | "cts" => "\u{e628}",
        "jsx" | "tsx" => "\u{e7ba}",
        "py" => "\u{e73c}",
        "go" => "\u{e627}",
        "c" | "h" => "\u{e61e}",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => "\u{e61d}",
        "cs" => "\u{f031b}",
        "java" => "\u{e738}",
        "kt" | "kts" => "\u{e634}",
        "swift" => "\u{e755}",
        "zig" => "\u{e6a9}",
        "lua" => "\u{e620}",
        "scm" | "ss" | "rkt" => "\u{e6b1}",
        "nix" => "\u{f313}",
        "sh" | "bash" | "zsh" | "fish" | "nu" | "ps1" => "\u{f489}",
        "html" | "htm" => "\u{e736}",
        "css" | "scss" | "sass" => "\u{e749}",
        "svg" | "png" | "jpg" | "jpeg" | "gif" | "webp" | "ico" | "bmp" => "\u{f1c5}",
        "pdf" => "\u{f1c1}",
        "zip" | "tar" | "gz" | "xz" | "zst" | "7z" => "\u{f410}",
        "txt" | "log" => "\u{f15c}",
        "sql" | "db" | "sqlite" => "\u{e706}",
        "xml" => "\u{f05c0}",
        "csproj" | "sln" => "\u{e70c}",
        "kdl" | "conf" | "ini" | "cfg" | "env" => "\u{e615}",
        _ => "\u{f15b}",
    }
}

/// Cuts `text` to `width` columns, ending in an ellipsis when it does not fit.
fn shorten(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    let mut cut = String::new();
    for ch in text.chars() {
        if cut.width() + 2 > width {
            break;
        }
        cut.push(ch);
    }
    cut.push('…');
    cut
}
