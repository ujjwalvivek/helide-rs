//! Helide IPC: a Unix socket per editor process that accepts newline-delimited JSON requests.
//!
//! Each request is `{"id": <any>, "method": "<name>", "params": {...}}` and gets exactly one
//! response line, `{"id": <same>, "ok": true, "result": ...}` or `{"id": ..., "ok": false,
//! "error": "..."}`. The socket path is exported as `HELIDE_SOCKET` so that processes spawned by
//! the editor (shell commands, terminal panes, agents) talk to their own editor.
//!
//! Methods:
//! - `ping`: version, pid and working directory.
//! - `open {path, line?, col?}`: open a file (1-based line and column) in the focused view.
//! - `command {line}`: run a typable command line, e.g. `reload-all` or `w`.
//! - `status`: mode, focused file and cursor, selection text and open buffers.
//! - `lsp`: every running language server with its state and in-flight progress.
//! - `diagnostics {path?}`: diagnostics for one file, or every file when `path` is omitted.
//! - `keys {keys}`: types a key sequence such as `<C-1>j<space>` as if it was pressed.

use std::path::{Path, PathBuf};

use helix_core::{coords_at_pos, pos_at_coords, Position, Range};
use helix_lsp::{lsp, LspProgressMap, ProgressStatus};
use helix_view::{align_view, editor::Action, Align, Editor};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use crate::{compositor, job::Jobs, ui::PromptEvent};

pub const SOCKET_ENV: &str = "HELIDE_SOCKET";

/// Selection text returned by `status` is cut off after this many chars.
const MAX_SELECTION_CHARS: usize = 64 * 1024;

#[derive(Debug, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

pub type Pending = (Request, oneshot::Sender<Value>);

/// The editor side of the socket. Requests are handed to the event loop through `rx`, so every
/// method runs on the main thread with full access to the editor.
pub struct Ipc {
    pub rx: mpsc::UnboundedReceiver<Pending>,
    // Keeps `rx` open even when the socket could not be bound, so the event loop branch simply
    // never fires instead of resolving to `None` on every iteration.
    _tx: mpsc::UnboundedSender<Pending>,
    path: Option<PathBuf>,
}

impl Ipc {
    pub fn start() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let path = match listen(tx.clone()) {
            Ok(path) => Some(path),
            Err(err) => {
                log::warn!("helide ipc disabled: {err}");
                None
            }
        };
        Self { rx, _tx: tx, path }
    }
}

impl Drop for Ipc {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = std::fs::remove_file(path);
        }
    }
}

pub fn socket_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("helide")
}

#[cfg(unix)]
fn listen(tx: mpsc::UnboundedSender<Pending>) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;

    let dir = socket_dir();
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let path = dir.join(format!("hx-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    std::env::set_var(SOCKET_ENV, &path);

    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    tokio::spawn(serve(stream, tx.clone()));
                }
                Err(err) => {
                    log::error!("helide ipc accept failed: {err}");
                    break;
                }
            }
        }
    });

    Ok(path)
}

#[cfg(not(unix))]
fn listen(_tx: mpsc::UnboundedSender<Pending>) -> std::io::Result<PathBuf> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "only Unix sockets are supported",
    ))
}

#[cfg(unix)]
async fn serve(stream: tokio::net::UnixStream, tx: mpsc::UnboundedSender<Pending>) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) => {
                let (reply_tx, reply_rx) = oneshot::channel();
                let id = request.id.clone();
                if tx.send((request, reply_tx)).is_err() {
                    break;
                }
                reply_rx
                    .await
                    .unwrap_or_else(|_| error(id, "editor dropped the request"))
            }
            Err(err) => error(Value::Null, format!("invalid request: {err}")),
        };
        let mut out = response.to_string();
        out.push('\n');
        if write.write_all(out.as_bytes()).await.is_err() {
            break;
        }
    }
}

fn error(id: Value, message: impl ToString) -> Value {
    json!({ "id": id, "ok": false, "error": message.to_string() })
}

