//! `lotse wait <class>`: block until no run of the class is under way.

use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::EXIT_WAIT_LIMIT;
use crate::config::Config;
use crate::proc::ProcSource;
use crate::snapshot::Snapshot;
use crate::state::StateDir;
use crate::units::format_duration;

pub const POLL: Duration = Duration::from_secs(1);
pub const REPORT_EVERY: Duration = Duration::from_secs(30);

pub fn wait(
    cfg: &Config,
    state: &StateDir,
    src: &dyn ProcSource,
    class: &str,
    target: Option<&str>,
    max_wait: Duration,
) -> Result<i32> {
    let begun = Instant::now();
    let mut reported: Option<Instant> = None;
    loop {
        let busy = {
            let lock = state.lock()?;
            Snapshot::take(cfg, state, &lock, src)?.busy(class, target)
        };
        if busy.is_empty() {
            return Ok(0);
        }
        if begun.elapsed() >= max_wait {
            eprintln!(
                "lotse: gave up after {}: still under way: {}",
                format_duration(begun.elapsed()),
                busy.join(", ")
            );
            return Ok(EXIT_WAIT_LIMIT);
        }
        if reported.is_none_or(|t| t.elapsed() >= REPORT_EVERY) {
            eprintln!("lotse: waiting for {}", busy.join(", "));
            reported = Some(Instant::now());
        }
        sleep(POLL);
    }
}
