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
use crate::classes::Classes;
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
        sub.demand = demand;
        sub.queue_ages = ages;
        sub.warm_floors = Vec::new();
        let placed = allocate_reserved(&sub, reserved);
        let held: Vec<ActiveLease> = placed
            .grants
            .iter()
            .map(|g| ActiveLease {
                drone_id: g.drone_id.clone(),
                class: g.class,
                scope: g.scope,
                state: LeaseState::Running,
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
            .filter(|l| classes.parse_label(l).is_none())
            .map(|l| l.to_ascii_lowercase())
            .filter(|l| offered.contains(l))
            .collect();
        if wants.is_empty() {
            unpinned.push(job.clone());
            continue;
        }
        let group = groups.entry(wants.clone()).or_insert_with(|| PinnedGroup {
            drones: carried
                .iter()
                .filter(|(_, has)| wants.is_subset(has))
                .map(|(drone, _)| (*drone).to_owned())
                .collect(),
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
}
