//! May this run start now? A pure function, so that every rule can be tested
//! without a process in sight.

use crate::config::Config;

pub struct Candidate<'a> {
    pub seq: u64,
    pub class: &'a str,
    pub target: Option<&'a str>,
}

/// A run that is under way, registered or merely observed.
#[derive(Clone, Debug)]
pub struct Active {
    pub class: String,
    pub target: Option<String>,
    /// Resident set of the whole process tree, bytes.
    pub rss_tree: u64,
    /// What that tree holds in swap, bytes.
    pub swap_tree: u64,
    /// Seconds since it started.
    pub age: u64,
    pub label: String,
}

impl Active {
    /// What this run will still take out of `MemAvailable`.
    pub fn pending(&self, cfg: &Config) -> u64 {
        let Some(class) = cfg.classes.get(&self.class) else {
            return 0;
        };
        if class.grows_for.is_some_and(|g| self.age >= g.as_secs()) {
            return 0;
        }
        class.memory.saturating_sub(self.footprint())
    }

    /// Resident and swapped together: what the run has grown to, wherever
    /// the kernel keeps it at the moment.
    pub fn footprint(&self) -> u64 {
        self.rss_tree.saturating_add(self.swap_tree)
    }
}

#[derive(Clone, Debug)]
pub struct Queued {
    pub seq: u64,
    pub class: String,
    pub target: Option<String>,
    pub label: String,
}

#[derive(Debug, PartialEq)]
pub enum Decision {
    /// `starved`: the budget said no, but nothing runs that could ever free
    /// memory, so waiting would be waiting for nobody.
    Admit {
        starved: bool,
    },
    Wait(Reason),
}

#[derive(Debug, PartialEq)]
pub enum Reason {
    Queue { ahead: String },
    Slots { class: String, holders: Vec<String> },
    Exclusive { with: String, holder: String },
    Priority { ahead: String },
    Memory { need: u64, have: u64 },
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use crate::units::format_size;
        match self {
            Reason::Queue { ahead } => write!(f, "queued behind {ahead}"),
            Reason::Slots { class, holders } => {
                write!(f, "all slots of {class} taken by {}", holders.join(", "))
            }
            Reason::Exclusive { with, holder } => {
                write!(f, "excluded by class {with}: {holder}")
            }
            Reason::Priority { ahead } => {
                write!(f, "yielding memory to {ahead}, which has priority")
            }
            Reason::Memory { need, have } => write!(
                f,
                "memory: need {} including the reserve, {} left after what running jobs will still grow or take back from swap",
                format_size(*need),
                format_size(*have)
            ),
        }
    }
}

/// Two runs meet if they name the same target or if either names none: a
/// deploy without `--target` may touch every machine.
pub fn targets_conflict(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    }
}

