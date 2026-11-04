//! Helide agents: Claude Code and OpenCode, run in the right-hand panel.
//!
//! Each agent tab is launched on a session id Helide chooses, so it can be resumed later:
//! Claude takes `--session-id <uuid>` for a new conversation and `--resume <uuid>` once its
//! transcript exists (the two are not interchangeable: `--resume` fails for an id with no
//! conversation yet). OpenCode's `--session <id>` creates the session or continues it, given an
//! id in OpenCode's own shape. The open tabs are saved per project and reopened next time.

use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Claude,
    OpenCode,
}

impl AgentKind {
    pub const ALL: [AgentKind; 2] = [AgentKind::Claude, AgentKind::OpenCode];

    /// The program, and the tab label.
    pub fn command(self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::OpenCode => "opencode",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.command().eq_ignore_ascii_case(name))
    }

    pub fn is_installed(self) -> bool {
        helix_stdx::env::which(self.command()).is_ok()
    }

    pub fn installed() -> Vec<Self> {
        Self::ALL
            .into_iter()
            .filter(|kind| kind.is_installed())
            .collect()
    }
}

/// One agent tab: which agent, and the session it is on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSession {
    pub kind: AgentKind,
    pub id: String,
    /// A name the tab was given by double-clicking it; the agent's name otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl AgentSession {
    pub fn new(kind: AgentKind) -> Self {
        let id = match kind {
            AgentKind::Claude => uuid_v4(),
            AgentKind::OpenCode => opencode_id(),
        };
        Self {
            kind,
            id,
            name: None,
        }
    }

    /// The command line that starts this tab, new or resumed.
    pub fn command(&self) -> Vec<String> {
        let program = self.kind.command().to_string();
        match self.kind {
            // --ide connects to Helide's IDE server on startup (see crate::ide), without asking
            // to turn on auto-connect for every IDE.
            AgentKind::Claude if claude_transcript_exists(&self.id) => {
                vec![program, "--ide".into(), "--resume".into(), self.id.clone()]
            }
            AgentKind::Claude => {
                vec![
                    program,
                    "--ide".into(),
                    "--session-id".into(),
                    self.id.clone(),
                ]
            }
            AgentKind::OpenCode => vec![program, "--session".into(), self.id.clone()],
        }
    }

    /// How a file is mentioned to this agent: `@path`, with a line range for Claude.
    pub fn mention(&self, path: &str, lines: Option<(usize, usize)>) -> String {
        match (self.kind, lines) {
            (AgentKind::Claude, Some((start, end))) if start == end => format!("@{path}#L{start} "),
            (AgentKind::Claude, Some((start, end))) => format!("@{path}#L{start}-{end} "),
            _ => format!("@{path} "),
        }
    }
}

impl AgentSession {
    /// Extra environment for the agent's process. Claude finds the editor through the IDE
    /// server's variables, already exported.
    ///
    /// OpenCode 2 draws a tab strip of its own, a second row of tabs inside a panel that already
    /// has Helide's, and its tabs open sessions Helide never hears of: closing the one a tab
    /// started on still brought that one back next time. So it is switched off, the way the
    /// Windows Helide does it: `OPENCODE_CLI_CONFIG_CONTENT` is merged over the user's own
    /// cli config for this process only, and no file of theirs is touched. (Pointing
    /// `OPENCODE_CONFIG` at a file instead replaced the user's config, MCP servers and all, so
    /// `hx mcp` is registered in their own config; see crate::mcp.) Each Helide tab is then
    /// one OpenCode session, and a new tab is a new session, like /new.
    pub fn env(&self) -> Vec<(String, String)> {
        match self.kind {
            AgentKind::Claude => Vec::new(),
            AgentKind::OpenCode => vec![(
                "OPENCODE_CLI_CONFIG_CONTENT".into(),
                r#"{"tabs":{"mode":"off"},"terminal":{"title":true}}"#.into(),
            )],
        }
    }
}

fn claude_config_dir() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            helix_stdx::path::home_dir()
                .ok()
                .map(|home| home.join(".claude"))
        })
}

