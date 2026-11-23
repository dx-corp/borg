//! Deploy classes take turns (2026-11-22: light-deploy behind heavy builds).

use super::*;

/// borg-deploy-1 as it stood on 2026-11-22: deploy-only, 8 vCPU, 30045 MiB,
/// the live class sizes.
fn deploy_1() -> DroneState {
    DroneState {
        trust: Trust::DeployOnly,
        vcpu: 8,
        mem_mib: 30_045,
        ..host("borg-deploy-1")
    }
}

fn served(class: Class, since_granted_secs: u64) -> ServedAge {
    ServedAge {
        class,
        scope: Scope::Deploy,
        since_granted_secs,
    }
}

/// 2026-11-22: two heavy-deploy builds queued since 21:45 never cleared,
/// because GitHub gave each new heavy runner to a younger platform-api
/// publish. Ordered by the oldest job's age, heavy went first every pass,
/// reserved the only deploy drone, and the light-deploy IaC apply queued at
/// 22:37 never got a slot. Ordered by time since borg last served the class,
/// light goes first once heavy has just been granted.
#[test]
fn light_deploy_is_not_starved_by_a_heavy_deploy_backlog() {
    let mut i = AllocatorInput {
        classes: live_classes(),
        ..input(
            &[
                (Class::Heavy, Scope::Deploy, 2),
                (Class::Light, Scope::Deploy, 2),
            ],
            vec![deploy_1()],
        )
    };
    // One heavy build and one earlier light job run: 7517 MiB left.
    i.leases = vec![
        lease(
            "borg-deploy-1",
            Class::Heavy,
            Scope::Deploy,
            LeaseState::Running,
        ),
        lease(
            "borg-deploy-1",
            Class::Light,
            Scope::Deploy,
            LeaseState::Running,
        ),
    ];
    aged(&mut i, Class::Heavy, Scope::Deploy, 7_851);
    aged(&mut i, Class::Light, Scope::Deploy, 4_637);

    // By job age alone heavy goes first, cannot fit, and holds the drone;
    // light, which fits, waits behind it.
    let by_age = allocate(&i);
    assert_eq!(count(&by_age, Class::Light, Scope::Deploy), 0, "{by_age:?}");
    assert_eq!(by_age.reserved_for.len(), 1);
    assert_eq!(by_age.reserved_for[0].class, Class::Heavy);

    // Heavy was granted a runner a minute ago (the one GitHub gave to a
    // younger publish); light has had nothing since its job queued.
    i.served = vec![served(Class::Heavy, 60)];
    let a = allocate(&i);
    // Both light jobs fit beside the running build.
    assert_eq!(count(&a, Class::Light, Scope::Deploy), 2, "{a:?}");
    assert_eq!(count(&a, Class::Heavy, Scope::Deploy), 0, "{a:?}");
    assert_within_budget(&i, &a);
    // Heavy still holds the drone for its next slot.
    assert_eq!(a.reserved_for.len(), 1);
    assert_eq!(a.reserved_for[0].class, Class::Heavy);
}

/// The other side of taking turns: once light has just been served, heavy
/// goes first again and gets the room that frees, so neither class starves.
#[test]
fn heavy_deploy_is_not_starved_by_light_deploy_turns() {
    let mut i = AllocatorInput {
        classes: live_classes(),
        ..input(
            &[
                (Class::Heavy, Scope::Deploy, 2),
                (Class::Light, Scope::Deploy, 20),
            ],
            vec![deploy_1()],
        )
    };
    // Five light jobs run: 19805 MiB left, short of one heavy (20480).
    i.leases = (0..5)
        .map(|_| {
            lease(
                "borg-deploy-1",
                Class::Light,
                Scope::Deploy,
                LeaseState::Running,
            )
        })
        .collect();
    aged(&mut i, Class::Heavy, Scope::Deploy, 7_851);
    aged(&mut i, Class::Light, Scope::Deploy, 9_000);
    i.served = vec![served(Class::Light, 30), served(Class::Heavy, 900)];
    let a = allocate(&i);
    // Heavy is first: it reserves the drone, and light does not refill it.
    assert_eq!(count(&a, Class::Light, Scope::Deploy), 0, "{a:?}");
    assert_eq!(a.reserved_for.len(), 1, "{a:?}");
    assert_eq!(a.reserved_for[0].class, Class::Heavy);
    // One light lease ends: heavy takes the room.
    i.leases.pop();
    let a = allocate(&i);
    assert_eq!(count(&a, Class::Heavy, Scope::Deploy), 1, "{a:?}");
    assert_within_budget(&i, &a);
}

/// Many passes over one deploy drone with both queues never emptying and
/// GitHub never handing a runner to either class's oldest job: both classes
/// keep getting runners, and no pass exceeds the drone's budget or its
/// per-class lease cap.
#[test]
fn deploy_classes_take_turns_when_neither_queue_empties() {
    let mut drone = deploy_1();
    drone.class_caps = [(Class::Heavy, 1), (Class::Light, 4)].into();
    let mut since: BTreeMap<Class, u64> = BTreeMap::new();
    // Each running lease with the passes it has left.
    let mut running: Vec<(Class, u32)> = Vec::new();
    let mut granted: BTreeMap<Class, u32> = BTreeMap::new();
    let mut longest_gap: BTreeMap<Class, u32> = BTreeMap::new();
    let mut gap: BTreeMap<Class, u32> = BTreeMap::new();
    for _pass in 0..60 {
        let mut i = AllocatorInput {
            classes: live_classes(),
            ..input(
                &[
                    (Class::Heavy, Scope::Deploy, 5),
                    (Class::Light, Scope::Deploy, 5),
                ],
                vec![drone.clone()],
            )
        };
        i.leases = running
            .iter()
            .map(|&(c, _)| lease("borg-deploy-1", c, Scope::Deploy, LeaseState::Running))
            .collect();
        // The oldest job of each class never clears.
        aged(&mut i, Class::Heavy, Scope::Deploy, 40_000);
        aged(&mut i, Class::Light, Scope::Deploy, 30_000);
        i.served = since.iter().map(|(&c, &s)| served(c, s)).collect();
        let a = allocate(&i);
        assert_within_budget(&i, &a);
        for c in [Class::Heavy, Class::Light] {
            let n = u32::try_from(count(&a, c, Scope::Deploy)).unwrap();
            let held = running.iter().filter(|&&(r, _)| r == c).count();
            let cap = drone.class_caps[&c];
            assert!(
                u32::try_from(held).unwrap() + n <= cap,
                "{c} over its cap of {cap}: {held} running + {n} granted"
            );
            *granted.entry(c).or_default() += n;
            let g = gap.entry(c).or_default();
            if n > 0 {
                since.insert(c, 0);
                *g = 0;
            } else {
                *g += 1;
                let l = longest_gap.entry(c).or_default();
                *l = (*l).max(*g);
            }
            // A heavy build runs four passes, a light job one.
            let passes = if c == Class::Heavy { 4 } else { 1 };
            running.extend((0..n).map(|_| (c, passes)));
        }
        for s in since.values_mut() {
            *s += 60;
        }
        for r in &mut running {
            r.1 -= 1;
        }
        running.retain(|&(_, left)| left > 0);
    }
    assert!(granted[&Class::Heavy] >= 10, "{granted:?}");
    assert!(granted[&Class::Light] >= 10, "{granted:?}");
    // Neither class goes longer than one heavy build without a runner.
    for c in [Class::Heavy, Class::Light] {
        assert!(
            longest_gap.get(&c).copied().unwrap_or(0) <= 5,
            "{longest_gap:?}"
        );
    }
}
