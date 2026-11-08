use super::*;

#[test]
fn admission_pressure_is_the_mean_of_the_fresh_window() {
    assert_eq!(pressure_window_mean(&[], 1000, 180), None);
    // One burst among quiet samples does not cross a threshold.
    let samples = [
        (900, 50),
        (910, 60),
        (920, 559),
        (930, 40),
        (940, 50),
        (950, 41),
    ];
    assert_eq!(pressure_window_mean(&samples, 1000, 180), Some(133));
    // Sustained pressure does.
    let busy = [(900, 540), (910, 560), (920, 559), (930, 530)];
    assert_eq!(pressure_window_mean(&busy, 1000, 180), Some(547));
    // Only samples within the window count; future ones never do.
    let aged = [(700, 1000), (900, 100), (1200, 1000)];
    assert_eq!(pressure_window_mean(&aged, 1000, 180), Some(100));
    assert_eq!(pressure_window_mean(&aged, 1000, 300), Some(550));
    // Readings are clamped to permille.
    assert_eq!(pressure_window_mean(&[(1000, 5000)], 1000, 180), Some(1000));
}

#[test]
fn pr_turns_come_after_consecutive_main_grants() {
    let grant = |class, scope| crate::allocator::Grant {
        drone_id: "a".into(),
        class,
        scope,
        labels: vec![],
        mem_mib: 0,
        vcpu: 0,
    };
    let mut counts = BTreeMap::new();
    assert!(pr_turn_classes(&counts, 2).is_empty());
    advance_pr_turns(&mut counts, &[grant(Class::Heavy, Scope::Main)], 2);
    assert!(pr_turn_classes(&counts, 2).is_empty());
    advance_pr_turns(
        &mut counts,
        &[
            grant(Class::Heavy, Scope::Main),
            grant(Class::Heavy, Scope::Deploy),
        ],
        2,
    );
    assert_eq!(pr_turn_classes(&counts, 2), [Class::Heavy]);
    // Due until a pull-request grant of the class, which resets it.
    advance_pr_turns(&mut counts, &[grant(Class::Heavy, Scope::Main)], 2);
    assert_eq!(pr_turn_classes(&counts, 2), [Class::Heavy]);
    advance_pr_turns(
        &mut counts,
        &[
            grant(Class::Heavy, Scope::Pr),
            grant(Class::Heavy, Scope::Main),
        ],
        2,
    );
    assert!(pr_turn_classes(&counts, 2).is_empty());
    assert_eq!(pr_turn_classes(&counts, 0), Class::ALL);
}

#[test]
fn jit_labels_list_platform_labels_first_then_class_labels() {
    let class = vec!["borg-ci-medium".to_owned(), "borg-ci-medium-pr".to_owned()];
    assert_eq!(
        jit_labels("linux", "x86_64", &[], &class),
        [
            "self-hosted",
            "Linux",
            "X64",
            "borg-ci-medium",
            "borg-ci-medium-pr"
        ]
    );
    assert_eq!(
        jit_labels("macos", "aarch64", &[], &class[..1]),
        ["self-hosted", "macOS", "ARM64", "borg-ci-medium"]
    );
    assert_eq!(jit_labels("plan9", "mips", &[], &[]), ["self-hosted"]);
    assert_eq!(
        jit_labels("linux", "x86_64", &["on-prem".to_owned()], &class[..1]),
        ["self-hosted", "Linux", "X64", "on-prem", "borg-ci-medium"],
        "extra labels sit between the platform and class labels"
    );
}

fn lease_row(id: &str, runner: &str) -> LeaseRow {
    LeaseRow {
        id: id.to_owned(),
        drone: "host-a".to_owned(),
        class: Class::Medium,
        scope: Scope::Pr,
        labels: Vec::new(),
        runner_name: runner.to_owned(),
        runner_id: 1,
        jit_config: None,
        state: LeaseStatus::Running,
        created_at: 0,
        expires_at: 0,
    }
}

fn job(id: u64, status: &str, runner: &str) -> WorkflowJob {
    serde_json::from_value(serde_json::json!({
        "id": id, "run_id": 77, "status": status, "created_at": "2026-01-01T00:00:00Z",
        "started_at": "2026-01-01T00:10:00Z", "runner_name": runner,
    }))
    .unwrap()
}

fn runner(name: &str, status: &str, labels: &[&str]) -> Runner {
    serde_json::from_value(serde_json::json!({
        "id": 1, "name": name, "status": status, "busy": false,
        "labels": labels.iter().map(|l| serde_json::json!({"name": l})).collect::<Vec<_>>(),
    }))
    .unwrap()
}

