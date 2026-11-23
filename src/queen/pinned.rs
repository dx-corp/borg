//! Queued jobs that ask for a label only some drones add to their runners
//! (`extra_labels` in the drone config, for example a high-memory host's
//! `highmem`).
//!
//! GitHub hands a runner a job only when the runner carries every label the
//! job asks for. The allocator places demand by class alone, so a pinned job
//! counted as ordinary class demand gets its runner minted on whichever drone
//! scores best, which then cannot take it: the job waits until a lease happens
//! to land on a drone with the label, and every runner minted for it elsewhere
//! idles or takes some other job. Pinned demand is therefore split out of the
//! class demand and placed on the drones whose labels cover it.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use time::OffsetDateTime;

use crate::allocator::{
    ActiveLease, AllocatorInput, Demand, Grant, LeaseState, PlacementDecision, QueueAge,
    Reservation, allocate_reserved,
};
use crate::classes::{Class, Classes, disk_labels_mib, parse_disk_label};
use crate::github::QueuedJob;

/// `Queen::summarize` of some jobs: its queue, demand, ages and families.
type Summary<Q, F> = (Q, Vec<Demand>, Vec<QueueAge>, F);

/// The full label set a JIT runner is registered with: the labels GitHub
/// assigns to every self-hosted runner, then the grant's class labels. They
/// are sent explicitly because the API documents only "custom" labels.
pub fn jit_labels(os: &str, arch: &str, extra: &[String], class_labels: &[String]) -> Vec<String> {
    let mut labels = vec!["self-hosted".to_owned()];
    match os {
        "linux" => labels.push("Linux".to_owned()),
        "macos" => labels.push("macOS".to_owned()),
        "windows" => labels.push("Windows".to_owned()),
        _ => {}
    }
    match arch {
        "x86_64" | "x86" => labels.push("X64".to_owned()),
        "aarch64" | "arm64" => labels.push("ARM64".to_owned()),
        "arm" => labels.push("ARM".to_owned()),
        _ => {}
    }
    labels.extend(extra.iter().cloned());
    labels.extend(class_labels.iter().cloned());
    labels
}

/// Split pinned jobs out of the class `demand` and `ages` summarized from
/// every job: the class demand and ages of the rest, and the pinned groups.
pub(super) fn split_demand<Q, F>(
    jobs: &[QueuedJob],
    classes: &Classes,
    extra_labels: &HashMap<String, Vec<String>>,
    (demand, ages): (Vec<Demand>, Vec<QueueAge>),
    now: OffsetDateTime,
    summarize: impl Fn(&[QueuedJob]) -> Summary<Q, F>,
) -> (Vec<Demand>, Vec<QueueAge>, BTreeMap<PinKey, PinnedGroup>) {
    let (unpinned, groups) = split(jobs, classes, extra_labels);
    if groups.is_empty() {
        return (demand, ages, groups);
    }
    tracing::info!("pinned demand: {}", describe(&groups, now));
    let (_, demand, ages, _) = summarize(&unpinned);
    (demand, ages, groups)
}

