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
}

impl AgentSession {
    pub fn new(kind: AgentKind) -> Self {
        let id = match kind {
            AgentKind::Claude => uuid_v4(),
            AgentKind::OpenCode => opencode_id(),
        };
        Self { kind, id }
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
    /// server's variables, already exported. OpenCode gets nothing extra: in OpenCode 2.x,
    /// pointing `OPENCODE_CONFIG` at a file replaced the user's config (their MCP servers
    /// disappeared) instead of merging with it, so `hx mcp` is registered in the user's own
    /// config instead (see crate::mcp).
    pub fn env(&self) -> Vec<(String, String)> {
        Vec::new()
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
    fn launch_commands_and_mentions() {
        let opencode = AgentSession {
            kind: AgentKind::OpenCode,
            id: "ses_x".into(),
        };
        assert_eq!(opencode.command(), vec!["opencode", "--session", "ses_x"]);
        assert_eq!(opencode.mention("src/a.rs", Some((3, 9))), "@src/a.rs ");

        let claude = AgentSession {
            kind: AgentKind::Claude,
            id: "00000000-0000-4000-8000-000000000000".into(),
        };
        // No transcript for this id, so it starts a new conversation under it.
        assert_eq!(claude.command()[1..3], ["--ide", "--session-id"]);
        assert_eq!(claude.mention("src/a.rs", Some((3, 9))), "@src/a.rs#L3-9 ");
        assert_eq!(claude.mention("src/a.rs", Some((4, 4))), "@src/a.rs#L4 ");
        assert_eq!(claude.mention("src/a.rs", None), "@src/a.rs ");
    }
}
