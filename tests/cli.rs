//! The binary against real processes. Every test has its own state directory,
//! so they do not queue behind each other; what they share is the real /proc,
//! hence the odd sleep durations that only one test each looks for.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process};

const CONFIG: &str = r#"
max_wait = "60s"

[class.one]
slots = 1

[class.net]
retry = { patterns = ['Could not resolve host'], times = 2, pause = "1s" }

[class.seen]
observe = ['^sleep 3123$']

[class.awaited]
observe = ['^sleep 3456$']

[class.counted]
observe = ['^sleep 3789$']
"#;

struct Env {
    tmp: tempfile::TempDir,
}

impl Env {
    fn new() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("lotse.toml"), CONFIG).unwrap();
        fs::create_dir(tmp.path().join("run")).unwrap();
        Env { tmp }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.tmp.path().join(name)
    }

    fn lotse(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_lotse"));
        cmd.env("XDG_RUNTIME_DIR", self.path("run"))
            .env("XDG_STATE_HOME", self.path("state"))
            .current_dir(self.tmp.path())
            // Not a terminal, whoever runs the tests: the child gets its own
            // process group, as it does under a session.
            .stdin(Stdio::null())
            .arg("--config")
            .arg(self.path("lotse.toml"))
            .args(args);
        cmd
    }

    fn output(&self, args: &[&str]) -> Output {
        self.lotse(args).output().unwrap()
    }

    fn code(&self, args: &[&str]) -> i32 {
        self.output(args).status.code().expect("an exit code")
    }

    fn spawn(&self, args: &[&str]) -> Child {
        self.lotse(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    fn status(&self) -> serde_json::Value {
        let out = self.output(&["status", "--json"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn runs(&self, state: &str, class: &str) -> usize {
        self.status()["runs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["state"] == state && r["class"] == class)
            .count()
    }

    fn until(&self, what: &str, cond: impl Fn(&Env) -> bool) {
        let begun = Instant::now();
        while !cond(self) {
            assert!(
                begun.elapsed() < Duration::from_secs(15),
                "never happened: {what}"
            );
            sleep(Duration::from_millis(100));
        }
    }

    fn the_log(&self) -> String {
        let dir = self.path("state/lotse/logs");
        let mut logs: Vec<PathBuf> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        assert_eq!(logs.len(), 1, "{logs:?}");
        fs::read_to_string(logs.remove(0)).unwrap()
    }
}

fn signal(pid: u32, sig: Signal) {
    kill_process(Pid::from_raw(pid as i32).unwrap(), sig).unwrap();
}

fn pid_from(file: &Path) -> u32 {
    fs::read_to_string(file).unwrap().trim().parse().unwrap()
}

#[test]
fn the_exit_code_is_the_commands() {
    let env = Env::new();
    assert_eq!(
        env.code(&["run", "--class", "one", "--", "sh", "-c", "exit 7"]),
        7
    );
    assert_eq!(env.code(&["run", "--class", "one", "--", "true"]), 0);
    assert!(env.the_log_count() == 2);
}

impl Env {
    fn the_log_count(&self) -> usize {
        fs::read_dir(self.path("state/lotse/logs")).unwrap().count()
    }
}

#[test]
fn death_by_signal_is_128_plus_the_signal() {
    let env = Env::new();
    let code = env.code(&["run", "--class", "one", "--", "sh", "-c", "kill -TERM $$"]);
    assert_eq!(code, 143);
}

#[test]
fn output_passes_through_and_lands_in_the_log() {
    let env = Env::new();
    let out = env.output(&[
        "run",
        "--class",
        "one",
        "--",
        "sh",
        "-c",
        "echo out; echo err >&2",
    ]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "out\n");
    assert!(String::from_utf8_lossy(&out.stderr).contains("err\n"));
    let log = env.the_log();
    assert!(log.contains("out\n") && log.contains("err\n"), "{log}");
    assert!(log.contains("lotse: exit=0 attempts=1"), "{log}");
}

#[test]
fn usage_errors_are_2() {
    let env = Env::new();
    assert_eq!(env.code(&["run", "--class", "nope", "--", "true"]), 2);
    assert_eq!(env.code(&["run", "--class", "one"]), 2);
    assert_eq!(env.code(&["wait", "nope"]), 2);
    assert_eq!(env.code(&["frobnicate"]), 2);
    assert_eq!(
        env.code(&["run", "--class", "one", "--", "/no/such/binary"]),
        127
    );
}

#[test]
fn a_run_below_a_run_does_not_queue_behind_its_parent() {
    // The only slot of `one` is held by the outer run; the inner one must
    // not wait for it.
    let env = Env::new();
    let me = env!("CARGO_BIN_EXE_lotse");
    let config = env.path("lotse.toml");
    let inner = format!(
        "{me} --config {} run --class one --max-wait 2s -- sh -c 'exit 9'",
        config.display()
    );
    let begun = Instant::now();
    assert_eq!(
        env.code(&["run", "--class", "one", "--", "sh", "-c", &inner]),
        9
    );
    assert!(
        begun.elapsed() < Duration::from_secs(2),
        "{:?}",
        begun.elapsed()
    );
}

#[test]
fn the_wait_limit_is_200() {
    let env = Env::new();
    let mut holder = env.spawn(&["run", "--class", "one", "--", "sleep", "20"]);
    env.until("holder runs", |e| e.runs("running", "one") == 1);
    let out = env.output(&["run", "--class", "one", "--max-wait", "1s", "--", "true"]);
    assert_eq!(out.status.code(), Some(200));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("all slots of one taken by one in"), "{err}");
    signal(holder.id(), Signal::TERM);
    assert_eq!(holder.wait().unwrap().code(), Some(143));
}

#[test]
fn a_killed_holder_frees_its_slot() {
    let env = Env::new();
    let pidfile = env.path("child.pid");
    let script = format!("echo $$ > {}; exec sleep 60", pidfile.display());
    let mut holder = env.spawn(&["run", "--class", "one", "--", "sh", "-c", &script]);
    env.until("holder runs", |e| {
        e.runs("running", "one") == 1 && pidfile.exists()
    });
    // What the memory watchdog does: the wrapper dies, nobody cleans up.
    signal(holder.id(), Signal::KILL);
    holder.wait().unwrap();
    let begun = Instant::now();
    assert_eq!(env.code(&["run", "--class", "one", "--", "true"]), 0);
    assert!(
        begun.elapsed() < Duration::from_secs(5),
        "{:?}",
        begun.elapsed()
    );
    signal(pid_from(&pidfile), Signal::KILL);
}

#[test]
fn first_come_first_served() {
    let env = Env::new();
    let order = env.path("order");
    let say = |name: &str, nap: &str| format!("echo {name} >> {}; sleep {nap}", order.display());
    let mut a = env.spawn(&["run", "--class", "one", "--", "sh", "-c", &say("A", "2")]);
    env.until("A runs", |e| e.runs("running", "one") == 1);
    let mut b = env.spawn(&["run", "--class", "one", "--", "sh", "-c", &say("B", "0")]);
    env.until("B waits", |e| e.runs("waiting", "one") == 1);
    let mut c = env.spawn(&["run", "--class", "one", "--", "sh", "-c", &say("C", "0")]);
    for child in [&mut a, &mut b, &mut c] {
        assert!(child.wait().unwrap().success());
    }
    assert_eq!(fs::read_to_string(order).unwrap(), "A\nB\nC\n");
}

fn flaky(env: &Env, failures: u32) -> String {
    format!(
        "n=$(cat {c} 2>/dev/null || echo 0); echo $((n+1)) > {c}; \
         if [ $n -lt {failures} ]; then echo 'curl: (6) Could not resolve host: x' >&2; exit 1; fi; \
         echo fine",
        c = env.path("count").display()
    )
}

#[test]
fn a_network_failure_is_retried() {
    let env = Env::new();
    let out = env.output(&["run", "--class", "net", "--", "sh", "-c", &flaky(&env, 1)]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(fs::read_to_string(env.path("count")).unwrap().trim(), "2");
    let log = env.the_log();
    assert!(log.contains("retrying in 1s (attempt 2 of 3)"), "{log}");
    assert!(log.contains("exit=0 attempts=2"), "{log}");
}

#[test]
fn a_network_failure_that_stays_is_201() {
    let env = Env::new();
    let out = env.output(&["run", "--class", "net", "--", "sh", "-c", &flaky(&env, 99)]);
    assert_eq!(out.status.code(), Some(201));
    assert_eq!(fs::read_to_string(env.path("count")).unwrap().trim(), "3");
}

#[test]
fn an_ordinary_failure_is_not_retried() {
    let env = Env::new();
    let script = format!(
        "echo x >> {}; echo 'error: assertion failed' >&2; exit 1",
        env.path("n").display()
    );
    assert_eq!(
        env.code(&["run", "--class", "net", "--", "sh", "-c", &script]),
        1
    );
    assert_eq!(fs::read_to_string(env.path("n")).unwrap(), "x\n");
}

#[test]
fn no_retry_means_no_retry() {
    let env = Env::new();
    let code = env.code(&[
        "run",
        "--class",
        "net",
        "--no-retry",
        "--",
        "sh",
        "-c",
        &flaky(&env, 1),
    ]);
    assert_eq!(code, 1);
    assert_eq!(fs::read_to_string(env.path("count")).unwrap().trim(), "1");
}

#[test]
fn sigterm_reaches_the_command_and_its_children() {
    let env = Env::new();
    let pidfile = env.path("grandchild.pid");
    let script = format!(
        "trap 'exit 42' TERM; sleep 60 & echo $! > {}; wait",
        pidfile.display()
    );
    let mut run = env.spawn(&["run", "--class", "one", "--", "sh", "-c", &script]);
    env.until("the trap is set", |_| pidfile.exists());
    sleep(Duration::from_millis(200));
    signal(run.id(), Signal::TERM);
    assert_eq!(run.wait().unwrap().code(), Some(42));
    // The whole group got it, not only the shell.
    let grandchild = pid_from(&pidfile);
    env.until("the grandchild is gone", |_| {
        !Path::new(&format!("/proc/{grandchild}")).exists()
    });
    assert_eq!(env.runs("running", "one"), 0);
}

#[test]
fn a_signal_while_queued_leaves_no_entry() {
    let env = Env::new();
    let mut holder = env.spawn(&["run", "--class", "one", "--", "sleep", "20"]);
    env.until("holder runs", |e| e.runs("running", "one") == 1);
    let mut waiter = env.spawn(&["run", "--class", "one", "--", "true"]);
    env.until("waiter waits", |e| e.runs("waiting", "one") == 1);
    signal(waiter.id(), Signal::TERM);
    assert_eq!(waiter.wait().unwrap().code(), Some(143));
    assert_eq!(env.runs("waiting", "one"), 0);
    signal(holder.id(), Signal::TERM);
    holder.wait().unwrap();
}

#[test]
fn an_unregistered_run_is_observed() {
    // The positive control: "nothing observed" only means something once
    // this has been seen to find one.
    let env = Env::new();
    assert_eq!(env.runs("observed", "seen"), 0);
    let mut stray = Command::new("sleep").arg("3123").spawn().unwrap();
    env.until("the stray sleep shows up", |e| {
        e.runs("observed", "seen") == 1
    });
    stray.kill().unwrap();
    stray.wait().unwrap();
    env.until("and goes away", |e| e.runs("observed", "seen") == 0);
}

#[test]
fn a_registered_run_is_not_observed_a_second_time() {
    let env = Env::new();
    let mut run = env.spawn(&["run", "--class", "counted", "--", "sleep", "3789"]);
    env.until("it runs", |e| e.runs("running", "counted") == 1);
    sleep(Duration::from_millis(300));
    assert_eq!(env.runs("observed", "counted"), 0);
    signal(run.id(), Signal::TERM);
    run.wait().unwrap();
}

#[test]
fn wait_blocks_until_the_class_is_idle() {
    let env = Env::new();
    assert_eq!(env.code(&["wait", "awaited", "--max-wait", "1s"]), 0);
    let mut stray = Command::new("sleep").arg("3456").spawn().unwrap();
    env.until("the stray sleep shows up", |e| {
        e.runs("observed", "awaited") == 1
    });
    assert_eq!(env.code(&["wait", "awaited", "--max-wait", "1s"]), 200);
    let waiter = env.spawn(&["wait", "awaited", "--max-wait", "30s"]);
    sleep(Duration::from_millis(500));
    stray.kill().unwrap();
    stray.wait().unwrap();
    assert_eq!(waiter.wait_with_output().unwrap().status.code(), Some(0));
}
