//! Claude Code IDE integration: the editor side of the protocol Claude Code's VS Code and
//! JetBrains extensions speak, so `claude` running in a Helide panel knows what you are looking
//! at and proposes edits as diffs you accept or reject.
//!
//! - Discovery: a WebSocket server on 127.0.0.1 and a lock file `~/.claude/ide/<port>.lock`
//!   naming it, with a random token Claude must present in `x-claude-code-ide-authorization`.
//!   `CLAUDE_CODE_SSE_PORT` and `ENABLE_IDE_INTEGRATION` are exported so the `claude` processes
//!   the editor starts connect to it.
//! - Transport: MCP (JSON-RPC 2.0) over the WebSocket. The editor is the server; Claude calls
//!   its tools and receives `selection_changed` notifications.
//! - `openDiff` opens Claude's proposed contents in a new tab, with the file on disk as the diff
//!   base so the gutter marks every change. `:accept` returns `FILE_SAVED` and the (possibly
//!   edited) contents, after which Claude writes the file; `:reject`, or closing the tab, returns
//!   `DIFF_REJECTED`.
//!
//! The protocol is not officially documented; this follows the reimplementation notes of
//! claudecode.nvim.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use futures_util::{SinkExt, StreamExt};
use helix_core::{coords_at_pos, Rope, Selection};
use helix_view::{document::Document, editor::Action, DocumentId, Editor};
use serde_json::{json, Value};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{broadcast, mpsc, oneshot},
};
use tokio_tungstenite::tungstenite::{
    handshake::server::{ErrorResponse, Request, Response},
    http::StatusCode,
    Message,
};

const PROTOCOL_VERSION: &str = "2024-11-05";

/// :accept and :reject, waiting for the event loop, which owns the pending proposals.
static DECISIONS: parking_lot::Mutex<Vec<bool>> = parking_lot::Mutex::new(Vec::new());

/// Accepts (or rejects) the proposal in the focused buffer, on the next turn of the event loop.
pub fn queue_decision(accept: bool) {
    DECISIONS.lock().push(accept);
    helix_event::request_redraw();
}
const AUTH_HEADER: &str = "x-claude-code-ide-authorization";

/// A tool call from Claude, handled on the main thread. The reply is a JSON-RPC `result`, or an
/// `error` object.
pub struct ToolCall {
    pub name: String,
    pub arguments: Value,
    pub reply: oneshot::Sender<Result<Value, Value>>,
}

struct PendingDiff {
    doc: DocumentId,
    tab_name: String,
    reply: oneshot::Sender<Result<Value, Value>>,
}

/// The editor side of the integration. Lives in the Application; tool calls arrive through `rx`.
pub struct Ide {
    pub rx: mpsc::UnboundedReceiver<ToolCall>,
    // Keeps `rx` open when the server could not start, so the event loop branch never fires.
    _tx: mpsc::UnboundedSender<ToolCall>,
    notify: broadcast::Sender<Value>,
    lock_file: Option<PathBuf>,
    /// Proposed edits waiting for :accept or :reject, by tab name.
    pending: HashMap<String, PendingDiff>,
    /// Diffs that were accepted, waiting for Claude to close their tab.
    accepted: HashMap<String, DocumentId>,
    last_selection: Option<Value>,
}

fn claude_dir() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            helix_stdx::path::home_dir()
                .ok()
                .map(|home| home.join(".claude"))
        })
}