pub fn decide(
    cfg: &Config,
    cand: &Candidate,
    active: &[Active],
    queued: &[Queued],
    mem_available: u64,
) -> Decision {
    let class = &cfg.classes[cand.class];
    let same_lane = |other_class: &str, other_target: Option<&str>| {
        other_class == cand.class
            && (!class.per_target || targets_conflict(cand.target, other_target))
    };

    // Also behind an older waiter of a class that excludes this one: without
    // that, an exclusive run starves while the class it excludes keeps
    // arriving.
    if let Some(ahead) = queued
        .iter()
        .filter(|q| {
            q.seq < cand.seq
                && (same_lane(&q.class, q.target.as_deref()) || cfg.excludes(cand.class, &q.class))
        })
        .min_by_key(|q| q.seq)
    {
        return Decision::Wait(Reason::Queue {
            ahead: ahead.label.clone(),
        });
    }

    if let Some(slots) = class.slots {
        let holders: Vec<String> = active
            .iter()
            .filter(|a| same_lane(&a.class, a.target.as_deref()))
            .map(|a| a.label.clone())
            .collect();
        if holders.len() >= slots as usize {
            return Decision::Wait(Reason::Slots {
                class: cand.class.to_string(),
                holders,
            });
        }
    }

    if let Some(holder) = active.iter().find(|a| cfg.excludes(cand.class, &a.class)) {
        return Decision::Wait(Reason::Exclusive {
            with: holder.class.clone(),
            holder: holder.label.clone(),
        });
    }

    if class.memory > 0 {
        // Yield to a waiter of a class with a higher priority — but only to
        // one that memory alone holds back. Without this a big run starves
        // behind small ones: every time one of them ends, the next takes the
        // room before enough has gathered. A waiter that is behind a slot or
        // an exclusion could not start anyway, and holding everyone back
        // for it would only idle the machine.
        if let Some(ahead) = queued
            .iter()
            .filter(|q| {
                cfg.classes
                    .get(&q.class)
                    .is_some_and(|qc| qc.priority > class.priority && qc.memory > 0)
                    && memory_is_all_it_waits_for(cfg, q, active, queued)
            })
            .min_by_key(|q| q.seq)
        {
            return Decision::Wait(Reason::Priority {
                ahead: ahead.label.clone(),
            });
        }

        // A run that has just started is still small but will grow to its
        // estimate; one that has grown is already missing from MemAvailable.
        let pending: u64 = active.iter().map(|a| a.pending(cfg)).sum();
        // What a running job holds in swap is NOT missing from MemAvailable:
        // the kernel made that room by pushing the job out, and the job
        // takes it back page by page. Three evaluations once ran side by
        // side this way, each showing 5G resident and 12G in swap.
        let swapped: u64 = active.iter().map(|a| a.swap_tree).sum();
        let have = mem_available
            .saturating_sub(pending)
            .saturating_sub(swapped);
        let need = class.memory + cfg.reserve;
        if need > have {
            if active.is_empty() {
                return Decision::Admit { starved: true };
            }
            return Decision::Wait(Reason::Memory { need, have });
        }
    }

    Decision::Admit { starved: false }
}