/// The conversation a running Claude is in now, or `None` when none of `pids` is a Claude that
/// has said.
///
/// The id Helide launches Claude with is only where it starts: `/clear` and `/resume` move the
/// same process onto another conversation, and without following it the saved id keeps naming
/// the first one, so a restart reopened a chat that had long been left. Claude writes
/// `sessions/<pid>.json` with its current `sessionId`, rewritten when the conversation changes;
/// when several of the processes have one, the most recently started is the one in use.
pub fn claude_live_session(pids: &[u32]) -> Option<String> {
    let folder = claude_config_dir()?.join("sessions");
    pids.iter()
        .filter_map(|pid| {
            let text = std::fs::read_to_string(folder.join(format!("{pid}.json"))).ok()?;
            let json: serde_json::Value = serde_json::from_str(&text).ok()?;
            let id = json["sessionId"].as_str()?;
            is_uuid(id).then(|| (json["startedAt"].as_i64().unwrap_or(0), id.to_string()))
        })
        .max_by_key(|(started, _)| *started)
        .map(|(_, id)| id)
}

/// Where an agent is in its work, from its own records rather than from its terminal: a
/// pause in output is not a turn ending (the model thinks, a tool runs, the TUI slows its
/// redraw), so only the agent's word for it counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Turn {
    /// A turn has started and not yet ended.
    pub busy: bool,
    /// Changes each time a turn ends: where the closing record sits in Claude's transcript,
    /// or OpenCode's idle stamp. `None` before any turn has ended.
    pub ended: Option<u64>,
}

/// The turn `agent` is in, or `None` when its records cannot be read (no conversation yet,
/// or no file or database).
pub fn turn_of(agent: &AgentSession) -> Option<Turn> {
    match agent.kind {
        AgentKind::Claude => claude_turn(&agent.id),
        AgentKind::OpenCode => opencode_turn(&agent.id),
    }
}

/// Claude's transcript, `projects/<folder>/<id>.jsonl`, says when a turn starts and ends. As
/// the first Helide read it: a prompt you typed starts one; an assistant message that stops
/// for any reason but a tool call ends it, as does the note Claude writes when Esc interrupts
/// it. Subagents, slash commands and tool results say nothing. Only the end of the file is
/// read, since it only grows.
fn claude_turn(id: &str) -> Option<Turn> {
    use std::io::{Read, Seek, SeekFrom};
    if !is_uuid(id) {
        return None;
    }
    let projects = claude_config_dir()?.join("projects");
    let path = std::fs::read_dir(projects)
        .ok()?
        .flatten()
        .map(|folder| folder.path().join(format!("{id}.jsonl")))
        .find(|path| path.is_file())?;
    const TAIL: u64 = 256 * 1024;
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    Some(read_claude_turn(&bytes, start))
}