fn random_token() -> String {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .is_err()
    {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        bytes = seed.to_le_bytes();
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

impl Ide {
    pub fn start() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let (notify, _) = broadcast::channel(64);
        let mut ide = Self {
            rx,
            _tx: tx.clone(),
            notify: notify.clone(),
            lock_file: None,
            pending: HashMap::new(),
            accepted: HashMap::new(),
            last_selection: None,
        };
        match listen(tx, notify) {
            Ok(lock_file) => ide.lock_file = Some(lock_file),
            Err(err) => log::warn!("Claude Code IDE integration disabled: {err}"),
        }
        ide
    }

    /// Runs one of Claude's tool calls against the editor. `openDiff` replies later, when the
    /// proposal is accepted or rejected.
    pub fn handle(&mut self, editor: &mut Editor, call: ToolCall) {
        let ToolCall {
            name,
            arguments,
            reply,
        } = call;
        if name == "openDiff" {
            match self.open_diff(editor, &arguments) {
                Ok(pending) => {
                    let tab_name = pending.0;
                    self.pending.insert(
                        tab_name.clone(),
                        PendingDiff {
                            doc: pending.1,
                            tab_name,
                            reply,
                        },
                    );
                }
                Err(err) => {
                    let _ = reply.send(Err(rpc_error(-32603, &err.to_string())));
                }
            }
            return;
        }
        let result = match name.as_str() {
            "openFile" => open_file(editor, &arguments),
            "getCurrentSelection" => Ok(text(current_selection(editor).unwrap_or_else(
                || json!({ "success": false, "message": "No active editor found" }),
            ))),
            "getLatestSelection" => Ok(text(
                self.last_selection
                    .clone()
                    .or_else(|| current_selection(editor))
                    .unwrap_or_else(
                        || json!({ "success": false, "message": "No selection available" }),
                    ),
            )),
            "getOpenEditors" => Ok(text(open_editors(editor))),
            "getWorkspaceFolders" => Ok(text(workspace_folders())),
            "getDiagnostics" => Ok(text(diagnostics(editor, &arguments))),
            "checkDocumentDirty" => Ok(text(document_dirty(editor, &arguments))),
            "saveDocument" => Ok(text(save_document(editor, &arguments))),
            "close_tab" => {
                let tab = arguments["tab_name"].as_str().unwrap_or_default();
                self.close_tab(editor, tab);
                Ok(plain("TAB_CLOSED"))
            }
            "closeAllDiffTabs" => {
                let tabs: Vec<String> = self
                    .pending
                    .keys()
                    .chain(self.accepted.keys())
                    .cloned()
                    .collect();
                for tab in &tabs {
                    self.close_tab(editor, tab);
                }
                Ok(plain(&format!("CLOSED_{}_DIFF_TABS", tabs.len())))
            }
            other => Err(anyhow::anyhow!("unknown tool '{other}'")),
        };
        let _ = reply.send(result.map_err(|err| rpc_error(-32603, &err.to_string())));
    }

    /// Opens `new_file_contents` in a new tab, diffed against the file on disk.
    fn open_diff(
        &mut self,
        editor: &mut Editor,
        args: &Value,
    ) -> anyhow::Result<(String, DocumentId)> {
        let path = PathBuf::from(
            args["new_file_path"]
                .as_str()
                .or_else(|| args["old_file_path"].as_str())
                .ok_or_else(|| anyhow::anyhow!("openDiff needs new_file_path"))?,
        );
        let contents = args["new_file_contents"].as_str().unwrap_or_default();
        let old_path = args["old_file_path"]
            .as_str()
            .map(PathBuf::from)
            .unwrap_or_else(|| path.clone());
        let base = std::fs::read(&old_path).unwrap_or_default();
        let tab_name = args["tab_name"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| format!("proposed {}", file_name(&path)));
        // A repeated proposal for the same tab replaces the earlier one.
        self.close_tab(editor, &tab_name);

        let mut doc = Document::from(
            Rope::from(contents),
            None,
            editor.config.clone(),
            editor.syn_loader.clone(),
        );
        let loader = editor.syn_loader.load();
        if let Some(language) = loader.language_for_filename(&path) {
            doc.set_language(Some(loader.language(language).config().clone()), &loader);
        }
        drop(loader);
        doc.set_diff_base(base);
        doc.label = Some(format!("◆ {}", file_name(&path)));
        let doc_id = editor.new_file_from_document(Action::NewTab, doc);
        editor.set_status(format!(
            "Claude proposes changes to {}. :accept or :reject.",
            relative(&path)
        ));
        self.accepted.remove(&tab_name);
        Ok((tab_name, doc_id))
    }

    /// `:accept` / `:reject` on the focused proposal. Returns false when the focused buffer is
    /// not one.
    pub fn decide(&mut self, editor: &mut Editor, accept: bool) -> bool {
        let focused = doc!(editor).id();
        let Some(tab) = self
            .pending
            .iter()
            .find(|(_, diff)| diff.doc == focused)
            .map(|(tab, _)| tab.clone())
        else {
            return false;
        };
        let diff = self.pending.remove(&tab).expect("found above");
        if accept {
            let contents = doc!(editor, &diff.doc).text().to_string();
            let _ = diff.reply.send(Ok(json!({
                "content": [
                    { "type": "text", "text": "FILE_SAVED" },
                    { "type": "text", "text": contents },
                ]
            })));
            // Claude writes the file, then closes the tab with close_tab.
            self.accepted.insert(diff.tab_name, diff.doc);
            editor.set_status("Accepted. Claude is writing the file.");
        } else {
            reject(diff);
            let _ = editor.close_document(focused, true);
            editor.set_status("Rejected.");
        }
        true
    }

    /// Closes a proposal's tab: rejecting it if still undecided, reloading the real file if it
    /// was accepted.
    fn close_tab(&mut self, editor: &mut Editor, tab: &str) {
        if let Some(diff) = self.pending.remove(tab) {
            let doc = diff.doc;
            reject(diff);
            let _ = editor.close_document(doc, true);
        }
        if let Some(doc) = self.accepted.remove(tab) {
            let _ = editor.close_document(doc, true);
            reload_from_disk(editor);
        }
    }

    /// After each event: applies queued :accept / :reject, notices proposals whose tab was
    /// closed (a rejection), and sends `selection_changed` when the selection moved.
    pub fn after_event(&mut self, editor: &mut Editor) {
        for accept in std::mem::take(&mut *DECISIONS.lock()) {
            if !self.decide(editor, accept) {
                editor.set_error("This buffer is not a change Claude proposed.");
            }
        }

        let closed: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, diff)| editor.document(diff.doc).is_none())
            .map(|(tab, _)| tab.clone())
            .collect();
        for tab in closed {
            if let Some(diff) = self.pending.remove(&tab) {
                reject(diff);
            }
        }

        if self.notify.receiver_count() == 0 {
            return;
        }
        let selection = current_selection(editor);
        // Proposals are not files Claude should treat as the user's selection.
        let selection = selection.filter(|_| {
            !self
                .pending
                .values()
                .any(|diff| diff.doc == doc!(editor).id())
        });
        if selection.is_some() && selection != self.last_selection {
            self.last_selection = selection.clone();
            if let Some(mut params) = selection {
                params
                    .as_object_mut()
                    .map(|object| object.remove("success"));
                let _ = self.notify.send(json!({
                    "jsonrpc": "2.0",
                    "method": "selection_changed",
                    "params": params,
                }));
            }
        }
    }
}

