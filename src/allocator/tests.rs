use super::*;

/// 28 GiB host with 16 vCPU and Docker, already net of the OS reserve.
fn host(id: &str) -> DroneState {
    DroneState {
        id: id.into(),
        vcpu: 16,
        mem_mib: 28 * 1024,
        docker: true,
        accepting: true,
        max_leases: None,
        bids: vec![],
        profiles: vec![],
        blocked_classes: vec![],
        cpu_psi_pm: None,
        cpu_load_pm: None,
        trust: Trust::Shared,
        cost_permille: OWNED_COST_PERMILLE,
    }
}

fn input(demand: &[(Class, Scope, u32)], drones: Vec<DroneState>) -> AllocatorInput {
    AllocatorInput {
        classes: Classes::default(),
        demand: demand
            .iter()
            .map(|&(class, scope, count)| Demand {
                class,
                scope,
                count,
            })
            .collect(),
        drones,
        leases: vec![],
        warm_floors: vec![],
        queue_ages: vec![],
        starvation_after_secs: None,
        weights: ScoreWeights::default(),
        admission: None,
        pr_turn: PrTurn::default(),
    }
}

fn count(a: &Allocation, class: Class, scope: Scope) -> usize {
    a.grants
        .iter()
        .filter(|g| g.class == class && g.scope == scope)
        .count()
}

fn lease(drone: &str, class: Class, scope: Scope, state: LeaseState) -> ActiveLease {
    ActiveLease {
        drone_id: drone.into(),
        class,
        scope,
        state,
    }
}

fn assert_within_budget(i: &AllocatorInput, a: &Allocation) {
    for d in &i.drones {
        let spec = |c| i.classes.spec(c);
        let mut mem = 0;
        let mut vcpu = 0;
        let mut n = 0u32;
        for l in i.leases.iter().filter(|l| l.drone_id == d.id) {
            mem += spec(l.class).mem_mib;
            vcpu += spec(l.class).vcpu;
            n += 1;
        }
        for g in a.grants.iter().filter(|g| g.drone_id == d.id) {
            mem += g.mem_mib;
            vcpu += g.vcpu;
            n += 1;
            assert!(d.accepting, "grant to non-accepting drone");
            assert!(
                d.docker || !spec(g.class).docker,
                "docker-less drone got docker class"
            );
        }
        if a.grants.iter().any(|g| g.drone_id == d.id) {
            assert!(
                mem <= d.mem_mib,
                "{} over-granted memory: {mem} > {}",
                d.id,
                d.mem_mib
            );
            assert!(
                vcpu <= d.vcpu,
                "{} over-granted vcpu: {vcpu} > {}",
                d.id,
                d.vcpu
            );
            if let Some(cap) = d.max_leases {
                assert!(n <= cap, "{} over lease cap", d.id);
            }
        }
    }
}

fn profile(class: Class, speed: u32) -> ProfileScore {
    ProfileScore {
        class,
        speed_permille: speed,
        speed_confidence_permille: 1000,
        reliability_permille: 1000,
    }
}

#[test]
fn main_heavy_prefers_fast_idle_drones_and_slow_or_loaded_ones_take_overflow() {
    let fast = DroneState {
        profiles: vec![profile(Class::Heavy, 1100)],
        ..host("fast")
    };
    // Ids sort before "fast", so ties would go to them.
    // As fast, but its host is under CPU pressure right now.
    let loaded = DroneState {
        cpu_psi_pm: Some(450),
        profiles: vec![profile(Class::Heavy, 1100)],
        ..host("a-loaded")
    };
    // Idle, but historically half as fast.
    let slow = DroneState {
        profiles: vec![profile(Class::Heavy, 550)],
        ..host("0-slow")
    };
    let drones = vec![loaded.clone(), slow.clone(), fast.clone()];
    let first = |n| {
        let a = allocate(&input(&[(Class::Heavy, Scope::Main, n)], drones.clone()));
        a.grants
            .iter()
            .map(|g| g.drone_id.clone())
            .collect::<Vec<_>>()
    };
    // One default heavy fits per 28 GiB host. Overflow goes to the less
    // loaded of the rest; the historically slow host comes last.
    assert_eq!(first(1), ["fast"]);
    assert_eq!(first(2), ["fast", "a-loaded"]);
    assert_eq!(first(3), ["fast", "a-loaded", "0-slow"]);

    // Same speed: the idle host wins medium/heavy, light still packs.
    let busy = host("busy");
    let idle = host("idle");
    let mut i = input(
        &[(Class::Medium, Scope::Main, 1)],
        vec![busy.clone(), idle.clone()],
    );
    i.leases = vec![lease(
        "busy",
        Class::Medium,
        Scope::Main,
        LeaseState::Running,
    )];
    assert_eq!(allocate(&i).grants[0].drone_id, "idle");
    i.demand = vec![Demand {
        class: Class::Light,
        scope: Scope::Main,
        count: 1,
    }];
    assert_eq!(allocate(&i).grants[0].drone_id, "busy");

    let w = ScoreWeights::default();
    assert_eq!(host_term(&loaded, 16, Class::Heavy, &w), -450 + 250);
    assert_eq!(host_term(&slow, 8, Class::Heavy, &w), 125);
    assert_eq!(host_term(&loaded, 16, Class::Light, &w), -450);
}

#[test]
fn load_estimate_approximates_waiting_share_and_ignores_idle_cpus() {
    assert_eq!(load_pressure_pm(None, Some(1000)), None);
    assert_eq!(load_pressure_pm(Some(800), None), Some(0));
    assert_eq!(load_pressure_pm(Some(1000), Some(1000)), Some(0));
    // Load 93 on 16 CPUs, CPUs saturated: ~83% of runnable tasks wait.
    assert_eq!(load_pressure_pm(Some(5838), Some(990)), Some(828));
    assert_eq!(load_pressure_pm(Some(2000), None), Some(500));
    // Same run queue with idle CPU time is not CPU contention.
    assert_eq!(load_pressure_pm(Some(5838), Some(550)), Some(0));
}

#[test]
fn light_only_sheds_pr_and_main_medium_heavy_but_keeps_light_main_and_deploy() {
    let t = AdmissionThresholds {
        psi_soft_pm: 300,
        psi_light_only_pm: 500,
        psi_hard_pm: 600,
    };
    let of = |p| AdmissionState::of(Some(p), Some(t));
    assert_eq!(of(299), AdmissionState::Open);
    assert_eq!(of(300), AdmissionState::PrBlocked);
    assert_eq!(of(540), AdmissionState::LightOnly);
    assert_eq!(of(600), AdmissionState::Closed);
    let s = AdmissionState::LightOnly;
    assert!(s.admits(Class::Light, Scope::Main));
    assert!(s.admits(Class::Heavy, Scope::Deploy));
    assert!(!s.admits(Class::Medium, Scope::Main));
    assert!(!s.admits(Class::Heavy, Scope::Main));
    assert!(!s.admits(Class::Light, Scope::Pr));
    // Unset light-only tier (== hard): PR-blocked straight to closed.
    let old = AdmissionThresholds {
        psi_light_only_pm: 600,
        ..t
    };
    assert_eq!(
        AdmissionState::of(Some(540), Some(old)),
        AdmissionState::PrBlocked
    );

    // A loaded drone without PSI: its load estimate drives admission.
    let loaded = DroneState {
        cpu_load_pm: Some(828),
        ..host("loaded")
    };
    let mut i = input(
        &[
            (Class::Heavy, Scope::Main, 1),
            (Class::Medium, Scope::Main, 1),
            (Class::Light, Scope::Main, 1),
        ],
        vec![loaded.clone()],
    );
    i.admission = Some(t);
    let a = allocate(&i);
    assert_eq!(a.admission[0].state, AdmissionState::Closed);
    assert!(a.grants.is_empty());

    let busy = DroneState {
        cpu_psi_pm: Some(540),
        ..loaded
    };
    i.drones = vec![busy];
    let a = allocate(&i);
    assert_eq!(
        a.admission[0].state,
        AdmissionState::LightOnly,
        "PSI wins over the load estimate"
    );
    assert_eq!(count(&a, Class::Light, Scope::Main), 1);
    assert_eq!(count(&a, Class::Medium, Scope::Main), 0);
    assert_eq!(count(&a, Class::Heavy, Scope::Main), 0);
    let rejected: Vec<_> = a
        .decisions
        .iter()
        .filter(|d| d.class == Class::Heavy)
        .flat_map(|d| {
            d.candidates
                .iter()
                .flat_map(|c| c.rejection_reasons.clone())
        })
        .collect();
    assert!(
        rejected.contains(&Rejection::LightOnlyPressure),
        "{rejected:?}"
    );
}

