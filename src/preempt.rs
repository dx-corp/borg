//! Preemption: a pure choice of running leases to cancel for starving demand.
//!
//! Demand of a `(class, scope)` that the allocator could not place and that has
//! waited at least `after_secs` may displace a running lease of a strictly
//! lower-priority scope, provided freeing that lease leaves its drone room for
//! the starving class. Priority is the [`Scope`] ordering: smaller outranks.

use std::collections::BTreeSet;

use serde::Deserialize;

use crate::allocator::{
    ActiveLease, DroneState, LeaseState, QueueAge, Unmet, free_room, free_slots,
};
use crate::classes::{Class, Classes, Scope};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreemptSettings {
    /// Starving demand must have waited this long.
    pub after_secs: u64,
    /// Most leases cancelled in one queen pass.
    pub max_per_pass: u32,
    /// A lease younger than this is never cancelled.
    pub min_victim_age_secs: u64,
}

impl PreemptSettings {
    pub fn validate(&self) -> Result<(), String> {
        if self.after_secs == 0 || self.max_per_pass == 0 || self.min_victim_age_secs == 0 {
            return Err(
                "preemption after_secs, max_per_pass and min_victim_age_secs must be positive"
                    .to_owned(),
            );
        }
        Ok(())
    }
}

/// A running lease whose job is known, so it can be cancelled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Running {
    pub lease_id: String,
    pub drone: String,
    pub class: Class,
    pub scope: Scope,
    pub started_at: i64,
    /// Already told to stop; its drone is not yet free.
    pub preempted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Victim {
    pub lease_id: String,
    pub drone: String,
    pub class: Class,
    pub scope: Scope,
    /// The starving demand this cancellation is for.
    pub for_class: Class,
    pub for_scope: Scope,
}

pub struct PreemptInput<'a> {
    pub classes: &'a Classes,
    pub settings: &'a PreemptSettings,
    pub drones: &'a [DroneState],
    /// Every active lease that is not in `running` (pending, unit-placed, or
    /// running with no known job); they hold capacity and are never victims.
    pub others: &'a [ActiveLease],
    pub running: &'a [Running],
    /// Demand the allocator could not place this pass.
    pub unmet: &'a [Unmet],
    pub queue_ages: &'a [QueueAge],
    pub now: i64,
}

/// Capacity held by each lease: `Some(i)` marks `running[i]`.
type Held = Vec<(Option<usize>, ActiveLease)>;

/// Whether `running[i]`'s drone could hold a `class` lease once it is gone.
fn frees_room_for(input: &PreemptInput<'_>, held: &Held, i: usize, class: Class) -> bool {
    let Some(drone) = input.drones.iter().find(|d| d.id == input.running[i].drone) else {
        return false;
    };
    let rest: Vec<ActiveLease> = held
        .iter()
        .filter(|(c, _)| *c != Some(i))
        .map(|(_, l)| l.clone())
        .collect();
    let room = free_room(drone, &rest, input.classes);
    !drone.blocked_classes.contains(&class)
        && free_slots(drone, &room, input.classes.spec(class)) > 0
}