impl Drop for Ide {
    fn drop(&mut self) {
        if let Some(lock_file) = &self.lock_file {
            let _ = std::fs::remove_file(lock_file);
        }
        for (_, diff) in self.pending.drain() {
            reject(diff);
        }
    }
}

fn reject(diff: PendingDiff) {
    let _ = diff.reply.send(Ok(json!({
        "content": [
            { "type": "text", "text": "DIFF_REJECTED" },
            { "type": "text", "text": diff.tab_name },
        ]
    })));
}

fn rpc_error(code: i64, message: &str) -> Value {
    json!({ "code": code, "message": message })
}

/// A tool result holding one JSON value, as text.
fn text(value: Value) -> Value {
    json!({ "content": [{ "type": "text", "text": value.to_string() }] })
}

fn plain(message: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": message }] })
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn relative(path: &Path) -> String {
    let cwd = helix_stdx::env::current_working_dir();
    path.strip_prefix(&cwd)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn file_url(path: &Path) -> String {
    format!("file://{}", path.display())
}

fn current_selection(editor: &Editor) -> Option<Value> {
    let (view, doc) = current_ref!(editor);
    let path = doc.path()?;
    let text = doc.text().slice(..);
    let range = doc.selection(view.id).primary();
    let (from, to) = (range.from(), range.to());
    let start = coords_at_pos(text, from);
    let end = coords_at_pos(text, to);
    // A one-character selection is Helix's cursor, not something the user selected.
    let is_empty = to - from <= 1;
    let selected: String = if is_empty {
        String::new()
    } else {
        text.slice(from..to).to_string()
    };
    Some(json!({
        "success": true,
        "text": selected,
        "filePath": path,
        "fileUrl": file_url(path),
        "selection": {
            "start": { "line": start.row, "character": start.col },
            "end": { "line": end.row, "character": end.col },
            "isEmpty": is_empty,
        },
    }))
}

fn open_editors(editor: &Editor) -> Value {
    let current = doc!(editor).id();
    let tabs: Vec<Value> = editor
        .documents()
        .filter_map(|doc| {
            let path = doc.path()?;
            Some(json!({
                "uri": file_url(path),
                "isActive": doc.id() == current,
                "label": file_name(path),
                "languageId": doc.language_name().unwrap_or("plaintext"),
                "isDirty": doc.is_modified(),
            }))
        })
        .collect();
    json!({ "tabs": tabs })
}

fn workspace_folders() -> Value {
    let cwd = helix_stdx::env::current_working_dir();
    json!({
        "success": true,
        "folders": [{ "name": file_name(&cwd), "uri": file_url(&cwd), "path": cwd }],
        "rootPath": cwd,
    })
}

fn diagnostics(editor: &Editor, args: &Value) -> Value {
    let only = args["uri"]
        .as_str()
        .map(|uri| PathBuf::from(uri.trim_start_matches("file://")));
    let files: Vec<Value> = editor
        .diagnostics
        .iter()
        .filter_map(|(uri, diags)| {
            let path = uri.as_path()?;
            if only.as_deref().is_some_and(|only| only != path) || diags.is_empty() {
                return None;
            }
            let diags: Vec<Value> = diags
                .iter()
                .map(|(diag, _)| {
                    let severity = match diag.severity {
                        Some(helix_lsp::lsp::DiagnosticSeverity::ERROR) => "Error",
                        Some(helix_lsp::lsp::DiagnosticSeverity::WARNING) => "Warning",
                        Some(helix_lsp::lsp::DiagnosticSeverity::INFORMATION) => "Information",
                        _ => "Hint",
                    };
                    json!({
                        "message": diag.message,
                        "severity": severity,
                        "range": diag.range,
                        "source": diag.source,
                    })
                })
                .collect();
            Some(json!({ "uri": file_url(path), "diagnostics": diags }))
        })
        .collect();
    json!(files)
}

fn document_for<'a>(editor: &'a Editor, args: &Value) -> Option<&'a Document> {
    let path = PathBuf::from(args["filePath"].as_str()?);
    let path = helix_stdx::path::canonicalize(path);
    editor.document_by_path(&path)
}

