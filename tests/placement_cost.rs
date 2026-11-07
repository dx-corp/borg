//! Cost-aware placement through the real queen pass: owned hosts before paid
//! burst drones, dry-run first (`placement.dry_run`).

use crate::support;

use serde_json::json;
use support::Harness;

/// Two identical drones; without a cost the id tie-break picks the burst
/// drone (`gcp-burst-a` sorts before `owned`), as faster learned profiles do
/// live.
async fn fleet(placement: serde_json::Value) -> (Harness, String, String) {
    let h = Harness::start(json!({
        "drone_expiry": {"burst_prefixes": ["gcp-burst-"]},
        "placement": placement,
    }))
    .await;
    let burst = h.enroll("gcp-burst-a").await;
    let owned = h.enroll("owned").await;
    h.heartbeat(&burst, &[]).await;
    h.heartbeat(&owned, &[]).await;
    h.queue_job(1, "borg-ci-light-pr", 5);
    h.tick().await;
    (h, burst, owned)
}

#[tokio::test]
async fn dry_run_places_as_before_and_reports_where_the_cost_would_move_grants() {
    let (h, burst, owned) = fleet(json!({})).await;
    assert_eq!(h.leases(&burst, &[]).await.len(), 1);
    assert!(h.leases(&owned, &[]).await.is_empty());

    let p = &h.status().await["placement"];
    assert_eq!(p["dry_run"], true);
    assert_eq!(p["burst_cost_permille"], 5000);
    assert_eq!(p["passes"], 1);
    assert_eq!(p["grants"], 1);
    assert_eq!(p["burst_grants"], 1);
    assert_eq!(p["would_grants"], 1);
    assert_eq!(p["would_burst_grants"], 0);
    assert_eq!(p["moved"], 1);
    assert_eq!(p["recent"][0]["actual"], "gcp-burst-a");
    assert_eq!(p["recent"][0]["would"], "owned");
    assert_eq!(p["recent"][0]["class"], "light");
    // Today's tie-break, made visible.
    assert_eq!(p["placed"][0]["drone"], "gcp-burst-a");
    assert_eq!(p["placed"][0]["reason"], "name");
}

#[tokio::test]
async fn live_cost_sends_work_to_the_owned_host_and_explains_the_burst_penalty() {
    let (h, burst, owned) = fleet(json!({"dry_run": false})).await;
    assert_eq!(h.leases(&owned, &[]).await.len(), 1);
    assert!(h.leases(&burst, &[]).await.is_empty());

    let p = &h.status().await["placement"];
    assert_eq!(p["dry_run"], false);
    assert_eq!(
        (p["grants"].as_u64(), p["burst_grants"].as_u64()),
        (Some(1), Some(0))
    );
    assert_eq!(p["moved"], 0);
    assert_eq!(p["would_grants"], 0);

    let e: serde_json::Value = h
        .http
        .get(format!("{}/explain?key=borg-ci-light-pr", h.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let d = &e["history"][0]["decision"];
    assert_eq!(d["winner"], "owned");
    assert_eq!(d["reason"], "cost");
    let placed = &h.status().await["placement"]["placed"][0];
    assert_eq!(
        (placed["drone"].as_str(), placed["reason"].as_str()),
        (Some("owned"), Some("cost"))
    );
    let cost = |id: &str| {
        d["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["drone_id"] == id)
            .map(|c| c["cost_score"].clone())
    };
    assert_eq!(cost("owned"), Some(json!(0)));
    assert_eq!(cost("gcp-burst-a"), Some(json!(-4000)));
}

#[tokio::test]
async fn an_out_of_range_burst_cost_is_rejected() {
    for bad in [json!({"burst_cost_permille": 999}), json!({"nope": 1})] {
        let cfg = format!(
            r#"{{"github":{{"org":"o","app_id":"1"}},"repos":["o/r"],"db_path":"/tmp/q.db","placement":{bad}}}"#
        );
        assert!(borg::config::QueenConfig::from_json(&cfg).is_err(), "{bad}");
    }
}

#[tokio::test]
async fn status_lists_the_queued_families_that_steer_placement() {
    let h = Harness::start(json!({})).await;
    let created = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    {
        let mut st = h.state();
        st.queued_runs.push(1);
        for (id, name) in [
            (1, "rust-test (shard 2)"),
            (2, "rust-test (shard 7)"),
            (3, "lint"),
        ] {
            st.jobs.push(json!({
                "id": id, "name": name, "status": "queued",
                "labels": ["borg-ci-heavy-pr"], "created_at": created,
            }));
        }
    }
    h.tick().await;
    let q = &h.status().await["queue"][0];
    assert_eq!(q["label"], "borg-ci-heavy-pr");
    assert_eq!(q["families"], json!([["rust-test", 2], ["lint", 1]]));
}

#[tokio::test]
async fn status_names_the_running_build_and_health_waits_for_a_drone() {
    let dir = tempfile::tempdir().unwrap();
    let health = dir.path().join("queen-health.json");
    let h = Harness::start(json!({"health_file": health})).await;
    let build = &h.status().await["queen"];
    let version = build["version"].as_str().unwrap().to_owned();
    assert!(version.starts_with("0.1.0+"), "{version}");
    assert_eq!(version.len(), "0.1.0+".len() + 12);
    assert!(build["started_at"].as_i64().unwrap() > 0);

    // Ticking with no drone reaching this process is not health.
    h.tick().await;
    assert!(!health.exists());
    let t = h.enroll("owned").await;
    h.heartbeat(&t, &[]).await;
    h.tick().await;
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&health).unwrap()).unwrap();
    assert_eq!(record["version"], version.as_str());
    assert_eq!(record["pid"], std::process::id());
}
