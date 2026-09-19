//! `lotse.toml`: the classes of runs, their limits and how to recognise them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::Deserialize;

use crate::units::{parse_duration, parse_size};

pub const FILE_NAME: &str = "lotse.toml";

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
    /// `None`: as many as the memory budget allows.
    pub slots: Option<u32>,
    /// Slots and queue count per `--target` instead of per class.
    pub per_target: bool,
    pub exclusive_with: Vec<String>,
    /// Command lines that are a run of this class even if nobody registered it.
    pub observe: Vec<Regex>,
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
    slots: Option<u32>,
    #[serde(default)]
    per_target: bool,
    #[serde(default)]
    exclusive_with: Vec<String>,
    #[serde(default)]
    observe: Vec<String>,
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

impl Config {
    pub fn parse(text: &str) -> Result<Config> {
        let raw: RawConfig = toml::from_str(text)?;
        let mut classes = BTreeMap::new();
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
                    slots: c.slots,
                    per_target: c.per_target,
                    exclusive_with: c.exclusive_with.clone(),
                    observe: patterns(&c.observe, &format!("class {name}, observe"))?,
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

    /// `lotse.toml` upwards from `start`, then the per-user file.
    pub fn find(start: &Path) -> Option<PathBuf> {
        for dir in start.ancestors() {
            let candidate = dir.join(FILE_NAME);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        let candidate = base.join("lotse").join("config.toml");
        candidate.is_file().then_some(candidate)
    }

    pub fn load(explicit: Option<&Path>, cwd: &Path) -> Result<Config> {
        let path = match explicit {
            Some(p) => p.to_path_buf(),
            None => Config::find(cwd).with_context(|| {
                format!(
                    "no {FILE_NAME} found upwards from {} and no per-user config.toml",
                    cwd.display()
                )
            })?,
        };
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
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
    fn defaults() {
        let cfg = Config::parse("[class.a]\n").unwrap();
        assert_eq!(cfg.reserve, 0);
        assert_eq!(cfg.max_wait, Duration::from_secs(1800));
        assert_eq!(cfg.classes["a"].slots, None);
    }

    #[test]
    fn finds_the_file_upwards() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("a/b");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(dir.path().join(FILE_NAME), "").unwrap();
        assert_eq!(Config::find(&deep), Some(dir.path().join(FILE_NAME)));
    }
}