/// Types `params.keys` (macro syntax, e.g. `<C-1>jj<ret>`) into the editor, as if pressed.
pub fn send_keys(
    compositor: &mut compositor::Compositor,
    editor: &mut Editor,
    jobs: &mut Jobs,
    request: Request,
) -> Value {
    let id = request.id;
    let keys = match str_param(&request.params, "keys").and_then(helix_view::input::parse_macro) {
        Ok(keys) => keys,
        Err(err) => return error(id, err),
    };
    let mut cx = compositor::Context {
        editor,
        jobs,
        scroll: None,
    };
    let mut sent = 0;
    for key in keys {
        compositor.handle_event(&compositor::Event::Key(key), &mut cx);
        sent += 1;
        if cx.editor.should_close() {
            break;
        }
    }
    json!({ "id": id, "ok": true, "result": { "sent": sent } })
}

/// Runs one request against the editor. Called from the event loop.
pub fn handle(
    editor: &mut Editor,
    jobs: &mut Jobs,
    lsp_progress: &LspProgressMap,
    request: Request,
) -> Value {
    let id = request.id;
    let result = match request.method.as_str() {
        "ping" => Ok(ping()),
        "open" => open(editor, &request.params),
        "command" => command(editor, jobs, &request.params),
        "status" => Ok(status(editor)),
        "lsp" => Ok(language_servers(editor, lsp_progress)),
        "diagnostics" => diagnostics(editor, &request.params),
        other => Err(anyhow::anyhow!("unknown method '{other}'")),
    };
    match result {
        Ok(result) => json!({ "id": id, "ok": true, "result": result }),
        Err(err) => error(id, err),
    }
}

fn ping() -> Value {
    json!({
        "version": helix_loader::VERSION_AND_GIT_HASH,
        "pid": std::process::id(),
        "cwd": helix_stdx::env::current_working_dir(),
    })
}

fn str_param<'a>(params: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    params
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing string param '{key}'"))
}

fn open(editor: &mut Editor, params: &Value) -> anyhow::Result<Value> {
    let path = helix_stdx::path::expand_tilde(Path::new(str_param(params, "path")?));
    let path = helix_stdx::path::canonicalize(path);
    let line = params.get("line").and_then(Value::as_u64).unwrap_or(1);
    let col = params.get("col").and_then(Value::as_u64).unwrap_or(1);

    let doc_id = editor.open(&path, Action::Replace)?;
    let view_id = editor.tree.focus;
    let doc = doc_mut!(editor, &doc_id);
    if params.get("line").is_some() {
        let coords = Position::new(
            line.saturating_sub(1) as usize,
            col.saturating_sub(1) as usize,
        );
        let pos = pos_at_coords(doc.text().slice(..), coords, true);
        doc.set_selection(view_id, Range::point(pos).into());
    }
    let (view, doc) = current!(editor);
    align_view(doc, view, Align::Center);
    Ok(json!({ "path": path }))
}

fn command(editor: &mut Editor, jobs: &mut Jobs, params: &Value) -> anyhow::Result<Value> {
    let line = str_param(params, "line")?;
    let line = line.strip_prefix(':').unwrap_or(line);
    let mut cx = compositor::Context {
        editor,
        jobs,
        scroll: None,
    };
    crate::commands::execute_command_line(&mut cx, line, PromptEvent::Validate)?;
    let status = editor.get_status().map(|(msg, _)| msg.to_string());
    Ok(json!({ "status": status }))
}

fn status(editor: &Editor) -> Value {
    let (view, doc) = current_ref!(editor);
    let text = doc.text().slice(..);
    let selection = doc.selection(view.id);
    let primary = selection.primary();
    let cursor = coords_at_pos(text, primary.cursor(text));
    let selected: String = primary
        .fragment(text)
        .chars()
        .take(MAX_SELECTION_CHARS)
        .collect();

    let buffers: Vec<Value> = editor
        .documents()
        .map(|doc| {
            json!({
                "id": doc.id().to_string(),
                "path": doc.path(),
                "name": doc.display_name(),
                "language": doc.language_name(),
                "modified": doc.is_modified(),
            })
        })
        .collect();

    json!({
        "mode": editor.mode().to_string(),
        "cwd": helix_stdx::env::current_working_dir(),
        "focused": {
            "path": doc.path(),
            "language": doc.language_name(),
            "modified": doc.is_modified(),
            "line": cursor.row + 1,
            "col": cursor.col + 1,
            "selections": selection.len(),
            "selection": selected,
        },
        "buffers": buffers,
    })
}

