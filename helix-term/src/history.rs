//! Time travel: a private history of every file, separate from git, after Nib's.
//!
//! Each file gets a folder under `$XDG_STATE_HOME/helide/history/<hash of its path>/`. A version
//! is a whole copy of the file named `<unix ms>-<kind>.<ext>`, kept with the file's own
//! extension so a preview of it is highlighted like the file. A `path` file in the folder names
//! the file it belongs to.
//!
//! Versions are taken when a file is opened, every time it is saved, and every 30 seconds while
//! it has unsaved edits. One identical to the newest is not kept. Old ones are thinned (one an
//! hour after a day, one a day after 30 days), and a file's history is held under 2 MB by
//! dropping its oldest versions. Files over 1 MB are not kept at all.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const MAX_FILE: u64 = 1 << 20;
const MAX_HISTORY: u64 = 2 << 20;
const HOUR_MS: u64 = 3_600_000;
const DAY_MS: u64 = 24 * HOUR_MS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The file as it was when it was opened.
    Open,
    Save,
    /// Unsaved edits, taken every 30 seconds.
    Snapshot,
    /// A git commit that changed the file (`:history git`).
    Commit,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Open => "open",
            Kind::Save => "save",
            Kind::Snapshot => "snapshot",
            Kind::Commit => "commit",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        match name {
            "open" => Some(Kind::Open),
            "save" => Some(Kind::Save),
            "snapshot" => Some(Kind::Snapshot),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Open => "opened",
            Kind::Save => "saved",
            Kind::Snapshot => "edit",
            Kind::Commit => "commit",
        }
    }
}

/// One kept version of a file.
#[derive(Debug, Clone)]
pub struct Version {
    /// Where its copy lives.
    pub copy: PathBuf,
    /// Milliseconds since the epoch.
    pub time: u64,
    pub kind: Kind,
    /// For a commit: its short hash and subject.
    pub subject: Option<String>,
}

impl Version {
    /// How long ago it was taken, in a few characters: "now", "4m", "3h", "2d".
    pub fn age(&self) -> String {
        let seconds = now_ms().saturating_sub(self.time) / 1000;
        match seconds {
            0..=59 => "now".into(),
            60..=3599 => format!("{}m", seconds / 60),
            3600..=86_399 => format!("{}h", seconds / 3600),
            _ => format!("{}d", seconds / 86_400),
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
}

fn root() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            helix_stdx::path::home_dir()
                .ok()
                .map(|home| home.join(".local/state"))
        })?;
    Some(state.join("helide/history"))
}

/// The folder holding `file`'s versions.
fn folder(file: &Path) -> Option<PathBuf> {
    // FNV-1a: stable across builds, unlike the standard library's hasher.
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in file.to_string_lossy().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    Some(root()?.join(format!("{hash:016x}")))
}

/// Every kept version of `file`, newest first.
pub fn versions(file: &Path) -> Vec<Version> {
    let Some(dir) = folder(file) else {
        return Vec::new();
    };
    versions_in(&dir)
}

fn versions_in(dir: &Path) -> Vec<Version> {
    let mut all: Vec<Version> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let stem = name.split('.').next()?;
            let (time, kind) = stem.split_once('-')?;
            Some(Version {
                copy: entry.path(),
                time: time.parse().ok()?,
                kind: Kind::from_name(kind)?,
                subject: None,
            })
        })
        .collect();
    all.sort_by(|a, b| b.time.cmp(&a.time));
    all
}

/// The commits that changed `file`, following renames, newest first and at most `limit`: git's
/// side of time travel. Each comes with the file as it was at that commit, copied out (once) so
/// the picker can preview it highlighted.
pub fn git_versions(file: &Path, limit: usize) -> Vec<Version> {
    use std::process::Command;
    let run = |dir: &Path, args: &[&str]| -> Option<Vec<u8>> {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .output()
            .ok()?;
        output.status.success().then_some(output.stdout)
    };
    let (Some(dir), Some(cache)) = (
        file.parent(),
        root().map(|r| r.with_file_name("git-history")),
    ) else {
        return Vec::new();
    };
    let Some(top) = run(dir, &["rev-parse", "--show-toplevel"])
        .map(|out| PathBuf::from(String::from_utf8_lossy(&out).trim()))
    else {
        return Vec::new();
    };
    let limit = limit.to_string();
    let file_arg = file.to_string_lossy();
    let Some(log) = run(
        &top,
        &[
            "log",
            "--follow",
            "-n",
            &limit,
            "--format=%x1e%H%x1f%ct%x1f%s",
            "--name-only",
            "--",
            &file_arg,
        ],
    ) else {
        return Vec::new();
    };
    let log = String::from_utf8_lossy(&log);
    let _ = fs::create_dir_all(&cache);
    let extension = file
        .extension()
        .map(|ext| format!(".{}", ext.to_string_lossy()))
        .unwrap_or_default();
    log.split('\x1e')
        .filter_map(|record| {
            let mut lines = record.lines().filter(|l| !l.trim().is_empty());
            let mut fields = lines.next()?.split('\x1f');
            let (hash, time, subject) = (fields.next()?, fields.next()?, fields.next()?);
            let name = lines.next()?;
            let copy = cache.join(format!("{hash}{extension}"));
            if !copy.exists() {
                let text = run(&top, &["show", &format!("{hash}:{name}")])?;
                fs::write(&copy, text).ok()?;
            }
            Some(Version {
                copy,
                time: time.parse::<u64>().ok()? * 1000,
                kind: Kind::Commit,
                subject: Some(format!("{} {subject}", &hash[..hash.len().min(7)])),
            })
        })
        .collect()
}

