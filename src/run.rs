//! `lotse run`: queue, start, copy the output, pass signals on, retry what
//! failed for the network's sake.

use std::fs::{self, File};
use std::io::{IsTerminal, Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use regex::Regex;
use rustix::process::{Pid, Signal, kill_process, kill_process_group};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};

use crate::admit::{Candidate, Decision, decide};
use crate::config::Config;
use crate::proc::ProcSource;
use crate::snapshot::Snapshot;
use crate::state::{OwnedEntry, RunState, StateDir, now};
use crate::units::{format_duration, utc_stamp};
use crate::wait::{POLL, REPORT_EVERY};
use crate::{EXIT_NETWORK, EXIT_WAIT_LIMIT};

pub struct RunArgs {
    pub class: String,
    pub target: Option<String>,
    pub max_wait: Option<Duration>,
    pub no_retry: bool,
    pub command: Vec<String>,
}

/// A second signal this long after the first one is no longer passed on
/// politely.
const PATIENCE: Duration = Duration::from_secs(10);
/// How long to wait for the output after the child is gone. A grandchild
/// that keeps the pipe (an ssh control master, say) must not hold us.
const DRAIN: Duration = Duration::from_secs(2);
const LINE_LIMIT: usize = 8 * 1024;

struct Signals {
    flags: [(i32, Arc<AtomicBool>); 3],
}

impl Signals {
    fn install() -> Result<Signals> {
        let make = |sig: i32| -> Result<(i32, Arc<AtomicBool>)> {
            let flag = Arc::new(AtomicBool::new(false));
            signal_hook::flag::register(sig, Arc::clone(&flag))?;
            Ok((sig, flag))
        };
        Ok(Signals {
            flags: [make(SIGINT)?, make(SIGTERM)?, make(SIGHUP)?],
        })
    }

    /// The pending signal, cleared.
    fn take(&self) -> Option<i32> {
        self.flags
            .iter()
            .find(|(_, f)| f.swap(false, Ordering::SeqCst))
            .map(|(s, _)| *s)
    }
}

/// Where the output of one attempt stands with respect to the network.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Tail {
    /// No retry pattern yet, or none since the last error of another kind.
    Clean,
    /// A retry pattern, and after it no error of another kind.
    Network,
}

/// Errors nix prints BECAUSE something could not be fetched: the
/// derivations and paths that failed for it. After a network pattern they
/// are its consequences, not an error of their own.
const CONSEQUENCES: &[&str] = &[
    r"^error: Cannot build '",
    r"^error: builder for '",
    r"^error: \d+ dependencies of derivation '",
    r"^error: some substitutes for the outputs of derivation '",
    r"^error: some references of path '",
    r"^error: some outputs of '",
    r"^error: path '[^']*' is required, but there is no substituter",
];

/// An error line of its own: `error:`, `fatal:` and the like at the start of
/// a line. Indented lines are the output of a nix builder, quoted under the
/// error that names it.
const ERROR_LINE: &str = r"(?i)^(error|fatal)\b";

/// Decides, line by line, whether an attempt died of the network.
///
/// The rule: the LAST error of the output must be a network pattern. A line
/// that matches one of the class's `retry.patterns` sets the verdict to
/// network; a later error line of another kind sets it back — unless it is
/// one of nix's consequences of a failed download. A pattern anywhere in the
/// output is not enough: nix prints download warnings long before the
/// assertion that is the real verdict (audit 3, CD-3).
struct Judge {
    patterns: Vec<Regex>,
    /// Colour codes, taken off before a line is judged.
    ansi: Regex,
    error: Regex,
    consequences: Vec<Regex>,
    tail: Tail,
}

impl Judge {
    fn new(patterns: Vec<Regex>) -> Judge {
        Judge {
            patterns,
            ansi: Regex::new(r"\x1b\[[0-9;]*[A-Za-z]").expect("a valid pattern"),
            error: Regex::new(ERROR_LINE).expect("a valid pattern"),
            consequences: CONSEQUENCES
                .iter()
                .map(|p| Regex::new(p).expect("a valid pattern"))
                .collect(),
            tail: Tail::Clean,
        }
    }