/// Place each pinned group with the allocator over the drones that carry its
/// labels. Runs after deploy and unit claims and before class demand: the
/// grants are added to `input` as running leases, which hold room in the class
/// pass but never count as cover for class demand. Pinned demand left unmet is
/// not burst or wake demand: a new VM would not carry the label.
pub(super) fn place<Q, F>(
    groups: &BTreeMap<PinKey, PinnedGroup>,
    input: &mut AllocatorInput,
    reserved: &[Reservation],
    summarize: impl Fn(&[QueuedJob]) -> Summary<Q, F>,
) -> (Vec<Grant>, Vec<PlacementDecision>) {
    let mut grants = Vec::new();
    let mut decisions = Vec::new();
    for group in groups.values() {
        let (_, demand, ages, _) = summarize(&group.jobs);
        let mut sub = input.clone();
        sub.drones.retain(|d| group.drones.contains(&d.id));
        // A disk label raises every class's floor on these drones for this
        // group; the allocator charges it per grant.
        if let Some(gib) = group.disk_gib {
            let need = gib << 10;
            // Only an idle runner minted for this disk floor carries the
            // label these jobs ask for. Any other pending runner, and every
            // deploy standby, cannot take them: it holds its room and disk
            // without covering them, so they get a runner of their own.
            for l in &mut sub.leases {
                if matches!(l.state, LeaseState::Pending | LeaseState::Standby)
                    && l.disk_mib != Some(need)
                {
                    l.state = LeaseState::Held;
                }
            }
            for d in &mut sub.drones {
                for class in Class::ALL {
                    let floor = d.disk_floor_mib.entry(class).or_default();
                    *floor = (*floor).max(need);
                }
            }
        }
        sub.demand = demand;
        sub.queue_ages = ages;
        sub.warm_floors = Vec::new();
        let mut placed = allocate_reserved(&sub, reserved);
        // The runner carries the job's disk labels as the job spells them
        // (GitHub matches labels by name, case-insensitively), so GitHub
        // hands it the job.
        for g in &mut placed.grants {
            g.labels.extend(group.disk_labels.iter().cloned());
        }
        let held: Vec<ActiveLease> = placed
            .grants
            .iter()
            .map(|g| ActiveLease {
                drone_id: g.drone_id.clone(),
                class: g.class,
                scope: g.scope,
                // Not started: holds its room and the group's disk floor,
                // in this tick's class pass and, from its labels, after.
                state: LeaseState::Held,
                disk_mib: disk_labels_mib(&g.labels),
            })
            .collect();
        super::spend_bids(&mut input.drones, &held);
        input.leases.extend(held);
        grants.extend(placed.grants);
        decisions.extend(placed.decisions);
    }
    (grants, decisions)
}

/// The labels a pinned group asks for (lower case, as GitHub compares them).
pub(super) type PinKey = BTreeSet<String>;

/// Split `jobs` into the ones class demand covers and, per label set, the
/// ones pinned to the drones carrying those labels.
///
/// A job is pinned by the labels it asks for that some fresh drone offers in
/// `extra_labels` (already sanitized: never a class label). A label no drone
/// offers pins nothing (`self-hosted`, `Linux`, `X64`, or a static runner's
/// own lane label), so such jobs keep today's class placement. A pinned job
/// whose labels no single drone covers is dropped from demand: no runner
/// borg can mint would be handed it.
// formal: begin pinned_labels/check_pinned.eligible
pub(super) fn split(
    jobs: &[QueuedJob],
    classes: &Classes,
    extra_labels: &HashMap<String, Vec<String>>,
) -> (Vec<QueuedJob>, BTreeMap<PinKey, PinnedGroup>) {
    let carried: BTreeMap<&str, BTreeSet<String>> = extra_labels
        .iter()
        .map(|(drone, labels)| {
            (
                drone.as_str(),
                labels.iter().map(|l| l.to_ascii_lowercase()).collect(),
            )
        })
        .collect();
    let offered: BTreeSet<&String> = carried.values().flatten().collect();
    let mut unpinned = Vec::new();
    let mut groups: BTreeMap<PinKey, PinnedGroup> = BTreeMap::new();
    for job in jobs {
        let wants: PinKey = job
            .labels
            .iter()
            .filter(|l| classes.parse_label(l).is_none() && parse_disk_label(l).is_none())
            .map(|l| l.to_ascii_lowercase())
            .filter(|l| offered.contains(l))
            .collect();
        // A disk label is never offered: it pins the job to no drone, only
        // to a disk floor (see `place`).
        let disk_labels: BTreeSet<String> = job
            .labels
            .iter()
            .filter(|l| parse_disk_label(l).is_some())
            .map(|l| l.to_ascii_lowercase())
            .collect();
        let disk_gib = disk_labels.iter().filter_map(|l| parse_disk_label(l)).max();
        if wants.is_empty() && disk_gib.is_none() {
            unpinned.push(job.clone());
            continue;
        }
        let mut key = wants.clone();
        key.extend(disk_labels.iter().cloned());
        let group = groups.entry(key).or_insert_with(|| PinnedGroup {
            drones: carried
                .iter()
                .filter(|(_, has)| wants.is_subset(has))
                .map(|(drone, _)| (*drone).to_owned())
                .collect(),
            disk_gib,
            disk_labels: disk_labels.clone(),
            jobs: Vec::new(),
        });
        group.jobs.push(job.clone());
    }
    (unpinned, groups)
}
// formal: end pinned_labels/check_pinned.eligible

