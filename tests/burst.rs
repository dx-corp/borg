use crate::support;

use serde_json::{Value, json};
use support::Harness;
use time::OffsetDateTime;

fn config() -> Value {
    json!({
        "drone_expiry": {"burst_prefixes": ["gcp-burst-"]},
        "capacity_alarm": {"enabled": false},
        "burst": {
            "enabled": true, "max_drones": 2,
            "budget": {"drone_hours_per_day": 24, "drone_hours_per_month": 150},
            "scale_up": {"classes": ["heavy"], "weights": {"main": 3, "pr": 1},
                         "min_weighted_unmet": 6, "sustained_secs": 600},
            "scale_down": {"idle_secs": 900},
            "gce": {"project": "p", "network": "borg-burst", "subnet": "borg-burst-usw1",
                    "zones": ["us-west1-b", "us-west1-a"], "machine_type": "c2d-highcpu-56"}
        }
    })
}

async fn status_at(h: &Harness, at: OffsetDateTime) -> Value {
    let s = borg::api::build_status(&h.queen, at.unix_timestamp())
        .await
        .unwrap();
    serde_json::to_value(s).unwrap()
}

fn burst_events(s: &Value) -> Vec<String> {
    s["health_events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["drone"] == "burst")
        .map(|e| e["event"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn burst_is_absent_from_status_when_not_configured() {
    let h = Harness::start(json!({})).await;
    h.tick().await;
    let s = h.status().await;
    assert!(s["burst"].is_null());
    assert!(!h.metrics().await.contains("borg_burst_"));
}

#[tokio::test]
async fn dry_run_logs_what_it_would_create_drain_and_delete_and_creates_nothing() {
    let h = Harness::start(config()).await;
    // Two main heavy jobs and no drone: weighted unmet 6.
    h.queue_job(1, "borg-ci-heavy", 5);
    h.queue_job(2, "borg-ci-heavy", 5);
    let t0 = OffsetDateTime::now_utc();
    let at = |s: i64| t0 + time::Duration::seconds(s);

    h.queen.tick(at(0)).await.unwrap();
    let s = status_at(&h, at(0)).await;
    assert_eq!(s["burst"]["dry_run"], true);
    assert_eq!(s["burst"]["demand"]["weighted_unmet"], 6);
    assert_eq!(s["burst"]["outcome"], "sustaining");
    assert!(burst_events(&s).is_empty());

    h.queen.tick(at(600)).await.unwrap();
    let s = status_at(&h, at(600)).await;
    assert_eq!(s["burst"]["outcome"], "create", "{}", s["burst"]);
    assert_eq!(s["burst"]["drones"]["managed"], 1);
    assert_eq!(s["burst"]["vms"][0]["zone"], "us-west1-b");
    assert_eq!(burst_events(&s), ["burst_would_create"]);
    // Nothing real: no drone was enrolled and no runner was minted.
    assert_eq!(s["drones"], json!([]));
    assert!(h.state().mints.is_empty());

    h.queen.tick(at(660)).await.unwrap();
    assert_eq!(
        status_at(&h, at(660)).await["burst"]["outcome"],
        "provisioning"
    );

    // Demand ends; the simulated drone enrolls, sits idle, is drained, then deleted.
    h.state().jobs.clear();
    for t in [1400, 2300, 2310] {
        h.queen.tick(at(t)).await.unwrap();
    }
    let s = status_at(&h, at(2310)).await;
    assert_eq!(
        burst_events(&s),
        [
            "burst_would_delete",
            "burst_would_drain",
            "burst_would_create"
        ],
        "newest first"
    );
    assert_eq!(s["burst"]["drones"]["managed"], 0);
    assert!(s["burst"]["spend"]["day_hours"].as_f64().unwrap() > 0.4);
    assert_eq!(s["burst"]["outcomes"]["no_demand"], 3);
    assert_eq!(s["burst"]["recent"].as_array().unwrap().len(), 3);

    let m = h.metrics().await;
    for line in [
        "borg_burst_dry_run 1",
        "borg_burst_drones{kind=\"managed\"} 0",
        "borg_burst_budget_drone_hours{window=\"day\"} 24",
        "borg_burst_passes{outcome=\"create\"} 1",
    ] {
        assert!(m.contains(line), "missing {line}\n{m}");
    }
}

#[tokio::test]
async fn drones_made_by_hand_count_toward_the_cap() {
    let h = Harness::start(config()).await;
    for name in ["gcp-burst-3", "gcp-burst-4"] {
        let t = h.enroll(name).await;
        h.heartbeat(&t, &[]).await;
    }
    // More heavy main jobs than the two test drones hold (one heavy each).
    // The cap needs no sustained window: it is checked first.
    for i in 0..10 {
        h.queue_job(i, "borg-ci-heavy", 5);
    }
    let t0 = OffsetDateTime::now_utc();
    let t1 = t0 + time::Duration::seconds(30);
    h.queen.tick(t0).await.unwrap();
    h.queen.tick(t1).await.unwrap();
    let s = status_at(&h, t1).await;
    assert_eq!(s["burst"]["drones"]["unmanaged"], 2, "{}", s["burst"]);
    assert_eq!(s["burst"]["outcome"], "at_max_drones");
    assert!(burst_events(&s).is_empty());
}