/// The turn the transcript lines in `bytes` describe; `start` is where they begin in the file,
/// and a first line cut off by the start is skipped.
fn read_claude_turn(bytes: &[u8], start: u64) -> Turn {
    let mut turn = Turn {
        busy: false,
        ended: None,
    };
    let mut at = 0;
    if start > 0 {
        match bytes.iter().position(|&b| b == b'\n') {
            Some(newline) => at = newline + 1,
            None => return turn,
        }
    }
    while at < bytes.len() {
        let end = bytes[at..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(bytes.len(), |i| at + i);
        // A line still being written has no newline yet; it is read on the next look.
        if end == bytes.len() && !bytes.ends_with(b"\n") {
            break;
        }
        if let Ok(record) = serde_json::from_slice::<serde_json::Value>(&bytes[at..end]) {
            match claude_mark(&record) {
                Some(true) => turn.busy = true,
                Some(false) => {
                    turn.busy = false;
                    turn.ended = Some(start + end as u64);
                }
                None => {}
            }
        }
        at = end + 1;
    }
    turn
}

/// `Some(true)` for a record that starts a turn, `Some(false)` for one that ends it.
fn claude_mark(record: &serde_json::Value) -> Option<bool> {
    if record["isSidechain"].as_bool() == Some(true) {
        return None;
    }
    let message = record.get("message")?.as_object()?;
    let interrupted = |text: &str| text.starts_with("[Request interrupted");
    match record["type"].as_str()? {
        "assistant" => {
            let stop = message.get("stop_reason")?.as_str()?;
            (!matches!(stop, "tool_use" | "pause_turn")).then_some(false)
        }
        "user" => {
            if record["isMeta"].as_bool() == Some(true) {
                return None;
            }
            match message.get("content")? {
                serde_json::Value::String(text) if interrupted(text) => Some(false),
                // Slash commands and ! lines are run by Claude itself, with no reply.
                serde_json::Value::String(text) => (!(text.starts_with("<command-")
                    || text.starts_with("<local-command")
                    || text.starts_with("<bash-")))
                .then_some(true),
                serde_json::Value::Array(blocks) => {
                    let mut prompt = false;
                    for block in blocks {
                        match block["type"].as_str() {
                            Some("tool_result") | None => {}
                            Some("text") if block["text"].as_str().is_some_and(interrupted) => {
                                return Some(false)
                            }
                            Some(_) => prompt = true,
                        }
                    }
                    prompt.then_some(true)
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// OpenCode stamps a session busy when a turn starts and idle when it ends, in its database.
fn opencode_turn(id: &str) -> Option<Turn> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            helix_stdx::path::home_dir()
                .ok()
                .map(|home| home.join(".local/share"))
        })?;
    let db = data.join("opencode/opencode.db");
    let connection = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let _ = connection.busy_timeout(std::time::Duration::from_millis(200));
    let (busy, idle): (Option<i64>, Option<i64>) = connection
        .query_row(
            "SELECT time_busy, time_idle FROM session_v2 WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok()?;
    Some(Turn {
        busy: busy.is_some_and(|busy| busy > idle.unwrap_or(0)),
        ended: idle.map(|idle| idle as u64),
    })
}

/// The OpenCode session a pane titled `title` is showing, among `project`'s sessions.
///
/// OpenCode keeps no record of which session a running TUI shows, but it sets the terminal's
/// title to `OC | <session title>` (cut to 37 characters and an ellipsis) whenever its view
/// changes, and `opencode session list` gives every session's title. Of the sessions whose
/// title fits, the most recently updated is taken. `None` for OpenCode's own home screen, a
/// session with no title yet, or no match.
pub fn opencode_session_for_title(project: &Path, title: &str) -> Option<String> {
    let shown = title.strip_prefix("OC | ")?;
    let output = Command::new(AgentKind::OpenCode.command())
        .args(["session", "list", "--format", "json", "-n", "200"])
        .current_dir(project)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let sessions: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).ok()?;
    match_title(&sessions, shown)
}

fn match_title(sessions: &[serde_json::Value], shown: &str) -> Option<String> {
    let fits = |title: &str| match shown.strip_suffix('\u{2026}') {
        Some(prefix) => title.starts_with(prefix),
        None => title == shown,
    };
    sessions
        .iter()
        .filter(|s| s["title"].as_str().is_some_and(fits))
        .max_by_key(|s| s["updated"].as_u64().unwrap_or(0))
        .and_then(|s| s["id"].as_str())
        .map(str::to_string)
}

/// A process and every process under it, read from /proc. Claude may run under a wrapper, so
/// its own id is not always the one the pane started.
pub fn self_and_descendants(pid: u32) -> Vec<u32> {
    let mut all = vec![pid];
    let mut next = 0;
    while next < all.len() {
        let parent = all[next];
        next += 1;
        let tasks = std::fs::read_dir(format!("/proc/{parent}/task"));
        for task in tasks.into_iter().flatten().filter_map(Result::ok) {
            let children =
                std::fs::read_to_string(task.path().join("children")).unwrap_or_default();
            all.extend(
                children
                    .split_whitespace()
                    .filter_map(|c| c.parse::<u32>().ok()),
            );
        }
    }
    all
}

fn is_uuid(id: &str) -> bool {
    id.len() == 36
        && id.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Claude keeps `projects/<flattened cwd>/<id>.jsonl`; the id is looked up in every project
/// folder so nothing depends on how the folder name is flattened.
fn claude_transcript_exists(id: &str) -> bool {
    let Some(projects) = claude_config_dir().map(|dir| dir.join("projects")) else {
        return false;
    };
    let file = format!("{id}.jsonl");
    std::fs::read_dir(projects)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .any(|project| project.path().join(&file).is_file())
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .is_err()
    {
        // Not cryptographic, only unique: time and process id.
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
            ^ u128::from(std::process::id());
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = (seed >> ((i % 16) * 8)) as u8 ^ i as u8;
        }
    }
    bytes
}

fn uuid_v4() -> String {
    let mut b = random_bytes::<16>();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// `ses_`, twelve hex digits of the creation time inverted so newer ids sort first, then
/// fourteen random alphanumerics: the shape OpenCode gives its own ids.
fn opencode_id() -> String {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default();
    let inverted = !(millis * 0x1000) & 0xFFFF_FFFF_FFFF;
    let random: String = random_bytes::<14>()
        .iter()
        .map(|byte| ALPHABET[*byte as usize % ALPHABET.len()] as char)
        .collect();
    format!("ses_{inverted:012x}{random}")
}

/// Where a project's saved agent tabs live: `$XDG_STATE_HOME/helide/workspaces/<hash>.json`.
fn workspace_file(project: &Path) -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            helix_stdx::path::home_dir()
                .ok()
                .map(|home| home.join(".local/state"))
        })?;
    // FNV-1a: stable across builds, unlike the standard library's hasher.
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in project.to_string_lossy().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    Some(
        state
            .join("helide/workspaces")
            .join(format!("{hash:016x}.json")),
    )
}

/// Everything Helide keeps per project: agent tabs, the editor's tabs and splits, and the
/// panels, so a project reopens as it was left.
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub project: PathBuf,
    #[serde(default)]
    pub agents: Vec<AgentSession>,
    #[serde(default)]
    pub editor: Option<EditorLayout>,
    #[serde(default)]
    pub panels: Option<Panels>,
    /// Seconds since the epoch when the project was last open, for the recent list.
    #[serde(default)]
    pub opened: u64,
}

