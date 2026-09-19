//! One consistent look at the machine: live entries, observed runs, memory.

use std::path::Path;

use anyhow::Result;

use crate::admit::{Active, Queued, targets_conflict};
use crate::config::Config;
use crate::proc::{Observed, ProcSource, ProcTable, observe};
use crate::state::{Entry, GlobalLock, RunState, StateDir};

pub struct Snapshot {
    pub entries: Vec<Entry>,
    pub observed: Vec<Observed>,
    pub table: ProcTable,
    pub mem_available: u64,
}

/// The part after `.claude/worktrees/`, else the last component: the name a
/// person would use for "that session over there".
pub fn worktree_name(cwd: &Path) -> String {
    let parts: Vec<String> = cwd
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if let Some(i) = parts
        .windows(2)
        .position(|w| w[0] == ".claude" && w[1] == "worktrees")
        && let Some(name) = parts.get(i + 2)
    {
        return name.clone();
    }
    parts.last().cloned().unwrap_or_else(|| "/".to_string())
}

pub fn label(class: &str, target: Option<&str>, cwd: Option<&Path>, pid: u32) -> String {
    let target = target.map(|t| format!("[{t}]")).unwrap_or_default();
    let place = cwd.map(worktree_name).unwrap_or_else(|| "?".to_string());
    format!("{class}{target} in {place} (pid {pid})")
}

impl Snapshot {
    pub fn take(
        cfg: &Config,
        state: &StateDir,
        lock: &GlobalLock,
        src: &dyn ProcSource,
    ) -> Result<Snapshot> {
        let entries = state.live(lock)?;
        let table = ProcTable::new(src.processes());
        let registered: Vec<u32> = entries.iter().map(|e| e.pid).collect();
        let observed = observe(cfg, &table, &registered);
        Ok(Snapshot {
            entries,
            observed,
            table,
            mem_available: src.mem_available(),
        })
    }

    /// Resident set of what an entry started. A run that is admitted but has
    /// not spawned yet is zero, and so counts with its whole estimate.
    pub fn entry_rss(&self, e: &Entry) -> u64 {
        e.child_pid.map_or(0, |pid| self.table.tree_rss(pid))
    }

    pub fn active(&self) -> Vec<Active> {
        let running = self
            .entries
            .iter()
            .filter(|e| e.state == RunState::Running)
            .map(|e| Active {
                class: e.class.clone(),
                target: e.target.clone(),
                rss_tree: self.entry_rss(e),
                label: label(&e.class, e.target.as_deref(), Some(&e.cwd), e.pid),
            });
        let observed = self.observed.iter().map(|o| Active {
            class: o.class.clone(),
            target: o.target.clone(),
            rss_tree: o.rss_tree,
            label: format!(
                "{}, not registered",
                label(&o.class, o.target.as_deref(), o.cwd.as_deref(), o.pid)
            ),
        });
        running.chain(observed).collect()
    }

    pub fn queued(&self, except_seq: u64) -> Vec<Queued> {
        self.entries
            .iter()
            .filter(|e| e.state == RunState::Waiting && e.seq != except_seq)
            .map(|e| Queued {
                seq: e.seq,
                class: e.class.clone(),
                target: e.target.clone(),
                label: label(&e.class, e.target.as_deref(), Some(&e.cwd), e.pid),
            })
            .collect()
    }

    /// Who is under way in this class (and on this target)? A waiting run is
    /// not: it has not touched anything yet.
    pub fn busy(&self, class: &str, target: Option<&str>) -> Vec<String> {
        self.active()
            .into_iter()
            .filter(|a| a.class == class)
            .filter(|a| target.is_none() || targets_conflict(target, a.target.as_deref()))
            .map(|a| a.label)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        let wt = Path::new("/home/x/homeserver/.claude/worktrees/audit-b12/hosts");
        assert_eq!(worktree_name(wt), "audit-b12");
        assert_eq!(worktree_name(Path::new("/home/x/homeserver")), "homeserver");
        assert_eq!(
            label("deploy", Some("vps"), Some(Path::new("/a/b")), 7),
            "deploy[vps] in b (pid 7)"
        );
    }
}
