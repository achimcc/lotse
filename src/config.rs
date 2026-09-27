//! `lotse.toml`: the classes of runs, their limits and how to recognise them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::Deserialize;

use crate::units::{parse_duration, parse_size};

/// Names the configuration file, ahead of the per-user one.
pub const ENV_VAR: &str = "LOTSE_CONFIG";

#[derive(Debug)]
pub struct Config {
    /// Memory that stays free whatever is admitted.
    pub reserve: u64,
    pub max_wait: Duration,
    pub classes: BTreeMap<String, Class>,
}

#[derive(Debug)]
pub struct Class {
    /// What one run of this class is expected to grow to. Zero: not budgeted.
    pub memory: u64,
    /// For how long after its start a run still grows towards `memory`.
    /// After that its resident set is taken as final: an evaluation that
    /// has long finished and only waits for the builders claims nothing
    /// any more. `None`: it always counts with its whole estimate.
    pub grows_for: Option<Duration>,
    /// `None`: as many as the memory budget allows.
    pub slots: Option<u32>,
    /// Slots and queue count per `--target` instead of per class.
    pub per_target: bool,
    pub exclusive_with: Vec<String>,
    /// Command lines that are a run of this class even if nobody registered it.
    pub observe: Vec<Regex>,
    /// Command lines that match `observe` and are still not of this class.
    pub ignore: Vec<Regex>,
    /// `lotse hook` puts `lotse run` in front of commands of this class.
    pub wrap: bool,
    pub retry: Option<Retry>,
    pub max_wait: Option<Duration>,
}

#[derive(Debug)]
pub struct Retry {
    pub patterns: Vec<Regex>,
    pub times: u32,
    pub pause: Duration,
}

// A typo in a limit must not silently select the default, hence
// `deny_unknown_fields` on every level.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    reserve: Option<String>,
    max_wait: Option<String>,
    #[serde(default)]
    class: BTreeMap<String, RawClass>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawClass {
    memory: Option<String>,
    grows_for: Option<String>,
    slots: Option<u32>,
    #[serde(default)]
    per_target: bool,
    #[serde(default)]
    exclusive_with: Vec<String>,
    #[serde(default)]
    observe: Vec<String>,
    #[serde(default)]
    ignore: Vec<String>,
    #[serde(default)]
    wrap: bool,
    retry: Option<RawRetry>,
    max_wait: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRetry {
    patterns: Vec<String>,
    times: u32,
    pause: String,
}

fn patterns(raw: &[String], what: &str) -> Result<Vec<Regex>> {
    raw.iter()
        .map(|p| Regex::new(p).with_context(|| format!("{what}: bad pattern {p:?}")))
        .collect()
}

/// A class name ends up in a command line (`lotse hook claude` writes
/// `lotse run --class=<name> --`), so it is one plain word or nothing.
fn plain_word(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// What makes a file or directory unfit to be trusted as configuration:
/// someone other than us (or root) owns it, or others may write to it.
fn trust_problem(what: &str, path: &Path, uid: u32, mode: u32, me: u32) -> Option<String> {
    if uid != me && uid != 0 {
        return Some(format!(
            "{what} {} belongs to uid {uid}, not to us ({me}) or root",
            path.display()
        ));
    }
    if mode & 0o022 != 0 {
        return Some(format!(
            "{what} {} is writable by group or others (mode {:04o})",
            path.display(),
            mode & 0o7777
        ));
    }
    None
}

/// Reads a configuration file only if nobody but us (or root) could have
/// written it: the file, after all symlinks, and the directory it is in.
/// A `lotse.toml` is as good as code — its class names end up in command
/// lines — so it gets the treatment ssh gives an `authorized_keys`.
fn read_trusted(path: &Path) -> Result<String> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let real = path
        .canonicalize()
        .with_context(|| format!("cannot read {}", path.display()))?;
    let me = rustix::process::geteuid().as_raw();
    let dir = real.parent().unwrap_or(Path::new("/"));
    let dir_meta =
        std::fs::metadata(dir).with_context(|| format!("cannot read {}", dir.display()))?;
    if let Some(problem) = trust_problem("directory", dir, dir_meta.uid(), dir_meta.mode(), me) {
        bail!("not trusted: {problem}");
    }
    // The resolved path, not followed a second time: what is checked is what is read.
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(&real)
        .with_context(|| format!("cannot read {}", real.display()))?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        bail!("not trusted: {} is not a regular file", real.display());
    }
    if let Some(problem) = trust_problem("file", &real, meta.uid(), meta.mode(), me) {
        bail!("not trusted: {problem}");
    }
    let mut text = String::new();
    file.read_to_string(&mut text)
        .with_context(|| format!("cannot read {}", real.display()))?;
    Ok(text)
}

