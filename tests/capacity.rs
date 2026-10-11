mod support;

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
