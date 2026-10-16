use crate::support;

use serde_json::{Value, json};
use support::Harness;
use time::OffsetDateTime;

fn add_static(h: &Harness, id: u64, label: &str, busy: bool) {
    h.state().runners.push(json!({
        "id": id, "name": format!("static-{id}"), "status": "online", "busy": busy,
        "labels": [{"name": "self-hosted"}, {"name": label}]
    }));
}

fn drone<'a>(status: &'a Value, name: &str) -> &'a Value {
    status["drones"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["name"] == name)
        .unwrap()
}

#[tokio::test]
async fn status_reports_free_capacity_next_to_theoretical_capacity() {
    let h = Harness::start(json!({})).await;
    let t = h.enroll("d1").await;
    h.heartbeat(&t, &[]).await;

    // An empty fleet: free equals theoretical (28 GiB budget, 16 vCPU).
    let s = h.status().await;
    let d = drone(&s, "d1");
    assert_eq!(d["budget_mem_mib"], 28 * 1024);
    assert_eq!(d["free_mem_mib"], 28 * 1024);
    assert_eq!(d["free_slots"]["heavy"], d["theoretical_slots"]["heavy"]);
    assert_eq!(d["free_slots"]["heavy"], 1);
    assert_eq!(s["classes"]["heavy"]["free_slots"], 1);

    // One heavy lease (24 GiB, 8 vCPU) eats the budget: the theoretical heavy
    // slot is still 1, but nothing heavy is free and the memory shows it.
    h.queue_job(1, "borg-ci-heavy-pr", 5);
    h.tick().await;
    let s = h.status().await;
    let d = drone(&s, "d1");
    assert_eq!(d["theoretical_slots"]["heavy"], 1);
    assert_eq!(d["free_slots"]["heavy"], 0);
    assert_eq!(d["free_mem_mib"], 4 * 1024);
    assert_eq!(d["free_vcpu"], 8);
    assert_eq!(d["free_slots"]["light"], 1);
    assert_eq!(s["classes"]["heavy"]["slots"], 1);
    assert_eq!(s["classes"]["heavy"]["free_slots"], 0);
    assert_eq!(s["classes"]["heavy"]["leases_pending"], 1);

    let m = h.metrics().await;
    for line in [
        "borg_drone_free_mem_mib{drone=\"d1\"} 4096",
        "borg_drone_free_slots{drone=\"d1\",class=\"heavy\"} 0",
        "borg_class_free_slots{class=\"heavy\"} 0",
    ] {
        assert!(m.contains(line), "missing {line}\n{m}");
    }
}

#[tokio::test]
async fn demand_is_split_into_what_non_borg_runners_will_take_and_what_borg_must_place() {
    let h = Harness::start(json!({})).await;
    // No drones at all: every queued job is unmet.
    for i in 0..5 {
        h.queue_job(i, "borg-ci-light-pr", 10);
    }
    for id in 901..904 {
        add_static(&h, id, "borg-ci-light-pr", false);
    }
    add_static(&h, 904, "borg-ci-light-pr", true); // busy: online, not idle
    add_static(&h, 905, "borg-ci-light", false); // main label: not for these jobs
    h.tick().await;

    let s = h.status().await;
    let pr = &s["classes"]["light"]["demand_pr"];
    assert_eq!(pr["queued"], 5);
    assert_eq!(pr["static_online"], 4);
    assert_eq!(pr["static_idle"], 3);
    assert_eq!(pr["claimable_by_static_estimate"], 3);
    assert_eq!(pr["borg_placeable"], 2);
    assert_eq!(s["classes"]["light"]["demand_main"]["static_idle"], 1);
    // The allocator's own unmet still says 5; the placeable view says 2.
    assert_eq!(s["unmet"][0]["count"], 5);
    assert_eq!(s["unmet_placeable"][0]["count"], 2);
    let m = h.metrics().await;
    assert!(
        m.contains("borg_queued_borg_placeable{class=\"light\",scope=\"pr\"} 2"),
        "{m}"
    );
    assert!(m.contains("borg_queued_claimable_by_static{class=\"light\",scope=\"pr\"} 3"));
}

#[tokio::test]
async fn wake_on_lan_ignores_demand_that_non_borg_runners_will_take() {
    let cfg = json!({
        "drone_stale_secs": 1_000_000,
        "wake": {"enabled": true, "after_secs": 300, "hosts": [
            {"name": "sleeper", "mac": "aa:bb:cc:00:00:01"}]}
    });
    let base = OffsetDateTime::now_utc();
    let at = |s: i64| base + time::Duration::seconds(s);

    // Three queued jobs, three idle static runners: nothing for borg to wake for.
    let h = Harness::start(cfg.clone()).await;
    for i in 0..3 {
        h.queue_job(i, "borg-ci-light-pr", 10);
        add_static(&h, 900 + i, "borg-ci-light-pr", false);
    }
    for s in [0, 310, 700] {
        h.queen.tick(at(s)).await.unwrap();
    }
    assert!(h.wakes.0.lock().unwrap().is_empty());

    // Two static runners for three jobs: one is borg's, and it wakes the host.
    let h = Harness::start(cfg).await;
    for i in 0..3 {
        h.queue_job(i, "borg-ci-light-pr", 10);
    }
    for i in 0..2 {
        add_static(&h, 900 + i, "borg-ci-light-pr", false);
    }
    for s in [0, 310] {
        h.queen.tick(at(s)).await.unwrap();
    }
    assert_eq!(h.wakes.0.lock().unwrap().len(), 1);
}