/// Keeps `text` as a version of `file`, unless it is the same as the newest one or too big.
pub fn record(file: &Path, text: &str, kind: Kind) -> std::io::Result<()> {
    if text.len() as u64 > MAX_FILE {
        return Ok(());
    }
    let Some(dir) = folder(file) else {
        return Ok(());
    };
    record_in(&dir, file, text, kind, now_ms())
}

fn record_in(dir: &Path, file: &Path, text: &str, kind: Kind, time: u64) -> std::io::Result<()> {
    let existing = versions_in(dir);
    if let Some(newest) = existing.first() {
        if fs::read(&newest.copy).is_ok_and(|bytes| bytes == text.as_bytes()) {
            return Ok(());
        }
    }
    fs::create_dir_all(dir)?;
    fs::write(dir.join("path"), file.to_string_lossy().as_bytes())?;
    let extension = file
        .extension()
        .map(|ext| format!(".{}", ext.to_string_lossy()))
        .unwrap_or_default();
    fs::write(dir.join(format!("{time}-{}{extension}", kind.name())), text)?;
    prune(dir, time);
    Ok(())
}

/// Thins old versions and holds the folder under its size cap, oldest first.
fn prune(dir: &Path, now: u64) {
    let all = versions_in(dir);
    let mut kept: Vec<&Version> = Vec::new();
    let mut last_bucket = None;
    for version in &all {
        let age = now.saturating_sub(version.time);
        let bucket = if age > 30 * DAY_MS {
            Some(version.time / DAY_MS)
        } else if age > DAY_MS {
            Some(version.time / HOUR_MS)
        } else {
            None
        };
        // Newest first, so the first version seen in a bucket is the one kept.
        if bucket.is_some() && bucket == last_bucket {
            let _ = fs::remove_file(&version.copy);
            continue;
        }
        last_bucket = bucket;
        kept.push(version);
    }
    let mut total = 0;
    for version in kept {
        total += fs::metadata(&version.copy).map_or(0, |m| m.len());
        if total > MAX_HISTORY {
            let _ = fs::remove_file(&version.copy);
        }
    }
}

/// Lines added and removed going from `old` to `new`, counted by a line diff.
pub fn line_changes(old: &str, new: &str) -> (usize, usize) {
    let old = helix_core::Rope::from_str(old);
    let new = helix_core::Rope::from_str(new);
    let transaction = helix_core::diff::compare_ropes(&old, &new);
    let changes = transaction.changes();
    let mut adds = 0;
    let mut dels = 0;
    let mut pos = 0;
    for change in changes.changes() {
        use helix_core::Operation;
        match change {
            Operation::Retain(n) => pos += n,
            Operation::Delete(n) => {
                let end = (pos + n).min(old.len_chars());
                dels += old.slice(pos..end).len_lines().max(1);
                pos += n;
            }
            Operation::Insert(text) => adds += text.lines().count().max(1),
        }
    }
    (adds, dels)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("helide-history-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn keeps_versions_newest_first_and_skips_repeats() {
        let dir = temp("order");
        let file = Path::new("/p/a.rs");
        record_in(&dir, file, "one", Kind::Open, 1_000).unwrap();
        record_in(&dir, file, "one", Kind::Save, 2_000).unwrap();
        record_in(&dir, file, "two", Kind::Snapshot, 3_000).unwrap();
        let all = versions_in(&dir);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].kind, Kind::Snapshot);
        assert_eq!(all[1].kind, Kind::Open);
        assert!(all[0].copy.to_string_lossy().ends_with("3000-snapshot.rs"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn thins_versions_older_than_a_day_to_one_an_hour() {
        let dir = temp("thin");
        let file = Path::new("/p/b.txt");
        let now = 40 * DAY_MS;
        let old = now - 2 * DAY_MS;
        for (i, minute) in [0u64, 10, 20].iter().enumerate() {
            record_in(
                &dir,
                file,
                &format!("v{i}"),
                Kind::Save,
                old + minute * 60_000,
            )
            .unwrap();
        }
        record_in(&dir, file, "today", Kind::Save, now).unwrap();
        let all = versions_in(&dir);
        // Three in one hour two days ago become one; today's stays.
        assert_eq!(all.len(), 2);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn reads_the_commits_that_changed_a_file() {
        let state = temp("git");
        std::env::set_var("XDG_STATE_HOME", &state);
        let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let commits = git_versions(&file, 3);
        std::env::remove_var("XDG_STATE_HOME");
        assert!(!commits.is_empty(), "this crate's Cargo.toml has commits");
        assert!(commits.len() <= 3);
        assert!(commits
            .iter()
            .all(|c| c.kind == Kind::Commit && c.copy.is_file()));
        assert!(commits.windows(2).all(|w| w[0].time >= w[1].time));
        let _ = fs::remove_dir_all(state);
    }

    #[test]
    fn counts_changed_lines() {
        assert_eq!(line_changes("a\nb\nc\n", "a\nb\nc\n"), (0, 0));
        let (adds, dels) = line_changes("a\nb\n", "a\nx\ny\nb\n");
        assert!(adds >= 2 && dels == 0, "{adds} {dels}");
    }
}