#[test]
fn free_room_and_slots_subtract_what_the_drone_already_holds() {
    let classes = Classes::default();
    let d = host("a"); // 28 GiB, 16 vCPU, docker
    let held = vec![
        lease("a", Class::Heavy, Scope::Pr, LeaseState::Running),
        lease("b", Class::Heavy, Scope::Pr, LeaseState::Running),
    ];
    let room = free_room(&d, &held, &classes);
    let heavy = classes.spec(Class::Heavy);
    assert_eq!(room.mem_mib, d.mem_mib - heavy.mem_mib);
    assert_eq!(room.vcpu, d.vcpu - heavy.vcpu);
    // Theoretical: one heavy fits an empty host; with one held, none do.
    let empty = free_room(&d, &[], &classes);
    assert_eq!(free_slots(&d, &empty, heavy), max_slots(&d, heavy));
    assert_eq!(free_slots(&d, &room, heavy), 0);
    assert!(free_slots(&d, &room, classes.spec(Class::Light)) > 0);
    // A draining drone and a docker-less one have no free slots.
    let draining = DroneState {
        accepting: false,
        ..host("c")
    };
    assert_eq!(free_slots(&draining, &empty, heavy), 0);
    let nodocker = DroneState {
        docker: false,
        ..host("e")
    };
    assert_eq!(
        free_slots(&nodocker, &empty, classes.spec(Class::Medium)),
        0
    );
    assert!(free_slots(&nodocker, &empty, classes.spec(Class::Light)) > 0);
    // The lease cap counts too.
    let capped = DroneState {
        max_leases: Some(1),
        ..host("f")
    };
    let one = vec![lease("f", Class::Light, Scope::Pr, LeaseState::Running)];
    assert_eq!(
        free_slots(
            &capped,
            &free_room(&capped, &one, &classes),
            classes.spec(Class::Light)
        ),
        0
    );
}

#[test]
fn no_demand_no_floor_no_grants() {
    let a = allocate(&input(&[], vec![host("a")]));
    assert!(a.grants.is_empty() && a.unmet.is_empty() && a.reserved_for.is_empty());
    assert!(a.pressure.iter().all(|p| p.pressure == 0));
}

#[test]
fn warm_floor_alone_grants_exactly_the_floor() {
    let mut i = input(&[], vec![host("a")]);
    i.warm_floors = default_warm_floors();
    let a = allocate(&i);
    assert_eq!(a.grants.len(), 1);
    assert_eq!(
        (a.grants[0].class, a.grants[0].scope),
        (Class::Medium, Scope::Main)
    );
    assert_eq!(a.grants[0].labels, ["borg-ci-medium"]);
    assert!(a.unmet.is_empty(), "an unfilled floor is not unmet demand");
}

#[test]
fn warm_floor_is_satisfied_by_an_existing_pending_lease() {
    let mut i = input(&[], vec![host("a")]);
    i.warm_floors = default_warm_floors();
    i.leases = vec![lease("a", Class::Medium, Scope::Main, LeaseState::Pending)];
    assert!(allocate(&i).grants.is_empty());
}

#[test]
fn running_lease_does_not_satisfy_the_floor() {
    let mut i = input(&[], vec![host("a")]);
    i.warm_floors = default_warm_floors();
    i.leases = vec![lease("a", Class::Medium, Scope::Main, LeaseState::Running)];
    assert_eq!(allocate(&i).grants.len(), 1);
}

#[test]
fn floor_and_queued_demand_do_not_double_count() {
    let mut i = input(&[(Class::Medium, Scope::Main, 1)], vec![host("a")]);
    i.warm_floors = default_warm_floors();
    assert_eq!(allocate(&i).grants.len(), 1);
}

#[test]
fn main_demand_is_placed_before_pr_demand_when_room_is_short() {
    // Room for exactly one heavy; main must win over pr.
    let i = input(
        &[(Class::Heavy, Scope::Pr, 1), (Class::Heavy, Scope::Main, 1)],
        vec![host("a")],
    );
    let a = allocate(&i);
    assert_eq!(a.grants.len(), 1);
    assert_eq!(a.grants[0].scope, Scope::Main);
    assert_eq!(
        a.unmet,
        [Unmet {
            class: Class::Heavy,
            scope: Scope::Pr,
            count: 1
        }]
    );
}

#[test]
fn small_main_beats_large_pr() {
    // 24 GiB budget: main medium x2 fills it, leaving no room for pr heavy.
    let mut h = host("a");
    h.mem_mib = 24 * 1024;
    let i = input(
        &[
            (Class::Heavy, Scope::Pr, 1),
            (Class::Medium, Scope::Main, 2),
        ],
        vec![h],
    );
    let a = allocate(&i);
    assert_eq!(count(&a, Class::Medium, Scope::Main), 2);
    assert_eq!(count(&a, Class::Heavy, Scope::Pr), 0);
}

#[test]
fn main_grants_carry_only_the_bare_label() {
    let a = allocate(&input(&[(Class::Light, Scope::Main, 1)], vec![host("a")]));
    assert_eq!(a.grants[0].labels, ["borg-ci-light"]);
}

#[test]
fn pr_grants_carry_both_labels() {
    let a = allocate(&input(&[(Class::Light, Scope::Pr, 1)], vec![host("a")]));
    assert_eq!(a.grants[0].labels, ["borg-ci-light", "borg-ci-light-pr"]);
}

#[test]
fn labels_follow_the_configured_prefix() {
    let mut i = input(&[(Class::Light, Scope::Pr, 1)], vec![host("a")]);
    i.classes.label_prefix = "example-ci-".into();
    assert_eq!(
        allocate(&i).grants[0].labels,
        ["example-ci-light", "example-ci-light-pr"]
    );
}

#[test]
fn carves_one_28gib_host_into_one_heavy() {
    let i = input(&[(Class::Heavy, Scope::Main, 3)], vec![host("a")]);
    let a = allocate(&i);
    assert_eq!(count(&a, Class::Heavy, Scope::Main), 1);
    assert_eq!(
        a.unmet,
        [Unmet {
            class: Class::Heavy,
            scope: Scope::Main,
            count: 2
        }]
    );
    assert_within_budget(&i, &a);
}

#[test]
fn carves_one_28gib_host_into_two_medium() {
    let i = input(&[(Class::Medium, Scope::Pr, 5)], vec![host("a")]);
    let a = allocate(&i);
    assert_eq!(count(&a, Class::Medium, Scope::Pr), 2);
    assert_within_budget(&i, &a);
}

#[test]
fn carves_one_28gib_host_into_eight_light() {
    // Memory alone would allow 11; the 16 vCPU budget caps it at 8.
    let i = input(&[(Class::Light, Scope::Pr, 20)], vec![host("a")]);
    let a = allocate(&i);
    assert_eq!(count(&a, Class::Light, Scope::Pr), 8);
    assert_within_budget(&i, &a);
}

