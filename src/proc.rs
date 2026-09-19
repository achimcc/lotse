//! The process table: who runs, who descends from whom, who is a run nobody
//! registered.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use crate::config::Config;

#[derive(Clone, Debug, Default)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    /// Resident set, bytes.
    pub rss: u64,
    /// Seconds since the epoch.
    pub start: u64,
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
}

pub trait ProcSource {
    fn processes(&self) -> Vec<Proc>;
    /// `MemAvailable`, bytes.
    fn mem_available(&self) -> u64;
}

/// `/proc`, restricted to the processes of the calling user: other users'
/// runs are neither ours to queue behind nor readable in full.
pub struct RealProc;

// USER_HZ. The kernel ABI has reported 100 on every Linux architecture for
// two decades, whatever CONFIG_HZ says.
const TICKS_PER_SECOND: u64 = 100;

fn boot_time() -> u64 {
    fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("btime "))
                .and_then(|v| v.trim().parse().ok())
        })
        .unwrap_or(0)
}

fn read_proc(pid: u32, uid: u32, boot: u64, page: u64) -> Option<Proc> {
    let dir = format!("/proc/{pid}");
    if fs::metadata(&dir).ok()?.uid() != uid {
        return None;
    }
    let stat = fs::read_to_string(format!("{dir}/stat")).ok()?;
    // The name in parentheses may itself hold spaces and parentheses; what
    // follows the LAST ')' is the part with a fixed layout.
    let rest = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // After the name: state(0) ppid(1) … starttime(19).
    let ppid = fields.get(1)?.parse().ok()?;
    let ticks: u64 = fields.get(19)?.parse().ok()?;
    let statm = fs::read_to_string(format!("{dir}/statm")).ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    let cmdline = fs::read(format!("{dir}/cmdline")).ok()?;
    let argv = cmdline
        .split(|b| *b == 0)
        .filter(|a| !a.is_empty())
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    Some(Proc {
        pid,
        ppid,
        rss: pages * page,
        start: boot + ticks / TICKS_PER_SECOND,
        argv,
        cwd: fs::read_link(format!("{dir}/cwd")).ok(),
    })
}

impl ProcSource for RealProc {
    fn processes(&self) -> Vec<Proc> {
        let uid = rustix::process::getuid().as_raw();
        let boot = boot_time();
        let page = rustix::param::page_size() as u64;
        let Ok(dir) = fs::read_dir("/proc") else {
            return Vec::new();
        };
        dir.flatten()
            .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
            // A process may vanish between the listing and the read.
            .filter_map(|pid| read_proc(pid, uid, boot, page))
            .collect()
    }

    fn mem_available(&self) -> u64 {
        fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("MemAvailable:"))
                    .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
            })
            .map_or(0, |kb| kb * 1024)
    }
}

pub struct ProcTable {
    by_pid: BTreeMap<u32, Proc>,
    children: BTreeMap<u32, Vec<u32>>,
}

impl ProcTable {
    pub fn new(procs: Vec<Proc>) -> Self {
        let mut children: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for p in &procs {
            children.entry(p.ppid).or_default().push(p.pid);
        }
        ProcTable {
            by_pid: procs.into_iter().map(|p| (p.pid, p)).collect(),
            children,
        }
    }

    pub fn get(&self, pid: u32) -> Option<&Proc> {
        self.by_pid.get(&pid)
    }

    /// `pid` and everything below it.
    pub fn subtree(&self, pid: u32) -> Vec<u32> {
        let mut seen = BTreeSet::new();
        let mut stack = vec![pid];
        while let Some(p) = stack.pop() {
            if seen.insert(p) {
                stack.extend(self.children.get(&p).into_iter().flatten().copied());
            }
        }
        seen.into_iter().collect()
    }

    pub fn tree_rss(&self, pid: u32) -> u64 {
        self.subtree(pid)
            .iter()
            .filter_map(|p| self.by_pid.get(p))
            .map(|p| p.rss)
            .sum()
    }