/// Jobs pinned to one label set and the drones that can serve them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PinnedGroup {
    /// Fresh drones whose runners carry every label of the group.
    pub(super) drones: BTreeSet<String>,
    /// Free work disk the group's jobs ask for (`borg-disk-<N>g`), GiB: the
    /// largest of their disk labels.
    pub(super) disk_gib: Option<u64>,
    /// The disk labels the group's jobs carry, lower case. Their runners
    /// carry every one, since GitHub requires each label a job names.
    pub(super) disk_labels: BTreeSet<String>,
    pub(super) jobs: Vec<QueuedJob>,
}

/// One line per group for the tick log: labels, drones, queued jobs.
pub(super) fn describe(groups: &BTreeMap<PinKey, PinnedGroup>, now: OffsetDateTime) -> String {
    groups
        .iter()
        .map(|(labels, g)| {
            let oldest = g
                .jobs
                .iter()
                .map(|j| (now - j.created_at).whole_seconds().max(0))
                .max()
                .unwrap_or(0);
            format!(
                "[{}] on {{{}}}: {} queued, oldest {oldest}s",
                labels.iter().cloned().collect::<Vec<_>>().join(","),
                g.drones.iter().cloned().collect::<Vec<_>>().join(","),
                g.jobs.len(),
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classes::disk_label;

    fn job(id: u64, labels: &[&str]) -> QueuedJob {
        QueuedJob {
            id,
            repo: "dx-corp/mono".to_owned(),
            name: format!("job {id}"),
            labels: labels.iter().map(|l| (*l).to_owned()).collect(),
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn fleet() -> HashMap<String, Vec<String>> {
        HashMap::from([
            ("heavy-1".to_owned(), vec!["highmem".to_owned()]),
            ("vm-1".to_owned(), vec!["on-prem".to_owned()]),
            (
                "vm-2".to_owned(),
                vec!["on-prem".to_owned(), "highmem".to_owned()],
            ),
            ("plain".to_owned(), vec![]),
        ])
    }

    #[test]
    fn class_only_jobs_and_unoffered_labels_stay_class_demand() {
        let jobs = [
            job(1, &["ci-heavy"]),
            job(2, &["self-hosted", "Linux", "X64", "ci-medium-pr"]),
            job(3, &["internal-arc", "some-static-lane"]),
        ];
        let (unpinned, groups) = split(&jobs, &Classes::default(), &fleet());
        assert_eq!(unpinned, jobs);
        assert!(groups.is_empty());
    }

    #[test]
    fn an_offered_label_pins_the_job_to_the_drones_that_carry_it() {
        let jobs = [
            job(1, &["ci-heavy", "HighMem"]),
            job(2, &["ci-heavy"]),
            job(3, &["ci-heavy", "highmem"]),
        ];
        let (unpinned, groups) = split(&jobs, &Classes::default(), &fleet());
        assert_eq!(unpinned, [jobs[1].clone()]);
        let key: PinKey = ["highmem".to_owned()].into();
        let g = &groups[&key];
        assert_eq!(
            g.drones,
            ["heavy-1".to_owned(), "vm-2".to_owned()]
                .into_iter()
                .collect()
        );
        assert_eq!(g.jobs, [jobs[0].clone(), jobs[2].clone()]);
    }

    #[test]
    fn several_labels_need_one_drone_carrying_all_of_them() {
        let jobs = [job(1, &["ci-medium", "on-prem", "highmem"])];
        let (_, groups) = split(&jobs, &Classes::default(), &fleet());
        let g = groups.values().next().unwrap();
        assert_eq!(g.drones, ["vm-2".to_owned()].into_iter().collect());

        let mut no_single = fleet();
        no_single.remove("vm-2");
        let (unpinned, groups) = split(&jobs, &Classes::default(), &no_single);
        assert!(unpinned.is_empty(), "no runner borg mints could take it");
        assert!(groups.values().next().unwrap().drones.is_empty());
    }

    fn drone(id: &str, work_free_gib: u64) -> crate::allocator::DroneState {
        let mut d: crate::allocator::DroneState = serde_json::from_value(serde_json::json!({
            "id": id, "vcpu": 16, "mem_mib": 28672, "docker": true,
            "accepting": true, "max_leases": null,
        }))
        .unwrap();
        d.work_free_mib = Some(work_free_gib << 10);
        // The queen's floors: 10 GiB for light and medium, 32 GiB for heavy.
        d.disk_floor_mib = [
            (Class::Light, 10 << 10),
            (Class::Medium, 10 << 10),
            (Class::Heavy, 32 << 10),
        ]
        .into_iter()
        .collect();
        d
    }

    fn placed(jobs: &[QueuedJob], drones: Vec<crate::allocator::DroneState>) -> Vec<Grant> {
        placed_with(jobs, drones, Vec::new())
    }

    fn placed_with(
        jobs: &[QueuedJob],
        drones: Vec<crate::allocator::DroneState>,
        leases: Vec<ActiveLease>,
    ) -> Vec<Grant> {
        let classes = Classes::default();
        let extra: HashMap<String, Vec<String>> =
            drones.iter().map(|d| (d.id.clone(), Vec::new())).collect();
        let (unpinned, groups) = split(jobs, &classes, &extra);
        let summarize = |jobs: &[QueuedJob]| {
            let demand: Vec<Demand> = jobs
                .iter()
                .filter_map(|j| j.labels.iter().find_map(|l| classes.parse_label(l)))
                .map(|(class, scope)| Demand {
                    class,
                    scope,
                    count: 1,
                })
                .collect();
            ((), demand, Vec::new(), ())
        };
        let mut input = AllocatorInput {
            classes: classes.clone(),
            demand: summarize(&unpinned).1,
            drones,
            leases,
            warm_floors: Vec::new(),
            queue_ages: Vec::new(),
            served: Vec::new(),
            starvation_after_secs: None,
            weights: crate::bid::ScoreWeights::default(),
            admission: None,
            pr_turn: crate::allocator::PrTurn::default(),
            larger_hold: crate::allocator::LargerHold::default(),
        };
        let (mut grants, _) = place(&groups, &mut input, &[], summarize);
        grants.extend(allocate_reserved(&input, &[]).grants);
        grants
    }

    #[test]
    fn a_disk_label_is_never_offered_and_groups_the_job_on_every_drone() {
        let jobs = [
            job(1, &["borg-ci-medium-pr", "borg-disk-32g"]),
            job(2, &["borg-ci-medium-pr"]),
        ];
        let (unpinned, groups) = split(&jobs, &Classes::default(), &fleet());
        assert_eq!(unpinned, [jobs[1].clone()]);
        let g = &groups[&PinKey::from(["borg-disk-32g".to_owned()])];
        assert_eq!(g.disk_gib, Some(32));
        assert_eq!(g.drones.len(), fleet().len());
        // A drone cannot advertise it as one of its own labels.
        let labels = Classes::default()
            .sanitize_extra_labels(&["borg-disk-32g".to_owned(), "highmem".to_owned()]);
        assert_eq!(labels, ["highmem"]);
    }

    #[test]
    fn a_32g_medium_job_goes_only_where_32_gib_of_work_disk_is_free() {
        let jobs = [job(1, &["borg-ci-medium-pr", "borg-disk-32g"])];
        // 15 GiB free: medium's own 10 GiB floor would admit it; 32g refuses.
        assert!(placed(&jobs, vec![drone("small", 15)]).is_empty());
        let grants = placed(&jobs, vec![drone("small", 15), drone("big", 69)]);
        assert_eq!(grants.len(), 1, "{grants:?}");
        assert_eq!(grants[0].drone_id, "big");
        assert!(grants[0].labels.contains(&"borg-disk-32g".to_owned()));
        assert!(grants[0].labels.contains(&"borg-ci-medium-pr".to_owned()));
    }

    fn active(drone: &str, labels: &[&str], state: LeaseState) -> ActiveLease {
        let labels: Vec<String> = labels.iter().map(|l| (*l).to_owned()).collect();
        let (class, scope) = labels
            .iter()
            .find_map(|l| Classes::default().parse_label(l))
            .unwrap();
        ActiveLease {
            drone_id: drone.into(),
            class,
            scope,
            state,
            disk_mib: disk_labels_mib(&labels),
        }
    }

    #[test]
    fn a_labelled_runner_still_holds_its_disk_on_later_ticks() {
        // A 32g runner minted on an earlier tick and not yet running holds 32
        // GiB, not medium's 10: of 69 GiB, 37 are left, room for one more.
        let job32 = |id| job(id, &["borg-ci-medium-pr", "borg-disk-32g"]);
        // Running: its writes are in the reading already, so both new jobs
        // fit in 69 GiB.
        // Memory and vCPU for four medium leases, so disk is what binds.
        let mut big = drone("big", 69);
        big.mem_mib = 48 * 1024;
        big.vcpu = 16;
        let held = |state| {
            placed_with(
                &[job32(1), job32(2)],
                vec![big.clone()],
                vec![active(
                    "big",
                    &["borg-ci-medium-pr", "borg-disk-32g"],
                    state,
                )],
            )
        };
        assert_eq!(held(LeaseState::Running).len(), 2);
        // Pending (an earlier tick's grant, read back from its labels): it
        // still holds 32 GiB, so only one more fits.
        assert_eq!(held(LeaseState::Held).len(), 1);
        assert_eq!(held(LeaseState::Pending).len(), 1);
    }

    #[test]
    fn unlabelled_runners_and_standbys_never_cover_a_disk_job() {
        let job32 = [job(1, &["borg-ci-medium-pr", "borg-disk-32g"])];
        for cover in [
            active("big", &["borg-ci-medium-pr"], LeaseState::Pending),
            active("big", &["borg-ci-medium-deploy"], LeaseState::Standby),
        ] {
            let grants = placed_with(&job32, vec![drone("big", 69)], vec![cover.clone()]);
            assert_eq!(grants.len(), 1, "{cover:?} covered it: {grants:?}");
            assert!(grants[0].labels.contains(&"borg-disk-32g".to_owned()));
        }
        // An idle runner minted for the same floor does cover it.
        let same = active(
            "big",
            &["borg-ci-medium-pr", "borg-disk-32g"],
            LeaseState::Pending,
        );
        assert!(placed_with(&job32, vec![drone("big", 69)], vec![same]).is_empty());
    }

    #[test]
    fn the_runner_carries_every_disk_label_as_the_job_spells_it() {
        // GitHub matches each label a job names by name, so a runner labelled
        // only `borg-disk-32g` could not take a job asking `borg-disk-032g`.
        let jobs = [job(
            1,
            &["borg-ci-medium-pr", "borg-disk-032g", "BORG-DISK-16G"],
        )];
        let grants = placed(&jobs, vec![drone("small", 20), drone("big", 69)]);
        assert_eq!(grants.len(), 1, "{grants:?}");
        assert_eq!(
            grants[0].drone_id, "big",
            "the largest label sets the floor"
        );
        for l in ["borg-disk-032g", "borg-disk-16g"] {
            assert!(grants[0].labels.contains(&l.to_owned()), "{l}: {grants:?}");
        }
    }

    #[test]
    fn ordinary_medium_jobs_place_as_before() {
        let jobs = [job(1, &["borg-ci-medium-pr"])];
        let grants = placed(&jobs, vec![drone("small", 15)]);
        assert_eq!(grants.len(), 1);
        assert!(!grants[0].labels.iter().any(|l| l.starts_with("borg-disk-")));
    }

    #[test]
    fn disk_labels_parse_strictly() {
        assert_eq!(parse_disk_label("borg-disk-32g"), Some(32));
        assert_eq!(parse_disk_label("BORG-DISK-8G"), Some(8));
        for bad in [
            "borg-disk-0g",
            "borg-disk-g",
            "borg-disk-32",
            "borg-disk--1g",
            "borg-disk-9999g",
        ] {
            assert_eq!(parse_disk_label(bad), None, "{bad}");
        }
        assert_eq!(disk_label(32), "borg-disk-32g");
    }
}
