use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use lexopt::prelude::*;

use lotse::config::Config;
use lotse::proc::RealProc;
use lotse::run::{RunArgs, run};
use lotse::snapshot::Snapshot;
use lotse::state::{StateDir, now};
use lotse::units::parse_duration;
use lotse::{EXIT_USAGE, status, wait};

const HELP: &str = "\
lotse: queue, admit and retry the heavy runs of parallel sessions

USAGE:
  lotse [--config FILE] run --class CLASS [--target T] [--max-wait D] [--no-retry] -- COMMAND…
  lotse [--config FILE] status [--json]
  lotse [--config FILE] wait CLASS [--target T] [--max-wait D]

run     waits for a slot of CLASS, runs COMMAND, copies its output into a log,
        retries it if it failed with one of the class's network patterns, and
        exits with COMMAND's exit code.
status  lists what runs and what waits, registered or merely observed.
wait    blocks until no run of CLASS (on target T) is under way.

Durations take a unit: 90s, 30m, 2h. The configuration is the nearest
lotse.toml upwards from the current directory, else
$XDG_CONFIG_HOME/lotse/config.toml.

EXIT CODES of lotse itself:
  2    usage, configuration or state directory
  200  --max-wait passed
  201  every attempt failed with a network pattern in its output
";

enum Cmd {
    Run(RunArgs),
    Status {
        json: bool,
    },
    Wait {
        class: String,
        target: Option<String>,
        max_wait: Option<Duration>,
    },
}

fn text(v: OsString) -> Result<String> {
    v.into_string()
        .map_err(|v| anyhow!("not valid UTF-8: {}", v.to_string_lossy()))
}

fn parse() -> Result<(Option<PathBuf>, Cmd)> {
    let mut parser = lexopt::Parser::from_env();
    let mut config = None;
    let mut sub: Option<String> = None;
    let mut class = None;
    let mut target = None;
    let mut max_wait = None;
    let mut no_retry = false;
    let mut json = false;
    let mut command: Vec<String> = Vec::new();

    while let Some(arg) = parser.next()? {
        match arg {
            Long("help") | Short('h') => {
                print!("{HELP}");
                std::process::exit(0);
            }
            Long("version") | Short('V') => {
                println!("lotse {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            Long("config") => config = Some(PathBuf::from(parser.value()?)),
            Long("class") => class = Some(text(parser.value()?)?),
            Long("target") => target = Some(text(parser.value()?)?),
            Long("max-wait") => max_wait = Some(parse_duration(&text(parser.value()?)?)?),
            Long("no-retry") => no_retry = true,
            Long("json") => json = true,
            Value(v) if sub.is_none() => sub = Some(text(v)?),
            Value(v) if sub.as_deref() == Some("run") => {
                // The command begins here; nothing after it is ours.
                command.push(text(v)?);
                for rest in parser.raw_args()? {
                    command.push(text(rest)?);
                }
            }
            Value(v) if sub.as_deref() == Some("wait") && class.is_none() => {
                class = Some(text(v)?);
            }
            other => return Err(other.unexpected().into()),
        }
    }

    let cmd = match sub.as_deref() {
        Some("run") => {
            if command.is_empty() {
                bail!("run: no command given");
            }
            Cmd::Run(RunArgs {
                class: class.context("run: --class is required")?,
                target,
                max_wait,
                no_retry,
                command,
            })
        }
        Some("status") => Cmd::Status { json },
        Some("wait") => Cmd::Wait {
            class: class.context("wait: name the class to wait for")?,
            target,
            max_wait,
        },
        Some(other) => bail!("unknown command {other:?}, see --help"),
        None => bail!("no command, see --help"),
    };
    Ok((config, cmd))
}

fn known<'a>(cfg: &'a Config, class: &str) -> Result<&'a lotse::config::Class> {
    cfg.classes.get(class).with_context(|| {
        let names: Vec<&str> = cfg.classes.keys().map(String::as_str).collect();
        format!("unknown class {class:?}; configured: {}", names.join(", "))
    })
}

fn real_main() -> Result<i32> {
    let (config, cmd) = parse()?;
    let cwd = std::env::current_dir().context("no current directory")?;
    let cfg = Config::load(config.as_deref(), &cwd)?;
    // Never start uncoordinated because the state is out of reach: that
    // would look like coordination and be none.
    let state = StateDir::open()?;
    match cmd {
        Cmd::Run(args) => {
            known(&cfg, &args.class)?;
            run(&cfg, &state, &RealProc, args)
        }
        Cmd::Status { json } => {
            let snap = {
                let lock = state.lock()?;
                Snapshot::take(&cfg, &state, &lock, &RealProc)?
            };
            if json {
                println!("{}", status::render_json(&cfg, &snap, now()));
            } else {
                print!("{}", status::render_text(&cfg, &snap, now()));
            }
            Ok(0)
        }
        Cmd::Wait {
            class,
            target,
            max_wait,
        } => {
            let max_wait = max_wait
                .or(known(&cfg, &class)?.max_wait)
                .unwrap_or(cfg.max_wait);
            wait::wait(&cfg, &state, &RealProc, &class, target.as_deref(), max_wait)
        }
    }
}

fn main() -> ExitCode {
    match real_main() {
        // 200 and 201 do not fit an i8, but they do fit the u8 the kernel keeps.
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("lotse: {e:#}");
            ExitCode::from(EXIT_USAGE as u8)
        }
    }
}