fn document_dirty(editor: &Editor, args: &Value) -> Value {
    match document_for(editor, args) {
        Some(doc) => json!({
            "success": true,
            "filePath": doc.path(),
            "isDirty": doc.is_modified(),
            "isUntitled": false,
        }),
        None => json!({ "success": false, "message": "Document not open" }),
    }
}

fn save_document(editor: &mut Editor, args: &Value) -> Value {
    let Some((doc_id, path)) =
        document_for(editor, args).map(|doc| (doc.id(), doc.path().map(Path::to_path_buf)))
    else {
        return json!({ "success": false, "message": "Document not open" });
    };
    match editor.save::<PathBuf>(doc_id, None, false) {
        Ok(()) => json!({ "success": true, "filePath": path, "saved": true, "message": "Saved." }),
        Err(err) => {
            json!({ "success": false, "filePath": path, "saved": false, "message": err.to_string() })
        }
    }
}

fn open_file(editor: &mut Editor, args: &Value) -> anyhow::Result<Value> {
    let path = PathBuf::from(
        args["filePath"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("openFile needs filePath"))?,
    );
    editor.open(&path, Action::Replace)?;
    // Select from startText through endText, when given.
    if let Some(start_text) = args["startText"].as_str().filter(|s| !s.is_empty()) {
        let (view, doc) = current!(editor);
        let contents = doc.text().to_string();
        if let Some(start) = contents.find(start_text) {
            let end = args["endText"]
                .as_str()
                .filter(|s| !s.is_empty())
                .and_then(|end_text| {
                    contents[start..]
                        .find(end_text)
                        .map(|offset| start + offset + end_text.len())
                })
                .unwrap_or(start + start_text.len());
            let text = doc.text();
            let (from, to) = (text.byte_to_char(start), text.byte_to_char(end));
            doc.set_selection(view.id, Selection::single(from, to));
            helix_view::align_view(doc, view, helix_view::Align::Center);
        }
    }
    let message = format!("Opened file: {}", path.display());
    if args["makeFrontmost"].as_bool() == Some(false) {
        let doc = doc!(editor);
        return Ok(text(json!({
            "success": true,
            "filePath": path,
            "languageId": doc.language_name().unwrap_or("plaintext"),
            "lineCount": doc.text().len_lines(),
        })));
    }
    Ok(plain(&message))
}

/// Re-reads unmodified open buffers after Claude wrote a file.
fn reload_from_disk(editor: &mut Editor) {
    let focus = editor.tree.focus;
    let docs: Vec<_> = editor
        .documents()
        .filter(|doc| doc.path().is_some() && !doc.is_modified())
        .map(|doc| {
            (
                doc.id(),
                doc.selections().keys().next().copied().unwrap_or(focus),
            )
        })
        .collect();
    for (doc_id, view_id) in docs {
        if !editor.tree.contains(view_id) {
            continue;
        }
        let doc = doc_mut!(editor, &doc_id);
        let view = view_mut!(editor, view_id);
        view.sync_changes(doc);
        let _ = doc.reload(view, &editor.diff_providers, true);
    }
}

