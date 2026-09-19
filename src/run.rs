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

/// Finds the retry patterns in a stream that arrives in arbitrary pieces.
struct LineScan {
    patterns: Vec<Regex>,
    line: Vec<u8>,
    hit: Arc<AtomicBool>,
}

impl LineScan {
    fn check(&mut self) {
        if !self.line.is_empty() {
            let text = String::from_utf8_lossy(&self.line);
            if self.patterns.iter().any(|re| re.is_match(&text)) {
                self.hit.store(true, Ordering::SeqCst);
            }
            self.line.clear();
        }
    }

    fn feed(&mut self, chunk: &[u8]) {
        if self.patterns.is_empty() {
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

fn open_log(entry: &OwnedEntry) -> (Log, Option<PathBuf>) {
    let opened = logs_dir().and_then(|dir| {
        fs::create_dir_all(&dir).ok()?;
        let path = dir.join(format!("{}-{}.log", utc_stamp(now()), entry.entry.id));
        let mut f = File::create(&path).ok()?;
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
    let code = loop {
        attempts += 1;
        let hit = Arc::new(AtomicBool::new(false));
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
                break 127;
            }
        };
        entry.update(|e| e.child_pid = Some(child.id()))?;

        let (done_tx, done_rx) = mpsc::channel();
        let scan = |hit: &Arc<AtomicBool>| LineScan {
            patterns: patterns.clone(),
            line: Vec::new(),
            hit: Arc::clone(hit),
        };
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        copy_stream(stdout, false, Arc::clone(&log), scan(&hit), done_tx.clone());
        copy_stream(stderr, true, Arc::clone(&log), scan(&hit), done_tx);

        let attempt = supervise(&mut child, own_group, &signals)?;
        let deadline = Instant::now() + DRAIN;
        for _ in 0..2 {
            let left = deadline.saturating_duration_since(Instant::now());
            if done_rx.recv_timeout(left).is_err() {
                break;
            }
        }

        let network = attempt.code != 0 && !attempt.interrupted && hit.load(Ordering::SeqCst);
        if !network {
            break attempt.code;
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
            break EXIT_NETWORK;
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
            break 128 + sig;
        }
    };

    log_line(
        &log,
        &format!(
            "lotse: exit={code} attempts={attempts} waited={}s ran={}s log={}",
            waited.as_secs(),
            started_at.elapsed().as_secs(),
            log_path
                .as_deref()
                .map_or("-".to_string(), |p| p.display().to_string())
        ),
    );
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(patterns: &[&str]) -> (LineScan, Arc<AtomicBool>) {
        let hit = Arc::new(AtomicBool::new(false));
        let scan = LineScan {
            patterns: patterns.iter().map(|p| Regex::new(p).unwrap()).collect(),
            line: Vec::new(),
            hit: Arc::clone(&hit),
        };
        (scan, hit)
    }

    #[test]
    fn a_pattern_split_across_chunks_is_found() {
        let (mut s, hit) = scan(&["Could not resolve host"]);
        s.feed(b"error: Could not res");
        s.feed(b"olve host: cache.nixos.org\n");
        assert!(hit.load(Ordering::SeqCst));
    }

    #[test]
    fn a_last_line_without_newline_is_found_at_the_end() {
        let (mut s, hit) = scan(&["daemon disconnected"]);
        s.feed(b"Nix daemon disconnected");
        assert!(!hit.load(Ordering::SeqCst));
        s.check();
        assert!(hit.load(Ordering::SeqCst));
    }

    #[test]
    fn other_output_is_no_hit() {
        let (mut s, hit) = scan(&["Could not resolve host"]);
        s.feed(b"error: assertion failed\nresolve\n");
        s.check();
        assert!(!hit.load(Ordering::SeqCst));
    }
}