#[test]
fn mixed_demand_fills_remaining_room_after_the_big_class() {
    let i = input(
        &[
            (Class::Light, Scope::Main, 4),
            (Class::Heavy, Scope::Main, 1),
        ],
        vec![host("a")],
    );
    let a = allocate(&i);
    assert_eq!(count(&a, Class::Heavy, Scope::Main), 1);
    // 28 - 24 = 4 GiB left: one light (2.5 GiB) fits.
    assert_eq!(count(&a, Class::Light, Scope::Main), 1);
    assert_within_budget(&i, &a);
}

#[test]
fn in_flight_leases_reduce_available_room() {
    let mut i = input(&[(Class::Medium, Scope::Main, 2)], vec![host("a")]);
    i.leases = vec![lease("a", Class::Medium, Scope::Pr, LeaseState::Running)];
    let a = allocate(&i);
    assert_eq!(a.grants.len(), 1);
    assert_within_budget(&i, &a);
}

#[test]
fn pending_leases_absorb_queued_demand_of_the_same_label() {
    let mut i = input(&[(Class::Light, Scope::Main, 2)], vec![host("a")]);
    i.leases = vec![lease("a", Class::Light, Scope::Main, LeaseState::Pending)];
    assert_eq!(allocate(&i).grants.len(), 1);
}

#[test]
fn pending_main_lease_cannot_serve_pr_demand() {
    let mut i = input(&[(Class::Light, Scope::Pr, 1)], vec![host("a")]);
    i.leases = vec![lease("a", Class::Light, Scope::Main, LeaseState::Pending)];
    assert_eq!(allocate(&i).grants.len(), 1);
}

#[test]
fn pending_pr_lease_can_serve_main_demand() {
    let mut i = input(&[(Class::Light, Scope::Main, 1)], vec![host("a")]);
    i.leases = vec![lease("a", Class::Light, Scope::Pr, LeaseState::Pending)];
    assert!(allocate(&i).grants.is_empty());
}

#[test]
fn a_pending_pr_lease_is_not_counted_twice() {
    let mut i = input(
        &[(Class::Light, Scope::Main, 1), (Class::Light, Scope::Pr, 1)],
        vec![host("a")],
    );
    i.leases = vec![lease("a", Class::Light, Scope::Pr, LeaseState::Pending)];
    let a = allocate(&i);
    assert_eq!(a.grants.len(), 1);
    assert_eq!(a.grants[0].scope, Scope::Pr);
}

#[test]
fn host_lease_cap_limits_grants() {
    let mut h = host("a");
    h.max_leases = Some(3);
    let i = input(&[(Class::Light, Scope::Main, 8)], vec![h]);
    let a = allocate(&i);
    assert_eq!(a.grants.len(), 3);
    assert_within_budget(&i, &a);
}

#[test]
fn host_lease_cap_counts_in_flight_leases() {
    let mut h = host("a");
    h.max_leases = Some(2);
    let mut i = input(&[(Class::Light, Scope::Main, 8)], vec![h]);
    i.leases = vec![lease("a", Class::Light, Scope::Pr, LeaseState::Running)];
    assert_eq!(allocate(&i).grants.len(), 1);
}

#[test]
fn owner_caps_shrink_the_budget() {
    let capacity = Capacity {
        hostname: "h".into(),
        os: "linux".into(),
        arch: "x86_64".into(),
        kind: crate::probe::HostKind::Wsl,
        vcpu: 20,
        mem_mib: 64 * 1024,
        free_disk_mib: None,
        docker_usable: true,
        mise_version: None,
        git_version: None,
    };
    let caps = HostCaps {
        max_vcpu: Some(8),
        max_mem_mib: Some(24 * 1024),
        ..HostCaps::default()
    };
    let d = DroneState::from_capacity("pc", &capacity, &caps);
    assert_eq!((d.vcpu, d.mem_mib), (8, 24 * 1024));

    let i = input(&[(Class::Light, Scope::Main, 20)], vec![d]);
    let a = allocate(&i);
    assert_eq!(a.grants.len(), 4); // 8 vCPU / 2
    assert_within_budget(&i, &a);
}

#[test]
fn reserve_is_subtracted_from_total_memory() {
    let capacity = Capacity {
        hostname: "h".into(),
        os: "linux".into(),
        arch: "x86_64".into(),
        kind: crate::probe::HostKind::Bare,
        vcpu: 16,
        mem_mib: 28 * 1024,
        free_disk_mib: None,
        docker_usable: true,
        mise_version: None,
        git_version: None,
    };
    let d = DroneState::from_capacity("a", &capacity, &HostCaps::default());
    assert_eq!(d.mem_mib, 26 * 1024);
    // Still fits one heavy.
    let a = allocate(&input(&[(Class::Heavy, Scope::Main, 1)], vec![d]));
    assert_eq!(a.grants.len(), 1);
}

#[test]
fn docker_classes_skip_hosts_without_docker() {
    let mut nodocker = host("a");
    nodocker.docker = false;
    let i = input(
        &[
            (Class::Medium, Scope::Main, 1),
            (Class::Light, Scope::Main, 1),
        ],
        vec![nodocker, host("b")],
    );
    let a = allocate(&i);
    let medium = a.grants.iter().find(|g| g.class == Class::Medium).unwrap();
    assert_eq!(medium.drone_id, "b");
    assert_within_budget(&i, &a);
}

#[test]
fn draining_drones_get_nothing() {
    let mut h = host("a");
    h.accepting = false;
    let a = allocate(&input(&[(Class::Light, Scope::Main, 2)], vec![h]));
    assert!(a.grants.is_empty());
    assert_eq!(
        a.unmet,
        [Unmet {
            class: Class::Light,
            scope: Scope::Main,
            count: 2
        }]
    );
}

#[test]
fn spreads_over_hosts_when_one_is_full_and_prefers_the_tightest_fit() {
    let mut small = host("small");
    small.mem_mib = 13 * 1024;
    let i = input(&[(Class::Medium, Scope::Main, 3)], vec![host("big"), small]);
    let a = allocate(&i);
    assert_eq!(a.grants.len(), 3);
    // The first medium goes to the tighter 13 GiB host.
    assert_eq!(a.grants[0].drone_id, "small");
    assert_within_budget(&i, &a);
}

#[test]
fn leases_on_unknown_drones_are_ignored() {
    let mut i = input(&[(Class::Light, Scope::Main, 1)], vec![host("a")]);
    i.leases = vec![lease(
        "gone",
        Class::Heavy,
        Scope::Main,
        LeaseState::Pending,
    )];
    assert_eq!(allocate(&i).grants.len(), 1);
}

#[test]
fn duplicate_demand_entries_are_summed() {
    let i = input(
        &[
            (Class::Light, Scope::Main, 1),
            (Class::Light, Scope::Main, 2),
        ],
        vec![host("a")],
    );
    assert_eq!(allocate(&i).grants.len(), 3);
}

#[test]
fn allocation_is_deterministic() {
    let i = input(
        &[
            (Class::Light, Scope::Pr, 5),
            (Class::Medium, Scope::Main, 1),
        ],
        vec![host("b"), host("a")],
    );
    assert_eq!(allocate(&i), allocate(&i));
}

fn aged(i: &mut AllocatorInput, class: Class, scope: Scope, secs: u64) {
    i.queue_ages.push(QueueAge {
        class,
        scope,
        oldest_age_secs: secs,
    });
    i.starvation_after_secs = Some(120);
}