/// The editor's tabs, each a tree of splits with files at the leaves.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Debug)]
pub struct EditorLayout {
    pub tabs: Vec<Pane>,
    pub active_tab: usize,
    /// The shown layout's file tabs, one row per side-by-side split, left to right.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rows: Vec<Vec<PathBuf>>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Pane {
    /// A file, with the cursor's line and column (1-based).
    File {
        path: PathBuf,
        line: usize,
        column: usize,
    },
    /// Panes side by side (`vertical`) or stacked.
    Split { vertical: bool, panes: Vec<Pane> },
}

impl Pane {
    /// The first file in reading order: what a split shows before its own panes are restored.
    pub fn first_file(&self) -> Option<&Path> {
        match self {
            Pane::File { path, .. } => Some(path),
            Pane::Split { panes, .. } => panes.iter().find_map(Pane::first_file),
        }
    }

    /// Drops files that no longer exist, and splits left with one pane or none.
    pub fn prune(self) -> Option<Pane> {
        match self {
            Pane::File { ref path, .. } if path.is_file() => Some(self),
            Pane::File { .. } => None,
            Pane::Split { vertical, panes } => {
                let mut panes: Vec<Pane> = panes.into_iter().filter_map(Pane::prune).collect();
                match panes.len() {
                    0 => None,
                    1 => panes.pop(),
                    _ => Some(Pane::Split { vertical, panes }),
                }
            }
        }
    }
}

#[derive(Default, Clone, Serialize, Deserialize, PartialEq, Eq, Debug)]
pub struct Panels {
    pub tree: bool,
    pub git: bool,
    pub terminal: bool,
    pub agents: bool,
    pub sidebar_width: Option<u16>,
    pub terminal_height: Option<u16>,
    pub agents_width: Option<u16>,
    /// The sidebar on the right and the agents on the left, swapped by dragging a panel's title.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub swapped: bool,
    /// The terminal's tabs, so they reopen with their names, folders and programs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub terminals: Vec<TerminalTab>,
}