    fn line(&mut self, text: &str) {
        let text = &*self.ansi.replace_all(text, "");
        if self.patterns.iter().any(|re| re.is_match(text)) {
            self.tail = Tail::Network;
        } else if self.tail == Tail::Network
            && self.error.is_match(text)
            && !self.consequences.iter().any(|re| re.is_match(text))
        {
            self.tail = Tail::Clean;
        }
    }
}

type SharedJudge = Arc<Mutex<Judge>>;

/// Cuts a stream that arrives in arbitrary pieces into lines for the judge.
/// One per stream; the judge is shared, so it sees the lines of stdout and
/// stderr in the order they arrived.
struct LineScan {
    judge: SharedJudge,
    line: Vec<u8>,
}

impl LineScan {
    fn check(&mut self) {
        if !self.line.is_empty() {
            let text = String::from_utf8_lossy(&self.line);
            self.judge.lock().unwrap().line(&text);
            self.line.clear();
        }
    }

    fn feed(&mut self, chunk: &[u8]) {
        if self.judge.lock().unwrap().patterns.is_empty() {
            return;
        }
        for &b in chunk {
            // Progress bars end their lines with a carriage return.
            if b == b'\n' || b == b'\r' || self.line.len() >= LINE_LIMIT {
                self.check();
            }
            if b != b'\n' && b != b'\r' {
                self.line.push(b);
            }
        }
    }
}

type Log = Arc<Mutex<Option<File>>>;

fn log_line(log: &Log, text: &str) {
    eprintln!("{text}");
    if let Some(f) = log.lock().unwrap().as_mut() {
        let _ = writeln!(f, "{text}");
    }
}

fn copy_stream(
    mut from: impl Read + Send + 'static,
    to_stderr: bool,
    log: Log,
    mut scan: LineScan,
    done: mpsc::Sender<()>,
) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            let n = match from.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let chunk = &buf[..n];
            if to_stderr {
                let mut e = std::io::stderr().lock();
                let _ = e.write_all(chunk);
            } else {
                let mut o = std::io::stdout().lock();
                let _ = o.write_all(chunk).and_then(|_| o.flush());
            }
            if let Some(f) = log.lock().unwrap().as_mut() {
                let _ = f.write_all(chunk);
            }
            scan.feed(chunk);
        }
        scan.check();
        let _ = done.send(());
    });
}

fn logs_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("lotse").join("logs"))
}

/// Logs older than this are removed when the next run opens its own. They
/// hold the whole output of every run, and nobody reads a month-old one.
pub const KEEP_LOGS: Duration = Duration::from_secs(30 * 24 * 3600);

/// Our logs only: `<stamp>-<id>.log`, regular files, ours.
fn prune_logs(dir: &std::path::Path) {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let me = rustix::process::geteuid().as_raw();
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "log") {
            continue;
        }
        // Not followed: a symlink here is nothing of ours.
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_file() || meta.uid() != me {
            continue;
        }
        let old = meta
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age > KEEP_LOGS);
        if old {
            let _ = fs::remove_file(&path);
        } else if meta.mode() & 0o077 != 0 {
            // Written by a lotse before 0.3.0, readable by everyone.
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
        }
    }
}

fn open_log(entry: &OwnedEntry) -> (Log, Option<PathBuf>) {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    let opened = logs_dir().and_then(|dir| {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .ok()?;
        // A directory from before 0.3.0 was created with the umask.
        let meta = fs::symlink_metadata(&dir).ok()?;
        if meta.is_dir()
            && meta.uid() == rustix::process::geteuid().as_raw()
            && meta.mode() & 0o077 != 0
        {
            let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
        }
        prune_logs(&dir);
        let path = dir.join(format!("{}-{}.log", utc_stamp(now()), entry.entry.id));
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .ok()?;
        let e = &entry.entry;
        let _ = writeln!(
            f,
            "lotse: class={} target={} cwd={} command={:?}",
            e.class,
            e.target.as_deref().unwrap_or("-"),
            e.cwd.display(),
            e.command
        );
        Some((f, path))
    });
    match opened {
        Some((f, path)) => (Arc::new(Mutex::new(Some(f))), Some(path)),
        None => {
            eprintln!("lotse: cannot write a log, running without one");
            (Arc::new(Mutex::new(None)), None)
        }
    }
}