/// Two 28 GiB drones, each too full for a heavy lease; `a` carries more
/// leases than `b`.
fn crowded() -> AllocatorInput {
    let mut i = input(
        &[
            (Class::Heavy, Scope::Pr, 1),
            (Class::Light, Scope::Main, 10),
        ],
        vec![host("a"), host("b")],
    );
    i.leases = vec![
        lease("a", Class::Medium, Scope::Pr, LeaseState::Running),
        lease("a", Class::Light, Scope::Pr, LeaseState::Running),
        lease("b", Class::Medium, Scope::Pr, LeaseState::Running),
    ];
    i
}

#[test]
fn starving_heavy_reserves_the_drone_with_fewest_leases() {
    let mut i = crowded();
    aged(&mut i, Class::Heavy, Scope::Pr, 300);
    let a = allocate(&i);
    assert_eq!(
        a.reserved_for,
        [Reservation {
            drone_id: "b".into(),
            class: Class::Heavy,
            scope: Scope::Pr
        }]
    );
    assert_eq!(a.grants.iter().filter(|g| g.drone_id == "b").count(), 1);
    let mut next = i.clone();
    next.leases.retain(|l| l.drone_id != "b");
    next.leases.extend(
        a.grants
            .iter()
            .map(|g| lease(&g.drone_id, g.class, g.scope, LeaseState::Running)),
    );
    next.demand.retain(|d| d.class == Class::Heavy);
    assert_eq!(count(&allocate(&next), Class::Heavy, Scope::Pr), 1);
    assert!(
        count(&a, Class::Light, Scope::Main) > 0,
        "other drone keeps working"
    );
    assert!(a.unmet.contains(&Unmet {
        class: Class::Heavy,
        scope: Scope::Pr,
        count: 1
    }));
    assert_within_budget(&i, &a);
}

/// A running heavy lease leaves 12 GiB idle on a 26 GiB host. Six
/// 2 GiB backfills can finish independently: the next 14 GiB heavy
/// still fits as soon as the original lease ends.
fn backfill_input() -> AllocatorInput {
    let mut i = input(
        &[
            (Class::Heavy, Scope::Main, 1),
            (Class::Light, Scope::Pr, 10),
        ],
        vec![DroneState {
            mem_mib: 26 * 1024,
            vcpu: 11,
            ..host("a")
        }],
    );
    i.classes.light = ClassSpec {
        mem_mib: 2048,
        vcpu: 1,
        docker: false,
    };
    i.classes.medium = ClassSpec {
        mem_mib: 8192,
        vcpu: 2,
        docker: true,
    };
    i.classes.heavy = ClassSpec {
        mem_mib: 14336,
        vcpu: 4,
        docker: true,
    };
    i.leases = vec![lease("a", Class::Heavy, Scope::Main, LeaseState::Running)];
    aged(&mut i, Class::Heavy, Scope::Main, 300);
    i
}

/// Main has its own occupied heavy host; the other heavy-capable host
/// must drain its existing medium lease for the waiting heavy PR.
fn cross_scope_reservation_input() -> AllocatorInput {
    let mut i = input(
        &[
            (Class::Heavy, Scope::Main, 1),
            (Class::Medium, Scope::Main, 1),
            (Class::Heavy, Scope::Pr, 1),
        ],
        vec![
            DroneState {
                mem_mib: 16 * 1024,
                vcpu: 8,
                max_leases: Some(1),
                ..host("a-main")
            },
            DroneState {
                mem_mib: 16 * 1024,
                vcpu: 6,
                ..host("b-pr")
            },
        ],
    );
    i.classes.medium = ClassSpec {
        mem_mib: 8192,
        vcpu: 2,
        docker: true,
    };
    i.classes.heavy = ClassSpec {
        mem_mib: 14336,
        vcpu: 4,
        docker: true,
    };
    i.leases = vec![
        lease("a-main", Class::Heavy, Scope::Main, LeaseState::Running),
        lease("b-pr", Class::Medium, Scope::Main, LeaseState::Running),
    ];
    for (class, scope) in [
        (Class::Heavy, Scope::Main),
        (Class::Medium, Scope::Main),
        (Class::Heavy, Scope::Pr),
    ] {
        aged(&mut i, class, scope, 1200);
    }
    i
}

#[test]
fn aged_heavy_pr_reservation_precedes_smaller_main_refill() {
    let i = cross_scope_reservation_input();
    let a = allocate(&i);
    assert_eq!(
        count(&a, Class::Medium, Scope::Main),
        0,
        "a smaller Main grant must not refill the host the heavy PR needs to drain"
    );
    assert_eq!(
        a.reserved_for,
        [
            Reservation {
                drone_id: "a-main".into(),
                class: Class::Heavy,
                scope: Scope::Main,
            },
            Reservation {
                drone_id: "b-pr".into(),
                class: Class::Heavy,
                scope: Scope::Pr,
            },
        ]
    );
    assert_within_budget(&i, &a);

    // Only the PR host's existing lease finishes. The normal heavy PR
    // turn can now start, while Main retains its occupied host.
    let mut next = i;
    next.leases.retain(|l| l.drone_id != "b-pr");
    next.pr_turn = PrTurn {
        after_secs: Some(900),
        classes: vec![Class::Heavy],
    };
    let admitted = allocate(&next);
    assert!(
        admitted
            .grants
            .iter()
            .any(|g| { g.drone_id == "b-pr" && g.class == Class::Heavy && g.scope == Scope::Pr })
    );
    assert_within_budget(&next, &admitted);
}

#[test]
fn aged_heavy_pr_reservation_precedes_a_smaller_pr_turn() {
    let mut i = cross_scope_reservation_input();
    i.demand.push(Demand {
        class: Class::Medium,
        scope: Scope::Pr,
        count: 1,
    });
    aged(&mut i, Class::Medium, Scope::Pr, 1200);
    i.pr_turn = PrTurn {
        after_secs: Some(900),
        classes: vec![Class::Medium],
    };
    let a = allocate(&i);
    assert_eq!(
        count(&a, Class::Medium, Scope::Pr),
        0,
        "a smaller PR turn must preserve the waiting heavy PR's drain"
    );
    assert!(a.reserved_for.contains(&Reservation {
        drone_id: "b-pr".into(),
        class: Class::Heavy,
        scope: Scope::Pr,
    }));
    assert_within_budget(&i, &a);
}

#[test]
fn reservation_backfills_without_delaying_the_reserved_job() {
    let mut i = backfill_input();
    let a = allocate(&i);
    assert_eq!(a.reserved_for.len(), 1);
    assert_eq!(count(&a, Class::Light, Scope::Pr), 6);
    assert_within_budget(&i, &a);
    // The original heavy finishes while every backfill keeps running.
    i.leases = a
        .grants
        .iter()
        .map(|g| lease(&g.drone_id, g.class, g.scope, LeaseState::Running))
        .collect();
    i.demand.retain(|d| d.class == Class::Heavy);
    let next = allocate(&i);
    assert_eq!(count(&next, Class::Heavy, Scope::Main), 1);
    assert_within_budget(&i, &next);
}

#[test]
fn reservation_backfill_preserves_cpu_and_lease_limits() {
    for (vcpu, cap, expected) in [
        (11, None, 6),
        (5, None, 1),
        (11, Some(2), 1),
        (11, Some(1), 0),
    ] {
        let mut i = backfill_input();
        i.drones[0].vcpu = vcpu;
        i.drones[0].max_leases = cap;
        let a = allocate(&i);
        assert_eq!(
            count(&a, Class::Light, Scope::Pr),
            expected,
            "vcpu={vcpu} cap={cap:?}"
        );
        assert_within_budget(&i, &a);
    }
}

#[test]
fn reservation_with_multiple_existing_leases_does_not_backfill() {
    let mut i = backfill_input();
    i.leases
        .push(lease("a", Class::Light, Scope::Main, LeaseState::Running));
    let a = allocate(&i);
    assert_eq!(a.reserved_for.len(), 1);
    assert!(a.grants.is_empty());
}

