use crate::support;

use borg::allocator::{
    ActiveLease, Allocation, AllocatorInput, Demand, DroneState, HostCaps, LeaseState, PrTurn,
    QueueAge, Rejection, allocate,
};
use borg::bid::ScoreWeights;
use borg::classes::{Class, Classes, Scope};
use borg::explain::{Explanation, MAX_HISTORY, PlacementKind, PlacementOutcome, PlacementRecord};
use borg::store::Store;
use serde_json::json;
use support::Harness;

fn host(id: &str) -> DroneState {
    DroneState::from_capacity(id, &support::capacity(), &HostCaps::default())
}

fn input(drones: Vec<DroneState>, count: u32) -> AllocatorInput {
    AllocatorInput {
        classes: Classes::default(),
        demand: vec![Demand {
            class: Class::Light,
            scope: Scope::Main,
            count,
        }],
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

fn one() -> Allocation {
    allocate(&input(vec![host("a")], 1))
}

#[test]
fn traces_actual_winner_and_order_with_bid_and_profile_terms() {
    let mut a = host("a");
    a.bids = serde_json::from_value(json!([{"class":"light","scope":"main","available_slots":1,"cache_affinity":1000,"expires_at":100}])).unwrap();
    a.profiles = serde_json::from_value(json!([{"class":"light","speed_permille":1500,"speed_confidence_permille":1000,"reliability_permille":1000}])).unwrap();
    let result = allocate(&input(vec![a, host("b")], 2));
    assert_eq!(
        result
            .grants
            .iter()
            .map(|g| g.drone_id.as_str())
            .collect::<Vec<_>>(),
        vec!["a", "b"]
    );
    assert_eq!(
        result
            .decisions
            .iter()
            .map(|d| d.winner.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("a"), Some("b")]
    );
    assert_eq!(result.decisions[1].grant_order, 1);
    let first = &result.decisions[0].candidates[0];
    assert_eq!(
        (first.bid_score, first.profile_score, first.score),
        (3000, 500, Some(3500))
    );
    let second = &result.decisions[1].candidates[0];
    assert!(second.rejection_reasons.contains(&Rejection::BidExhausted));
    assert_eq!(
        second.room.mem_mib,
        first.room.mem_mib - result.grants[0].mem_mib
    );
}

#[test]
fn traces_owner_net_capacity_blocked_environment_and_nonaccepting_hosts() {
    let mut cap = support::capacity();
    cap.vcpu = 32;
    cap.mem_mib = 64 * 1024;
    let limited = DroneState::from_capacity(
        "limited",
        &cap,
        &HostCaps {
            max_vcpu: Some(1),
            max_mem_mib: Some(1000),
            max_leases: Some(0),
            reserve_mem_mib: 2048,
        },
    );
    let mut blocked = host("blocked");
    blocked.blocked_classes.push(Class::Light);
    let mut quarantined = host("quarantined");
    quarantined.accepting = false;
    let result = allocate(&input(vec![limited, blocked, quarantined], 1));
    assert!(result.grants.is_empty());
    let decision = &result.decisions[0];
    assert_eq!(decision.winner, None);
    let find = |id| {
        decision
            .candidates
            .iter()
            .find(|c| c.drone_id == id)
            .unwrap()
    };
    assert!(
        find("blocked")
            .rejection_reasons
            .contains(&Rejection::Environment)
    );
    assert!(
        find("quarantined")
            .rejection_reasons
            .contains(&Rejection::NotAccepting)
    );
    assert_eq!(
        (find("limited").budget.vcpu, find("limited").budget.mem_mib),
        (1, 1000)
    );
    assert_eq!(
        find("limited").rejection_reasons,
        vec![Rejection::Memory, Rejection::Vcpu, Rejection::LeaseCap]
    );
}

#[test]
fn records_reservation_and_whether_backfill_preserves_the_reserved_job() {
    // Heavy needs 24 GiB, medium uses 12 GiB, light needs 2.5 GiB.
    // Current room admits light in both cases; future heavy room decides.
    for (mem_gib, backfill) in [(24, false), (28, true)] {
        let mut drone = host("a");
        drone.mem_mib = mem_gib * 1024;
        drone.vcpu = 16;
        let mut i = input(vec![drone], 1);
        i.demand.push(Demand {
            class: Class::Heavy,
            scope: Scope::Main,
            count: 1,
        });
        i.leases.push(ActiveLease {
            drone_id: "a".into(),
            class: Class::Medium,
            scope: Scope::Main,
            state: LeaseState::Running,
        });
        i.queue_ages.push(QueueAge {
            class: Class::Heavy,
            scope: Scope::Main,
            oldest_age_secs: 60,
        });
        i.starvation_after_secs = Some(30);
        let result = allocate(&i);
        assert_eq!(result.decisions[0].reservation.as_deref(), Some("a"));
        let light = result
            .decisions
            .iter()
            .find(|d| d.class == Class::Light)
            .unwrap();
        if backfill {
            assert_eq!(light.winner.as_deref(), Some("a"));
            assert!(light.candidates[0].rejection_reasons.is_empty());
        } else {
            assert!(light.winner.is_none());
            assert!(
                light.candidates[0]
                    .rejection_reasons
                    .contains(&Rejection::Reserved)
            );
        }
        assert_eq!(
            light.candidates[0].reserved_for,
            Some((Class::Heavy, Scope::Main))
        );
    }
}

#[test]
fn inherited_reservations_are_enforced_and_explained_at_the_selection_boundary() {
    let i = input(vec![host("a")], 1);
    let reserved = [borg::allocator::Reservation {
        drone_id: "a".into(),
        class: Class::Heavy,
        scope: Scope::Main,
    }];
    let result = borg::allocator::allocate_reserved(&i, &reserved);
    assert!(result.grants.is_empty());
    assert_eq!(
        result.decisions[0].candidates[0].rejection_reasons,
        vec![Rejection::Reserved]
    );
}

#[test]
fn fair_single_unit_selection_uses_the_real_backlog_for_bid_pressure() {
    let mut drone = host("a");
    drone.bids = serde_json::from_value(json!([{"class":"light","scope":"main","available_slots":10,"min_pressure":500,"expires_at":100}])).unwrap();
    let i = input(vec![drone], 1);
    assert!(allocate(&i).grants.is_empty());
    let backlog = [Demand {
        class: Class::Light,
        scope: Scope::Main,
        count: 20,
    }];
    let result = borg::allocator::allocate_reserved_with_backlog(&i, &[], &backlog);
    assert_eq!(
        result.grants.len(),
        1,
        "backlog controls pressure, not the number selected by fairness"
    );
    assert_eq!(result.decisions[0].pressure, 1000);
    assert_eq!(result.decisions[0].winner.as_deref(), Some("a"));
}

#[test]
fn starvation_never_reserves_a_host_that_declines_the_demand() {
    let mut drone = host("a");
    drone.bids = serde_json::from_value(
        json!([{"class":"heavy","scope":"main","available_slots":0,"expires_at":100}]),
    )
    .unwrap();
    let mut i = input(vec![drone], 0);
    i.demand.push(Demand {
        class: Class::Heavy,
        scope: Scope::Main,
        count: 1,
    });
    i.leases.push(ActiveLease {
        drone_id: "a".into(),
        class: Class::Light,
        scope: Scope::Main,
        state: LeaseState::Running,
    });
    i.queue_ages.push(QueueAge {
        class: Class::Heavy,
        scope: Scope::Main,
        oldest_age_secs: 60,
    });
    i.starvation_after_secs = Some(30);
    let result = allocate(&i);
    assert!(result.reserved_for.is_empty());
    assert_eq!(result.decisions[0].reservation, None);
    assert!(
        result.decisions[0].candidates[0]
            .rejection_reasons
            .contains(&Rejection::BidExhausted)
    );
}

#[test]
fn diagnostic_bounds_never_change_the_actual_winner_or_grant_count() {
    let mut drones = (0..borg::allocator::MAX_CANDIDATES + 1)
        .map(|i| host(&format!("host-{i:04}")))
        .collect::<Vec<_>>();
    drones.last_mut().unwrap().profiles = serde_json::from_value(json!([{"class":"light","speed_permille":1500,"speed_confidence_permille":1000,"reliability_permille":1000}])).unwrap();
    let result = allocate(&input(drones, 1));
    assert_eq!(
        result.decisions[0].total_candidates,
        borg::allocator::MAX_CANDIDATES + 1
    );
    assert_eq!(
        result.decisions[0].candidates.len(),
        borg::allocator::MAX_CANDIDATES
    );
    assert_eq!(result.decisions[0].winner.as_deref(), Some("host-0128"));
    assert_eq!(result.grants[0].drone_id, "host-0128");
    assert!(
        result.decisions[0]
            .candidates
            .iter()
            .any(|c| c.drone_id == "host-0128")
    );
    let mut drone = host("huge");
    drone.vcpu = 1000;
    drone.mem_mib = 1_000_000;
    let result = allocate(&input(vec![drone], 130));
    assert_eq!(result.grants.len(), 130);
    assert_eq!(result.decisions.len(), borg::allocator::MAX_DECISIONS);
    assert_eq!(result.omitted_decisions, 2);
}

#[tokio::test]
async fn history_is_bounded_and_survives_reopening_the_owned_store() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queen.db");
    let store = Store::open(&path).await.unwrap();
    let result = one();
    for n in 0..MAX_HISTORY + 5 {
        let key = format!("label-{n}");
        store
            .record_placement(PlacementRecord {
                at: n,
                kind: PlacementKind::Runner,
                key: &key,
                client: None,
                outcome: PlacementOutcome::Granted,
                decision: &result.decisions[0],
            })
            .await
            .unwrap();
    }
    drop(store);
    let reopened = Store::open(&path).await.unwrap();
    assert!(
        reopened
            .placement_history(PlacementKind::Runner, "label-4", None)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        reopened
            .placement_history(PlacementKind::Runner, "label-5", None)
            .await
            .unwrap()
            .len(),
        1
    );
    let latest = reopened
        .placement_history(PlacementKind::Runner, "label-1004", None)
        .await
        .unwrap();
    assert_eq!(latest[0].decision, result.decisions[0]);
    assert_eq!(latest[0].outcome, PlacementOutcome::Granted);
}

#[tokio::test]
async fn history_rejects_oversized_and_wrong_owner_records_without_pruning() {
    let store = Store::open_in_memory().await.unwrap();
    let mut decision = one().decisions.remove(0);
    decision.winner = Some("x".repeat(borg::explain::MAX_DECISION_BYTES));
    assert!(
        store
            .record_placement(PlacementRecord {
                at: 0,
                kind: PlacementKind::Runner,
                key: "label",
                client: None,
                outcome: PlacementOutcome::Granted,
                decision: &decision
            })
            .await
            .is_err()
    );
    assert!(
        store
            .record_placement(PlacementRecord {
                at: 0,
                kind: PlacementKind::Unit,
                key: "missing-unit",
                client: Some("wrong"),
                outcome: PlacementOutcome::Granted,
                decision: &one().decisions[0]
            })
            .await
            .is_err()
    );
    assert!(
        store
            .placement_history(PlacementKind::Runner, "label", None)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn api_requires_unit_owner_and_public_label_queries_never_include_units() {
    let h = Harness::start(json!({"units":{"enabled":true}})).await;
    for (name, token) in [("alice", "alice-token"), ("bob", "bob-token")] {
        h.queen
            .store()
            .enroll_client(name, &borg::enroll::hash_token(token), 0)
            .await
            .unwrap();
    }
    let req = h.http.post(format!("{}/v1/units", h.url)).bearer_auth("alice-token")
        .json(&json!({"repo":"example-org/app","tree_sha":"0123456789abcdef0123456789abcdef01234567","argv":["private-command"],"platform":"linux-x86_64","class":"light"})).send().await.unwrap();
    assert_eq!(req.status(), 201);
    let submitted: serde_json::Value = req.json().await.unwrap();
    let id = submitted["unit_id"].as_str().unwrap();
    let result = one();
    let store = h.queen.store();
    store
        .record_placement(PlacementRecord {
            at: 1,
            kind: PlacementKind::Unit,
            key: id,
            client: Some("alice"),
            outcome: PlacementOutcome::Granted,
            decision: &result.decisions[0],
        })
        .await
        .unwrap();
    assert!(
        store
            .record_placement(PlacementRecord {
                at: 1,
                kind: PlacementKind::Unit,
                key: id,
                client: Some("bob"),
                outcome: PlacementOutcome::Granted,
                decision: &result.decisions[0]
            })
            .await
            .is_err()
    );
    store
        .record_placement(PlacementRecord {
            at: 1,
            kind: PlacementKind::Runner,
            key: "borg-ci-light",
            client: None,
            outcome: PlacementOutcome::Granted,
            decision: &result.decisions[0],
        })
        .await
        .unwrap();
    let url = format!("{}/explain", h.url);
    assert_eq!(
        h.http
            .get(&url)
            .query(&[("key", id)])
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let bob: Explanation = h
        .http
        .get(&url)
        .query(&[("key", id)])
        .bearer_auth("bob-token")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(bob.history.is_empty());
    let alice: Explanation = h
        .http
        .get(&url)
        .query(&[("key", id)])
        .bearer_auth("alice-token")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(alice.history.len(), 1);
    assert_eq!(alice.history[0].key, id);
    let public: Explanation = h
        .http
        .get(&url)
        .query(&[("key", "borg-ci-light")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(public.history.len(), 1);
    assert_eq!(public.history[0].kind, PlacementKind::Runner);
    assert!(!serde_json::to_string(&public).unwrap().contains(id));
    assert!(
        !serde_json::to_string(&alice)
            .unwrap()
            .contains("private-command")
    );
    let missing: Explanation = h
        .http
        .get(&url)
        .query(&[("key", "unknown-unit")])
        .bearer_auth("alice-token")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(missing.history.is_empty());
    assert!(borg::explain::render_text(&missing).contains("no retained placement decision"));
}

#[tokio::test]
async fn disabled_units_hide_retained_unit_history_but_keep_runner_explanations() {
    let h = Harness::start(json!({"units":{"enabled":false}})).await;
    let store = h.queen.store();
    store
        .enroll_client("alice", &borg::enroll::hash_token("alice-token"), 0)
        .await
        .unwrap();
    // Persist directly to model history retained after an operator disables units.
    let unit: borg::units::SubmitUnit = serde_json::from_value(json!({
        "repo":"example-org/app",
        "tree_sha":"0123456789abcdef0123456789abcdef01234567",
        "argv":["private-command"],
        "platform":"linux-x86_64",
        "class":"light"
    }))
    .unwrap();
    unit.validate().unwrap();
    store
        .submit_unit(&unit, "alice", "retained-unit", 3600, 3600, 0)
        .await
        .unwrap();
    let allocation = one();
    for (kind, key, client) in [
        (PlacementKind::Unit, "retained-unit", Some("alice")),
        (PlacementKind::Runner, "borg-ci-light", None),
    ] {
        store
            .record_placement(PlacementRecord {
                at: 1,
                kind,
                key,
                client,
                outcome: PlacementOutcome::Granted,
                decision: &allocation.decisions[0],
            })
            .await
            .unwrap();
    }
    assert_eq!(
        store
            .placement_history(PlacementKind::Unit, "retained-unit", Some("alice"))
            .await
            .unwrap()
            .len(),
        1
    );
    for authenticated in [false, true] {
        let request = h
            .http
            .get(format!("{}/explain", h.url))
            .query(&[("key", "retained-unit")]);
        let request = if authenticated {
            request.bearer_auth("alice-token")
        } else {
            request
        };
        assert_eq!(request.send().await.unwrap().status(), 404);
    }
    let response = h
        .http
        .get(format!("{}/explain", h.url))
        .query(&[("key", "borg-ci-light")])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let runner: Explanation = response.json().await.unwrap();
    assert_eq!(runner.history.len(), 1);
    assert_eq!(runner.history[0].kind, PlacementKind::Runner);
}