fn language_servers(editor: &Editor, lsp_progress: &LspProgressMap) -> Value {
    let servers: Vec<Value> = editor
        .language_servers
        .iter_clients()
        .map(|client| {
            let id = client.id();
            let progress: Vec<Value> = lsp_progress
                .progress_map(id)
                .into_iter()
                .flat_map(|tokens| tokens.values())
                .filter_map(|status| match status {
                    ProgressStatus::Created => None,
                    ProgressStatus::Started { title, progress } => {
                        let (message, percentage) = match progress {
                            lsp::WorkDoneProgress::Begin(p) => (p.message.clone(), p.percentage),
                            lsp::WorkDoneProgress::Report(p) => (p.message.clone(), p.percentage),
                            lsp::WorkDoneProgress::End(p) => (p.message.clone(), None),
                        };
                        Some(json!({
                            "title": title,
                            "message": message,
                            "percentage": percentage,
                        }))
                    }
                })
                .collect();
            let documents = editor
                .documents()
                .filter(|doc| doc.language_servers().any(|ls| ls.id() == id))
                .count();
            json!({
                "id": format!("{id:?}"),
                "name": client.name(),
                "initialized": client.is_initialized(),
                "busy": lsp_progress.is_progressing(id),
                "progress": progress,
                "documents": documents,
            })
        })
        .collect();
    json!(servers)
}

fn diagnostics(editor: &Editor, params: &Value) -> anyhow::Result<Value> {
    let only = params.get("path").and_then(Value::as_str).map(|path| {
        helix_stdx::path::canonicalize(helix_stdx::path::expand_tilde(Path::new(path)))
    });
    let files: Vec<Value> = editor
        .diagnostics
        .iter()
        .filter_map(|(uri, diags)| {
            let path = uri.as_path()?;
            if only.as_deref().is_some_and(|only| only != path) || diags.is_empty() {
                return None;
            }
            let diags: Vec<&lsp::Diagnostic> = diags.iter().map(|(diag, _)| diag).collect();
            Some(json!({ "path": path, "diagnostics": diags }))
        })
        .collect();
    Ok(json!(files))
}

/// `hx msg <method> [args]`: sends one request to a running editor and prints the response.
pub mod client {
    use super::*;

    const USAGE: &str = "\
usage: hx msg <method> [args]

methods:
    ping                         editor version, pid and working directory
    open <file[:line[:col]]>     open a file in the focused view
    command <command line>       run a typable command, e.g. `hx msg command reload-all`
    status                       mode, focused file, cursor, selection and open buffers
    lsp                          language servers with their state and progress
    diagnostics [file]           diagnostics for one file, or all files
    keys <keys>                  type keys as if pressed, e.g. `hx msg keys '<C-1>j<space>'`

The editor is found through $HELIDE_SOCKET, which is set for processes started by the editor;
otherwise the most recently started editor is used.";

    pub fn run(args: &[String]) -> anyhow::Result<i32> {
        let Some((method, rest)) = args.split_first() else {
            eprintln!("{USAGE}");
            return Ok(2);
        };
        let params = match method.as_str() {
            "open" => {
                let target = rest
                    .first()
                    .ok_or_else(|| anyhow::anyhow!("open needs a file"))?;
                parse_target(target)
            }
            "command" => json!({ "line": rest.join(" ") }),
            "keys" => json!({ "keys": rest.join(" ") }),
            "diagnostics" => match rest.first() {
                Some(path) => json!({ "path": absolute(path) }),
                None => json!({}),
            },
            "-h" | "--help" | "help" => {
                println!("{USAGE}");
                return Ok(0);
            }
            _ => json!({}),
        };
        let request = json!({ "id": 1, "method": method, "params": params });
        let response = send(&request)?;
        println!("{}", serde_json::to_string_pretty(&response)?);
        Ok(if response["ok"].as_bool() == Some(true) {
            0
        } else {
            1
        })
    }