enum Queue {
    Admitted,
    GaveUp,
    Signalled(i32),
}

fn queue_up(
    cfg: &Config,
    state: &StateDir,
    src: &dyn ProcSource,
    entry: &mut OwnedEntry,
    max_wait: Duration,
    signals: &Signals,
) -> Result<Queue> {
    let begun = Instant::now();
    let mut reported: Option<Instant> = None;
    loop {
        if let Some(sig) = signals.take() {
            return Ok(Queue::Signalled(sig));
        }
        let reason = {
            let lock = state.lock()?;
            let snap = Snapshot::take(cfg, state, &lock, src)?;
            let cand = Candidate {
                seq: entry.entry.seq,
                class: &entry.entry.class,
                target: entry.entry.target.as_deref(),
            };
            let decision = decide(
                cfg,
                &cand,
                &snap.active(now()),
                &snap.queued(entry.entry.seq),
                snap.mem_available,
            );
            match decision {
                Decision::Admit { starved } => {
                    if starved {
                        eprintln!(
                            "lotse: WARNING: not enough memory for the estimate of class {}, \
                             but nothing runs that would free any. Starting anyway.",
                            entry.entry.class
                        );
                    }
                    // Still under the lock: the next one to decide sees us running.
                    entry.update(|e| {
                        e.state = RunState::Running;
                        e.started = Some(now());
                    })?;
                    return Ok(Queue::Admitted);
                }
                Decision::Wait(reason) => reason,
            }
        };
        if begun.elapsed() >= max_wait {
            eprintln!(
                "lotse: gave up after {}: {reason}",
                format_duration(begun.elapsed())
            );
            return Ok(Queue::GaveUp);
        }
        if reported.is_none_or(|t| t.elapsed() >= REPORT_EVERY) {
            eprintln!("lotse: waiting: {reason}");
            reported = Some(Instant::now());
        }
        sleep(POLL);
    }
}

struct Attempt {
    /// Exit code, 128 + signal if a signal ended it.
    code: i32,
    /// We passed a signal on: whatever happened, it was asked for.
    interrupted: bool,
}

fn supervise(child: &mut Child, own_group: bool, signals: &Signals) -> Result<Attempt> {
    let pid = Pid::from_child(child);
    let mut first_signal: Option<Instant> = None;
    loop {
        if let Some(status) = child.try_wait()? {
            let code = status
                .code()
                .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
            return Ok(Attempt {
                code,
                interrupted: first_signal.is_some(),
            });
        }
        if let Some(sig) = signals.take() {
            let impatient = first_signal.is_some_and(|t| t.elapsed() >= PATIENCE);
            let signal = if impatient {
                Signal::KILL
            } else {
                Signal::from_named_raw(sig).unwrap_or(Signal::TERM)
            };
            first_signal.get_or_insert_with(Instant::now);
            if own_group {
                let _ = kill_process_group(pid, signal);
            } else if sig != SIGINT || impatient {
                // In the terminal's foreground group the child got its SIGINT
                // from the terminal already.
                let _ = kill_process(pid, signal);
            }
        }
        sleep(Duration::from_millis(100));
    }
}

/// Set for everything a `lotse run` starts.
pub const NESTED: &str = "LOTSE_RUN";

/// A `lotse run` below a `lotse run`: a recipe that queues its build, called
/// from a command that was queued already. The inner one would wait for the
/// memory the outer one has claimed for exactly this work, until the wait
/// limit. It is part of the outer run, so it just runs.
fn run_nested(args: &RunArgs) -> Result<i32> {
    let status = match Command::new(&args.command[0])
        .args(&args.command[1..])
        .status()
    {
        Ok(status) => status,
        Err(e) => {
            eprintln!("lotse: cannot start {:?}: {e}", args.command[0]);
            return Ok(127);
        }
    };
    Ok(status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)))
}

