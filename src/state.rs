//! The state directory: one entry per run, each held alive by an `flock`.
//!
//! Whether a holder still lives is never guessed from a PID or a timestamp.
//! The kernel drops the lock when the process dies, SIGKILL included, and a
//! recycled PID cannot fake it.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rustix::fs::{FlockOperation, flock};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum RunState {
    Waiting,
    Running,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Entry {
    /// `<seq>-<pid>`
    pub id: String,
    pub seq: u64,
    /// The lotse process.
    pub pid: u32,
    /// What it started, once it has.
    pub child_pid: Option<u32>,
    pub class: String,
    pub target: Option<String>,
    pub cwd: PathBuf,
    pub command: Vec<String>,
    pub state: RunState,
    pub created: u64,
    pub started: Option<u64>,
    pub log: Option<PathBuf>,
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub struct StateDir {
    root: PathBuf,
}

/// Held for the length of one admission decision, released on drop.
pub struct GlobalLock {
    _file: File,
}

pub struct OwnedEntry {
    pub entry: Entry,
    json: PathBuf,
    lock_path: PathBuf,
    _lock: File,
}

fn open_lock_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path)
}

impl StateDir {
    /// `$XDG_RUNTIME_DIR/lotse`, else `/tmp/lotse-<uid>`.
    pub fn open() -> Result<StateDir> {
        let root = match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("lotse"),
            _ => PathBuf::from(format!("/tmp/lotse-{}", rustix::process::getuid().as_raw())),
        };
        StateDir::at(root)
    }

    pub fn at(root: PathBuf) -> Result<StateDir> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(root.join("entries"))
            .with_context(|| format!("cannot create the state directory {}", root.display()))?;
        Ok(StateDir { root })
    }

    fn entries(&self) -> PathBuf {
        self.root.join("entries")
    }

    pub fn lock(&self) -> Result<GlobalLock> {
        let path = self.root.join("lock");
        let file =
            open_lock_file(&path).with_context(|| format!("cannot open {}", path.display()))?;
        flock(&file, FlockOperation::LockExclusive)
            .with_context(|| format!("cannot lock {}", path.display()))?;
        Ok(GlobalLock { _file: file })
    }

    fn next_seq(&self) -> Result<u64> {
        let path = self.root.join("seq");
        let last: u64 = fs::read_to_string(&path)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        fs::write(&path, format!("{}\n", last + 1))
            .with_context(|| format!("cannot write {}", path.display()))?;
        Ok(last + 1)
    }

    pub fn create(
        &self,
        _held: &GlobalLock,
        class: &str,
        target: Option<&str>,
        cwd: &Path,
        command: &[String],
    ) -> Result<OwnedEntry> {
        let seq = self.next_seq()?;
        let pid = std::process::id();
        let id = format!("{seq:08}-{pid}");
        let lock_path = self.entries().join(format!("{id}.lock"));
        let json = self.entries().join(format!("{id}.json"));
        // The lock first: nobody may ever see an entry that is not held.
        let lock = open_lock_file(&lock_path)
            .with_context(|| format!("cannot create {}", lock_path.display()))?;
        flock(&lock, FlockOperation::LockExclusive)?;
        let mut owned = OwnedEntry {
            entry: Entry {
                id,
                seq,
                pid,
                child_pid: None,
                class: class.to_string(),
                target: target.map(str::to_string),
                cwd: cwd.to_path_buf(),
                command: command.to_vec(),
                state: RunState::Waiting,
                created: now(),
                started: None,
                log: None,
            },
            json,
            lock_path,
            _lock: lock,
        };
        owned.write()?;
        Ok(owned)
    }

    /// The entries whose holders live, oldest first. Buries the others.
    pub fn live(&self, _held: &GlobalLock) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        for file in fs::read_dir(self.entries())?.flatten() {
            let json = file.path();
            if json.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let lock_path = json.with_extension("lock");
            let alive = match OpenOptions::new().write(true).open(&lock_path) {
                // Getting the lock means nobody holds it: the holder is dead.
                // Our own entry is refused like anyone else's, because flock
                // belongs to the open file description, not to the process.
                Ok(f) => flock(&f, FlockOperation::NonBlockingLockExclusive).is_err(),
                Err(_) => false,
            };
            if !alive {
                let _ = fs::remove_file(&json);
                let _ = fs::remove_file(&lock_path);
                continue;
            }
            // An unreadable entry of a live holder is skipped, not buried.
            if let Some(entry) = fs::read(&json)
                .ok()
                .and_then(|b| serde_json::from_slice::<Entry>(&b).ok())
            {
                out.push(entry);
            }
        }
        out.sort_by_key(|e| e.seq);
        Ok(out)
    }
}

impl OwnedEntry {
    fn write(&mut self) -> Result<()> {
        let tmp = self.json.with_extension("tmp");
        let mut f = File::create(&tmp)?;
        f.write_all(&serde_json::to_vec(&self.entry)?)?;
        drop(f);
        fs::rename(&tmp, &self.json)?;
        Ok(())
    }

    pub fn update(&mut self, change: impl FnOnce(&mut Entry)) -> Result<()> {
        change(&mut self.entry);
        self.write()
    }
}

impl Drop for OwnedEntry {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.json);
        let _ = fs::remove_file(&self.lock_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> (tempfile::TempDir, StateDir) {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::at(tmp.path().join("lotse")).unwrap();
        (tmp, state)
    }

    fn create(state: &StateDir, class: &str) -> OwnedEntry {
        let lock = state.lock().unwrap();
        state
            .create(&lock, class, None, Path::new("/"), &["true".to_string()])
            .unwrap()
    }

    fn live(state: &StateDir) -> Vec<Entry> {
        state.live(&state.lock().unwrap()).unwrap()
    }

    #[test]
    fn entries_are_numbered_and_listed_in_order() {
        let (_tmp, state) = dir();
        let a = create(&state, "a");
        let b = create(&state, "b");
        assert_eq!((a.entry.seq, b.entry.seq), (1, 2));
        let classes: Vec<String> = live(&state).into_iter().map(|e| e.class).collect();
        assert_eq!(classes, ["a", "b"]);
    }

    #[test]
    fn a_dropped_entry_is_gone() {
        let (_tmp, state) = dir();
        let a = create(&state, "a");
        let _b = create(&state, "b");
        drop(a);
        assert_eq!(live(&state).len(), 1);
    }

    #[test]
    fn an_entry_nobody_holds_is_buried() {
        let (_tmp, state) = dir();
        let held = create(&state, "held");
        // What a SIGKILL leaves behind: both files, no lock.
        let json = state.entries().join("00000099-1.json");
        let mut orphan = held.entry.clone();
        orphan.seq = 99;
        fs::write(&json, serde_json::to_vec(&orphan).unwrap()).unwrap();
        fs::write(json.with_extension("lock"), "").unwrap();
        let seen = live(&state);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].class, "held");
        assert!(!json.exists());
        assert!(!json.with_extension("lock").exists());
    }

    #[test]
    fn an_update_is_visible() {
        let (_tmp, state) = dir();
        let mut a = create(&state, "a");
        a.update(|e| {
            e.state = RunState::Running;
            e.child_pid = Some(42);
        })
        .unwrap();
        let seen = live(&state);
        assert_eq!(seen[0].state, RunState::Running);
        assert_eq!(seen[0].child_pid, Some(42));
    }
}
