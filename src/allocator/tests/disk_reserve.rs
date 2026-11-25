//! Running leases keep a disk reserve: jobs on one drone share its work
//! filesystem, so a floor checked at each start does not stop them from
//! filling it together.

use super::*;

/// A burst drone (52 vCPU, 102 GiB budget) late in its life: sixteen jobs
/// running on a work filesystem they share, each keeping `reserve_gib`.
fn shared_disk_burst(free_gib: u64, reserve_gib: u64) -> (DroneState, Vec<ActiveLease>) {
    let drone = DroneState {
        vcpu: 52,
        mem_mib: 102 * 1024,
        work_free_mib: Some(free_gib * 1024),
        disk_reserve_mib: Class::ALL
            .into_iter()
            .map(|c| (c, reserve_gib * 1024))
            .collect(),
        ..disk_bound_main_1()
    };
    let running = |class, n: usize| {
        std::iter::repeat_n(
            lease("borg-main-1", class, Scope::Main, LeaseState::Running),
            n,
        )
    };
    // 90 GiB and 44 vCPU of the budget: four more light leases would fit.
    let leases = running(Class::Heavy, 1)
        .chain(running(Class::Medium, 3))
        .chain(running(Class::Light, 12))
        .collect();
    (drone, leases)
}

#[test]
fn running_leases_keep_their_disk_reserve_out_of_new_admissions() {
    // 40 GiB free is four light floors, and memory and vCPU have room, but
    // the sixteen jobs already running may still write 4 GiB each (64 GiB).
    let demand = [(Class::Light, Scope::Main, 3)];
    let (drone, leases) = shared_disk_burst(40, 4);
    let i = AllocatorInput {
        leases,
        ..input(&demand, vec![drone])
    };
    let a = allocate(&i);
    assert_eq!(count(&a, Class::Light, Scope::Main), 0, "{:?}", a.grants);
    let refused = a
        .decisions
        .iter()
        .flat_map(|d| &d.candidates)
        .all(|c| c.rejection_reasons.contains(&Rejection::Disk));
    assert!(refused, "{:?}", a.decisions);
    let idle = idle_after(&i.drones, &i.leases, &i.classes, None);
    assert!(idle.iter().all(|c| c.idle_slots == 0), "{idle:?}");

    // 78 GiB less the 64 reserved holds one more light floor; that lease's
    // own 10 GiB then leaves no second.
    let (drone, leases) = shared_disk_burst(78, 4);
    let a = allocate(&AllocatorInput {
        leases,
        ..input(&demand, vec![drone])
    });
    assert_eq!(count(&a, Class::Light, Scope::Main), 1, "{:?}", a.grants);

    // With no reserve the reading alone decides, as before.
    let (drone, leases) = shared_disk_burst(40, 0);
    let a = allocate(&AllocatorInput {
        leases,
        ..input(&demand, vec![drone])
    });
    assert_eq!(count(&a, Class::Light, Scope::Main), 3, "{:?}", a.grants);
}

#[test]
fn a_running_lease_holds_its_class_reserve_not_its_floor() {
    // borg-main-1 with three heavy jobs running and 69 GiB free: at 8 GiB a
    // running heavy lease, 45 GiB is left for one more heavy floor (32).
    // Charging a running lease its whole floor would have admitted none.
    let drone = DroneState {
        disk_reserve_mib: BTreeMap::from([(Class::Heavy, 8 * 1024)]),
        ..disk_bound_main_1()
    };
    let heavy = lease(
        "borg-main-1",
        Class::Heavy,
        Scope::Main,
        LeaseState::Running,
    );
    let i = AllocatorInput {
        leases: vec![heavy.clone(); 3],
        ..input(&[(Class::Heavy, Scope::Main, 2)], vec![drone.clone()])
    };
    let a = allocate(&i);
    assert_eq!(count(&a, Class::Heavy, Scope::Main), 1, "{:?}", a.grants);
    assert!(drone.disk_admits(&i.leases, Class::Heavy));
    // Five running heavy leases keep 40 GiB: 29 is under the heavy floor.
    let mut five = vec![heavy; 5];
    assert!(!drone.disk_admits(&five, Class::Heavy));
    assert!(drone.disk_admits(&five, Class::Light));
    // Leases on another drone hold nothing here.
    five.iter_mut()
        .for_each(|l| l.drone_id = "elsewhere".into());
    assert!(drone.disk_admits(&five, Class::Heavy));
}

#[test]
fn a_standby_the_running_reserve_leaves_no_room_for_covers_no_deploy_demand() {
    // A running medium job keeps 4 GiB of 35: 31 is under the heavy floor,
    // so the heavy standby cannot start and must not hide the queued deploy
    // job. One GiB more and it is promoted.
    let drone = |free_gib: u64| DroneState {
        trust: Trust::Deploy,
        work_free_mib: Some(free_gib * 1024),
        disk_reserve_mib: Class::ALL.into_iter().map(|c| (c, 4 * 1024)).collect(),
        ..disk_bound_main_1()
    };
    let leases = vec![
        lease(
            "borg-main-1",
            Class::Medium,
            Scope::Deploy,
            LeaseState::Running,
        ),
        lease(
            "borg-main-1",
            Class::Heavy,
            Scope::Deploy,
            LeaseState::Standby,
        ),
    ];
    let promoted = |free_gib| {
        allocate(&AllocatorInput {
            leases: leases.clone(),
            ..input(&[(Class::Heavy, Scope::Deploy, 1)], vec![drone(free_gib)])
        })
        .promote
    };
    assert_eq!(promoted(35), vec![]);
    assert_eq!(
        promoted(36),
        vec![Promotion {
            class: Class::Heavy,
            count: 1
        }]
    );
}

#[test]
fn a_reserve_above_the_floor_is_what_a_new_lease_needs_and_holds() {
    // Medium starts at 10 GiB free but its running jobs keep 16. Two grants
    // on 24 GiB would owe 32 once both run: the second waits, at placement
    // and while the first is still pending.
    let drone = DroneState {
        work_free_mib: Some(24 * 1024),
        disk_reserve_mib: BTreeMap::from([(Class::Medium, 16 * 1024)]),
        ..disk_bound_main_1()
    };
    assert_eq!(drone.disk_need(Class::Medium), 16 * 1024);
    let demand = [(Class::Medium, Scope::Main, 2)];
    let a = allocate(&input(&demand, vec![drone.clone()]));
    assert_eq!(count(&a, Class::Medium, Scope::Main), 1, "{:?}", a.grants);
    let pending = lease("borg-main-1", Class::Medium, Scope::Main, LeaseState::Held);
    assert_eq!(pending.disk_hold_on(&drone), 16 * 1024);
    assert!(!drone.disk_admits(std::slice::from_ref(&pending), Class::Medium));
    let idle = idle_after(std::slice::from_ref(&drone), &[], &Classes::default(), None);
    let medium = idle
        .iter()
        .find(|c| c.class == Class::Medium)
        .expect("medium");
    assert_eq!(medium.idle_slots, 1);
    assert_eq!(
        admissible_slots(&drone, &[], Class::Medium, &Classes::default()),
        1
    );
    assert_eq!(
        admissible_slots(&drone, &[pending], Class::Medium, &Classes::default()),
        0
    );
}
