//! Which held leases cover queued demand: undelivered leases a drone's
//! heartbeat withholds, and the standbys to start.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::allocator::{
    ActiveLease, DroneState, LeaseState, Promotion, ServedAge, StandbyStarts, Unmet, free_room,
    free_slots, max_slots,
};
use crate::classes::{Class, Classes, Scope, disk_labels_mib};
use crate::probe::Capacity;
use crate::store::{LeaseRow, LeaseStatus};

/// A lease row as the allocator sees it; its disk labels set its floor.
pub(super) fn as_active(l: &LeaseRow) -> ActiveLease {
    ActiveLease {
        drone_id: l.drone.clone(),
        class: l.class,
        scope: l.scope,
        state: match l.state {
            LeaseStatus::Running => LeaseState::Running,
            LeaseStatus::Standby => LeaseState::Standby,
            _ => LeaseState::Pending,
        },
        disk_mib: disk_labels_mib(&l.labels),
    }
}

/// A lease as placement sees it. A pending lease not yet delivered to a
/// drone whose heartbeat withholds it (`withheld`) can take no job until that
/// ends: it keeps its room but covers no queued demand, so a healthy drone
/// gets the job instead.
pub(super) fn as_placed(l: &LeaseRow, withheld: &HashMap<String, Vec<Class>>) -> ActiveLease {
    let mut a = as_active(l);
    if a.state == LeaseState::Pending
        && l.jit_config.is_some()
        && withheld
            .get(&l.drone)
            .is_some_and(|classes| classes.contains(&l.class))
    {
        a.state = LeaseState::Held;
    }
    a
}