impl Config {
    pub fn parse(text: &str) -> Result<Config> {
        let raw: RawConfig = toml::from_str(text)?;
        let mut classes = BTreeMap::new();
        for name in raw.class.keys() {
            if !plain_word(name) {
                bail!("class name {name:?}: only letters, digits, '_' and '-' are allowed");
            }
        }
        for (name, c) in &raw.class {
            for other in &c.exclusive_with {
                if !raw.class.contains_key(other) {
                    bail!("class {name}: exclusive_with names an unknown class {other:?}");
                }
            }
            if c.slots == Some(0) {
                bail!("class {name}: slots = 0 would never admit anything");
            }
            let retry = match &c.retry {
                None => None,
                Some(r) => {
                    if r.times == 0 || r.patterns.is_empty() {
                        bail!("class {name}: retry needs times >= 1 and at least one pattern");
                    }
                    Some(Retry {
                        patterns: patterns(&r.patterns, &format!("class {name}, retry"))?,
                        times: r.times,
                        pause: parse_duration(&r.pause)?,
                    })
                }
            };
            classes.insert(
                name.clone(),
                Class {
                    memory: c
                        .memory
                        .as_deref()
                        .map(parse_size)
                        .transpose()?
                        .unwrap_or(0),
                    grows_for: c.grows_for.as_deref().map(parse_duration).transpose()?,
                    slots: c.slots,
                    per_target: c.per_target,
                    exclusive_with: c.exclusive_with.clone(),
                    observe: patterns(&c.observe, &format!("class {name}, observe"))?,
                    ignore: patterns(&c.ignore, &format!("class {name}, ignore"))?,
                    wrap: c.wrap,
                    retry,
                    max_wait: c.max_wait.as_deref().map(parse_duration).transpose()?,
                },
            );
        }
        Ok(Config {
            reserve: raw
                .reserve
                .as_deref()
                .map(parse_size)
                .transpose()?
                .unwrap_or(0),
            max_wait: raw
                .max_wait
                .as_deref()
                .map(parse_duration)
                .transpose()?
                .unwrap_or(Duration::from_secs(30 * 60)),
            classes,
        })
    }

    /// Where the configuration is: `--config`, else `$LOTSE_CONFIG`, else
    /// `$XDG_CONFIG_HOME/lotse/config.toml` if it exists. `None`: nowhere.
    ///
    /// Never from the current directory or above it. The hook runs in every
    /// session on the machine, in whatever directory it works — a foreign
    /// checkout, a scratch directory under `/tmp` — and a `lotse.toml` found
    /// there would put its class names into the session's command lines.
    pub fn locate(explicit: Option<&Path>) -> Option<PathBuf> {
        if let Some(p) = explicit {
            return Some(p.to_path_buf());
        }
        if let Some(p) = std::env::var_os(ENV_VAR).filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(p));
        }
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        let candidate = base.join("lotse").join("config.toml");
        // `exists` follows symlinks: a dangling one counts as none.
        candidate.exists().then_some(candidate)
    }

    pub fn load(explicit: Option<&Path>) -> Result<Config> {
        let path = Config::locate(explicit).with_context(|| {
            format!(
                "no configuration: pass --config FILE, set {ENV_VAR}, \
                 or create $XDG_CONFIG_HOME/lotse/config.toml"
            )
        })?;
        Config::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Result<Config> {
        let text = read_trusted(path)?;
        Config::parse(&text).with_context(|| format!("in {}", path.display()))
    }

    /// Exclusion holds in both directions, whichever side declared it.
    pub fn excludes(&self, a: &str, b: &str) -> bool {
        let names = |x: &str, y: &str| {
            self.classes
                .get(x)
                .is_some_and(|c| c.exclusive_with.iter().any(|e| e == y))
        };
        names(a, b) || names(b, a)
    }
}

