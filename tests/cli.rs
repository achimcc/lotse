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
    assert!(
        log.contains("lotse: exit=0 verdict=command attempts=1"),
        "{log}"
    );
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
    assert!(log.contains("exit=0 verdict=command attempts=2"), "{log}");
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

// ---------------------------------------------------------------------------
// The configuration the hook trusts (audit 3, CD-1).

const WRAP_GIT: &str = "[class.eval]\nwrap = true\nobserve = ['^git\\b']\n";

/// `lotse hook claude` in `cwd`, with nothing of the caller's environment
/// that could point it at a configuration.
fn hook(home: &Path, cwd: &Path, extra: &[(&str, &Path)]) -> Output {
    use std::io::Write;
    let event = serde_json::json!({
        "tool_name": "Bash",
        "tool_input": {"command": "git log -1"},
        "cwd": cwd,
    });
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lotse"));
    cmd.env_remove("LOTSE_CONFIG")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .current_dir(cwd)
        .args(["hook", "claude"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in extra {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(event.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "the hook never fails the call");
    out
}

fn mode(path: &Path, bits: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(bits)).unwrap();
}

fn rewritten(out: &Output) -> Option<String> {
    let text = String::from_utf8_lossy(&out.stdout);
    if text.trim().is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    Some(
        v["hookSpecificOutput"]["updatedInput"]["command"]
            .as_str()
            .unwrap()
            .to_string(),
    )
}

#[test]
fn the_hook_reads_no_lotse_toml_upwards_from_the_session() {
    // What a foreign checkout, or anyone who can write /tmp, would plant.
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let planted = tmp.path().join("planted");
    let deep = planted.join("a/b/c");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&deep).unwrap();
    fs::write(planted.join("lotse.toml"), WRAP_GIT).unwrap();
    mode(&planted, 0o777);
    let out = hook(&home, &deep, &[]);
    assert_eq!(rewritten(&out), None);
    // Not even a trusted-looking one in the session's own directory.
    mode(&planted, 0o700);
    assert_eq!(rewritten(&hook(&home, &deep, &[])), None);
}

#[test]
fn a_class_name_that_is_not_a_word_rewrites_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = home.join(".config/lotse");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("config.toml"),
        "[class.\"x -- true; echo INJECTED >&2; #\"]\nwrap = true\nobserve = ['^git\\b']\n",
    )
    .unwrap();
    let out = hook(&home, &home, &[]);
    assert_eq!(rewritten(&out), None);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("class name"), "{err}");
}

#[test]
fn the_per_user_config_is_read_through_symlinks() {
    // The workstation's layout: ~/.config/lotse/config.toml -> the store ->
    // the repository's lotse.toml.
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let repo = tmp.path().join("repo");
    let store = tmp.path().join("store");
    fs::create_dir_all(home.join(".config/lotse")).unwrap();
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir_all(&store).unwrap();
    fs::write(repo.join("lotse.toml"), WRAP_GIT).unwrap();
    std::os::unix::fs::symlink(repo.join("lotse.toml"), store.join("hm_lotse.toml")).unwrap();
    std::os::unix::fs::symlink(
        store.join("hm_lotse.toml"),
        home.join(".config/lotse/config.toml"),
    )
    .unwrap();
    assert_eq!(
        rewritten(&hook(&home, &tmp.path().join("repo"), &[])),
        Some("lotse run --class=eval -- git log -1".to_string())
    );
}

#[test]
fn lotse_config_names_the_configuration() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("lotse.toml"), WRAP_GIT).unwrap();
    let config = repo.join("lotse.toml");
    assert_eq!(
        rewritten(&hook(&home, &home, &[("LOTSE_CONFIG", &config)])),
        Some("lotse run --class=eval -- git log -1".to_string())
    );
}

#[test]
fn a_config_others_can_write_is_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let dir = home.join(".config/lotse");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("config.toml");
    fs::write(&file, WRAP_GIT).unwrap();
    // Its directory writable by the world.
    mode(&dir, 0o777);
    let out = hook(&home, &home, &[]);
    assert_eq!(rewritten(&out), None);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("writable"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The file itself writable by the group.
    mode(&dir, 0o755);
    mode(&file, 0o664);
    assert_eq!(rewritten(&hook(&home, &home, &[])), None);
    // And the control: the same file, trustworthy, is read.
    mode(&file, 0o644);
    assert!(rewritten(&hook(&home, &home, &[])).is_some());
}