/// Up to `max_per_pass` leases to cancel. Deterministic.
pub fn select(input: &PreemptInput<'_>) -> Vec<Victim> {
    let s = input.settings;
    let min_age = i64::try_from(s.min_victim_age_secs).unwrap_or(i64::MAX);
    let mut held: Held = input
        .others
        .iter()
        .map(|l| (None, l.clone()))
        .chain(input.running.iter().enumerate().map(|(i, r)| {
            (
                Some(i),
                ActiveLease {
                    drone_id: r.drone.clone(),
                    class: r.class,
                    scope: r.scope,
                    state: LeaseState::Running,
                },
            )
        }))
        .collect();

    let mut starving: Vec<(Scope, Class)> = input
        .unmet
        .iter()
        .filter(|u| u.count > 0)
        .filter(|u| {
            input.queue_ages.iter().any(|a| {
                a.class == u.class && a.scope == u.scope && a.oldest_age_secs >= s.after_secs
            })
        })
        .map(|u| (u.scope, u.class))
        .collect();
    starving.sort_by_key(|&(scope, class)| (scope, std::cmp::Reverse(class)));
    starving.dedup();

    let mut out = Vec::new();
    let mut picked = BTreeSet::new();
    for (scope, class) in starving {
        if out.len() >= s.max_per_pass as usize {
            break;
        }
        // A cancellation already under way that frees room for this demand
        // covers it; cancelling more would take another job for nothing.
        let covered = input
            .running
            .iter()
            .enumerate()
            .any(|(i, r)| r.preempted && frees_room_for(input, &held, i, class));
        if covered {
            continue;
        }
        let best = input
            .running
            .iter()
            .enumerate()
            .filter(|(i, r)| {
                !r.preempted
                    && !picked.contains(i)
                    && r.scope > scope
                    && input.now - r.started_at >= min_age
                    && frees_room_for(input, &held, *i, class)
            })
            .min_by(|(_, a), (_, b)| {
                b.scope
                    .cmp(&a.scope)
                    .then(a.class.cmp(&b.class))
                    .then(b.started_at.cmp(&a.started_at))
                    .then(a.lease_id.cmp(&b.lease_id))
            });
        let Some((i, r)) = best else {
            continue;
        };
        picked.insert(i);
        // The freed room now belongs to the starving demand.
        if let Some((owner, lease)) = held.iter_mut().find(|(c, _)| *c == Some(i)) {
            *owner = None;
            lease.class = class;
            lease.scope = scope;
        }
        out.push(Victim {
            lease_id: r.lease_id.clone(),
            drone: r.drone.clone(),
            class: r.class,
            scope: r.scope,
            for_class: class,
            for_scope: scope,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 10_000;

    fn settings() -> PreemptSettings {
        PreemptSettings {
            after_secs: 600,
            max_per_pass: 1,
            min_victim_age_secs: 60,
        }
    }

    fn drone(id: &str, mem_mib: u64, vcpu: u32) -> DroneState {
        DroneState {
            id: id.to_owned(),
            vcpu,
            mem_mib,
            docker: true,
            accepting: true,
            max_leases: None,
            bids: Vec::new(),
            profiles: Vec::new(),
            blocked_classes: Vec::new(),
            trust: crate::classes::Trust::Shared,
            cpu_psi_pm: None,
            cpu_load_pm: None,
        }
    }

    /// A drone sized for `n` leases of `class`.
    fn sized_for(id: &str, class: Class, n: u32) -> DroneState {
        let classes = Classes::default();
        let spec = classes.spec(class);
        drone(id, spec.mem_mib * u64::from(n), spec.vcpu * n)
    }

    fn run(id: &str, drone: &str, class: Class, scope: Scope, started_at: i64) -> Running {
        Running {
            lease_id: id.to_owned(),
            drone: drone.to_owned(),
            class,
            scope,
            started_at,
            preempted: false,
        }
    }

    type Demand = (Vec<Unmet>, Vec<QueueAge>);

    fn starving(keys: &[(Class, Scope)], age: u64) -> Demand {
        (
            keys.iter()
                .map(|&(class, scope)| Unmet {
                    class,
                    scope,
                    count: 1,
                })
                .collect(),
            keys.iter()
                .map(|&(class, scope)| QueueAge {
                    class,
                    scope,
                    oldest_age_secs: age,
                })
                .collect(),
        )
    }

    fn pick(
        settings: &PreemptSettings,
        drones: &[DroneState],
        running: &[Running],
        demand: &Demand,
    ) -> Vec<String> {
        select(&PreemptInput {
            classes: &Classes::default(),
            settings,
            drones,
            others: &[],
            running,
            unmet: &demand.0,
            queue_ages: &demand.1,
            now: NOW,
        })
        .into_iter()
        .map(|v| v.lease_id)
        .collect()
    }

    const MEDIUM_MAIN: (Class, Scope) = (Class::Medium, Scope::Main);

    #[test]
    fn nothing_starves_before_the_threshold_or_without_unmet_demand() {
        let drones = [sized_for("host-a", Class::Medium, 1)];
        let running = [run("a", "host-a", Class::Medium, Scope::Pr, 0)];
        assert!(
            pick(
                &settings(),
                &drones,
                &running,
                &starving(&[MEDIUM_MAIN], 599)
            )
            .is_empty()
        );
        assert!(pick(&settings(), &drones, &running, &(vec![], vec![])).is_empty());
        let unplaced_but_not_queued = (starving(&[MEDIUM_MAIN], 9000).0, vec![]);
        assert!(pick(&settings(), &drones, &running, &unplaced_but_not_queued).is_empty());
    }

    #[test]
    fn starving_main_displaces_a_running_pr_lease() {
        let drones = [sized_for("host-a", Class::Medium, 1)];
        let running = [run("a", "host-a", Class::Medium, Scope::Pr, 0)];
        assert_eq!(
            pick(
                &settings(),
                &drones,
                &running,
                &starving(&[MEDIUM_MAIN], 600)
            ),
            ["a"]
        );
    }

    #[test]
    fn equal_or_higher_priority_leases_are_never_victims() {
        let drones = [sized_for("host-a", Class::Medium, 1)];
        let main = [run("a", "host-a", Class::Medium, Scope::Main, 0)];
        let demand = starving(&[MEDIUM_MAIN], 9000);
        assert!(pick(&settings(), &drones, &main, &demand).is_empty());
        let demand = starving(&[(Class::Medium, Scope::Pr)], 9000);
        assert!(pick(&settings(), &drones, &main, &demand).is_empty());
    }

    #[test]
    fn no_victim_when_freeing_it_would_not_fit_the_class() {
        // The drone fits one heavy lease; a medium Main lease stays beside the
        // light Pr one, so removing the light one cannot make room.
        let drones = [sized_for("host-a", Class::Heavy, 1)];
        let running = [
            run("a", "host-a", Class::Light, Scope::Pr, 0),
            run("b", "host-a", Class::Medium, Scope::Main, 0),
        ];
        let demand = starving(&[(Class::Heavy, Scope::Main)], 9000);
        assert!(pick(&settings(), &drones, &running, &demand).is_empty());
    }

    #[test]
    fn prefers_lowest_priority_scope_then_smallest_class_then_newest() {
        let drones = [sized_for("host-a", Class::Heavy, 4)];
        let demand = starving(&[MEDIUM_MAIN], 9000);
        let running = [
            run("main-light", "host-a", Class::Light, Scope::Main, 900),
            run("old-pr", "host-a", Class::Light, Scope::Pr, 100),
            run("new-pr", "host-a", Class::Light, Scope::Pr, 500),
            run("medium-pr", "host-a", Class::Medium, Scope::Pr, 900),
        ];
        assert_eq!(pick(&settings(), &drones, &running, &demand), ["new-pr"]);
    }

    #[test]
    fn respects_the_minimum_victim_age() {
        let drones = [sized_for("host-a", Class::Medium, 1)];
        let demand = starving(&[MEDIUM_MAIN], 9000);
        let young = [run("a", "host-a", Class::Medium, Scope::Pr, NOW - 59)];
        assert!(pick(&settings(), &drones, &young, &demand).is_empty());
        let old = [run("a", "host-a", Class::Medium, Scope::Pr, NOW - 60)];
        assert_eq!(pick(&settings(), &drones, &old, &demand), ["a"]);
    }

    #[test]
    fn respects_max_per_pass_and_never_reuses_freed_room() {
        let drones = [
            sized_for("host-a", Class::Medium, 1),
            sized_for("host-b", Class::Medium, 1),
        ];
        let running = [
            run("a", "host-a", Class::Medium, Scope::Pr, 0),
            run("b", "host-b", Class::Medium, Scope::Pr, 0),
        ];
        let two = PreemptSettings {
            max_per_pass: 2,
            ..settings()
        };
        // One starving key claims one drone, whatever the cap.
        let one_key = starving(&[MEDIUM_MAIN], 9000);
        assert_eq!(pick(&two, &drones, &running, &one_key), ["a"]);
        let two_keys = starving(&[MEDIUM_MAIN, (Class::Light, Scope::Main)], 9000);
        assert_eq!(pick(&two, &drones, &running, &two_keys), ["a", "b"]);
        assert_eq!(pick(&settings(), &drones, &running, &two_keys), ["a"]);
    }

    #[test]
    fn a_cancellation_under_way_covers_the_demand() {
        let drones = [sized_for("host-a", Class::Medium, 2)];
        let mut stopping = run("a", "host-a", Class::Medium, Scope::Pr, 0);
        stopping.preempted = true;
        let running = [stopping, run("b", "host-a", Class::Medium, Scope::Pr, 0)];
        let demand = starving(&[MEDIUM_MAIN], 9000);
        assert!(pick(&settings(), &drones, &running, &demand).is_empty());
    }

    #[test]
    fn settings_reject_zero_values() {
        assert!(settings().validate().is_ok());
        for bad in [
            PreemptSettings {
                after_secs: 0,
                ..settings()
            },
            PreemptSettings {
                max_per_pass: 0,
                ..settings()
            },
            PreemptSettings {
                min_victim_age_secs: 0,
                ..settings()
            },
        ] {
            assert!(bad.validate().is_err());
        }
    }
}
