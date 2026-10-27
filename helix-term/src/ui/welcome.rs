//! Helide's welcome screen: shown when the editor starts on no file in a folder with no saved
//! workspace. A wordmark, a few actions on single keys, and the recent projects; opening one
//! changes the working directory and reopens it as it was left.

use std::path::{Path, PathBuf};

use helix_core::unicode::width::UnicodeWidthStr;
use helix_view::{
    editor::Action,
    graphics::{Modifier, Rect},
    input::KeyEvent,
    keyboard::{KeyCode, KeyModifiers},
    Editor,
};
use tui::buffer::Buffer as Surface;

use super::{
    agents,
    frame::{self, End, Label},
    overlay::overlaid,
    panel_style::Roles,
    EditorView, Prompt, PromptEvent,
};
use crate::compositor::{Callback, Component, Compositor, Context, Event, EventResult};

pub const ID: &str = "helide-welcome";

const WORDMARK: [&str; 6] = [
    "██╗  ██╗███████╗██╗     ██╗██████╗ ███████╗",
    "██║  ██║██╔════╝██║     ██║██╔══██╗██╔════╝",
    "███████║█████╗  ██║     ██║██║  ██║█████╗  ",
    "██╔══██║██╔══╝  ██║     ██║██║  ██║██╔══╝  ",
    "██║  ██║███████╗███████╗██║██████╔╝███████╗",
    "╚═╝  ╚═╝╚══════╝╚══════╝╚═╝╚═════╝ ╚══════╝",
];
const TAGLINE: &str = "helix, with the workspace built in";

const ACTIONS: [(char, &str); 6] = [
    ('o', "Open folder"),
    ('f', "Find file"),
    ('t', "File tree"),
    ('n', "New file"),
    ('c', "Config"),
    ('q', "Quit"),
];

pub struct Welcome {
    recent: Vec<PathBuf>,
}

impl Default for Welcome {
    fn default() -> Self {
        Self::new()
    }
}

impl Welcome {
    pub fn new() -> Self {
        let cwd = helix_stdx::env::current_working_dir();
        let recent = agents::recent_projects()
            .into_iter()
            .map(|workspace| workspace.project)
            .filter(|project| *project != cwd)
            .take(9)
            .collect();
        Self { recent }
    }
}

/// `~` for the home directory, so paths read short.
fn tilde(path: &Path) -> String {
    match helix_stdx::path::home_dir() {
        Ok(home) => match path.strip_prefix(&home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        },
        Err(_) => path.display().to_string(),
    }
}

/// Makes `project` the working directory and reopens it, or shows its file tree when it has
/// nothing saved.
pub fn open_project(project: &Path, editor: &mut Editor, compositor: &mut Compositor) {
    log::info!("welcome: opening project {}", project.display());
    if let Err(err) = helix_stdx::env::set_current_working_dir(project) {
        editor.set_error(format!("cannot open {}: {err}", project.display()));
        return;
    }
    compositor.remove(ID);
    if let Some(editor_view) = compositor.find::<EditorView>() {
        if !editor_view.restore_workspace(editor) {
            editor_view.toggle_file_tree(editor);
        }
    }
    editor.set_status(format!("Opened {}.", tilde(project)));
}

fn close() -> Callback {
    Box::new(|compositor, _| {
        compositor.remove(ID);
    })
}

impl Welcome {
    fn act(&self, key: KeyEvent) -> EventResult {
        let KeyCode::Char(ch) = key.code else {
            return match key.code {
                KeyCode::Esc | KeyCode::Enter => EventResult::Consumed(Some(close())),
                _ => EventResult::Consumed(None),
            };
        };
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return EventResult::Ignored(None);
        }
        if let Some(index) = ch.to_digit(10).filter(|d| *d > 0) {
            let Some(project) = self.recent.get(index as usize - 1).cloned() else {
                return EventResult::Consumed(None);
            };
            return EventResult::Consumed(Some(Box::new(move |compositor, cx| {
                open_project(&project, cx.editor, compositor)
            })));
        }
        let callback: Callback = match ch {
            'o' => Box::new(|compositor, cx| {
                let cwd = helix_stdx::env::current_working_dir();
                let prompt = Prompt::new(
                    "open folder: ".into(),
                    None,
                    super::completers::directory,
                    |cx, input, event| {
                        if event != PromptEvent::Validate || input.trim().is_empty() {
                            return;
                        }
                        let path = helix_stdx::path::canonicalize(helix_stdx::path::expand_tilde(
                            Path::new(input.trim()),
                        ));
                        cx.jobs.callback(async move {
                            Ok(crate::job::Callback::EditorCompositor(Box::new(
                                move |editor, compositor| open_project(&path, editor, compositor),
                            )))
                        });
                    },
                )
                .with_line(format!("{}/", tilde(&cwd)), cx.editor);
                compositor.push(Box::new(prompt));
            }),
            'f' => Box::new(|compositor, cx| {
                compositor.remove(ID);
                let cwd = helix_stdx::env::current_working_dir();
                let picker = super::file_picker(cx.editor, cwd);
                compositor.push(Box::new(overlaid(picker)));
            }),
            't' => Box::new(|compositor, cx| {
                compositor.remove(ID);
                if let Some(editor_view) = compositor.find::<EditorView>() {
                    editor_view.toggle_file_tree(cx.editor);
                }
            }),
            'n' => close(),
            'c' => Box::new(|compositor, cx| {
                compositor.remove(ID);
                let path = helix_loader::config_file();
                if let Err(err) = cx.editor.open(&path, Action::Replace) {
                    cx.editor
                        .set_error(format!("cannot open {}: {err}", path.display()));
                }
            }),
            'q' => Box::new(|_, cx| {
                // Closing the only view ends the editor.
                let view = cx.editor.tree.focus;
                cx.editor.close(view);
            }),
            _ => return EventResult::Consumed(None),
        };
        EventResult::Consumed(Some(callback))
    }
}

