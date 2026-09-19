//! `lotse status`: who does what, where, since when.

use std::time::Duration;

use serde_json::{Value, json};

use crate::config::Config;
use crate::snapshot::{Snapshot, worktree_name};
use crate::state::RunState;
use crate::units::{format_duration, format_size};

struct Row {
    state: &'static str,
    class: String,
    target: Option<String>,
    worktree: String,
    pid: u32,
    age: u64,
    rss: u64,
    position: Option<usize>,
    command: String,
}

fn rows(snap: &Snapshot, now: u64) -> Vec<Row> {
    let mut out = Vec::new();
    for e in &snap.entries {
        let waiting = e.state == RunState::Waiting;
        let position = waiting.then(|| {
            1 + snap
                .entries
                .iter()
                .filter(|o| o.state == RunState::Waiting && o.class == e.class && o.seq < e.seq)
                .count()
        });
        out.push(Row {
            state: if waiting { "waiting" } else { "running" },
            class: e.class.clone(),
            target: e.target.clone(),
            worktree: worktree_name(&e.cwd),
            pid: e.pid,
            age: now.saturating_sub(e.started.unwrap_or(e.created)),
            rss: snap.entry_rss(e),
            position,
            command: e.command.join(" "),
        });
    }
    for o in &snap.observed {
        out.push(Row {
            state: "observed",
            class: o.class.clone(),
            target: o.target.clone(),
            worktree: o.cwd.as_deref().map(worktree_name).unwrap_or_default(),
            pid: o.pid,
            age: now.saturating_sub(o.start),
            rss: o.rss_tree,
            position: None,
            command: o.command.clone(),
        });
    }
    out
}

fn pending_growth(cfg: &Config, snap: &Snapshot, now: u64) -> u64 {
    snap.active(now).iter().map(|a| a.pending(cfg)).sum()
}

pub fn render_json(cfg: &Config, snap: &Snapshot, now: u64) -> Value {
    let runs: Vec<Value> = rows(snap, now)
        .into_iter()
        .map(|r| {
            json!({
                "state": r.state,
                "class": r.class,
                "target": r.target,
                "worktree": r.worktree,
                "pid": r.pid,
                "age_s": r.age,
                "rss": r.rss,
                "memory": cfg.classes.get(&r.class).map_or(0, |c| c.memory),
                "position": r.position,
                "command": r.command,
            })
        })
        .collect();
    json!({
        "mem_available": snap.mem_available,
        "reserve": cfg.reserve,
        "pending_growth": pending_growth(cfg, snap, now),
        "runs": runs,
    })
}

fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max - 1).collect();
    format!("{head}…")
}

pub fn render_text(cfg: &Config, snap: &Snapshot, now: u64) -> String {
    let rows = rows(snap, now);
    let mut out = String::new();
    if rows.is_empty() {
        out.push_str("nothing runs, nothing waits\n");
    } else {
        out.push_str(&format!(
            "{:<9} {:<12} {:<8} {:<22} {:>7} {:>7} {:>13}  {}\n",
            "STATE", "CLASS", "TARGET", "WORKTREE", "PID", "AGE", "RSS/ESTIMATE", "COMMAND"
        ));
        for r in &rows {
            let state = match r.position {
                Some(p) => format!("wait #{p}"),
                None => r.state.to_string(),
            };
            let memory = cfg.classes.get(&r.class).map_or(0, |c| c.memory);
            let rss = if memory > 0 {
                format!("{}/{}", format_size(r.rss), format_size(memory))
            } else {
                format_size(r.rss)
            };
            out.push_str(&format!(
                "{:<9} {:<12} {:<8} {:<22} {:>7} {:>7} {:>13}  {}\n",
                state,
                shorten(&r.class, 12),
                shorten(r.target.as_deref().unwrap_or("-"), 8),
                shorten(&r.worktree, 22),
                r.pid,
                format_duration(Duration::from_secs(r.age)),
                rss,
                shorten(&r.command, 70),
            ));
        }
    }
    out.push_str(&format!(
        "memory: {} available, {} reserve, {} still to be claimed by running jobs\n",
        format_size(snap.mem_available),
        format_size(cfg.reserve),
        format_size(pending_growth(cfg, snap, now)),
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::{Observed, ProcTable};
    use crate::state::Entry;

    #[test]
    fn json_carries_positions_and_observed_runs() {
        let cfg = Config::parse(crate::config::EXAMPLE).unwrap();
        let entry = |seq: u64, state: RunState| Entry {
            id: format!("{seq}"),
            seq,
            pid: seq as u32,
            child_pid: None,
            class: "eval".into(),
            target: None,
            cwd: "/x/.claude/worktrees/wt".into(),
            command: vec!["nix".into()],
            state,
            created: 99,
            started: None,
            log: None,
        };
        let snap = Snapshot {
            entries: vec![
                entry(1, RunState::Running),
                entry(2, RunState::Waiting),
                entry(3, RunState::Waiting),
            ],
            observed: vec![Observed {
                pid: 50,
                class: "deploy".into(),
                target: Some("vps".into()),
                cwd: None,
                start: 40,
                rss_tree: 5,
                command: "colmena apply --on vps".into(),
            }],
            table: ProcTable::new(Vec::new()),
            mem_available: 1 << 34,
        };
        let v = render_json(&cfg, &snap, 100);
        let runs = v["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 4);
        assert_eq!(runs[0]["position"], Value::Null);
        assert_eq!(runs[1]["position"], 1);
        assert_eq!(runs[2]["position"], 2);
        assert_eq!(runs[2]["worktree"], "wt");
        assert_eq!(runs[3]["state"], "observed");
        assert_eq!(runs[3]["age_s"], 60);
        // One running eval that has not grown yet: its whole estimate is pending.
        assert_eq!(v["pending_growth"], 10u64 << 30);
        assert!(render_text(&cfg, &snap, 100).contains("wait #2"));
    }
}