#[test]
fn reservation_ties_break_by_drone_id() {
    let mut i = crowded();
    i.leases = vec![
        lease("b", Class::Medium, Scope::Pr, LeaseState::Running),
        lease("a", Class::Medium, Scope::Pr, LeaseState::Running),
    ];
    aged(&mut i, Class::Heavy, Scope::Pr, 300);
    assert_eq!(allocate(&i).reserved_for[0].drone_id, "a");
}

#[test]
fn young_demand_is_not_starving() {
    let mut i = crowded();
    aged(&mut i, Class::Heavy, Scope::Pr, 119);
    let a = allocate(&i);
    assert!(a.reserved_for.is_empty());
    assert!(a.grants.iter().any(|g| g.drone_id == "b"));
}

#[test]
fn guard_can_be_disabled() {
    let mut i = crowded();
    aged(&mut i, Class::Heavy, Scope::Pr, 9999);
    i.starvation_after_secs = None;
    assert!(allocate(&i).reserved_for.is_empty());
}

#[test]
fn starving_demand_that_fits_is_granted_first_without_reservation() {
    let mut i = input(
        &[
            (Class::Light, Scope::Main, 20),
            (Class::Heavy, Scope::Pr, 1),
        ],
        vec![host("a")],
    );
    aged(&mut i, Class::Heavy, Scope::Pr, 500);
    let a = allocate(&i);
    assert_eq!(count(&a, Class::Heavy, Scope::Pr), 1);
    assert!(a.reserved_for.is_empty());
    assert_within_budget(&i, &a);
}

#[test]
fn drones_that_could_never_fit_are_not_reserved() {
    let mut small = host("small");
    small.mem_mib = 8 * 1024;
    let mut nodocker = host("nodocker");
    nodocker.docker = false;
    let mut i = input(&[(Class::Heavy, Scope::Main, 1)], vec![small, nodocker]);
    aged(&mut i, Class::Heavy, Scope::Main, 500);
    let a = allocate(&i);
    assert!(a.reserved_for.is_empty());
    assert!(a.grants.is_empty());
}

#[test]
fn starving_medium_also_reserves() {
    let mut h = host("a");
    h.mem_mib = 14 * 1024;
    let mut i = input(
        &[
            (Class::Medium, Scope::Main, 1),
            (Class::Light, Scope::Pr, 5),
        ],
        vec![h],
    );
    i.leases = vec![lease("a", Class::Light, Scope::Pr, LeaseState::Running)];
    aged(&mut i, Class::Medium, Scope::Main, 200);
    let a = allocate(&i);
    assert_eq!(a.reserved_for.len(), 1);
    assert!(
        a.grants.is_empty(),
        "nothing else may land on the reserved drone"
    );
}

#[test]
fn never_over_grants_across_a_demand_sweep() {
    let drones = vec![
        host("a"),
        DroneState {
            id: "b".into(),
            vcpu: 4,
            mem_mib: 8 * 1024,
            docker: false,
            accepting: true,
            max_leases: Some(2),
            bids: vec![],
            profiles: vec![],
            blocked_classes: vec![],
            cpu_psi_pm: None,
            cpu_load_pm: None,
            trust: Trust::Shared,
            cost_permille: OWNED_COST_PERMILLE,
        },
        DroneState {
            id: "c".into(),
            vcpu: 8,
            mem_mib: 14 * 1024,
            docker: true,
            accepting: true,
            max_leases: None,
            bids: vec![],
            profiles: vec![],
            blocked_classes: vec![],
            cpu_psi_pm: None,
            cpu_load_pm: None,
            trust: Trust::Shared,
            cost_permille: OWNED_COST_PERMILLE,
        },
    ];
    for lm in 0..3 {
        for mm in 0..3 {
            for hm in 0..3 {
                for lp in [0, 4] {
                    for hp in [0, 2] {
                        let mut i = input(
                            &[
                                (Class::Light, Scope::Main, lm),
                                (Class::Medium, Scope::Main, mm),
                                (Class::Heavy, Scope::Main, hm),
                                (Class::Light, Scope::Pr, lp),
                                (Class::Heavy, Scope::Pr, hp),
                            ],
                            drones.clone(),
                        );
                        i.warm_floors = default_warm_floors();
                        i.leases = vec![lease("c", Class::Light, Scope::Pr, LeaseState::Running)];
                        assert_within_budget(&i, &allocate(&i));
                    }
                }
            }
        }
    }
}

// ---- bids -----------------------------------------------------------

use crate::bid::DroneBid;

fn bid(class: Class, scope: Scope) -> DroneBid {
    DroneBid {
        class,
        scope,
        available_slots: 8,
        estimated_start_ms: 0,
        cache_affinity: 0,
        resource_pressure: 0,
        estimated_cost: 1000,
        confidence: 1000,
        expires_at: i64::MAX,
        min_pressure: 0,
    }
}

fn bidder(id: &str, b: DroneBid) -> DroneState {
    DroneState {
        bids: vec![b],
        ..host(id)
    }
}

fn only_drone(a: &Allocation) -> &str {
    assert_eq!(a.grants.len(), 1, "{a:?}");
    &a.grants[0].drone_id
}

#[test]
fn the_warmest_bid_wins_over_the_tightest_fit() {
    let mut cold = bid(Class::Light, Scope::Pr);
    cold.cache_affinity = 100;
    let mut warm = bid(Class::Light, Scope::Pr);
    warm.cache_affinity = 900;
    // "a" is the emptier host and would lose a pure tightest-fit contest
    // only if it were fuller; give "b" a lease so tightest-fit prefers b.
    let mut i = input(
        &[(Class::Light, Scope::Pr, 1)],
        vec![bidder("a", warm), bidder("b", cold)],
    );
    i.leases = vec![lease("b", Class::Medium, Scope::Pr, LeaseState::Running)];
    let a = allocate(&i);
    assert_eq!(only_drone(&a), "a");
}

#[test]
fn loaded_slow_expensive_hosts_lose_and_ties_fall_back_to_fit_then_id() {
    let mut loaded = bid(Class::Light, Scope::Pr);
    loaded.resource_pressure = 800;
    let i = input(
        &[(Class::Light, Scope::Pr, 1)],
        vec![
            bidder("a", loaded),
            bidder("b", bid(Class::Light, Scope::Pr)),
        ],
    );
    assert_eq!(only_drone(&allocate(&i)), "b");

    // Equal bids: tightest fit (b has a lease), then id.
    let mut i = input(
        &[(Class::Light, Scope::Pr, 1)],
        vec![
            bidder("a", bid(Class::Light, Scope::Pr)),
            bidder("b", bid(Class::Light, Scope::Pr)),
        ],
    );
    i.leases = vec![lease("b", Class::Light, Scope::Pr, LeaseState::Running)];
    assert_eq!(only_drone(&allocate(&i)), "b");
    i.leases.clear();
    assert_eq!(only_drone(&allocate(&i)), "a");
}

#[test]
fn a_bidding_drone_declines_keys_it_did_not_bid_on_and_exhausted_bids() {
    let i = input(
        &[(Class::Medium, Scope::Pr, 1)],
        vec![bidder("a", bid(Class::Light, Scope::Pr))],
    );
    assert!(allocate(&i).grants.is_empty());

    let mut one = bid(Class::Light, Scope::Pr);
    one.available_slots = 2;
    let i = input(&[(Class::Light, Scope::Pr, 5)], vec![bidder("a", one)]);
    let a = allocate(&i);
    assert_eq!(count(&a, Class::Light, Scope::Pr), 2);
    assert_eq!(a.unmet[0].count, 3);
}

