//! The authenticated control API: burst overrides within ceilings, the audit
//! log, persistence across a restart, and the burst rule obeying an override.

use crate::support;

use serde_json::{Value, json};
use support::Harness;
use time::OffsetDateTime;

const TOKEN: &str = "borg_control_token_for_tests_0123456789abcdef";

fn config(token_path: &std::path::Path) -> Value {
    json!({
        "drone_expiry": {"burst_prefixes": ["gcp-burst-"]},
        "capacity_alarm": {"enabled": false},
        "control": {"token_path": token_path},
        "burst": {
            "enabled": true, "max_drones": 2,
            "budget": {"drone_hours_per_day": 24, "drone_hours_per_month": 150},
            "ceilings": {"max_drones": 4, "drone_hours_per_day": 48, "drone_hours_per_month": 300},
            "scale_up": {"classes": ["heavy"], "weights": {"main": 3, "pr": 1},
                         "min_weighted_unmet": 6, "sustained_secs": 600},
            "scale_down": {"idle_secs": 900},
            "gce": {"project": "p", "network": "borg-burst", "subnet": "borg-burst-usw1",
                    "zones": ["us-west1-b"], "machine_type": "c2d-highcpu-56"}
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

async fn get(h: &Harness, path: &str, token: Option<&str>) -> reqwest::Response {
    let mut req = h.http.get(format!("{}{path}", h.url));
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    req.send().await.unwrap()
}

async fn post(h: &Harness, body: Value) -> reqwest::Response {
    h.http
        .post(format!("{}/v1/control/burst", h.url))
        .bearer_auth(TOKEN)
        .json(&body)
        .send()
        .await
        .unwrap()
}

async fn status_at(h: &Harness, at: OffsetDateTime) -> Value {
    serde_json::to_value(
        borg::api::build_status(&h.queen, at.unix_timestamp())
            .await
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn control_api_is_absent_without_a_control_block() {
    let h = Harness::start(json!({})).await;
    assert_eq!(
        get(&h, "/v1/control/burst", Some(TOKEN)).await.status(),
        404
    );
    assert_eq!(
        get(&h, "/v1/control/audit", Some(TOKEN)).await.status(),
        404
    );
}

#[tokio::test]
async fn control_api_requires_the_token() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::start(config(&token_file(&dir))).await;
    assert_eq!(get(&h, "/v1/control/burst", None).await.status(), 401);
    assert_eq!(
        get(
            &h,
            "/v1/control/burst",
            Some("borg_wrong_token_wrong_token_wrong_token")
        )
        .await
        .status(),
        401
    );
    let ok = get(&h, "/v1/control/burst", Some(TOKEN)).await;
    assert_eq!(ok.status(), 200);
    let v: Value = ok.json().await.unwrap();
    assert_eq!(v["burst_control"]["enabled"]["value"], true);
    assert_eq!(v["burst_control"]["enabled"]["source"], "config");
    assert_eq!(v["burst_control"]["max_drones"]["ceiling"], 4);
    assert!(v["capacity"]["heavy"].is_object(), "{v}");
}

#[tokio::test]
async fn writes_need_actor_and_reason_and_stay_under_the_ceilings() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::start(config(&token_file(&dir))).await;
    let r = post(&h, json!({"actor": "lead", "reason": "", "enabled": false})).await;
    assert_eq!(r.status(), 400);
    assert!(r.text().await.unwrap().contains("reason"));
    let r = post(&h, json!({"reason": "no actor", "enabled": false})).await;
    assert_eq!(r.status(), 400);
    let r = post(
        &h,
        json!({"actor": "lead", "reason": "more", "max_drones": 5}),
    )
    .await;
    assert_eq!(r.status(), 400);
    assert!(r.text().await.unwrap().contains("ceiling 4"));
    let r = post(
        &h,
        json!({"actor": "lead", "reason": "sneak", "enabled": true, "dry_run": false}),
    )
    .await;
    assert_eq!(r.status(), 400, "unknown fields are refused");
    let audit: Value = get(&h, "/v1/control/audit", Some(TOKEN))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(audit, json!([]), "refused writes leave no audit row");

    let r = post(
        &h,
        json!({"actor": "lead", "reason": "heavy CI starving", "max_drones": 4,
               "drone_hours_per_day": 48}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["burst_control"]["max_drones"]["value"], 4);
    assert_eq!(v["burst_control"]["max_drones"]["source"], "override");
    assert_eq!(v["burst_control"]["max_drones"]["set_by"], "lead");
    assert_eq!(v["burst_control"]["drone_hours_per_day"]["value"], 48.0);
    assert_eq!(v["recent_audit"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn turning_burst_off_stops_creates_drains_and_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&token_file(&dir));
    let db = dir.path().join("queen.db");
    let h = Harness::start_file(cfg.clone(), &db).await;
    h.queue_job(1, "borg-ci-heavy", 5);
    h.queue_job(2, "borg-ci-heavy", 5);
    let t0 = OffsetDateTime::now_utc().replace_time(time::Time::from_hms(12, 0, 0).unwrap());
    let at = |s: i64| t0 + time::Duration::seconds(s);

    // Demand sustained: the (dry-run) rule creates a drone.
    h.queen.tick(at(0)).await.unwrap();
    h.queen.tick(at(600)).await.unwrap();
    let s = status_at(&h, at(600)).await;
    assert_eq!(s["burst"]["outcome"], "create", "{}", s["burst"]);
    assert_eq!(s["burst"]["enabled"], true);

    let r = post(
        &h,
        json!({"actor": "ops@example", "reason": "GCP spend review", "enabled": false}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let s = status_at(&h, at(600)).await;
    assert_eq!(s["burst_control"]["enabled"]["value"], false);
    assert_eq!(s["burst_control"]["enabled"]["source"], "override");

    // Demand still holds, but nothing is created and the drone is drained,
    // then deleted once it is empty.
    h.queen.tick(at(1400)).await.unwrap();
    let s = status_at(&h, at(1400)).await;
    assert_eq!(s["burst"]["outcome"], "disabled", "{}", s["burst"]);
    assert_eq!(s["burst"]["enabled"], false);
    let recent = s["burst"]["recent"].as_array().unwrap();
    assert!(
        recent
            .iter()
            .any(|d| d["action"] == "drain" && d["detail"] == "burst disabled"),
        "{recent:?}"
    );
    assert!(h.metrics().await.contains("borg_burst_enabled 0"));
    h.queen.tick(at(1460)).await.unwrap();
    let s = status_at(&h, at(1460)).await;
    assert_eq!(s["burst"]["drones"]["managed"], 0, "{}", s["burst"]);

    // The audit log records who, why, and the old and new values.
    let audit: Value = get(&h, "/v1/control/audit", Some(TOKEN))
        .await
        .json()
        .await
        .unwrap();
    let row = &audit[0];
    assert_eq!(row["actor"], "ops@example");
    assert_eq!(row["reason"], "GCP spend review");
    assert_eq!(row["key"], "burst.enabled");
    assert_eq!(row["old_value"], true);
    assert_eq!(row["old_source"], "config");
    assert_eq!(row["new_value"], false);
    assert_eq!(row["new_source"], "override");
    drop(h);

    // A fresh queen over the same store keeps the override and its audit.
    let again = Harness::start_file(cfg, &db).await;
    let v: Value = get(&again, "/v1/control/burst", Some(TOKEN))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(v["burst_control"]["enabled"]["value"], false);
    assert_eq!(v["burst_control"]["enabled"]["set_by"], "ops@example");
    assert_eq!(v["recent_audit"].as_array().unwrap().len(), 1);

    // Clearing the override restores the configured value.
    let r = again
        .http
        .post(format!("{}/v1/control/burst", again.url))
        .bearer_auth(TOKEN)
        .json(&json!({"actor": "lead", "reason": "spend review done", "clear": ["enabled"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["burst_control"]["enabled"]["value"], true);
    assert_eq!(v["burst_control"]["enabled"]["source"], "config");
}
