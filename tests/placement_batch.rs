mod support;

use borg::allocator::{AllocatorInput, Demand, DroneState, HostCaps, PlacementDecision, allocate};
use borg::bid::ScoreWeights;
use borg::classes::{Class, Classes, Scope};
use borg::explain::{
    MAX_DECISION_BYTES, MAX_HISTORY, PlacementKind, PlacementOutcome, PlacementRecord,
};
use borg::store::Store;
use serde_json::json;

fn decision() -> PlacementDecision {
    allocate(&AllocatorInput {
        classes: Classes::default(),
        demand: vec![Demand {
            class: Class::Light,
            scope: Scope::Main,
            count: 1,
        }],
        drones: vec![DroneState::from_capacity(
            "host",
            &support::capacity(),
            &HostCaps::default(),
        )],
        leases: vec![],
        warm_floors: vec![],
        queue_ages: vec![],
        starvation_after_secs: None,
        weights: ScoreWeights::default(),
        admission: None,
    })
    .decisions
    .remove(0)
}

fn runner<'a>(key: &'a str, at: i64, decision: &'a PlacementDecision) -> PlacementRecord<'a> {
    PlacementRecord {
        at,
        kind: PlacementKind::Runner,
        key,
        client: None,
        outcome: PlacementOutcome::Granted,
        decision,
    }
}

#[tokio::test]
async fn batch_rejects_wrong_authoritative_unit_owner_without_partial_inserts() {
    let store = Store::open_in_memory().await.unwrap();
    let decision = decision();
    let unit = serde_json::from_value(json!({
        "repo":"example-org/app", "tree_sha":"0123456789abcdef0123456789abcdef01234567",
        "argv":["cargo","check"], "platform":"linux-x86_64", "toolchain":"rust-1.95",
        "class":"light"
    }))
    .unwrap();
    store
        .submit_unit(&unit, "alice", "unit", 3600, 3600, 0)
        .await
        .unwrap();
    store
        .record_placement(runner("sentinel", 0, &decision))
        .await
        .unwrap();
    let bad = PlacementRecord {
        at: 1,
        kind: PlacementKind::Unit,
        key: "unit",
        client: Some("bob"),
        outcome: PlacementOutcome::Unplaced,
        decision: &decision,
    };
    assert!(
        store
            .record_placements(&[runner("partial", 1, &decision), bad])
            .await
            .is_err()
    );
    assert!(
        store
            .placement_history(PlacementKind::Runner, "partial", None)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .placement_history(PlacementKind::Unit, "unit", Some("alice"))
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .placement_history(PlacementKind::Runner, "sentinel", None)
            .await
            .unwrap()
            .len(),
        1
    );
    store
        .record_placements(&[
            runner("accepted", 2, &decision),
            PlacementRecord {
                at: 2,
                kind: PlacementKind::Unit,
                key: "unit",
                client: Some("alice"),
                outcome: PlacementOutcome::Unplaced,
                decision: &decision,
            },
        ])
        .await
        .unwrap();
    let unit_history = store
        .placement_history(PlacementKind::Unit, "unit", Some("alice"))
        .await
        .unwrap();
    assert_eq!(unit_history.len(), 1);
    assert_eq!(unit_history[0].outcome, PlacementOutcome::Unplaced);
    assert_eq!(
        store
            .placement_history(PlacementKind::Runner, "accepted", None)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn invalid_batch_bounds_preserve_existing_history_and_insert_nothing() {
    let store = Store::open_in_memory().await.unwrap();
    let decision = decision();
    store
        .record_placement(runner("sentinel", 0, &decision))
        .await
        .unwrap();
    let mut oversized = decision.clone();
    oversized.winner = Some("x".repeat(MAX_DECISION_BYTES));
    assert!(
        store
            .record_placements(&[
                runner("partial", 1, &decision),
                runner("oversized", 1, &oversized),
            ])
            .await
            .is_err()
    );
    let mut too_many_candidates = decision.clone();
    too_many_candidates.candidates =
        vec![decision.candidates[0].clone(); borg::allocator::MAX_CANDIDATES + 1];
    assert!(
        store
            .record_placements(&[
                runner("partial", 1, &decision),
                runner("candidates", 1, &too_many_candidates),
            ])
            .await
            .is_err()
    );
    for bad_key in [String::new(), "k".repeat(257)] {
        assert!(
            store
                .record_placements(&[
                    runner("partial", 1, &decision),
                    runner(&bad_key, 1, &decision),
                ])
                .await
                .is_err()
        );
    }
    let too_many_rows: Vec<_> = (0..=MAX_HISTORY)
        .map(|at| runner("partial", at, &decision))
        .collect();
    assert!(store.record_placements(&too_many_rows).await.is_err());
    assert!(
        store
            .placement_history(PlacementKind::Runner, "partial", None)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .placement_history(PlacementKind::Runner, "sentinel", None)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn batch_retains_newest_records_at_the_global_cap_across_reopening() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queen.db");
    let store = Store::open(&path).await.unwrap();
    let decision = decision();
    store
        .record_placement(runner("old", 0, &decision))
        .await
        .unwrap();
    let records: Vec<_> = (1..=MAX_HISTORY)
        .map(|at| runner("batch", at, &decision))
        .collect();
    store.record_placements(&records).await.unwrap();
    drop(store);
    let store = Store::open(&path).await.unwrap();
    assert!(
        store
            .placement_history(PlacementKind::Runner, "old", None)
            .await
            .unwrap()
            .is_empty()
    );
    let latest = store
        .placement_history(PlacementKind::Runner, "batch", None)
        .await
        .unwrap();
    assert_eq!(latest.len(), borg::explain::MAX_QUERY_ROWS as usize);
    assert_eq!(latest[0].at, MAX_HISTORY);
    assert_eq!(latest[0].decision, decision);
    assert!(latest.windows(2).all(|pair| pair[0].at == pair[1].at + 1));
    // Check total retention through the same SQLite file, independently of the
    // public read limit, rather than adding test-only store accessors.
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    let (count, oldest): (i64, i64) =
        // sqlx-runtime-query: integration assertion on bounded SQLite diagnostics
        sqlx::query_as("SELECT COUNT(*), MIN(at) FROM placement_history")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((count, oldest), (MAX_HISTORY, 1));
}