#[test]
fn offline_static_runners_are_tracked_until_they_return() {
    let mut seen = BTreeMap::new();
    let mut fleet = vec![
        runner(
            "gha-heavy-pve60-1",
            "offline",
            &["self-hosted", "Linux", "ci-heavy"],
        ),
        runner("gha-heavy-pve60-2", "online", &["ci-heavy"]),
        runner("borg-host-a-l1", "offline", &["ci-heavy"]),
    ];
    let first = offline_static_runners(&fleet, "borg-", 100, &mut seen);
    assert_eq!(
        first,
        [OfflineRunner {
            name: "gha-heavy-pve60-1".to_owned(),
            labels: vec!["ci-heavy".to_owned()],
            offline_since: 100,
        }]
    );
    fleet.push(runner("wsl-a", "offline", &[]));
    let second = offline_static_runners(&fleet, "borg-", 160, &mut seen);
    let since: Vec<_> = second
        .iter()
        .map(|o| (o.name.as_str(), o.offline_since))
        .collect();
    assert_eq!(since, [("gha-heavy-pve60-1", 100), ("wsl-a", 160)]);
    fleet.remove(0);
    fleet.push(runner("gha-heavy-pve60-1", "online", &[]));
    let third = offline_static_runners(&fleet, "borg-", 200, &mut seen);
    assert_eq!(third.len(), 1);
    fleet[3] = runner("gha-heavy-pve60-1", "offline", &[]);
    let again = offline_static_runners(&fleet, "borg-", 300, &mut seen);
    let back = again
        .iter()
        .find(|o| o.name == "gha-heavy-pve60-1")
        .unwrap();
    assert_eq!(
        back.offline_since, 300,
        "a runner that came back starts over"
    );
}

#[test]
fn only_leases_with_an_in_progress_job_on_their_runner_are_preemptible() {
    let active = [
        lease_row("l1", "borg-host-a-l1"),
        lease_row("l2", "borg-host-a-l2"),
        lease_row("l3", "borg-host-a-l3"),
    ];
    let observed = [
        ("example-org/app", job(5, "in_progress", "borg-host-a-l1")),
        ("example-org/app", job(6, "queued", "borg-host-a-l2")),
    ];
    let preempted = HashSet::from(["l1"]);
    let (running, others, jobs) = split_running(&active, &[], &observed, &preempted);
    assert_eq!(running.len(), 1);
    assert_eq!(running[0].lease_id, "l1");
    assert_eq!(running[0].started_at, 1_767_226_200);
    assert!(running[0].preempted);
    assert_eq!(others.len(), 2);
    assert_eq!(jobs["l1"], ("example-org/app", 77, 5));
}

#[test]
fn the_queued_family_picks_the_owned_host_that_runs_it_fastest() {
    use crate::allocator::{Demand, PrTurn, allocate};
    use crate::bid::ScoreWeights;
    use crate::profile::FamilyStat;

    // Live 11/07 medians (secs) against the fleet median.
    let profile = |drone: &str, speed: u32, families: &[(&str, u64, u64)]| ClassProfile {
        drone: drone.into(),
        class: Class::Medium,
        jobs: 500,
        speed_permille: speed,
        speed_confidence_permille: 1000,
        reliability_permille: 1000,
        families: families
            .iter()
            .map(|&(family, median_secs, fleet_median_secs)| FamilyStat {
                family: family.into(),
                jobs: 20,
                median_secs,
                fleet_median_secs,
            })
            .collect(),
    };
    let profiles = vec![
        profile(
            "build-medium-1",
            633,
            &[("platform-ui-quality", 1779, 1288), ("live-gate", 368, 402)],
        ),
        profile(
            "wsl-desktop",
            748,
            &[("platform-ui-quality", 1121, 1288), ("live-gate", 500, 402)],
        ),
    ];
    let host = |id: &str| DroneState {
        id: id.into(),
        vcpu: 8,
        mem_mib: 16 * 1024,
        docker: true,
        accepting: true,
        max_leases: None,
        bids: vec![],
        profiles: vec![],
        blocked_classes: vec![],
        trust: crate::classes::Trust::Shared,
        cpu_psi_pm: None,
        cpu_load_pm: None,
        cost_permille: crate::allocator::OWNED_COST_PERMILLE,
        live: crate::allocator::LiveFacts::default(),
    };
    let winner = |queued: &[(&str, u32)], family_aware: bool| {
        let mut by_key = BTreeMap::new();
        by_key.insert(
            (Class::Medium, Scope::Pr),
            queued
                .iter()
                .map(|&(f, n)| (f.to_owned(), n))
                .collect::<QueuedFamilies>(),
        );
        let families = families_by_class(&by_key);
        let drones = ["build-medium-1", "wsl-desktop"]
            .into_iter()
            .map(|id| DroneState {
                profiles: placement_profiles(&profiles, id, &families, family_aware),
                ..host(id)
            })
            .collect();
        let a = allocate(&AllocatorInput {
            classes: crate::classes::Classes::default(),
            demand: vec![Demand {
                class: Class::Medium,
                scope: Scope::Pr,
                count: 1,
            }],
            drones,
            leases: vec![],
            warm_floors: vec![],
            queue_ages: vec![],
            starvation_after_secs: None,
            weights: ScoreWeights::default(),
            admission: None,
            pr_turn: PrTurn::default(),
        });
        a.grants[0].drone_id.clone()
    };
    // The class profile alone always prefers wsl-desktop (748 > 633).
    assert_eq!(winner(&[("live-gate", 2)], false), "wsl-desktop");
    // live-gate runs faster on build-medium-1 (1092) than on wsl (804).
    assert_eq!(winner(&[("live-gate", 2)], true), "build-medium-1");
    // platform-ui-quality runs faster on wsl (1148 against 724).
    assert_eq!(winner(&[("platform-ui-quality", 2)], true), "wsl-desktop");
    // An unseen family falls back to the class profiles.
    assert_eq!(winner(&[("brand-new", 2)], true), "wsl-desktop");
}