/// One terminal tab: its panes, side by side.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Debug)]
pub struct TerminalTab {
    pub panes: Vec<TerminalPane>,
}

/// One terminal pane: what it ran, where, and the name it was given.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Debug)]
pub struct TerminalPane {
    /// The program and its arguments; empty for the user's shell.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// The folder the pane's program was in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// The workspace saved for `project`, or an empty one.
pub fn load_workspace(project: &Path) -> Workspace {
    workspace_file(project)
        .and_then(|file| std::fs::read(file).ok())
        .and_then(|bytes| serde_json::from_slice::<Workspace>(&bytes).ok())
        .unwrap_or_else(|| Workspace {
            project: project.to_path_buf(),
            ..Default::default()
        })
}

pub fn save_workspace(workspace: &Workspace) {
    let Some(file) = workspace_file(&workspace.project) else {
        return;
    };
    let result = file
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|_| {
            let json = serde_json::to_vec_pretty(workspace).map_err(std::io::Error::other)?;
            std::fs::write(&file, json)
        });
    if let Err(err) = result {
        log::warn!("cannot save the workspace to {}: {err}", file.display());
    }
}

/// Every saved project that still exists, most recently opened first.
pub fn recent_projects() -> Vec<Workspace> {
    let Some(dir) =
        workspace_file(Path::new("")).and_then(|file| file.parent().map(Path::to_path_buf))
    else {
        return Vec::new();
    };
    let mut projects: Vec<Workspace> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::read(entry.path()).ok())
        .filter_map(|bytes| serde_json::from_slice::<Workspace>(&bytes).ok())
        .filter(|workspace| workspace.project.is_dir())
        .collect();
    projects.sort_by(|a, b| b.opened.cmp(&a.opened));
    projects
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// The agent tabs saved for `project`, oldest first.
pub fn load(project: &Path) -> Vec<AgentSession> {
    load_workspace(project).agents
}

/// Saves the agent tabs, keeping the rest of the workspace.
pub fn save(project: &Path, agents: &[AgentSession]) {
    let mut workspace = load_workspace(project);
    workspace.agents = agents.to_vec();
    save_workspace(&workspace);
}