fn tool_list() -> Value {
    let schema = |properties: Value, required: &[&str]| json!({ "type": "object", "properties": properties, "required": required });
    let none = || schema(json!({}), &[]);
    let path = || json!({ "type": "string", "description": "Absolute path of the file" });
    json!([
        {
            "name": "openFile",
            "description": "Open a file in the editor, optionally selecting from startText to endText.",
            "inputSchema": schema(json!({
                "filePath": path(),
                "preview": { "type": "boolean" },
                "startText": { "type": "string" },
                "endText": { "type": "string" },
                "selectToEndOfLine": { "type": "boolean" },
                "makeFrontmost": { "type": "boolean" },
            }), &["filePath"]),
        },
        {
            "name": "openDiff",
            "description": "Show proposed contents for a file as a diff the user accepts or rejects.",
            "inputSchema": schema(json!({
                "old_file_path": path(),
                "new_file_path": path(),
                "new_file_contents": { "type": "string" },
                "tab_name": { "type": "string" },
            }), &["old_file_path", "new_file_path", "new_file_contents", "tab_name"]),
        },
        { "name": "getCurrentSelection", "description": "The selection in the focused editor.", "inputSchema": none() },
        { "name": "getLatestSelection", "description": "The most recent selection.", "inputSchema": none() },
        { "name": "getOpenEditors", "description": "The open files.", "inputSchema": none() },
        { "name": "getWorkspaceFolders", "description": "The workspace folders.", "inputSchema": none() },
        {
            "name": "getDiagnostics",
            "description": "Language server diagnostics for one file, or all files.",
            "inputSchema": schema(json!({ "uri": { "type": "string" } }), &[]),
        },
        {
            "name": "checkDocumentDirty",
            "description": "Whether a file has unsaved changes.",
            "inputSchema": schema(json!({ "filePath": path() }), &["filePath"]),
        },
        {
            "name": "saveDocument",
            "description": "Save a file's unsaved changes.",
            "inputSchema": schema(json!({ "filePath": path() }), &["filePath"]),
        },
        {
            "name": "close_tab",
            "description": "Close a diff tab.",
            "inputSchema": schema(json!({ "tab_name": { "type": "string" } }), &["tab_name"]),
        },
        { "name": "closeAllDiffTabs", "description": "Close every diff tab.", "inputSchema": none() },
    ])
}

/// Binds the server, writes the lock file and exports the environment for `claude`.
fn listen(
    calls: mpsc::UnboundedSender<ToolCall>,
    notify: broadcast::Sender<Value>,
) -> anyhow::Result<PathBuf> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let listener = TcpListener::from_std(listener)?;
    let token = random_token();

    let dir = claude_dir()
        .ok_or_else(|| anyhow::anyhow!("no home directory"))?
        .join("ide");
    std::fs::create_dir_all(&dir)?;
    remove_stale_locks(&dir);
    let lock_file = dir.join(format!("{port}.lock"));
    let lock = json!({
        "pid": std::process::id(),
        "workspaceFolders": [helix_stdx::env::current_working_dir()],
        "ideName": "Helide",
        "transport": "ws",
        "authToken": token,
    });
    std::fs::write(&lock_file, lock.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&lock_file, std::fs::Permissions::from_mode(0o600))?;
    }
    std::env::set_var("CLAUDE_CODE_SSE_PORT", port.to_string());
    std::env::set_var("ENABLE_IDE_INTEGRATION", "true");

    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    tokio::spawn(serve(
                        stream,
                        token.clone(),
                        calls.clone(),
                        notify.subscribe(),
                    ));
                }
                Err(err) => {
                    log::error!("Claude Code IDE server stopped: {err}");
                    break;
                }
            }
        }
    });
    Ok(lock_file)
}

/// Removes lock files left by Helide editors that are gone (killed before they could clean up),
/// so Claude does not offer them. Other IDEs' lock files are left alone.
fn remove_stale_locks(dir: &Path) {
    for entry in std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
    {
        let path = entry.path();
        let Some(lock) = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        else {
            continue;
        };
        let ours = lock["ideName"] == "Helide";
        let alive = lock["pid"]
            .as_u64()
            .is_some_and(|pid| Path::new(&format!("/proc/{pid}")).exists());
        if ours && !alive {
            let _ = std::fs::remove_file(path);
        }
    }
}

