//! The burst enrollment circuit breaker: two consecutive burst VMs that
//! never enroll stop all creates until an operator resets the breaker
//! through the audited control API; an enrollment in between resets the count.

use crate::support;

use serde_json::{Value, json};
use support::Harness;
use time::OffsetDateTime;

const TOKEN: &str = "borg_control_token_for_breaker_tests_0123456789";
const GRACE: i64 = 1500;

fn config(token_path: &std::path::Path) -> Value {
    json!({
        "drone_expiry": {"burst_prefixes": ["gcp-burst-"]},
        "capacity_alarm": {"enabled": false},
        // Heartbeats happen at wall-clock time while ticks use a fixed clock.
        "drone_stale_secs": 1_000_000,
        "control": {"token_path": token_path},
        "burst": {
            "enabled": true, "dry_run": false, "max_drones": 3,
            "budget": {"drone_hours_per_day": 100, "drone_hours_per_month": 500},
            "scale_up": {"classes": ["heavy"], "weights": {"main": 3, "pr": 1},
                         "min_weighted_unmet": 6, "sustained_secs": 600},
            "scale_down": {"idle_secs": 900, "provision_grace_secs": GRACE},
            "gce": {"project": "p", "network": "borg-burst", "subnet": "borg-burst-usw1",
                    "zones": ["us-west1-b", "us-west1-a"], "machine_type": "c2d-highcpu-56"}
        }
    })
}

