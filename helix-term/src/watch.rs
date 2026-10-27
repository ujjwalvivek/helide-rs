//! Reloads open files that change on disk, so edits made by agents, formatters or git show up
//! without `:reload`. The folders holding open files are watched (not the whole project, which
//! keeps it light in large repositories); a change to an open file reloads its buffer when it
//! has no unsaved edits, and only reports it otherwise.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use helix_view::{DocumentId, Editor};
use notify::{EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;

pub struct FileWatcher {
    watcher: Option<notify::RecommendedWatcher>,
    pub rx: mpsc::UnboundedReceiver<Vec<PathBuf>>,
    // Keeps `rx` open when the watcher could not start.
    _tx: mpsc::UnboundedSender<Vec<PathBuf>>,
    watched: HashSet<PathBuf>,
}

impl FileWatcher {
    pub fn start() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let sender = tx.clone();
        // Events arrive on notify's thread; the redraw is requested from inside the editor's
        // runtime, where helix-event keeps its state.
        let runtime = tokio::runtime::Handle::try_current().ok();
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            let Ok(event) = event else { return };
            if matches!(
                event.kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
            ) && sender.send(event.paths).is_ok()
            {
                let _guard = runtime.as_ref().map(|runtime| runtime.enter());
                helix_event::request_redraw();
            }
        });
        let watcher = match watcher {
            Ok(watcher) => Some(watcher),
            Err(err) => {
                log::warn!("file watching disabled: {err}");
                None
            }
        };
        Self {
            watcher,
            rx,
            _tx: tx,
            watched: HashSet::new(),
        }
    }

    /// Watches the folders of the open files, and stops watching folders with none left.
    pub fn sync(&mut self, editor: &Editor) {
        let Some(watcher) = self.watcher.as_mut() else {
            return;
        };
        let wanted: HashSet<PathBuf> = editor
            .documents()
            .filter_map(|doc| doc.path()?.parent().map(Path::to_path_buf))
            .collect();
        for dir in self.watched.difference(&wanted) {
            let _ = watcher.unwatch(dir);
        }
        for dir in wanted.difference(&self.watched) {
            if let Err(err) = watcher.watch(dir, RecursiveMode::NonRecursive) {
                log::debug!("cannot watch {}: {err}", dir.display());
            }
        }
        self.watched = wanted;
    }
}

/// Reloads the open, unmodified files among `paths` whose contents differ from the buffer.
/// Files with unsaved edits are left alone and reported in the status line.
pub fn reload_changed(editor: &mut Editor, paths: Vec<PathBuf>) {
    let mut seen = HashSet::new();
    let mut reloaded = 0;
    for path in paths {
        let path = helix_stdx::path::canonicalize(path);
        if !seen.insert(path.clone()) {
            continue;
        }
        let Some(doc) = editor.document_by_path(&path) else {
            continue;
        };
        let doc_id: DocumentId = doc.id();
        // Our own writes and touches without a change are not reloads.
        let Ok(on_disk) = std::fs::read(&path) else {
            continue;
        };
        if on_disk == doc.text().to_string().as_bytes() {
            continue;
        }
        if doc.is_modified() {
            editor.set_status(format!(
                "{} changed on disk; you have unsaved edits. :reload! takes the disk version.",
                doc.display_name()
            ));
            continue;
        }
        let focus = editor.tree.focus;
        let view_id = doc.selections().keys().next().copied().unwrap_or(focus);
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
        match doc.reload(view, &editor.diff_providers, trust_full) {
            Ok(()) => reloaded += 1,
            Err(err) => log::warn!("cannot reload {}: {err}", path.display()),
        }
    }
    if reloaded > 0 {
        editor.set_status(match reloaded {
            1 => "Reloaded a file that changed on disk.".to_string(),
            n => format!("Reloaded {n} files that changed on disk."),
        });
    }
}