    fn ancestors(&self, pid: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut cur = pid;
        while let Some(p) = self.by_pid.get(&cur) {
            if p.ppid == 0 || out.contains(&p.ppid) {
                break;
            }
            out.push(p.ppid);
            cur = p.ppid;
        }
        out
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Observed {
    pub pid: u32,
    pub class: String,
    pub target: Option<String>,
    pub cwd: Option<PathBuf>,
    pub start: u64,
    pub rss_tree: u64,
    pub command: String,
}

/// The value after `--on`, the way colmena names its node.
pub fn target_from_argv(argv: &[String]) -> Option<String> {
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        if a == "--on" {
            return it.next().cloned();
        }
        if let Some(v) = a.strip_prefix("--on=") {
            return Some(v.to_string());
        }
    }
    None
}

fn program(p: &Proc) -> &str {
    p.argv
        .first()
        .and_then(|a| a.rsplit('/').next())
        .unwrap_or("")
}

fn is_lotse(p: &Proc) -> bool {
    program(p) == "lotse"
}

/// A shell is never the run itself. Its command line is a script that may
/// merely MENTION a build: a loop that waits for one, a `pgrep` for one.
/// Such a shell once sat here for 26 hours, waiting for a pattern that
/// matched its own command line. The program that does the work is matched
/// on its own.
fn is_shell(p: &Proc) -> bool {
    matches!(
        program(p),
        "sh" | "bash" | "dash" | "zsh" | "ksh" | "fish" | "nu"
    )
}

/// Runs that match a class but that no lotse accounts for.
///
/// `registered`: the PIDs of all live lotse processes, running and waiting.
pub fn observe(cfg: &Config, table: &ProcTable, registered: &[u32]) -> Vec<Observed> {
    let registered: BTreeSet<u32> = registered.iter().copied().collect();
    let mut hits: Vec<(u32, &str)> = Vec::new();
    for p in table.by_pid.values() {
        if is_lotse(p) || is_shell(p) {
            continue;
        }
        let line = p.argv.join(" ");
        for (name, class) in &cfg.classes {
            if class.observe.iter().any(|re| re.is_match(&line))
                && !class.ignore.iter().any(|re| re.is_match(&line))
            {
                hits.push((p.pid, name));
            }
        }
    }
    let hit_set: BTreeSet<(u32, &str)> = hits.iter().copied().collect();
    let mut out = Vec::new();
    for (pid, class) in hits {
        let ancestors = table.ancestors(pid);
        // Below a registered lotse: already counted as that entry.
        if ancestors.iter().any(|a| registered.contains(a)) {
            continue;
        }
        // Only the root of a matching chain counts, `sh -c "nix build …"`
        // and the nix below it are one run.
        if ancestors.iter().any(|a| hit_set.contains(&(*a, class))) {
            continue;
        }
        // A registered lotse BELOW the match: the wrapper shell whose
        // command line repeats what lotse was asked to run.
        if table.subtree(pid).iter().any(|d| registered.contains(d)) {
            continue;
        }
        let p = &table.by_pid[&pid];
        out.push(Observed {
            pid,
            class: class.to_string(),
            target: target_from_argv(&p.argv),
            cwd: p.cwd.clone(),
            start: p.start,
            rss_tree: table.tree_rss(pid),
            command: p.argv.join(" "),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EXAMPLE;

    fn p(pid: u32, ppid: u32, rss: u64, argv: &[&str]) -> Proc {
        Proc {
            pid,
            ppid,
            rss,
            argv: argv.iter().map(|s| s.to_string()).collect(),
            ..Proc::default()
        }
    }

    const NIX: &[&str] = &["nix", "build", ".#nixosConfigurations.server.x"];

    fn cfg() -> Config {
        Config::parse(EXAMPLE).unwrap()
    }

    #[test]
    fn a_bare_run_is_observed() {
        let t = ProcTable::new(vec![p(1, 0, 1, &["systemd"]), p(10, 1, 5, NIX)]);
        let o = observe(&cfg(), &t, &[]);
        assert_eq!(o.len(), 1);
        assert_eq!((o[0].pid, o[0].class.as_str()), (10, "eval"));
    }

    #[test]
    fn lotse_itself_is_never_a_run() {
        let mut argv = vec!["/nix/store/x/bin/lotse", "run", "--class", "eval", "--"];
        argv.extend(NIX);
        let t = ProcTable::new(vec![p(10, 1, 5, &argv)]);
        assert!(observe(&cfg(), &t, &[]).is_empty());
    }

    #[test]
    fn below_a_registered_lotse_is_already_counted() {
        let t = ProcTable::new(vec![p(10, 1, 1, &["lotse", "run"]), p(11, 10, 5, NIX)]);
        assert!(observe(&cfg(), &t, &[10]).is_empty());
    }

    #[test]
    fn the_wrapper_shell_above_a_registered_lotse_is_not_a_second_run() {
        let shell = [
            "bash",
            "-c",
            "lotse run --class eval -- nix build .#nixosConfigurations.server.x",
        ];
        let t = ProcTable::new(vec![
            p(9, 1, 1, &shell),
            p(10, 9, 1, &["lotse", "run"]),
            p(11, 10, 5, NIX),
        ]);
        assert!(observe(&cfg(), &t, &[10]).is_empty());
    }

    #[test]
    fn only_the_root_of_a_chain_counts_and_carries_the_tree() {
        let wrapper = [
            "timeout",
            "600",
            "nix",
            "build",
            ".#nixosConfigurations.server.x",
        ];
        let t = ProcTable::new(vec![p(9, 1, 2, &wrapper), p(10, 9, 5, NIX)]);
        let o = observe(&cfg(), &t, &[]);
        assert_eq!(o.len(), 1);
        assert_eq!((o[0].pid, o[0].rss_tree), (9, 7));
    }

    #[test]
    fn a_shell_that_mentions_a_build_is_not_one() {
        let waiting = [
            "/nix/store/x-bash-5.3/bin/bash",
            "-c",
            "while pgrep -f 'nix eval .#nixosConfigurations.server'; do sleep 10; done",
        ];
        let t = ProcTable::new(vec![p(9, 1, 2, &waiting), p(10, 9, 1, &["sleep", "10"])]);
        assert!(observe(&cfg(), &t, &[]).is_empty());
        // The build below a shell still counts, as itself.
        let t = ProcTable::new(vec![p(9, 1, 2, &waiting), p(10, 9, 5, NIX)]);
        let o = observe(&cfg(), &t, &[]);
        assert_eq!((o.len(), o[0].pid, o[0].rss_tree), (1, 10, 5));
    }

    #[test]
    fn ignore_takes_a_command_line_out_of_a_class() {
        let cfg = Config::parse(
            r#"
            [class.deploy]
            observe = ['\bcolmena apply\b']
            ignore = ['\bcolmena apply\b.* (build|dry-activate)\b']
            "#,
        )
        .unwrap();
        let t = ProcTable::new(vec![
            p(
                10,
                1,
                1,
                &[
                    "colmena",
                    "apply",
                    "--on",
                    "server",
                    "build",
                    "--keep-result",
                ],
            ),
            p(11, 1, 1, &["colmena", "apply", "--on", "vps", "switch"]),
        ]);
        let o = observe(&cfg, &t, &[]);
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].target.as_deref(), Some("vps"));
    }

    #[test]
    fn a_deploy_names_its_target() {
        let t = ProcTable::new(vec![p(
            10,
            1,
            1,
            &["colmena", "apply", "--on", "vps", "switch"],
        )]);
        let o = observe(&cfg(), &t, &[]);
        assert_eq!(o[0].class, "deploy");
        assert_eq!(o[0].target.as_deref(), Some("vps"));
        assert_eq!(target_from_argv(&["colmena".into(), "apply".into()]), None);
    }

    #[test]
    fn the_real_proc_sees_this_test() {
        let me = std::process::id();
        let procs = RealProc.processes();
        let mine = procs.iter().find(|p| p.pid == me).expect("own pid");
        assert!(mine.rss > 0);
        assert!(mine.start > 1_600_000_000, "start {}", mine.start);
        assert!(!mine.argv.is_empty());
        assert!(RealProc.mem_available() > 0);
    }
}