#[test]
fn legacy_drones_without_bids_stay_eligible_with_a_neutral_score() {
    let mut bad = bid(Class::Light, Scope::Pr);
    bad.resource_pressure = 900;
    // The legacy drone ("a", no bids) beats a heavily loaded bidder.
    let i = input(
        &[(Class::Light, Scope::Pr, 1)],
        vec![host("a"), bidder("b", bad)],
    );
    assert_eq!(only_drone(&allocate(&i)), "a");
}

#[test]
fn response_thresholds_gate_participation_on_demand_pressure() {
    let mut shy = bid(Class::Light, Scope::Pr);
    shy.min_pressure = 600;
    // One queued job against eight idle slots: low pressure, shy host out.
    let i = input(&[(Class::Light, Scope::Pr, 1)], vec![bidder("a", shy)]);
    let a = allocate(&i);
    assert!(a.grants.is_empty());
    assert!(
        a.pressure
            .iter()
            .any(|p| p.class == Class::Light && p.pressure < 600)
    );

    // Many queued jobs against the same room: pressure crosses the
    // threshold and the shy host joins.
    let i = input(&[(Class::Light, Scope::Pr, 30)], vec![bidder("a", shy)]);
    let a = allocate(&i);
    assert!(!a.grants.is_empty());
    assert_eq!(
        a.pressure
            .iter()
            .find(|p| p.class == Class::Light)
            .map(|p| p.pressure),
        Some(1000)
    );
}

#[test]
fn starving_demand_overrides_every_threshold() {
    let mut shy = bid(Class::Light, Scope::Pr);
    shy.min_pressure = 1000;
    let mut i = input(&[(Class::Light, Scope::Pr, 1)], vec![bidder("a", shy)]);
    assert!(allocate(&i).grants.is_empty());
    aged(&mut i, Class::Light, Scope::Pr, 500);
    assert_eq!(only_drone(&allocate(&i)), "a");
}

#[test]
fn a_warm_floor_is_placed_only_on_always_willing_hosts() {
    let mut shy = bid(Class::Medium, Scope::Main);
    shy.min_pressure = 200;
    let mut i = input(&[], vec![bidder("a", shy)]);
    i.warm_floors = default_warm_floors();
    assert!(allocate(&i).grants.is_empty());
    let eager = bid(Class::Medium, Scope::Main);
    i.drones = vec![bidder("b", eager)];
    assert_eq!(only_drone(&allocate(&i)), "b");
}

#[test]
fn bid_scoring_is_deterministic_and_never_over_grants() {
    let mut drones = Vec::new();
    for (n, id) in ["a", "b", "c", "d"].into_iter().enumerate() {
        let mut b = bid(Class::Light, Scope::Pr);
        b.cache_affinity = u16::try_from(n * 250).unwrap();
        b.available_slots = 3;
        drones.push(bidder(id, b));
    }
    let i = input(&[(Class::Light, Scope::Pr, 9)], drones);
    let first = allocate(&i);
    assert_eq!(first, allocate(&i));
    assert_within_budget(&i, &first);
    assert_eq!(count(&first, Class::Light, Scope::Pr), 9);
    // The warmest host fills its three slots first.
    assert_eq!(first.grants.iter().filter(|g| g.drone_id == "d").count(), 3);
}

// ---- learned profiles -------------------------------------------------

fn profiled(id: &str, class: Class, speed: u32, conf: u32, rel: u32) -> DroneState {
    DroneState {
        profiles: vec![ProfileScore {
            class,
            speed_permille: speed,
            speed_confidence_permille: conf,
            reliability_permille: rel,
        }],
        ..host(id)
    }
}

#[test]
fn a_proven_faster_host_wins_ties_that_would_go_to_the_lower_id() {
    let i = input(
        &[(Class::Light, Scope::Pr, 1)],
        vec![
            profiled("a", Class::Light, 1000, 0, 1000),
            profiled("b", Class::Light, 1600, 1000, 1000),
        ],
    );
    assert_eq!(only_drone(&allocate(&i)), "b");
}

#[test]
fn unreliable_hosts_lose_and_unproven_profiles_change_nothing() {
    let i = input(
        &[(Class::Light, Scope::Pr, 1)],
        vec![
            profiled("a", Class::Light, 1000, 1000, 700),
            profiled("b", Class::Light, 1000, 1000, 1000),
        ],
    );
    assert_eq!(only_drone(&allocate(&i)), "b");
    // Speed with no confidence is ignored: the id tie-break decides.
    let i = input(
        &[(Class::Light, Scope::Pr, 1)],
        vec![
            profiled("a", Class::Light, 1000, 0, 1000),
            profiled("b", Class::Light, 2000, 0, 1000),
        ],
    );
    assert_eq!(only_drone(&allocate(&i)), "a");
}

#[test]
fn profiles_are_per_class_and_stack_with_bids() {
    // The profile for medium does not help a light placement.
    let i = input(
        &[(Class::Light, Scope::Pr, 1)],
        vec![
            profiled("a", Class::Light, 1000, 0, 1000),
            profiled("b", Class::Medium, 2000, 1000, 1000),
        ],
    );
    assert_eq!(only_drone(&allocate(&i)), "a");
    // A bidding drone's score includes its profile.
    let mut warm = bid(Class::Light, Scope::Pr);
    warm.cache_affinity = 100;
    let fast = DroneState {
        bids: vec![bid(Class::Light, Scope::Pr)],
        ..profiled("b", Class::Light, 2000, 1000, 1000)
    };
    let i = input(
        &[(Class::Light, Scope::Pr, 1)],
        vec![bidder("a", warm), fast],
    );
    // a: 3*100 = 300; b: 1000 from the profile.
    assert_eq!(only_drone(&allocate(&i)), "b");
    assert_eq!(allocate(&i), allocate(&i));
}

/// A big, fast, idle cloud drone, as the 52 vCPU burst VMs look on 11/07.
fn burst(id: &str, class: Class, cost: u32) -> DroneState {
    DroneState {
        vcpu: 52,
        mem_mib: 100 * 1024,
        cost_permille: cost,
        ..profiled(id, class, 1600, 1000, 1000)
    }
}

#[test]
fn without_a_cost_a_faster_burst_drone_takes_work_from_an_idle_owned_host() {
    // The behaviour seen live on 11/07: no drone bids, so nothing prices
    // the burst drone and its learned speed wins every grant.
    for class in Class::ALL {
        let i = input(
            &[(class, Scope::Main, 1)],
            vec![
                profiled("build-heavy-1", class, 730, 1000, 1000),
                burst("gcp-burst-a", class, OWNED_COST_PERMILLE),
            ],
        );
        assert_eq!(only_drone(&allocate(&i)), "gcp-burst-a", "{class}");
    }
}

#[test]
fn a_costed_burst_drone_gets_only_what_owned_hosts_have_no_room_for() {
    for class in Class::ALL {
        let owned = profiled("build-heavy-1", class, 500, 1000, 1000);
        let slots = max_slots(&owned, Classes::default().spec(class));
        let i = input(
            &[(class, Scope::Main, slots + 2)],
            vec![owned, burst("gcp-burst-a", class, 5000)],
        );
        let a = allocate(&i);
        let on = |d: &str| a.grants.iter().filter(|g| g.drone_id == d).count();
        assert_eq!(on("build-heavy-1"), slots as usize, "{class}");
        assert_eq!(on("gcp-burst-a"), 2, "{class}");
        // The owned host's grants come first.
        assert!(
            a.grants[..slots as usize]
                .iter()
                .all(|g| g.drone_id == "build-heavy-1"),
            "{class}"
        );
        let d = &a.decisions[0];
        let c = |id: &str| d.candidates.iter().find(|c| c.drone_id == id).unwrap();
        assert_eq!(c("build-heavy-1").cost_score, 0);
        assert_eq!(c("gcp-burst-a").cost_score, -4000);
    }
}