/// The standby leases to start now: per promoted class, the oldest standby
/// leases on fresh drones that accept work of that class (not one in its
/// host-lost cool-down or below the class's disk floor, which the heartbeat
/// would not hand the start to), and never more on a drone than its class
/// cap has room to start ([`StandbyStarts`]) given everything it holds in
/// `held` (the allocator's leases, units included and this tick's grants)
/// among the accepting `drones`: a cap lowered below the
/// standbys a drone already holds leaves the extra ones unstarted.
pub(super) fn promotions(
    promote: &[Promotion],
    active: &[LeaseRow],
    held: &[ActiveLease],
    fresh: &HashMap<String, Capacity>,
    drones: &[DroneState],
) -> BTreeSet<String> {
    let accepting: BTreeMap<&str, &DroneState> = drones
        .iter()
        .filter(|d| d.accepting)
        .map(|d| (d.id.as_str(), d))
        .collect();
    let mut starts = StandbyStarts::new(held);
    let mut out = BTreeSet::new();
    for p in promote {
        let mut standby: Vec<(&LeaseRow, &DroneState)> = active
            .iter()
            .filter(|l| {
                l.state == LeaseStatus::Standby
                    && l.scope == Scope::Deploy
                    && l.class == p.class
                    && fresh.contains_key(&l.drone)
            })
            .filter_map(|l| {
                let drone = *accepting.get(l.drone.as_str())?;
                (!drone.blocked_classes.contains(&l.class)).then_some((l, drone))
            })
            .collect();
        standby.sort_by(|(a, _), (b, _)| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        out.extend(
            standby
                .into_iter()
                .filter(|(l, drone)| starts.take(drone, l.class))
                .take(usize::try_from(p.count).unwrap_or(usize::MAX))
                .map(|(l, _)| l.id.clone()),
        );
    }
    out
}

/// Whether `drone` could run deploy work of a class this pass left unmet: it
/// serves deploy work and the class fits it when empty. Such a drone holds no
/// new standby while that work waits; its room goes to the queued job.
pub(super) fn awaits_deploy(drone: &DroneState, unmet: &[Unmet], classes: &Classes) -> bool {
    drone.trust.serves_deploy()
        && unmet.iter().any(|u| {
            u.scope == Scope::Deploy
                && !drone.blocked_classes.contains(&u.class)
                && max_slots(drone, u.class, classes.spec(u.class)) > 0
        })
}

/// The drones topped up with standbys this pass: those serving deploy work
/// that no unmet deploy work could use ([`awaits_deploy`]).
pub(super) fn standby_hosts(
    serves_deploy: &BTreeSet<String>,
    drones: &[DroneState],
    unmet: &[Unmet],
    classes: &Classes,
) -> BTreeSet<String> {
    serves_deploy
        .iter()
        .filter(|id| {
            drones
                .iter()
                .find(|d| &d.id == *id)
                .is_none_or(|d| !awaits_deploy(d, unmet, classes))
        })
        .cloned()
        .collect()
}

/// How long ago borg last served each key, from `last` (updated in place). A
/// lease still pending or running counts as served at its creation, so the
/// first pass after a queen restart already knows what the fleet runs.
pub(super) fn served_ages(
    last: &mut BTreeMap<(Class, Scope), i64>,
    now: i64,
    active: &[LeaseRow],
) -> Vec<ServedAge> {
    for l in active
        .iter()
        .filter(|l| matches!(l.state, LeaseStatus::Pending | LeaseStatus::Running))
    {
        let at = last.entry((l.class, l.scope)).or_insert(l.created_at);
        *at = (*at).max(l.created_at);
    }
    last.iter()
        .map(|(&(class, scope), &at)| ServedAge {
            class,
            scope,
            since_granted_secs: u64::try_from(now.saturating_sub(at)).unwrap_or(0),
        })
        .collect()
}

/// Standby leases to end so deploy work of another class gets their room.
///
/// A standby is a warm deploy runner for a class with nothing queued: it runs
/// nothing but holds its class's room. On 2026-11-22 borg-deploy-1 (30 GiB)
/// ran one heavy publish (20 GiB) and held a medium standby (8 GiB), so the
/// light-deploy IaC apply (2 GiB) did not fit and waited behind heavy builds
/// for hours. When deploy work of class `k` is unmet after a pass, a deploy
/// drone with no room for `k` that would have room once its idle standbys of
/// other classes are gone ends just enough of them, largest first; the next
/// pass places `k` there. Kept: standbys of a class with queued deploy work
/// (`queued`, they are being promoted) and any whose runner `idle` says has
/// come online or taken a job. Nothing is ended where it would not make room.
/// `held` is every lease after this pass's grants; it is updated in place.
pub(super) fn standbys_to_yield(
    unmet: &[Unmet],
    queued: &BTreeSet<Class>,
    active: &[LeaseRow],
    drones: &[DroneState],
    held: &mut Vec<ActiveLease>,
    classes: &Classes,
    idle: impl Fn(&LeaseRow) -> bool,
) -> Vec<(String, Class)> {
    // Jobs per class still to make room for: one release covers the slots
    // it opens, so one waiting job never empties several drones.
    let mut left: BTreeMap<Class, u32> = BTreeMap::new();
    for u in unmet.iter().filter(|u| u.scope == Scope::Deploy) {
        *left.entry(u.class).or_default() += u.count;
    }
    let slots = |drone: &DroneState, held: &[ActiveLease], k: Class| {
        if drone.disk_admits(held, k) {
            free_slots(drone, &free_room(drone, held, classes), k, classes.spec(k))
        } else {
            0
        }
    };
    let mut ended: Vec<(String, Class)> = Vec::new();
    for drone in drones.iter().filter(|d| d.trust.serves_deploy()) {
        for (&k, want) in &mut left {
            if *want == 0
                || drone.blocked_classes.contains(&k)
                || max_slots(drone, k, classes.spec(k)) == 0
                || slots(drone, held, k) > 0
            {
                continue;
            }
            let mut spare: Vec<&LeaseRow> = active
                .iter()
                .filter(|l| {
                    l.state == LeaseStatus::Standby
                        && l.scope == Scope::Deploy
                        && l.drone == drone.id
                        && l.class != k
                        && !queued.contains(&l.class)
                        && !ended.iter().any(|(id, _)| id == &l.id)
                        && idle(l)
                })
                .collect();
            spare.sort_by(|a, b| (b.class, &a.id).cmp(&(a.class, &b.id)));
            let mut trial = held.clone();
            let mut take = Vec::new();
            for l in spare {
                let Some(i) = trial.iter().position(|h| {
                    h.drone_id == l.drone
                        && h.class == l.class
                        && h.scope == Scope::Deploy
                        && h.state == LeaseState::Standby
                }) else {
                    continue;
                };
                trial.remove(i);
                take.push((l.id.clone(), k));
                let opened = slots(drone, &trial, k);
                if opened > 0 {
                    *held = trial;
                    ended.append(&mut take);
                    *want = want.saturating_sub(opened);
                    break;
                }
            }
        }
    }
    ended
}

/// `unmet` less one deploy job per standby released for its class (`freed`).
pub(super) fn less_freed(unmet: &[Unmet], freed: &[Class]) -> Vec<Unmet> {
    unmet
        .iter()
        .filter_map(|u| {
            let n = freed
                .iter()
                .filter(|&&c| u.scope == Scope::Deploy && c == u.class)
                .count();
            let count = u.count.saturating_sub(u32::try_from(n).unwrap_or(u32::MAX));
            (count > 0).then_some(Unmet { count, ..*u })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allocator::{LiveFacts, OWNED_COST_PERMILLE};
    use crate::classes::{ClassSpec, Trust};

    fn classes() -> Classes {
        let spec = |mem_mib, vcpu, docker| ClassSpec {
            mem_mib,
            vcpu,
            docker,
        };
        Classes {
            light: spec(2048, 1, false),
            medium: spec(8192, 2, true),
            heavy: spec(20480, 4, true),
            ..Classes::default()
        }
    }

    /// borg-deploy-1 on 2026-11-22.
    fn deploy_1() -> DroneState {
        DroneState {
            id: "borg-deploy-1".into(),
            vcpu: 8,
            mem_mib: 30_045,
            docker: true,
            accepting: true,
            max_leases: None,
            class_caps: BTreeMap::new(),
            bids: vec![],
            profiles: vec![],
            blocked_classes: vec![],
            trust: Trust::DeployOnly,
            cpu_psi_pm: None,
            cpu_load_pm: None,
            cost_permille: OWNED_COST_PERMILLE,
            live: LiveFacts::default(),
            work_free_mib: None,
            disk_floor_mib: BTreeMap::new(),
        }
    }

    fn row(id: &str, class: Class, state: LeaseStatus) -> LeaseRow {
        LeaseRow {
            id: id.into(),
            drone: "borg-deploy-1".into(),
            class,
            scope: Scope::Deploy,
            labels: vec![],
            runner_name: format!("borg-borg-deploy-1-{id}"),
            runner_id: 1,
            jit_config: None,
            state,
            created_at: 0,
            expires_at: i64::MAX,
        }
    }

    fn unmet(class: Class) -> Unmet {
        Unmet {
            class,
            scope: Scope::Deploy,
            count: 1,
        }
    }

    /// A heavy cap lowered to the one heavy lease already running: the heavy
    /// standby minted before that is not started. With room under the cap,
    /// the oldest standbys are, up to the room.
    #[test]
    fn promotions_never_start_a_standby_past_its_drones_class_cap() {
        let capacity = Capacity {
            hostname: "h".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            kind: crate::probe::HostKind::Wsl,
            vcpu: 8,
            mem_mib: 30_045,
            free_disk_mib: None,
            docker_usable: true,
            mise_version: None,
            git_version: None,
        };
        let fresh = HashMap::from([("borg-deploy-1".to_owned(), capacity)]);
        let active = vec![
            row("heavy", Class::Heavy, LeaseStatus::Running),
            row("s1", Class::Heavy, LeaseStatus::Standby),
            row("s2", Class::Heavy, LeaseStatus::Standby),
        ];
        let promote = [Promotion {
            class: Class::Heavy,
            count: 2,
        }];
        let started = |cap: Option<u32>| {
            let mut drone = deploy_1();
            drone.class_caps = cap.map(|n| (Class::Heavy, n)).into_iter().collect();
            let held: Vec<ActiveLease> = active.iter().map(as_active).collect();
            promotions(&promote, &active, &held, &fresh, &[drone])
        };
        assert_eq!(started(Some(1)), BTreeSet::new());
        assert_eq!(started(Some(0)), BTreeSet::new());
        assert_eq!(started(Some(2)), BTreeSet::from(["s1".to_owned()]));
        assert_eq!(
            started(None),
            BTreeSet::from(["s1".to_owned(), "s2".to_owned()])
        );
    }

    /// A work unit fills host A's medium cap; A holds the older standby and
    /// host B a newer one with room. The one start goes to B: the units the
    /// allocator counted are counted here too.
    #[test]
    fn promotions_count_work_units_against_the_class_cap() {
        let capacity = || Capacity {
            hostname: "h".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            kind: crate::probe::HostKind::Wsl,
            vcpu: 8,
            mem_mib: 30_045,
            free_disk_mib: None,
            docker_usable: true,
            mise_version: None,
            git_version: None,
        };
        let on = |drone: &str, id: &str, created_at: i64| LeaseRow {
            drone: drone.into(),
            created_at,
            ..row(id, Class::Medium, LeaseStatus::Standby)
        };
        let active = vec![on("a", "older", 1), on("b", "newer", 2)];
        let fresh = HashMap::from([("a".to_owned(), capacity()), ("b".to_owned(), capacity())]);
        let drone = |id: &str| {
            let mut d = deploy_1();
            d.id = id.into();
            d.class_caps = [(Class::Medium, 1)].into();
            d
        };
        let drones = [drone("a"), drone("b")];
        let mut held: Vec<ActiveLease> = active.iter().map(as_active).collect();
        held.push(ActiveLease {
            drone_id: "a".into(),
            class: Class::Medium,
            scope: Scope::Deploy,
            state: LeaseState::Held,
            disk_mib: None,
        });
        let promote = [Promotion {
            class: Class::Medium,
            count: 1,
        }];
        assert_eq!(
            promotions(&promote, &active, &held, &fresh, &drones),
            BTreeSet::from(["newer".to_owned()])
        );
    }

    /// The live shape: one heavy publish running and a medium standby leave
    /// 1373 MiB, short of a light lease. The standby is ended, which makes
    /// room for the light-deploy job; the running build is never touched.
    #[test]
    fn an_idle_standby_yields_its_room_to_unmet_light_deploy_work() {
        let active = vec![
            row("heavy", Class::Heavy, LeaseStatus::Running),
            row("standby", Class::Medium, LeaseStatus::Standby),
        ];
        let mut held: Vec<ActiveLease> = active.iter().map(as_active).collect();
        let drones = [deploy_1()];
        let ended = standbys_to_yield(
            &[unmet(Class::Light)],
            &BTreeSet::from([Class::Heavy, Class::Light]),
            &active,
            &drones,
            &mut held,
            &classes(),
            |_| true,
        );
        assert_eq!(ended, [("standby".to_owned(), Class::Light)]);
        assert_eq!(held.len(), 1);
        let room = free_room(&drones[0], &held, &classes());
        assert!(
            free_slots(
                &drones[0],
                &room,
                Class::Light,
                classes().spec(Class::Light)
            ) > 0
        );
        // With the standby gone the drone no longer holds one for medium
        // while light deploy work is unmet.
        assert!(awaits_deploy(
            &drones[0],
            &[unmet(Class::Light)],
            &classes()
        ));
    }

    #[test]
    fn standbys_are_kept_when_they_cover_queued_work_or_would_not_make_room() {
        let drones = [deploy_1()];
        let standby = || {
            vec![
                row("heavy", Class::Heavy, LeaseStatus::Running),
                row("standby", Class::Medium, LeaseStatus::Standby),
            ]
        };
        let run = |active: &[LeaseRow], unmet: &[Unmet], queued: &[Class], idle: bool| {
            let mut held: Vec<ActiveLease> = active.iter().map(as_active).collect();
            standbys_to_yield(
                unmet,
                &queued.iter().copied().collect(),
                active,
                &drones,
                &mut held,
                &classes(),
                |_| idle,
            )
        };
        // Medium deploy work is queued: its standby is being promoted.
        assert!(run(&standby(), &[unmet(Class::Light)], &[Class::Medium], true).is_empty());
        // The standby's runner came online: the drone may have started it.
        assert!(run(&standby(), &[unmet(Class::Light)], &[], false).is_empty());
        // Nothing deploy is unmet.
        assert!(run(&standby(), &[], &[], true).is_empty());
        // Heavy is unmet, but even without the standby a second heavy does not
        // fit beside the running one: ending it would only cost a re-mint.
        assert!(run(&standby(), &[unmet(Class::Heavy)], &[], true).is_empty());
        // Light already fits: nothing to end.
        let roomy = vec![row("standby", Class::Medium, LeaseStatus::Standby)];
        assert!(run(&roomy, &[unmet(Class::Light)], &[], true).is_empty());
        // A standby of the unmet class itself is not ended for it.
        let own = vec![
            row("heavy", Class::Heavy, LeaseStatus::Running),
            row("standby", Class::Light, LeaseStatus::Standby),
            row("medium", Class::Medium, LeaseStatus::Running),
        ];
        assert!(run(&own, &[unmet(Class::Light)], &[], true).is_empty());
    }

    #[test]
    fn a_drone_that_cannot_run_the_unmet_class_holds_its_standbys() {
        let mut shared = deploy_1();
        shared.trust = Trust::Shared;
        assert!(!awaits_deploy(&shared, &[unmet(Class::Light)], &classes()));
        let mut no_docker = deploy_1();
        no_docker.docker = false;
        assert!(!awaits_deploy(
            &no_docker,
            &[unmet(Class::Heavy)],
            &classes()
        ));
        assert!(awaits_deploy(
            &no_docker,
            &[unmet(Class::Light)],
            &classes()
        ));
    }

    #[test]
    fn one_waiting_job_empties_one_standby_not_every_drone() {
        let mut d2 = deploy_1();
        d2.id = "borg-deploy-2".into();
        let drones = [deploy_1(), d2];
        let on = |drone: &str, id: &str, class, state| LeaseRow {
            drone: drone.into(),
            ..row(id, class, state)
        };
        let active = vec![
            on("borg-deploy-1", "h1", Class::Heavy, LeaseStatus::Running),
            on("borg-deploy-1", "s1", Class::Medium, LeaseStatus::Standby),
            on("borg-deploy-2", "h2", Class::Heavy, LeaseStatus::Running),
            on("borg-deploy-2", "s2", Class::Medium, LeaseStatus::Standby),
        ];
        let mut held: Vec<ActiveLease> = active.iter().map(as_active).collect();
        let ended = standbys_to_yield(
            &[unmet(Class::Light)],
            &BTreeSet::new(),
            &active,
            &drones,
            &mut held,
            &classes(),
            |_| true,
        );
        assert_eq!(ended.len(), 1, "{ended:?}");
        assert_eq!(held.len(), 3);
        // Preemption no longer counts the job the release made room for.
        let freed: Vec<Class> = ended.iter().map(|(_, c)| *c).collect();
        assert!(less_freed(&[unmet(Class::Light)], &freed).is_empty());
    }

    #[test]
    fn a_standby_holding_the_disk_floor_yields_it() {
        let mut drone = deploy_1();
        drone.work_free_mib = Some(15 * 1024);
        drone.disk_floor_mib = [(Class::Light, 10 * 1024), (Class::Medium, 10 * 1024)].into();
        let drones = [drone];
        let active = vec![row("standby", Class::Medium, LeaseStatus::Standby)];
        let run = |drones: &[DroneState]| {
            let mut held: Vec<ActiveLease> = active.iter().map(as_active).collect();
            standbys_to_yield(
                &[unmet(Class::Light)],
                &BTreeSet::new(),
                &active,
                drones,
                &mut held,
                &classes(),
                |_| true,
            )
        };
        // Memory and CPU fit light beside the standby; its disk floor does not.
        assert_eq!(run(&drones).len(), 1);
        // Too little disk even without the standby: it is kept.
        let mut tight = drones[0].clone();
        tight.work_free_mib = Some(8 * 1024);
        assert!(run(&[tight]).is_empty());
    }
}