fn token_file(dir: &tempfile::TempDir) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.path().join("control.token");
    std::fs::write(&p, format!("{TOKEN}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
    p
}

fn clock() -> impl Fn(i64) -> OffsetDateTime {
    let t0 = OffsetDateTime::now_utc().replace_time(time::Time::from_hms(12, 0, 0).unwrap());
    move |s| t0 + time::Duration::seconds(s)
}

async fn status_at(h: &Harness, at: OffsetDateTime) -> Value {
    serde_json::to_value(
        borg::api::build_status(&h.queen, at.unix_timestamp())
            .await
            .unwrap(),
    )
    .unwrap()
}

/// VMs the fake Compute Engine was asked to create so far.
fn creates(h: &Harness) -> usize {
    let st = h.state();
    st.gce_instances.len() + st.gce_deleted.len()
}

/// The enrollment token in the newest live VM's metadata.
fn newest_token(h: &Harness) -> String {
    let st = h.state();
    let inst = st.gce_instances.last().unwrap();
    inst["body"]["metadata"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["key"] == "borg-enroll-token")
        .and_then(|i| i["value"].as_str())
        .unwrap()
        .to_owned()
}

/// Start with sustained demand and run until the first VM is created (t=600).
async fn first_create(h: &Harness, at: &impl Fn(i64) -> OffsetDateTime) {
    h.queue_job(1, "borg-ci-heavy", 5);
    h.queue_job(2, "borg-ci-heavy", 5);
    h.queen.tick(at(0)).await.unwrap();
    h.queen.tick(at(600)).await.unwrap();
    assert_eq!(creates(h), 1);
}

async fn reset(h: &Harness, body: Value) -> reqwest::Response {
    h.http
        .post(format!("{}/v1/control/burst/reset-breaker", h.url))
        .bearer_auth(TOKEN)
        .json(&body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn two_vms_that_never_enroll_trip_the_breaker_and_it_holds_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&token_file(&dir));
    let db = dir.path().join("queen.db");
    let h = Harness::start_file(cfg.clone(), &db).await;
    let at = clock();
    first_create(&h, &at).await;

    // VM 1 never heartbeats: deleted at the grace period, one failure; demand
    // still holds, so the same pass creates VM 2.
    h.queen.tick(at(600 + GRACE)).await.unwrap();
    let s = status_at(&h, at(600 + GRACE)).await;
    assert_eq!(s["burst_breaker"]["failures"], 1, "{}", s["burst_breaker"]);
    assert_eq!(s["burst_breaker"]["tripped"], false);
    assert_eq!(creates(&h), 2);

    // VM 2 never heartbeats either: two in a row trips the breaker.
    h.queen.tick(at(600 + 2 * GRACE)).await.unwrap();
    let s = status_at(&h, at(600 + 2 * GRACE)).await;
    let b = &s["burst_breaker"];
    assert_eq!(b["tripped"], true, "{b}");
    assert_eq!(b["failures"], 2);
    assert_eq!(b["threshold"], 2);
    assert_eq!(b["tripped_at"], at(600 + 2 * GRACE).unix_timestamp());
    assert!(
        b["reason"]
            .as_str()
            .unwrap()
            .contains("2 consecutive burst VMs never enrolled"),
        "{b}"
    );
    assert_eq!(creates(&h), 2, "the tripping pass creates nothing");
    assert!(
        s["health_events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["event"] == "burst_breaker_tripped"),
        "{}",
        s["health_events"]
    );
    let m = h.metrics().await;
    assert!(m.contains("borg_burst_breaker_tripped 1"), "{m}");
    assert!(m.contains("borg_burst_breaker_failures 2"), "{m}");

    // Held: demand persists for hours and nothing more is created.
    for t in [4000, 5000, 8000] {
        h.queen.tick(at(t)).await.unwrap();
    }
    let s = status_at(&h, at(8000)).await;
    assert_eq!(s["burst"]["outcome"], "breaker_tripped", "{}", s["burst"]);
    assert_eq!(creates(&h), 2);
    drop(h);

    // A restart keeps it tripped.
    let again = Harness::start_file(cfg, &db).await;
    again.queue_job(1, "borg-ci-heavy", 5);
    again.queue_job(2, "borg-ci-heavy", 5);
    again.queen.tick(at(9000)).await.unwrap();
    let s = status_at(&again, at(9000)).await;
    assert_eq!(s["burst_breaker"]["tripped"], true);
    assert_eq!(s["burst"]["outcome"], "breaker_tripped");
    assert!(again.state().gce_instances.is_empty());
}

#[tokio::test]
async fn an_operator_reset_is_audited_and_burst_creates_again() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::start(config(&token_file(&dir))).await;
    let at = clock();
    first_create(&h, &at).await;
    h.queen.tick(at(600 + GRACE)).await.unwrap();
    h.queen.tick(at(600 + 2 * GRACE)).await.unwrap();
    assert_eq!(
        status_at(&h, at(600 + 2 * GRACE)).await["burst_breaker"]["tripped"],
        true
    );

    // Unauthenticated and reasonless resets are refused and change nothing.
    let r = h
        .http
        .post(format!("{}/v1/control/burst/reset-breaker", h.url))
        .json(&json!({"actor": "ops", "reason": "fixed it"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let r = reset(&h, json!({"actor": "ops", "reason": ""})).await;
    assert_eq!(r.status(), 400);
    assert_eq!(
        status_at(&h, at(3700)).await["burst_breaker"]["tripped"],
        true
    );

    let r = reset(
        &h,
        json!({"actor": "ops@example", "reason": "queen rebuilt on glibc 2.35"}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["burst_breaker"]["tripped"], false, "{v}");
    assert_eq!(v["burst_breaker"]["failures"], 0);
    let row = &v["recent_audit"][0];
    assert_eq!(row["key"], "burst.breaker");
    assert_eq!(row["actor"], "ops@example");
    assert_eq!(row["reason"], "queen rebuilt on glibc 2.35");
    assert_eq!(row["old_value"]["tripped"], true);
    assert_eq!(row["old_value"]["failures"], 2);
    assert_eq!(row["new_value"]["tripped"], false);

    // Demand still holds: the next pass creates a VM again.
    h.queen.tick(at(4000)).await.unwrap();
    let s = status_at(&h, at(4000)).await;
    assert_eq!(s["burst"]["outcome"], "create", "{}", s["burst"]);
    assert_eq!(creates(&h), 3);
    assert!(h.metrics().await.contains("borg_burst_breaker_tripped 0"));
}

#[tokio::test]
async fn an_enrollment_between_failures_resets_the_count() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::start(config(&token_file(&dir))).await;
    let at = clock();
    first_create(&h, &at).await;

    // VM 1 never enrolls: one failure, and VM 2 is created.
    h.queen.tick(at(600 + GRACE)).await.unwrap();
    assert_eq!(
        status_at(&h, at(600 + GRACE)).await["burst_breaker"]["failures"],
        1
    );
    assert_eq!(creates(&h), 2);

    // VM 2 enrolls: its first heartbeat (the queen scrubs it) is a success.
    // It takes some of the heavy jobs, so queue enough that demand holds.
    for id in 3..=10 {
        h.queue_job(id, "borg-ci-heavy", 5);
    }
    let token = newest_token(&h);
    assert!(h.heartbeat(&token, &[]).await.status().is_success());
    h.queen.tick(at(600 + GRACE + 60)).await.unwrap();
    let s = status_at(&h, at(600 + GRACE + 60)).await;
    assert_eq!(s["burst_breaker"]["failures"], 0, "{}", s["burst_breaker"]);
    assert_eq!(s["burst_breaker"]["tripped"], false);

    // The next sustained window creates VM 3, which never enrolls: one
    // failure since the success, so the breaker stays closed.
    h.queen.tick(at(600 + GRACE + 700)).await.unwrap();
    assert_eq!(creates(&h), 3, "{}", status_at(&h, at(2800)).await["burst"]);
    let vm3 = at(600 + GRACE + 700).unix_timestamp();
    let deleted_at = vm3 + GRACE;
    h.queen
        .tick(OffsetDateTime::from_unix_timestamp(deleted_at).unwrap())
        .await
        .unwrap();
    let s = status_at(&h, OffsetDateTime::from_unix_timestamp(deleted_at).unwrap()).await;
    assert_eq!(s["burst_breaker"]["failures"], 1, "{}", s["burst_breaker"]);
    assert_eq!(s["burst_breaker"]["tripped"], false);
}
