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

#[derive(Default, Serialize, Deserialize)]
struct Workspace {
    project: PathBuf,
    #[serde(default)]
    agents: Vec<AgentSession>,
}

/// The agent tabs saved for `project`, oldest first.
pub fn load(project: &Path) -> Vec<AgentSession> {
    workspace_file(project)
        .and_then(|file| std::fs::read(file).ok())
        .and_then(|bytes| serde_json::from_slice::<Workspace>(&bytes).ok())
        .map(|workspace| workspace.agents)
        .unwrap_or_default()
}

pub fn save(project: &Path, agents: &[AgentSession]) {
    let Some(file) = workspace_file(project) else {
        return;
    };
    let workspace = Workspace {
        project: project.to_path_buf(),
        agents: agents.to_vec(),
    };
    let result = file
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|_| {
            let json = serde_json::to_vec_pretty(&workspace).map_err(std::io::Error::other)?;
            std::fs::write(&file, json)
        });
    if let Err(err) = result {
        log::warn!("cannot save agent tabs to {}: {err}", file.display());
    }
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