#[cfg(test)]
pub(crate) const EXAMPLE: &str = r#"
reserve = "4G"
max_wait = "30m"

[class.eval]
memory = "10G"
grows_for = "5m"
slots = 3
observe = ['\bnix (build|eval)\b.*nixosConfigurations', '\bcolmena build\b']
retry = { patterns = ['Could not resolve host', 'unable to download', 'daemon disconnected'], times = 3, pause = "30s" }

[class.deploy]
per_target = true
slots = 1
observe = ['\bcolmena apply\b']

[class.pruefungen]
slots = 1
exclusive_with = ["eval"]
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_loads() {
        let cfg = Config::parse(EXAMPLE).unwrap();
        assert_eq!(cfg.reserve, 4 << 30);
        assert_eq!(cfg.classes["eval"].memory, 10 << 30);
        assert_eq!(cfg.classes["eval"].slots, Some(3));
        assert_eq!(cfg.classes["eval"].retry.as_ref().unwrap().times, 3);
        assert!(cfg.classes["deploy"].per_target);
        assert!(cfg.classes["deploy"].retry.is_none());
        assert_eq!(cfg.classes["pruefungen"].memory, 0);
    }

    #[test]
    fn exclusion_is_symmetric() {
        let cfg = Config::parse(EXAMPLE).unwrap();
        assert!(cfg.excludes("pruefungen", "eval"));
        assert!(cfg.excludes("eval", "pruefungen"));
        assert!(!cfg.excludes("eval", "deploy"));
    }

    #[test]
    fn unknown_key_is_an_error() {
        assert!(Config::parse("[class.a]\nslotz = 1\n").is_err());
        assert!(Config::parse("reserv = \"1G\"\n").is_err());
    }

    #[test]
    fn unknown_exclusive_class_is_an_error() {
        let err = Config::parse("[class.a]\nexclusive_with = [\"nope\"]\n").unwrap_err();
        assert!(err.to_string().contains("nope"), "{err}");
    }

    #[test]
    fn a_class_name_must_be_a_plain_word() {
        // It ends up in a command line (`lotse hook claude`), CD-1.
        for bad in [
            "x -- true; echo INJECTED >&2; #",
            "a b",
            "$(id)",
            "a'b",
            "",
            "ä",
        ] {
            let text = format!("[class.{bad:?}]\n");
            let err = Config::parse(&text).expect_err(bad);
            assert!(err.to_string().contains("class name"), "{bad}: {err}");
        }
        assert!(Config::parse("[class.eval-2_x]\n").is_ok());
    }

    #[test]
    fn defaults() {
        let cfg = Config::parse("[class.a]\n").unwrap();
        assert_eq!(cfg.reserve, 0);
        assert_eq!(cfg.max_wait, Duration::from_secs(1800));
        assert_eq!(cfg.classes["a"].slots, None);
    }

    #[test]
    fn a_foreign_or_open_file_is_not_trusted() {
        let p = Path::new("/x");
        assert!(trust_problem("file", p, 1000, 0o100644, 1000).is_none());
        assert!(trust_problem("file", p, 0, 0o100444, 1000).is_none());
        assert!(trust_problem("file", p, 1001, 0o100644, 1000).is_some());
        assert!(trust_problem("file", p, 1000, 0o100664, 1000).is_some());
        assert!(trust_problem("file", p, 1000, 0o100646, 1000).is_some());
        // A sticky world-writable directory (/tmp) is not a home for it either.
        assert!(trust_problem("directory", p, 0, 0o41777, 1000).is_some());
    }

    #[test]
    fn a_trusted_file_loads() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("lotse.toml");
        std::fs::write(&file, "[class.a]\n").unwrap();
        assert!(Config::load_from(&file).unwrap().classes.contains_key("a"));
    }
}