/// A desktop notification, best effort: the shell's notification daemon shows it.
pub fn notify(summary: &str, body: &str) {
    let spawned = Command::new("notify-send")
        .args(["--app-name=Helide", summary, body])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut child) = spawned {
        // Reap it off the main thread.
        std::thread::spawn(move || child.wait());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_have_the_agents_shapes() {
        let uuid = uuid_v4();
        assert_eq!(uuid.len(), 36);
        assert_eq!(&uuid[14..15], "4");
        assert!(matches!(&uuid[19..20], "8" | "9" | "a" | "b"));

        let id = opencode_id();
        assert!(id.starts_with("ses_"));
        assert_eq!(id.len(), 4 + 12 + 14);
        assert!(id[4..16].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn reads_turns_from_claude_transcripts() {
        let line = |v: serde_json::Value| format!("{v}\n");
        let prompt = line(serde_json::json!({"type": "user", "message": {"content": "fix it"}}));
        let tool =
            line(serde_json::json!({"type": "assistant", "message": {"stop_reason": "tool_use"}}));
        let result = line(
            serde_json::json!({"type": "user", "message": {"content": [{"type": "tool_result"}]}}),
        );
        let done =
            line(serde_json::json!({"type": "assistant", "message": {"stop_reason": "end_turn"}}));
        let side = line(
            serde_json::json!({"type": "assistant", "isSidechain": true, "message": {"stop_reason": "end_turn"}}),
        );

        // Mid-turn: a tool call and its result keep it busy, and a subagent finishing does not end it.
        let working = [prompt.clone(), tool, result, side].concat();
        let turn = read_claude_turn(working.as_bytes(), 0);
        assert!(turn.busy);
        assert_eq!(turn.ended, None);

        let finished = [working.clone(), done].concat();
        let turn = read_claude_turn(finished.as_bytes(), 0);
        assert!(!turn.busy);
        assert_eq!(turn.ended, Some(finished.len() as u64 - 1));

        // A slash command opens no turn; an interruption closes one.
        let command = line(
            serde_json::json!({"type": "user", "message": {"content": "<command-name>/clear</command-name>"}}),
        );
        assert!(!read_claude_turn(command.as_bytes(), 0).busy);
        let stopped = line(
            serde_json::json!({"type": "user", "message": {"content": [{"type": "text", "text": "[Request interrupted by user]"}]}}),
        );
        let turn = read_claude_turn([prompt, stopped].concat().as_bytes(), 0);
        assert!(!turn.busy && turn.ended.is_some());
    }

    #[test]
    fn follows_the_conversation_claude_is_in() {
        let dir = std::env::temp_dir().join(format!("helide-claude-{}", std::process::id()));
        let sessions = dir.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let old = "11111111-1111-4111-8111-111111111111";
        let new = "22222222-2222-4222-8222-222222222222";
        std::fs::write(
            sessions.join("100.json"),
            format!(r#"{{"pid":100,"sessionId":"{old}","startedAt":1}}"#),
        )
        .unwrap();
        std::fs::write(
            sessions.join("101.json"),
            format!(r#"{{"pid":101,"sessionId":"{new}","startedAt":2}}"#),
        )
        .unwrap();
        std::fs::write(sessions.join("102.json"), r#"{"sessionId":"../../etc"}"#).unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", &dir);
        assert_eq!(claude_live_session(&[100, 101, 102]).as_deref(), Some(new));
        assert_eq!(claude_live_session(&[100]).as_deref(), Some(old));
        assert_eq!(claude_live_session(&[102, 999]), None);
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn finds_the_session_an_opencode_title_names() {
        let sessions: Vec<serde_json::Value> = serde_json::from_str(
            r#"[
                {"id": "ses_a", "title": "Token-tide development status", "updated": 5},
                {"id": "ses_b", "title": "PROMPTS.md step 15 datacenter counters and more words", "updated": 9},
                {"id": "ses_c", "title": "PROMPTS.md step 15 datacenter counters and other", "updated": 3}
            ]"#,
        )
        .unwrap();
        assert_eq!(
            match_title(&sessions, "Token-tide development status").as_deref(),
            Some("ses_a")
        );
        // Cut to 37 characters and an ellipsis: the most recently updated fit wins.
        assert_eq!(
            match_title(&sessions, "PROMPTS.md step 15 datacenter counte\u{2026}").as_deref(),
            Some("ses_b")
        );
        assert_eq!(match_title(&sessions, "Something else"), None);
    }

    #[test]
    fn launch_commands_and_mentions() {
        let opencode = AgentSession {
            kind: AgentKind::OpenCode,
            id: "ses_x".into(),
            name: None,
        };
        assert_eq!(opencode.command(), vec!["opencode", "--session", "ses_x"]);
        assert_eq!(
            opencode.env(),
            vec![(
                "OPENCODE_CLI_CONFIG_CONTENT".to_string(),
                r#"{"tabs":{"mode":"off"},"terminal":{"title":true}}"#.to_string()
            )]
        );
        assert_eq!(opencode.mention("src/a.rs", Some((3, 9))), "@src/a.rs ");

        let claude = AgentSession {
            kind: AgentKind::Claude,
            id: "00000000-0000-4000-8000-000000000000".into(),
            name: None,
        };
        // No transcript for this id, so it starts a new conversation under it.
        assert_eq!(claude.command()[1..3], ["--ide", "--session-id"]);
        assert_eq!(claude.mention("src/a.rs", Some((3, 9))), "@src/a.rs#L3-9 ");
        assert_eq!(claude.mention("src/a.rs", Some((4, 4))), "@src/a.rs#L4 ");
        assert_eq!(claude.mention("src/a.rs", None), "@src/a.rs ");
    }
}