impl Component for Welcome {
    fn handle_event(&mut self, event: &Event, _cx: &mut Context) -> EventResult {
        match event {
            Event::Key(key) => self.act(*key),
            Event::Paste(_) | Event::Mouse(_) => EventResult::Consumed(None),
            _ => EventResult::Ignored(None),
        }
    }

    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        let theme = &cx.editor.theme;
        let roles = Roles::new(theme);
        surface.clear_with(area, theme.get("ui.background"));

        // A centred box: the wordmark, the tagline, then two columns: actions on the left,
        // recent projects on the right.
        let rows = ACTIONS.len().max(self.recent.len()) as u16;
        // Border, padding, wordmark, gap, tagline, gap, labels, gap, rows, padding, border.
        let height = (1 + 1 + 6 + 1 + 1 + 3 + 1 + 1 + rows + 2 + 1).min(area.height);
        let width = 76.min(area.width);
        let block = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        frame::draw_box(surface, block, roles.muted, End::Corner, End::Corner);
        frame::labels_right(
            surface,
            block.bottom() - 1,
            block.x,
            block.right() - 1,
            &[Label::new(helix_loader::VERSION_AND_GIT_HASH, roles.muted)],
        );

        // The wordmark: solid blocks in the accent, their drawn edges in muted ink for depth.
        let mark_width = WORDMARK[0].chars().count() as u16;
        let mark_x = block.x + block.width.saturating_sub(mark_width) / 2;
        let mut y = block.y + 2;
        for line in WORDMARK {
            for (i, ch) in line.chars().enumerate() {
                let style = if ch == '█' {
                    roles.accent
                } else {
                    roles.muted
                };
                if ch != ' ' {
                    surface.set_string(mark_x + i as u16, y, &ch.to_string(), style);
                }
            }
            y += 1;
        }
        y += 1;
        let tagline_x = block.x + block.width.saturating_sub(TAGLINE.width() as u16) / 2;
        surface.set_string(tagline_x, y, TAGLINE, roles.text);
        y += 3;

        // Two columns under section labels.
        let left = block.x + 8;
        let right = block.x + block.width / 2 + 2;
        let key_style = roles.accent.add_modifier(Modifier::BOLD);
        surface.set_string(left, y, "START", roles.label());
        if !self.recent.is_empty() {
            surface.set_string(right, y, "RECENT", roles.label());
        }
        y += 2;
        for (row, (key, label)) in ACTIONS.iter().enumerate() {
            let line = y + row as u16;
            if line >= block.bottom() - 1 {
                break;
            }
            surface.set_string(left, line, &key.to_string(), key_style);
            surface.set_string(left + 4, line, label, roles.text);
        }
        let room = block.right().saturating_sub(right + 4 + 3) as usize;
        for (row, project) in self.recent.iter().enumerate() {
            let line = y + row as u16;
            if line >= block.bottom() - 1 {
                break;
            }
            let name = project
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            surface.set_string(right, line, &(row + 1).to_string(), key_style);
            surface.set_stringn(right + 4, line, &name, room, roles.text);
            // The folder it lives in, muted, when there is room after the name.
            let parent = project.parent().map(tilde).unwrap_or_default();
            let parent_x = right + 4 + name.width() as u16 + 2;
            let parent_room = block.right().saturating_sub(parent_x + 3) as usize;
            if parent_room > 6 {
                let parent = if parent.width() > parent_room {
                    let tail: String = parent
                        .chars()
                        .rev()
                        .take(parent_room - 1)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    format!("…{tail}")
                } else {
                    parent
                };
                surface.set_string(parent_x, line, &parent, roles.muted);
            }
        }
    }

    fn id(&self) -> Option<&'static str> {
        Some(ID)
    }
}