async fn serve(
    stream: TcpStream,
    token: String,
    calls: mpsc::UnboundedSender<ToolCall>,
    mut notifications: broadcast::Receiver<Value>,
) {
    let check = |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
        log::debug!(
            "IDE connection: {} {:?}",
            request.uri(),
            request
                .headers()
                .iter()
                .filter(|(name, _)| name.as_str() != AUTH_HEADER)
                .map(|(name, value)| format!("{name}: {}", value.to_str().unwrap_or("?")))
                .collect::<Vec<_>>()
        );
        let authorized = request
            .headers()
            .get(AUTH_HEADER)
            .and_then(|value| value.to_str().ok())
            == Some(token.as_str());
        if authorized {
            // Claude asks for the `mcp` subprotocol and drops a server that does not confirm it.
            let mut response = response;
            let wants_mcp = request
                .headers()
                .get_all("sec-websocket-protocol")
                .iter()
                .filter_map(|value| value.to_str().ok())
                .any(|value| value.split(',').any(|protocol| protocol.trim() == "mcp"));
            if wants_mcp {
                response.headers_mut().insert(
                    "sec-websocket-protocol",
                    tokio_tungstenite::tungstenite::http::HeaderValue::from_static("mcp"),
                );
            }
            Ok(response)
        } else {
            let mut refusal = ErrorResponse::new(Some("unauthorized".into()));
            *refusal.status_mut() = StatusCode::UNAUTHORIZED;
            Err(refusal)
        }
    };
    let socket = match tokio_tungstenite::accept_hdr_async(stream, check).await {
        Ok(socket) => socket,
        Err(err) => {
            log::warn!("IDE handshake failed: {err}");
            return;
        }
    };
    let (mut sink, mut source) = socket.split();
    // Responses come from several tasks (openDiff answers late), so one task does the writing.
    let (out, mut outgoing) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        loop {
            let message = tokio::select! {
                Some(message) = outgoing.recv() => message,
                Ok(notification) = notifications.recv() => notification,
                else => break,
            };
            if sink.send(Message::text(message.to_string())).await.is_err() {
                break;
            }
        }
    });

    while let Some(Ok(message)) = source.next().await {
        let Ok(text) = message.to_text() else {
            continue;
        };
        let Ok(request) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        let id = request.get("id").cloned();
        let method = request["method"].as_str().unwrap_or_default().to_string();
        let respond = |out: &mpsc::UnboundedSender<Value>, result: Result<Value, Value>| {
            if let Some(id) = &id {
                let _ = out.send(match result {
                    Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                    Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
                });
            }
        };
        match method.as_str() {
            "initialize" => respond(
                &out,
                Ok(json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {
                        "logging": {},
                        "prompts": { "listChanged": true },
                        "tools": { "listChanged": true },
                    },
                    "serverInfo": { "name": "helide", "version": helix_loader::VERSION_AND_GIT_HASH },
                })),
            ),
            "tools/list" => respond(&out, Ok(json!({ "tools": tool_list() }))),
            "prompts/list" => respond(&out, Ok(json!({ "prompts": [] }))),
            "resources/list" => respond(&out, Ok(json!({ "resources": [] }))),
            "tools/call" => {
                let (reply, answer) = oneshot::channel();
                let call = ToolCall {
                    name: request["params"]["name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    arguments: request["params"]["arguments"].clone(),
                    reply,
                };
                if calls.send(call).is_err() {
                    break;
                }
                let out = out.clone();
                let id = id.clone();
                tokio::spawn(async move {
                    let result = answer
                        .await
                        .unwrap_or_else(|_| Err(rpc_error(-32603, "editor closed")));
                    if let Some(id) = id {
                        let _ = out.send(match result {
                            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                            Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
                        });
                    }
                });
            }
            _ if id.is_some() => respond(&out, Err(rpc_error(-32601, "Method not found"))),
            // Notifications such as notifications/initialized need no answer.
            _ => {}
        }
    }
    writer.abort();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_results_are_mcp_content() {
        assert_eq!(
            plain("TAB_CLOSED"),
            json!({ "content": [{ "type": "text", "text": "TAB_CLOSED" }] })
        );
        let wrapped = text(json!({ "success": true }));
        assert_eq!(wrapped["content"][0]["text"], "{\"success\":true}");
        let tools = tool_list();
        let names: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"openDiff") && names.contains(&"close_tab"));
    }

    #[test]
    fn tokens_are_128_bit_hex() {
        let token = random_token();
        assert_eq!(token.len(), 32);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
