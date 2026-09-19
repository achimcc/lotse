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
        class.memory.saturating_sub(self.rss_tree)
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
            Reason::Memory { need, have } => write!(
                f,
                "memory: need {} including the reserve, {} left after what running jobs will still grow",
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
        // A run that has just started is still small but will grow to its
        // estimate; one that has grown is already missing from MemAvailable.
        let pending: u64 = active.iter().map(|a| a.pending(cfg)).sum();
        let have = mem_available.saturating_sub(pending);
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

    #[test]
    fn an_unbudgeted_class_ignores_memory() {
        assert_eq!(
            decide(&cfg(), &cand("deploy", Some("vps")), &[], &[], 0),
            OK
        );
    }
}
