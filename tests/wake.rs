mod support;

use serde_json::{Value, json};
use support::Harness;
use time::OffsetDateTime;

fn cfg() -> Value {
    json!({
        "drone_stale_secs": 1_000_000_000,
        "wake": {
            "enabled": true,
            "after_secs": 300,
            "min_interval_secs": 600,
            "broadcast": ["192.0.2.0/24"],
            "ports": [9],
            "hosts": [
                {"name": "sleeper", "mac": "aa:bb:cc:00:00:01", "classes": ["heavy"]},
                {"name": "other", "mac": "aa:bb:cc:00:00:02"}
            ]
        }
    })
}

fn at(base: OffsetDateTime, secs: i64) -> OffsetDateTime {
    base + time::Duration::seconds(secs)
}

#[tokio::test]
async fn sustained_unmet_demand_wakes_a_sleeping_host_once_and_status_shows_it() {
    let h = Harness::start(cfg()).await;
    let base = OffsetDateTime::now_utc();
    // No drone at all: a queued heavy job cannot be placed.
    h.queue_job(1, "borg-ci-heavy-pr", 10);

    h.queen.tick(at(base, 0)).await.unwrap();
    h.queen.tick(at(base, 200)).await.unwrap();
    assert!(h.wakes.0.lock().unwrap().is_empty(), "too early");

    h.queen.tick(at(base, 310)).await.unwrap();
    {
        let sent = h.wakes.0.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, [0xaa, 0xbb, 0xcc, 0, 0, 1]);
        assert_eq!(sent[0].1[0].to_string(), "192.0.2.255:9");
    }
    // Rate limited: the same unmet demand does not wake again within the interval.
    h.queen.tick(at(base, 400)).await.unwrap();
    assert_eq!(h.wakes.0.lock().unwrap().len(), 1);

    let status = h.status().await;
    assert_eq!(status["wake"]["wakes_last_hour"], 1);
    assert_eq!(status["wake"]["events"][0]["host"], "sleeper");
    assert!(status["wake"]["unmet_secs"]["heavy"].as_u64().unwrap() >= 300);
    let metrics = h.metrics().await;
    assert!(
        metrics.contains("borg_wake_attempts_total{host=\"sleeper\"} 1"),
        "{metrics}"
    );
}

#[tokio::test]
async fn nothing_is_woken_when_the_host_is_already_awake_or_the_feature_is_off() {
    // The host is awake: a fresh drone named like it exists (but cannot hold
    // a heavy job, so demand stays unmet).
    let h = Harness::start(cfg()).await;
    let t = h.enroll("sleeper").await;
    let mut cap = serde_json::to_value(support::capacity()).unwrap();
    cap["mem_mib"] = json!(4096);
    h.heartbeat_with(&t, json!({"capacity": cap})).await;
    let base = OffsetDateTime::now_utc();
    h.queue_job(1, "borg-ci-heavy-pr", 10);
    for secs in [0, 310, 700, 1500] {
        h.queen.tick(at(base, secs)).await.unwrap();
    }
    // "other" has no class restriction so it is woken instead of "sleeper".
    let sent = h.wakes.0.lock().unwrap().clone();
    assert!(
        sent.iter()
            .all(|(mac, _)| *mac == [0xaa, 0xbb, 0xcc, 0, 0, 2]),
        "{sent:?}"
    );

    let off = Harness::start(json!({"drone_stale_secs": 1_000_000_000})).await;
    off.queue_job(1, "borg-ci-heavy-pr", 10);
    for secs in [0, 310, 700] {
        off.queen.tick(at(base, secs)).await.unwrap();
    }
    assert!(off.wakes.0.lock().unwrap().is_empty());
}