pub fn run(cfg: &Config, state: &StateDir, src: &dyn ProcSource, args: RunArgs) -> Result<i32> {
    if std::env::var_os(NESTED).is_some() {
        return run_nested(&args);
    }
    let class = &cfg.classes[&args.class];
    let max_wait = args.max_wait.or(class.max_wait).unwrap_or(cfg.max_wait);
    let cwd = std::env::current_dir().context("no current directory")?;
    // Before anything that waits: a signal must find a handler, or the entry
    // outlives us until someone stumbles over it.
    let signals = Signals::install()?;

    let mut entry = {
        let lock = state.lock()?;
        state.create(
            &lock,
            &args.class,
            args.target.as_deref(),
            &cwd,
            &args.command,
        )?
    };
    let queued_at = Instant::now();
    match queue_up(cfg, state, src, &mut entry, max_wait, &signals)? {
        Queue::Admitted => {}
        Queue::GaveUp => return Ok(EXIT_WAIT_LIMIT),
        Queue::Signalled(sig) => return Ok(128 + sig),
    }
    let waited = queued_at.elapsed();

    let (log, log_path) = open_log(&entry);
    entry.update(|e| e.log = log_path.clone())?;

    let retry = class.retry.as_ref().filter(|_| !args.no_retry);
    let max_attempts = 1 + retry.map_or(0, |r| r.times);
    let patterns: Vec<Regex> = retry.map(|r| r.patterns.clone()).unwrap_or_default();
    // Its own process group, so that a signal reaches what the command
    // started as well (bash does not pass SIGTERM on). With a terminal on
    // stdin it stays in ours: a background group reading the terminal would
    // be stopped with SIGTTIN.
    let own_group = !std::io::stdin().is_terminal();

    let started_at = Instant::now();
    let mut attempts = 0;
    // The command's own code, whatever lotse exits with.
    let mut code;
    // Whose word the code is: the command's, the network's (every attempt
    // died of it), or a signal's during the pause between attempts.
    let verdict;
    loop {
        attempts += 1;
        let judge: SharedJudge = Arc::new(Mutex::new(Judge::new(patterns.clone())));
        let mut cmd = Command::new(&args.command[0]);
        cmd.args(&args.command[1..])
            .env(NESTED, &entry.entry.id)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if own_group {
            cmd.process_group(0);
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                log_line(
                    &log,
                    &format!("lotse: cannot start {:?}: {e}", args.command[0]),
                );
                code = 127;
                verdict = "command";
                break;
            }
        };
        entry.update(|e| e.child_pid = Some(child.id()))?;

        let (done_tx, done_rx) = mpsc::channel();
        let scan = |judge: &SharedJudge| LineScan {
            judge: Arc::clone(judge),
            line: Vec::new(),
        };
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        copy_stream(
            stdout,
            false,
            Arc::clone(&log),
            scan(&judge),
            done_tx.clone(),
        );
        copy_stream(stderr, true, Arc::clone(&log), scan(&judge), done_tx);

        let attempt = supervise(&mut child, own_group, &signals)?;
        let deadline = Instant::now() + DRAIN;
        for _ in 0..2 {
            let left = deadline.saturating_duration_since(Instant::now());
            if done_rx.recv_timeout(left).is_err() {
                break;
            }
        }

        let network = attempt.code != 0
            && !attempt.interrupted
            && judge.lock().unwrap().tail == Tail::Network;
        code = attempt.code;
        if !network {
            verdict = "command";
            break;
        }
        if attempts >= max_attempts {
            log_line(
                &log,
                &format!(
                    "lotse: network failure, not a verdict of the command itself \
                     (its exit code was {})",
                    attempt.code
                ),
            );
            verdict = "network";
            break;
        }
        let pause = retry.map_or(Duration::ZERO, |r| r.pause);
        log_line(
            &log,
            &format!(
                "lotse: network failure, retrying in {} (attempt {} of {max_attempts})",
                format_duration(pause),
                attempts + 1
            ),
        );
        let resume = Instant::now() + pause;
        let mut interrupted = None;
        while Instant::now() < resume {
            if let Some(sig) = signals.take() {
                interrupted = Some(sig);
                break;
            }
            sleep(Duration::from_millis(100));
        }
        if let Some(sig) = interrupted {
            code = 128 + sig;
            verdict = "interrupted";
            break;
        }
    }

    log_line(
        &log,
        &format!(
            "lotse: exit={code} verdict={verdict} attempts={attempts} waited={}s ran={}s log={}",
            waited.as_secs(),
            started_at.elapsed().as_secs(),
            log_path
                .as_deref()
                .map_or("-".to_string(), |p| p.display().to_string())
        ),
    );
    // 201 as the process's exit code: a caller that reads only the code
    // still tells "no verdict" apart. The line above names the real one.
    Ok(if verdict == "network" {
        EXIT_NETWORK
    } else {
        code
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(patterns: &[&str]) -> (LineScan, SharedJudge) {
        let judge = Arc::new(Mutex::new(Judge::new(
            patterns.iter().map(|p| Regex::new(p).unwrap()).collect(),
        )));
        let scan = LineScan {
            judge: Arc::clone(&judge),
            line: Vec::new(),
        };
        (scan, judge)
    }

    fn network(judge: &SharedJudge) -> bool {
        judge.lock().unwrap().tail == Tail::Network
    }

    #[test]
    fn a_pattern_split_across_chunks_is_found() {
        let (mut s, judge) = scan(&["Could not resolve host"]);
        s.feed(b"error: Could not res");
        s.feed(b"olve host: cache.nixos.org\n");
        assert!(network(&judge));
    }

    #[test]
    fn a_last_line_without_newline_is_found_at_the_end() {
        let (mut s, judge) = scan(&["daemon disconnected"]);
        s.feed(b"Nix daemon disconnected");
        assert!(!network(&judge));
        s.check();
        assert!(network(&judge));
    }

    #[test]
    fn other_output_is_no_hit() {
        let (mut s, judge) = scan(&["Could not resolve host"]);
        s.feed(b"error: assertion failed\nresolve\n");
        s.check();
        assert!(!network(&judge));
    }

    #[test]
    fn an_error_after_the_pattern_is_the_verdict() {
        let (mut s, judge) = scan(&["Could not resolve host:"]);
        s.feed(b"warning: Could not resolve host: x\nerror: assertion failed\n");
        assert!(!network(&judge));
        // A later network error is the last word again.
        s.feed(b"error: unable to fetch: Could not resolve host: y\n");
        assert!(network(&judge));
        // Output that is no error does not change it.
        s.feed(b"some progress\n       > error: from a builder\n");
        assert!(network(&judge));
    }

    #[test]
    fn nix_s_consequences_keep_the_network_verdict() {
        let (mut s, judge) = scan(&["unable to download"]);
        s.feed(b"warning: unable to download 'https://c/x.narinfo': timeout\n");
        s.feed(b"error: path '/nix/store/abc-x' is required, but there is no substituter that can build it\n");
        s.feed(b"error: some references of path '/nix/store/abc-x' could not be realised\n");
        s.feed(b"error: some substitutes for the outputs of derivation '/nix/store/d.drv' failed to build\n");
        s.feed(b"error: Cannot build '/nix/store/e.drv'.\n");
        s.feed(b"error: builder for '/nix/store/f.drv' failed with exit code 1\n");
        s.feed(b"error: 2 dependencies of derivation '/nix/store/g.drv' failed to build\n");
        assert!(network(&judge));
        s.feed(b"error:\n       Failed assertions:\n");
        assert!(!network(&judge));
    }

    #[test]
    fn colour_does_not_hide_an_error() {
        let (mut s, judge) = scan(&["Could not resolve host:"]);
        s.feed(b"warning: Could not resolve host: x\n\x1b[31;1merror:\x1b[0m assertion failed\n");
        assert!(!network(&judge));
    }
}