#[test]
fn cost_never_overrides_eligibility() {
    // A draining owned host, or one the trust or admission rules exclude,
    // still declines: the cost only reorders eligible drones.
    let draining = DroneState {
        accepting: false,
        ..host("owned")
    };
    let i = input(
        &[(Class::Light, Scope::Pr, 1)],
        vec![draining, burst("gcp-burst-a", Class::Light, 5000)],
    );
    assert_eq!(only_drone(&allocate(&i)), "gcp-burst-a");
    let deploy_only = DroneState {
        trust: Trust::DeployOnly,
        ..host("owned")
    };
    let i = input(
        &[(Class::Light, Scope::Pr, 1)],
        vec![deploy_only, burst("gcp-burst-a", Class::Light, 5000)],
    );
    assert_eq!(only_drone(&allocate(&i)), "gcp-burst-a");
}

#[test]
fn each_decision_names_the_term_that_separated_winner_from_runner_up() {
    let reason = |drones: Vec<DroneState>, leases: Vec<ActiveLease>| {
        let mut i = input(&[(Class::Medium, Scope::Main, 1)], drones);
        i.leases = leases;
        let a = allocate(&i);
        (a.grants[0].drone_id.clone(), a.decisions[0].reason)
    };
    let r = |id: &str, why| (id.to_owned(), Some(why));
    assert_eq!(
        reason(vec![host("a")], vec![]),
        r("a", PlacementReason::OnlyEligible)
    );
    assert_eq!(
        reason(
            vec![
                burst("a-burst", Class::Medium, 5000),
                profiled("owned", Class::Medium, 700, 1000, 1000)
            ],
            vec![]
        ),
        r("owned", PlacementReason::Cost)
    );
    assert_eq!(
        reason(
            vec![
                profiled("a", Class::Medium, 800, 1000, 1000),
                profiled("b", Class::Medium, 1400, 1000, 1000)
            ],
            vec![]
        ),
        r("b", PlacementReason::Profile)
    );
    let loaded = DroneState {
        cpu_psi_pm: Some(300),
        ..host("a")
    };
    assert_eq!(
        reason(vec![loaded, host("b")], vec![]),
        r("b", PlacementReason::Host)
    );
    let mut warm = bid(Class::Medium, Scope::Main);
    warm.cache_affinity = 900;
    assert_eq!(
        reason(
            vec![
                bidder("a", bid(Class::Medium, Scope::Main)),
                bidder("b", warm)
            ],
            vec![]
        ),
        r("b", PlacementReason::Bid)
    );
    assert_eq!(
        reason(vec![host("a"), host("b")], vec![]),
        r("a", PlacementReason::Name)
    );
    // Equal scores with a light lease on b: b fits tighter. Light work
    // has no idle term, so the lease leaves the scores equal.
    let mut i = input(
        &[(Class::Light, Scope::Main, 1)],
        vec![host("a"), host("b")],
    );
    i.leases = vec![lease("b", Class::Light, Scope::Pr, LeaseState::Running)];
    let a = allocate(&i);
    assert_eq!(
        (a.grants[0].drone_id.as_str(), a.decisions[0].reason),
        ("b", Some(PlacementReason::TightestFit))
    );
    // Nothing eligible: no winner and no reason.
    let i = input(
        &[(Class::Medium, Scope::Main, 1)],
        vec![DroneState {
            accepting: false,
            ..host("a")
        }],
    );
    let a = allocate(&i);
    assert_eq!(
        (a.decisions[0].winner.as_deref(), a.decisions[0].reason),
        (None, None)
    );
}

#[test]
fn cost_term_is_zero_for_owned_and_clamped_for_hostile_values() {
    let w = ScoreWeights::default();
    assert_eq!(cost_term(&host("a"), &w), 0);
    let priced = |cost| DroneState {
        cost_permille: cost,
        ..host("a")
    };
    assert_eq!(cost_term(&priced(5000), &w), -4000);
    assert_eq!(cost_term(&priced(u32::MAX), &w), -99_000);
    // Cheaper than owned is not offered by the config, but scores higher.
    assert_eq!(cost_term(&priced(0), &w), 1000);
}

fn deploy_host(id: &str) -> DroneState {
    DroneState {
        trust: Trust::Deploy,
        ..host(id)
    }
}

#[test]
fn deploy_demand_lands_only_on_deploy_drones_with_only_the_deploy_label() {
    let a = allocate(&input(
        &[(Class::Medium, Scope::Deploy, 2)],
        vec![host("shared"), deploy_host("dep")],
    ));
    assert_eq!(count(&a, Class::Medium, Scope::Deploy), 2);
    assert!(a.grants.iter().all(|g| g.drone_id == "dep"));
    assert!(
        a.grants
            .iter()
            .all(|g| g.labels == ["borg-ci-medium-deploy"])
    );

    let a = allocate(&input(
        &[(Class::Medium, Scope::Deploy, 1)],
        vec![host("shared")],
    ));
    assert!(a.grants.is_empty());
    assert_eq!(a.unmet.len(), 1);
    assert_eq!(a.unmet[0].scope, Scope::Deploy);
}

#[test]
fn pr_demand_never_lands_on_deploy_drones_but_main_may() {
    let a = allocate(&input(
        &[(Class::Light, Scope::Pr, 3), (Class::Light, Scope::Main, 1)],
        vec![deploy_host("dep")],
    ));
    assert_eq!(count(&a, Class::Light, Scope::Pr), 0);
    assert_eq!(count(&a, Class::Light, Scope::Main), 1);
    assert_eq!(a.unmet[0].scope, Scope::Pr);
}

#[test]
fn deploy_is_placed_before_main_and_pr_under_contention() {
    let mut d = deploy_host("dep");
    d.max_leases = Some(1);
    let a = allocate(&input(
        &[
            (Class::Light, Scope::Pr, 1),
            (Class::Light, Scope::Main, 1),
            (Class::Light, Scope::Deploy, 1),
        ],
        vec![d],
    ));
    assert_eq!(a.grants.len(), 1);
    assert_eq!(a.grants[0].scope, Scope::Deploy);
}

#[test]
fn starved_deploy_demand_reserves_only_a_deploy_drone() {
    let mut i = input(
        &[(Class::Heavy, Scope::Deploy, 1)],
        vec![host("shared"), deploy_host("dep")],
    );
    i.starvation_after_secs = Some(10);
    i.queue_ages = vec![QueueAge {
        class: Class::Heavy,
        scope: Scope::Deploy,
        oldest_age_secs: 60,
    }];
    i.leases = vec![
        lease("dep", Class::Heavy, Scope::Main, LeaseState::Running),
        lease("dep", Class::Light, Scope::Main, LeaseState::Running),
    ];
    let a = allocate(&i);
    assert!(a.grants.is_empty());
    assert_eq!(a.reserved_for.len(), 1);
    assert_eq!(a.reserved_for[0].drone_id, "dep");
}