#[test]
fn run_without_a_configuration_is_a_usage_error() {
    // A lotse.toml in the current directory is not read any more.
    let env = Env::new();
    let out = Command::new(env!("CARGO_BIN_EXE_lotse"))
        .env_remove("LOTSE_CONFIG")
        .env("HOME", env.path("nohome"))
        .env("XDG_CONFIG_HOME", env.path("nohome/.config"))
        .env("XDG_RUNTIME_DIR", env.path("run"))
        .current_dir(env.tmp.path())
        .args(["run", "--class", "one", "--", "true"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("LOTSE_CONFIG"), "{err}");
}

// ---------------------------------------------------------------------------
// The network verdict (audit 3, CD-3).

impl Env {
    fn last_log_line(&self) -> String {
        self.the_log().lines().last().unwrap().to_string()
    }
}

#[test]
fn a_network_word_before_the_real_error_is_the_real_error() {
    let env = Env::new();
    let script = format!(
        "echo x >> {}; echo 'warning: Could not resolve host: x'; \
         echo 'error: assertion failed (a real red check)'; exit 1",
        env.path("n").display()
    );
    assert_eq!(
        env.code(&["run", "--class", "net", "--", "sh", "-c", &script]),
        1
    );
    assert_eq!(fs::read_to_string(env.path("n")).unwrap(), "x\n");
    let last = env.last_log_line();
    assert!(
        last.starts_with("lotse: exit=1 verdict=command attempts=1"),
        "{last}"
    );
}

#[test]
fn nix_s_follow_up_errors_do_not_hide_the_network() {
    // What nix prints after a substitution died of DNS: the derivations that
    // could not be built for it. They are consequences, not a new error.
    let env = Env::new();
    let script = format!(
        "n=$(cat {c} 2>/dev/null || echo 0); echo $((n+1)) > {c}; \
         if [ $n -lt 1 ]; then \
           echo \"error: Cannot build '/nix/store/abc-x.drv'.\" >&2; \
           echo '       > curl: (6) Could not resolve host: example.org' >&2; \
           echo '       > error: cannot download x from any mirror' >&2; \
           echo \"error: Cannot build '/nix/store/def-y.drv'.\" >&2; \
           echo \"error: 1 dependencies of derivation '/nix/store/ghi-z.drv' failed to build\" >&2; \
           exit 1; fi",
        c = env.path("count").display()
    );
    assert_eq!(
        env.code(&["run", "--class", "net", "--", "sh", "-c", &script]),
        0
    );
    assert_eq!(fs::read_to_string(env.path("count")).unwrap().trim(), "2");
}

#[test]
fn the_last_line_carries_the_real_code_and_the_verdict() {
    let env = Env::new();
    let out = env.output(&["run", "--class", "net", "--", "sh", "-c", &flaky(&env, 99)]);
    // The process exit stays 201: scripts tell "no verdict" by it.
    assert_eq!(out.status.code(), Some(201));
    let last = env.last_log_line();
    assert!(
        last.starts_with("lotse: exit=1 verdict=network attempts=3"),
        "{last}"
    );
}

#[test]
fn a_command_that_exits_201_itself_says_so() {
    let env = Env::new();
    assert_eq!(
        env.code(&["run", "--class", "one", "--", "sh", "-c", "exit 201"]),
        201
    );
    let last = env.last_log_line();
    assert!(
        last.starts_with("lotse: exit=201 verdict=command"),
        "{last}"
    );
}

// ---------------------------------------------------------------------------
// Logs (audit 3, CD-11).

#[test]
fn logs_are_private_and_old_ones_are_removed() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    let dir = env.path("state/lotse/logs");
    fs::create_dir_all(&dir).unwrap();
    let old = dir.join("20250101-000000-00000001-1.log");
    let young = dir.join("20260101-000000-00000002-2.log");
    let other = dir.join("notes.txt");
    for f in [&old, &young, &other] {
        fs::write(f, "x").unwrap();
    }
    mode(&young, 0o644);
    let long_ago = std::time::SystemTime::now() - Duration::from_secs(60 * 24 * 3600);
    for f in [&old, &other] {
        fs::File::options()
            .write(true)
            .open(f)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
    }
    assert_eq!(env.code(&["run", "--class", "one", "--", "true"]), 0);
    assert!(!old.exists(), "a log older than the limit stays");
    assert!(young.exists() && other.exists());
    let perm = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(perm(&dir), 0o700);
    assert_eq!(perm(&young), 0o600);
    let new: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p != &young && p != &other)
        .collect();
    assert_eq!(new.len(), 1, "{new:?}");
    assert_eq!(perm(&new[0]), 0o600);
}