    fn absolute(path: &str) -> PathBuf {
        let path = helix_stdx::path::expand_tilde(Path::new(path));
        std::env::current_dir()
            .map(|cwd| cwd.join(&path))
            .unwrap_or_else(|_| path.into_owned())
    }

    /// `file`, `file:line` or `file:line:col`.
    pub(super) fn parse_target(target: &str) -> Value {
        let mut parts = target.rsplitn(3, ':');
        let mut nums = Vec::new();
        let mut path = target;
        for _ in 0..2 {
            let Some(part) = parts.next() else { break };
            match part.parse::<u64>() {
                Ok(n) => {
                    nums.insert(0, n);
                    path = &path[..path.len() - part.len() - 1];
                }
                Err(_) => break,
            }
        }
        let mut params = json!({ "path": absolute(path) });
        if let Some(line) = nums.first() {
            params["line"] = json!(line);
        }
        if let Some(col) = nums.get(1) {
            params["col"] = json!(col);
        }
        params
    }

    /// `$HELIDE_SOCKET` alone if set, otherwise every socket in the socket dir, newest first.
    fn candidate_sockets() -> Vec<PathBuf> {
        if let Some(path) = std::env::var_os(SOCKET_ENV) {
            return vec![PathBuf::from(path)];
        }
        let mut sockets: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(socket_dir())
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let modified = entry.metadata().ok()?.modified().ok()?;
                Some((modified, entry.path()))
            })
            .collect();
        sockets.sort();
        sockets.into_iter().rev().map(|(_, path)| path).collect()
    }

    #[cfg(unix)]
    fn connect() -> anyhow::Result<std::os::unix::net::UnixStream> {
        use std::os::unix::net::UnixStream;

        let explicit = std::env::var_os(SOCKET_ENV).is_some();
        for path in candidate_sockets() {
            match UnixStream::connect(&path) {
                Ok(stream) => return Ok(stream),
                Err(err) if explicit => anyhow::bail!("cannot connect to {path:?}: {err}"),
                // Left behind by an editor that crashed.
                Err(err) if err.kind() == std::io::ErrorKind::ConnectionRefused => {
                    let _ = std::fs::remove_file(&path);
                }
                Err(_) => {}
            }
        }
        anyhow::bail!("no running editor found in {:?}", socket_dir())
    }

    #[cfg(unix)]
    fn send(request: &Value) -> anyhow::Result<Value> {
        use std::io::{BufRead, BufReader, Write};

        let mut stream = connect()?;
        writeln!(stream, "{request}")?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line)?;
        Ok(serde_json::from_str(&line)?)
    }

    #[cfg(not(unix))]
    fn send(_request: &Value) -> anyhow::Result<Value> {
        anyhow::bail!("hx msg needs Unix sockets")
    }

    /// Sends one request to the running editor and returns its response.
    pub fn request(method: &str, params: Value) -> anyhow::Result<Value> {
        send(&json!({ "id": 1, "method": method, "params": params }))
    }
}

#[cfg(test)]
mod tests {
    use super::client::parse_target;
    use serde_json::json;

    #[test]
    fn parses_targets() {
        let cwd = std::env::current_dir().unwrap();
        let at = |path: &str| json!(cwd.join(path));

        assert_eq!(parse_target("a.rs"), json!({ "path": at("a.rs") }));
        assert_eq!(
            parse_target("a.rs:12"),
            json!({ "path": at("a.rs"), "line": 12 })
        );
        assert_eq!(
            parse_target("src/a.rs:12:4"),
            json!({ "path": at("src/a.rs"), "line": 12, "col": 4 })
        );
        assert_eq!(
            parse_target("/abs/a.rs:3"),
            json!({ "path": "/abs/a.rs", "line": 3 })
        );
        // A non-numeric suffix stays part of the path.
        assert_eq!(parse_target("a:b.rs"), json!({ "path": at("a:b.rs") }));
    }
}