#[test]
fn pending_absorption_respects_trust_scopes() {
    // A pending PR runner (bare label) absorbs main demand, never deploy.
    let mut i = input(&[(Class::Light, Scope::Main, 1)], vec![host("a")]);
    i.leases = vec![lease("a", Class::Light, Scope::Pr, LeaseState::Pending)];
    assert!(allocate(&i).grants.is_empty());

    let mut i = input(
        &[(Class::Light, Scope::Deploy, 1)],
        vec![host("a"), deploy_host("dep")],
    );
    i.leases = vec![lease("a", Class::Light, Scope::Pr, LeaseState::Pending)];
    assert_eq!(count(&allocate(&i), Class::Light, Scope::Deploy), 1);
    i.leases = vec![lease(
        "dep",
        Class::Light,
        Scope::Deploy,
        LeaseState::Pending,
    )];
    assert!(allocate(&i).grants.is_empty());

    // A pending deploy runner carries only the deploy label: main demand
    // still needs its own runner.
    let mut i = input(&[(Class::Light, Scope::Main, 1)], vec![deploy_host("dep")]);
    i.leases = vec![lease(
        "dep",
        Class::Light,
        Scope::Deploy,
        LeaseState::Pending,
    )];
    assert_eq!(count(&allocate(&i), Class::Light, Scope::Main), 1);
}

#[test]
fn warm_floors_may_name_the_deploy_scope() {
    let mut i = input(&[], vec![deploy_host("dep")]);
    i.warm_floors = vec![WarmFloor {
        class: Class::Light,
        scope: Scope::Deploy,
        count: 2,
    }];
    assert_eq!(count(&allocate(&i), Class::Light, Scope::Deploy), 2);
}

fn deploy_only_host(id: &str) -> DroneState {
    DroneState {
        trust: Trust::DeployOnly,
        ..host(id)
    }
}

#[test]
fn deploy_only_drones_take_deploy_demand_and_nothing_else() {
    let a = allocate(&input(
        &[
            (Class::Medium, Scope::Main, 1),
            (Class::Medium, Scope::Pr, 1),
        ],
        vec![deploy_only_host("only")],
    ));
    assert!(a.grants.is_empty(), "{:?}", a.grants);
    assert_eq!(a.unmet.len(), 2);
    let a = allocate(&input(
        &[(Class::Medium, Scope::Deploy, 1)],
        vec![deploy_only_host("only")],
    ));
    assert_eq!(count(&a, Class::Medium, Scope::Deploy), 1);
    assert_eq!(a.grants[0].drone_id, "only");
}

#[test]
fn a_standby_holds_capacity_and_counts_as_pending_deploy_capacity() {
    // No queued deploy demand: the standby keeps the deploy floor met and
    // nothing is promoted or minted.
    let mut i = input(&[], vec![deploy_only_host("dep")]);
    i.warm_floors = vec![WarmFloor {
        class: Class::Medium,
        scope: Scope::Deploy,
        count: 1,
    }];
    i.leases = vec![lease(
        "dep",
        Class::Medium,
        Scope::Deploy,
        LeaseState::Standby,
    )];
    let a = allocate(&i);
    assert!(a.grants.is_empty() && a.promote.is_empty());
    // It also takes room: a 1-lease cap is full.
    let mut capped = deploy_only_host("dep");
    capped.max_leases = Some(1);
    let mut i = input(&[(Class::Light, Scope::Deploy, 1)], vec![capped]);
    i.leases = vec![lease(
        "dep",
        Class::Medium,
        Scope::Deploy,
        LeaseState::Standby,
    )];
    assert!(allocate(&i).grants.is_empty());
}

#[test]
fn deploy_demand_with_a_standby_promotes_it_instead_of_minting() {
    let mut i = input(
        &[(Class::Medium, Scope::Deploy, 1)],
        vec![deploy_only_host("dep")],
    );
    i.leases = vec![lease(
        "dep",
        Class::Medium,
        Scope::Deploy,
        LeaseState::Standby,
    )];
    let a = allocate(&i);
    assert!(a.grants.is_empty(), "{:?}", a.grants);
    assert!(a.unmet.is_empty());
    assert_eq!(
        a.promote,
        vec![Promotion {
            class: Class::Medium,
            count: 1
        }]
    );
    // Two queued jobs, one standby: one promotion, one mint.
    let mut i = input(
        &[(Class::Medium, Scope::Deploy, 2)],
        vec![deploy_only_host("dep")],
    );
    i.leases = vec![lease(
        "dep",
        Class::Medium,
        Scope::Deploy,
        LeaseState::Standby,
    )];
    let a = allocate(&i);
    assert_eq!(a.promote.len(), 1);
    assert_eq!(count(&a, Class::Medium, Scope::Deploy), 1);
    // A pending deploy runner already covers the job: no promotion.
    let mut i = input(
        &[(Class::Medium, Scope::Deploy, 1)],
        vec![deploy_only_host("dep")],
    );
    i.leases = vec![
        lease("dep", Class::Medium, Scope::Deploy, LeaseState::Standby),
        lease("dep", Class::Medium, Scope::Deploy, LeaseState::Pending),
    ];
    let a = allocate(&i);
    assert!(a.grants.is_empty() && a.promote.is_empty());
}

fn pr_turn(i: &mut AllocatorInput, class: Class) {
    i.queue_ages.retain(|q| q.scope != Scope::Pr);
    i.queue_ages.push(QueueAge {
        class,
        scope: Scope::Pr,
        oldest_age_secs: 1000,
    });
    i.pr_turn = PrTurn {
        after_secs: Some(900),
        classes: vec![class],
    };
}

#[test]
fn pr_turn_gives_starving_pr_demand_one_grant_ahead_of_main() {
    let mut i = input(
        &[(Class::Heavy, Scope::Main, 3), (Class::Heavy, Scope::Pr, 3)],
        vec![host("a")],
    );
    pr_turn(&mut i, Class::Heavy);
    let a = allocate(&i);
    assert_eq!(count(&a, Class::Heavy, Scope::Pr), 1);
    assert_eq!(count(&a, Class::Heavy, Scope::Main), 0);
    assert_within_budget(&i, &a);
    // Room for two: the turn is one grant, main takes the rest.
    i.drones.push(host("b"));
    let a = allocate(&i);
    assert_eq!(count(&a, Class::Heavy, Scope::Pr), 1);
    assert_eq!(count(&a, Class::Heavy, Scope::Main), 1);
    assert_within_budget(&i, &a);
}

#[test]
fn pr_turn_needs_the_class_listed_and_its_demand_aged() {
    let mut i = input(
        &[(Class::Heavy, Scope::Main, 3), (Class::Heavy, Scope::Pr, 3)],
        vec![host("a")],
    );
    pr_turn(&mut i, Class::Medium);
    assert_eq!(count(&allocate(&i), Class::Heavy, Scope::Main), 1);
    pr_turn(&mut i, Class::Heavy);
    i.queue_ages
        .iter_mut()
        .for_each(|q| q.oldest_age_secs = 100);
    assert_eq!(count(&allocate(&i), Class::Heavy, Scope::Main), 1);
    i.queue_ages
        .iter_mut()
        .for_each(|q| q.oldest_age_secs = 5000);
    i.pr_turn.after_secs = None;
    assert_eq!(count(&allocate(&i), Class::Heavy, Scope::Main), 1);
}

#[test]
fn pr_turn_obeys_admission_and_the_scope_filter() {
    let mut i = input(
        &[(Class::Heavy, Scope::Main, 1), (Class::Heavy, Scope::Pr, 1)],
        vec![host("a")],
    );
    pr_turn(&mut i, Class::Heavy);
    i.drones[0].cpu_psi_pm = Some(400);
    i.admission = Some(AdmissionThresholds {
        psi_soft_pm: 300,
        psi_light_only_pm: 500,
        psi_hard_pm: 600,
    });
    let a = allocate(&i);
    assert_eq!(count(&a, Class::Heavy, Scope::Pr), 0);
    assert_eq!(count(&a, Class::Heavy, Scope::Main), 1);
    i.admission = None;
    i.drones[0].cpu_psi_pm = None;
    let a = allocate_scope(&i, Scope::Main);
    assert_eq!(count(&a, Class::Heavy, Scope::Pr), 0);
    assert_eq!(count(&a, Class::Heavy, Scope::Main), 1);
}