/// Would this waiter start at once if memory were no question? The answer
/// recurses only into classes of a still higher priority, so it ends.
fn memory_is_all_it_waits_for(
    cfg: &Config,
    waiter: &Queued,
    active: &[Active],
    queued: &[Queued],
) -> bool {
    let cand = Candidate {
        seq: waiter.seq,
        class: &waiter.class,
        target: waiter.target.as_deref(),
    };
    let others: Vec<Queued> = queued
        .iter()
        .filter(|q| q.seq != waiter.seq)
        .cloned()
        .collect();
    matches!(
        decide(cfg, &cand, active, &others, u64::MAX),
        Decision::Admit { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EXAMPLE;

    const G: u64 = 1 << 30;

    fn cfg() -> Config {
        Config::parse(EXAMPLE).unwrap()
    }

    fn cand<'a>(class: &'a str, target: Option<&'a str>) -> Candidate<'a> {
        Candidate {
            seq: 10,
            class,
            target,
        }
    }

    fn active(class: &str, target: Option<&str>, rss: u64) -> Active {
        Active {
            class: class.into(),
            target: target.map(Into::into),
            rss_tree: rss,
            swap_tree: 0,
            age: 10,
            label: format!("{class} {}", target.unwrap_or("-")),
        }
    }

    fn queued(seq: u64, class: &str) -> Queued {
        Queued {
            seq,
            class: class.into(),
            target: None,
            label: format!("{class} #{seq}"),
        }
    }

    const OK: Decision = Decision::Admit { starved: false };

    #[test]
    fn an_empty_machine_admits() {
        assert_eq!(decide(&cfg(), &cand("eval", None), &[], &[], 64 * G), OK);
    }

    #[test]
    fn fifo_within_a_class_only() {
        let c = cfg();
        assert_eq!(
            decide(&c, &cand("eval", None), &[], &[queued(3, "eval")], 64 * G),
            Decision::Wait(Reason::Queue {
                ahead: "eval #3".into()
            })
        );
        // Someone who arrived later does not count, nor does another class.
        let others = [queued(11, "eval"), queued(3, "deploy")];
        assert_eq!(decide(&c, &cand("eval", None), &[], &others, 64 * G), OK);
    }

    #[test]
    fn an_exclusive_waiter_is_not_overtaken_by_what_it_excludes() {
        let waiting = [queued(3, "pruefungen")];
        assert_eq!(
            decide(&cfg(), &cand("eval", None), &[], &waiting, 64 * G),
            Decision::Wait(Reason::Queue {
                ahead: "pruefungen #3".into()
            })
        );
    }

    #[test]
    fn slots() {
        let three: Vec<Active> = (0..3).map(|_| active("eval", None, 10 * G)).collect();
        match decide(&cfg(), &cand("eval", None), &three, &[], 64 * G) {
            Decision::Wait(Reason::Slots { holders, .. }) => assert_eq!(holders.len(), 3),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn deploys_queue_per_target() {
        let c = cfg();
        let server = [active("deploy", Some("server"), 0)];
        let is_slots = |d: Decision| matches!(d, Decision::Wait(Reason::Slots { .. }));
        assert!(is_slots(decide(
            &c,
            &cand("deploy", Some("server")),
            &server,
            &[],
            64 * G
        )));
        assert_eq!(
            decide(&c, &cand("deploy", Some("vps")), &server, &[], 64 * G),
            OK
        );
        // No target: may touch any machine, so it meets every deploy.
        assert!(is_slots(decide(
            &c,
            &cand("deploy", None),
            &server,
            &[],
            64 * G
        )));
    }

    #[test]
    fn exclusion_in_both_directions() {
        let c = cfg();
        let is_excl = |d: Decision| matches!(d, Decision::Wait(Reason::Exclusive { .. }));
        assert!(is_excl(decide(
            &c,
            &cand("eval", None),
            &[active("pruefungen", None, 0)],
            &[],
            64 * G
        )));
        assert!(is_excl(decide(
            &c,
            &cand("pruefungen", None),
            &[active("eval", None, 10 * G)],
            &[],
            64 * G
        )));
    }

    #[test]
    fn a_fresh_run_counts_with_its_full_estimate() {
        let fresh = [active("eval", None, G)];
        assert_eq!(
            decide(&cfg(), &cand("eval", None), &fresh, &[], 20 * G),
            Decision::Wait(Reason::Memory {
                need: 14 * G,
                have: 11 * G
            })
        );
    }

    #[test]
    fn a_grown_run_is_already_in_mem_available() {
        let grown = [active("eval", None, 10 * G)];
        assert_eq!(decide(&cfg(), &cand("eval", None), &grown, &[], 20 * G), OK);
    }

    #[test]
    fn a_swapped_out_run_still_owns_what_it_swapped() {
        // 5G resident, 12G in swap: grown past its estimate, so nothing is
        // pending, and MemAvailable looks roomy because the run was pushed out.
        let mut pushed_out = active("eval", None, 5 * G);
        pushed_out.swap_tree = 12 * G;
        assert_eq!(pushed_out.pending(&cfg()), 0);
        assert_eq!(
            decide(
                &cfg(),
                &cand("eval", None),
                &[pushed_out.clone()],
                &[],
                20 * G
            ),
            Decision::Wait(Reason::Memory {
                need: 14 * G,
                have: 8 * G
            })
        );
        // Enough room even after it has taken everything back.
        assert_eq!(
            decide(&cfg(), &cand("eval", None), &[pushed_out], &[], 26 * G),
            OK
        );
    }

    #[test]
    fn swap_counts_towards_the_estimate_only_once() {
        // 2G resident and 3G swapped of a 10G estimate: 5G still to grow,
        // 3G to take back. Not 8G pending plus 3G swapped.
        let mut young = active("eval", None, 2 * G);
        young.swap_tree = 3 * G;
        assert_eq!(young.pending(&cfg()), 5 * G);
        assert_eq!(
            decide(&cfg(), &cand("eval", None), &[young], &[], 22 * G),
            OK
        );
    }

    #[test]
    fn a_run_past_its_growth_claims_nothing_more() {
        // Small and old: the evaluation is over, it waits for the builders.
        let mut settled = active("eval", None, G / 32);
        settled.age = 16 * 60;
        assert_eq!(
            decide(&cfg(), &cand("eval", None), &[settled], &[], 20 * G),
            OK
        );
    }

    #[test]
    fn nobody_to_wait_for_means_go() {
        assert_eq!(
            decide(&cfg(), &cand("eval", None), &[], &[], 8 * G),
            Decision::Admit { starved: true }
        );
    }

    /// Small runs and a big one that goes first, both budgeted.
    fn cfg_with_priority() -> Config {
        Config::parse(
            r#"
reserve = "4G"

[class.eval]
memory = "10G"
slots = 3

[class.deploy]
memory = "12G"
per_target = true
slots = 1
priority = 10
"#,
        )
        .unwrap()
    }

    fn queued_on(seq: u64, class: &str, target: &str) -> Queued {
        Queued {
            seq,
            class: class.into(),
            target: Some(target.into()),
            label: format!("{class} {target} #{seq}"),
        }
    }

    #[test]
    fn a_small_run_yields_to_a_waiter_with_priority() {
        // One evaluation runs, 20G are free: room for another evaluation
        // (14G) but the waiting deploy needs 16G and would never get them
        // if evaluations kept arriving. It arrived LATER and still goes first.
        let c = cfg_with_priority();
        let running = [active("eval", None, 10 * G)];
        let waiting = [queued_on(11, "deploy", "server")];
        assert_eq!(
            decide(&c, &cand("eval", None), &running, &waiting, 14 * G),
            Decision::Wait(Reason::Priority {
                ahead: "deploy server #11".into()
            })
        );
        // Without the waiter the same evaluation starts.
        assert_eq!(decide(&c, &cand("eval", None), &running, &[], 14 * G), OK);
    }

    #[test]
    fn nobody_yields_to_a_waiter_that_could_not_start_anyway() {
        // The waiting deploy is behind a running deploy of its own target:
        // its slot is taken, memory is not what holds it back.
        let c = cfg_with_priority();
        let running = [active("deploy", Some("server"), 12 * G)];
        let waiting = [queued_on(11, "deploy", "server")];
        assert_eq!(
            decide(&c, &cand("eval", None), &running, &waiting, 20 * G),
            OK
        );
        // Nor to the second in line: the first one is what it waits for.
        let two = [
            queued_on(11, "deploy", "server"),
            queued_on(12, "deploy", "server"),
        ];
        match decide(&c, &cand("eval", None), &[], &two, 20 * G) {
            Decision::Wait(Reason::Priority { ahead }) => assert_eq!(ahead, "deploy server #11"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn priority_is_one_way_and_spares_unbudgeted_runs() {
        let c = cfg_with_priority();
        // The deploy does not yield to a waiting evaluation.
        let waiting = [queued(3, "eval")];
        assert_eq!(
            decide(&c, &cand("deploy", Some("server")), &[], &waiting, 64 * G),
            OK
        );
        // In the example nothing has a priority and deploys are unbudgeted:
        // an evaluation passes a waiting deploy as before.
        let waiting = [queued_on(3, "deploy", "server")];
        assert_eq!(
            decide(&cfg(), &cand("eval", None), &[], &waiting, 64 * G),
            OK
        );
    }

    #[test]
    fn an_unbudgeted_class_ignores_memory() {
        assert_eq!(
            decide(&cfg(), &cand("deploy", Some("vps")), &[], &[], 0),
            OK
        );
    }
}