fn events(s: &Value) -> Vec<(String, String)> {
    s["health_events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["drone"].as_str().unwrap().to_owned(),
                e["event"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

async fn status_at(h: &Harness, at: OffsetDateTime) -> Value {
    let s = borg::api::build_status(&h.queen, at.unix_timestamp())
        .await
        .unwrap();
    serde_json::to_value(s).unwrap()
}

#[tokio::test]
async fn silent_drones_are_stale_then_expired_burst_drones_first() {
    let h = Harness::start(json!({
        "drone_expiry": {"after_secs": 3600, "burst_prefixes": ["gcp-burst-"], "burst_after_secs": 600},
        "capacity_alarm": {"enabled": false}
    }))
    .await;
    let burst = h.enroll("gcp-burst-1").await;
    let onprem = h.enroll("pve-1").await;
    h.heartbeat(&burst, &[]).await;
    h.heartbeat(&onprem, &[]).await;
    let t0 = OffsetDateTime::now_utc();
    h.queen.tick(t0).await.unwrap();

    let s = status_at(&h, t0).await;
    assert_eq!(drone(&s, "gcp-burst-1")["availability"], "alive");
    assert_eq!(s["capacity"]["heavy"]["schedulable_slots"], 2);
    assert_eq!(s["stale_drones"], json!([]));

    // Silent past drone_stale_secs: stale, no schedulable capacity, health
    // untouched, and one drone_stale event each.
    let t1 = t0 + time::Duration::seconds(120);
    h.queen.tick(t1).await.unwrap();
    let s = status_at(&h, t1).await;
    let d = drone(&s, "gcp-burst-1");
    assert_eq!(d["availability"], "stale");
    assert!(d["last_seen_age_secs"].as_i64().unwrap() >= 120);
    assert!(d["expires_in_secs"].as_i64().unwrap() <= 480);
    assert_eq!(s["capacity"]["heavy"]["schedulable_slots"], 0);
    assert_eq!(s["stale_drones"], json!(["gcp-burst-1", "pve-1"]));
    let ev = events(&s);
    assert!(
        ev.contains(&("gcp-burst-1".into(), "drone_stale".into())),
        "{ev:?}"
    );
    assert!(
        ev.contains(&("pve-1".into(), "drone_stale".into())),
        "{ev:?}"
    );

    // Past the burst expiry only the burst drone is removed.
    let t2 = t0 + time::Duration::seconds(700);
    h.queen.tick(t2).await.unwrap();
    let s = status_at(&h, t2).await;
    let names: Vec<&str> = s["drones"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["pve-1"]);
    assert!(events(&s).contains(&("gcp-burst-1".into(), "drone_expired".into())));
    assert!(h.heartbeat(&burst, &[]).await.status().is_client_error());

    let t3 = t0 + time::Duration::seconds(3700);
    h.queen.tick(t3).await.unwrap();
    assert_eq!(status_at(&h, t3).await["drones"], json!([]));
}

#[tokio::test]
async fn unmet_main_demand_without_capacity_raises_and_clears_the_alarm() {
    let h = Harness::start(json!({
        "capacity_alarm": {"after_secs": 60, "max_slots": 0}
    }))
    .await;
    h.queue_job(1, "borg-ci-heavy", 5);
    let t0 = OffsetDateTime::now_utc();
    h.queen.tick(t0).await.unwrap();
    let s = status_at(&h, t0).await;
    assert_eq!(s["capacity"]["heavy"]["unmet_main"], 1);
    assert_eq!(s["capacity"]["heavy"]["schedulable_slots"], 0);
    assert_eq!(s["capacity"]["heavy"]["alarm"], false);
    assert!(s["capacity"]["heavy"]["starved_since"].is_i64());

    let t1 = t0 + time::Duration::seconds(61);
    h.queen.tick(t1).await.unwrap();
    let s = status_at(&h, t1).await;
    assert_eq!(s["capacity"]["heavy"]["alarm"], true);
    assert!(events(&s).contains(&("fleet".into(), "capacity_starved".into())));
    assert!(
        h.metrics()
            .await
            .contains("borg_capacity_starved{class=\"heavy\"} 1")
    );

    // A drone that can take the job arrives: the alarm clears.
    let t = h.enroll("d1").await;
    h.heartbeat(&t, &[]).await;
    h.tick().await;
    let s = h.status().await;
    assert_eq!(s["capacity"]["heavy"]["alarm"], false);
    assert!(events(&s).contains(&("fleet".into(), "capacity_restored".into())));
}
