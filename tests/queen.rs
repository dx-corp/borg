mod support;

use serde_json::{Value, json};
use support::Harness;
use time::OffsetDateTime;

fn labels(mint: &Value) -> Vec<String> {
    mint["labels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn mints_a_pr_runner_with_exact_labels_and_delivers_its_config_until_acknowledged() {
    let h = Harness::start(json!({})).await;
    let token = h.enroll("d1").await;
    h.queue_job(10, "borg-ci-medium-pr", 5);
    assert!(h.leases(&token, &[]).await.is_empty());

    h.tick().await;

    let mint = h.state().mints.clone();
    assert_eq!(mint.len(), 1);
    assert_eq!(
        labels(&mint[0]),
        [
            "self-hosted",
            "Linux",
            "X64",
            "borg-ci-medium",
            "borg-ci-medium-pr"
        ],
        "exact label array sent to generate-jitconfig"
    );
    assert_eq!(mint[0]["runner_group_id"], 1);
    assert_eq!(mint[0]["work_folder"], "_work");
    let name = mint[0]["name"].as_str().unwrap().to_owned();
    assert!(name.starts_with("borg-d1-"), "{name}");

    let leases = h.leases(&token, &[]).await;
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0]["jit_config"], format!("jit-{name}"));
    assert_eq!(leases[0]["runner_name"], name);
    assert_eq!(leases[0]["class"], "medium");
    let id = leases[0]["lease_id"].as_str().unwrap().to_owned();

    assert_eq!(
        h.leases(&token, &[]).await.len(),
        1,
        "redelivered until acknowledged"
    );
    assert!(h.leases(&token, &[&id]).await.is_empty(), "acknowledged");
    assert!(h.leases(&token, &[&id]).await.is_empty());

    // The job is still queued but the pending lease already covers it.
    h.tick().await;
    assert_eq!(
        h.state().mints.len(),
        1,
        "no second mint for the same demand"
    );
}

#[tokio::test]
async fn main_jobs_get_a_runner_with_only_the_bare_class_label() {
    let h = Harness::start(json!({})).await;
    let token = h.enroll("d1").await;
    h.heartbeat(&token, &[]).await;
    h.queue_job(1, "borg-ci-light", 5);
    h.tick().await;
    let mints = h.state().mints.clone();
    assert_eq!(
        labels(&mints[0]),
        ["self-hosted", "Linux", "X64", "borg-ci-light"]
    );
}

#[tokio::test]
async fn default_warm_floor_keeps_one_idle_main_medium_runner() {
    let h =
        Harness::start(json!({"warm_floors": [{"class": "medium", "scope": "main", "count": 1}]}))
            .await;
    let token = h.enroll("d1").await;
    h.heartbeat(&token, &[]).await;
    h.tick().await;
    let mints = h.state().mints.clone();
    assert_eq!(mints.len(), 1);
    assert_eq!(
        labels(&mints[0]).last().map(String::as_str),
        Some("borg-ci-medium")
    );
    h.tick().await;
    assert_eq!(
        h.state().mints.len(),
        1,
        "floor satisfied by the pending runner"
    );
}

#[tokio::test]
async fn drone_api_requires_a_valid_token_and_ownership() {
    let h = Harness::start(json!({})).await;
    let t1 = h.enroll("d1").await;
    let t2 = h.enroll("d2").await;
    h.heartbeat(&t1, &[]).await;
    h.queue_job(1, "borg-ci-light-pr", 1);
    h.tick().await;
    let id = h.leases(&t1, &[]).await[0]["lease_id"]
        .as_str()
        .unwrap()
        .to_owned();

    assert_eq!(h.heartbeat("borg_wrong", &[]).await.status(), 401);
    let no_auth = h
        .http
        .post(format!("{}/v1/heartbeat", h.url))
        .json(&json!({"capacity": support::capacity()}))
        .send()
        .await
        .unwrap();
    assert_eq!(no_auth.status(), 401);

    let url = format!("{}/v1/leases/{id}/result", h.url);
    let body = json!({"exit_code": 0, "duration_secs": 12, "peak_rss_mib": 900});
    let other = h
        .http
        .post(&url)
        .bearer_auth(&t2)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        other.status(),
        404,
        "another drone cannot report this lease"
    );
    let mine = h
        .http
        .post(&url)
        .bearer_auth(&t1)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(mine.status(), 204);
    let again = h
        .http
        .post(&url)
        .bearer_auth(&t1)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 204, "idempotent");
    assert!(h.queen.store().active_leases().await.unwrap().is_empty());
}

#[tokio::test]
async fn busy_runner_marks_lease_running_and_a_vanished_runner_finishes_it() {
    let h = Harness::start(json!({})).await;
    let token = h.enroll("d1").await;
    h.heartbeat(&token, &[]).await;
    h.queue_job(1, "borg-ci-medium", 1);
    h.tick().await;

    h.set_runner("borg-d1-", "online", true);
    h.state().jobs.clear();
    h.tick().await;
    assert_eq!(h.status().await["classes"]["medium"]["leases_running"], 1);

    h.state().runners.clear(); // ephemeral runner deregistered after its job
    h.tick().await;
    let status = h.status().await;
    assert_eq!(status["classes"]["medium"]["leases_running"], 0);
    assert_eq!(status["classes"]["medium"]["leases_pending"], 0);
}

#[tokio::test]
async fn lease_whose_runner_never_came_online_expires_and_is_deregistered() {
    let h = Harness::start(json!({"lease_ttl_secs": 300})).await;
    let token = h.enroll("d1").await;
    h.heartbeat(&token, &[]).await;
    h.queue_job(1, "borg-ci-light", 1);
    h.tick().await;
    let runner_id = h.state().runners[0]["id"].as_u64().unwrap();
    h.state().jobs.clear();

    let later = OffsetDateTime::now_utc() + time::Duration::seconds(301);
    h.queen.tick(later).await.unwrap();
    assert!(h.state().deleted.contains(&runner_id));
    assert!(h.queen.store().active_leases().await.unwrap().is_empty());
}

#[tokio::test]
async fn stale_borg_registrations_are_deleted_but_other_runners_are_not() {
    let h = Harness::start(json!({})).await;
    h.state().runners = vec![
        json!({"id": 900, "name": "borg-x-deadbeef", "status": "offline", "busy": false}),
        json!({"id": 901, "name": "borg-y-busy", "status": "online", "busy": true}),
        json!({"id": 902, "name": "static-runner", "status": "offline", "busy": false}),
    ];
    h.tick().await;
    assert_eq!(h.state().deleted, [900]);
}

#[tokio::test]
async fn app_jwt_is_valid_and_the_installation_token_is_cached() {
    let h = Harness::start(json!({})).await;
    h.tick().await;
    h.tick().await;
    h.tick().await;
    let st = h.state();
    assert_eq!(st.jwt_rejections, 0);
    assert_eq!(st.token_requests, 1);
}

#[tokio::test]
async fn starving_heavy_job_reserves_a_drone_and_shows_in_status() {
    let h = Harness::start(json!({})).await;
    let token = h.enroll("d1").await;
    h.heartbeat(&token, &[]).await;

    h.queue_job(1, "borg-ci-medium", 1);
    h.queue_job(2, "borg-ci-medium", 1);
    h.tick().await;
    assert_eq!(h.state().mints.len(), 2);
    h.set_runner("borg-d1-", "online", true);
    h.state().jobs.clear();
    h.tick().await; // both running; 4 GiB of the 28 GiB left

    h.queue_job(3, "borg-ci-heavy-pr", 300);
    h.queue_job(4, "borg-ci-light-pr", 300);
    h.tick().await;

    assert_eq!(
        h.state().mints.len(),
        2,
        "nothing new lands on the reserved drone"
    );
    let status = h.status().await;
    assert_eq!(
        status["reserved_for"],
        json!([{"drone_id": "d1", "class": "heavy", "scope": "pr"}])
    );
    assert_eq!(
        status["classes"]["heavy"]["oldest_age_secs_pr"]
            .as_u64()
            .map(|a| a >= 300),
        Some(true)
    );
}

#[tokio::test]
async fn metrics_and_status_report_the_queue() {
    let h = Harness::start(json!({})).await;
    let token = h.enroll("d1").await;
    h.heartbeat(&token, &[]).await;
    h.queue_job(1, "borg-ci-medium-pr", 30);
    h.tick().await;

    let metrics = h
        .http
        .get(format!("{}/metrics", h.url))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for line in [
        "borg_queued_jobs{class=\"medium\",scope=\"pr\"} 1",
        "borg_active_leases{class=\"medium\",state=\"pending\"} 1",
        "borg_drones{state=\"fresh\"} 1",
        "borg_last_tick_ok 1",
    ] {
        assert!(metrics.contains(line), "missing {line}\n{metrics}");
    }
    let status = h.status().await;
    assert_eq!(status["classes"]["medium"]["queued_pr"], 1);
    assert_eq!(status["drones"][0]["name"], "d1");
    assert_eq!(status["drones"][0]["fresh"], true);
    assert!(status["classes"]["heavy"]["slots"].as_u64() == Some(1));
}

#[tokio::test]
async fn quarantined_drones_get_no_leases_and_are_counted() {
    let h = Harness::start(json!({})).await;
    let token = h.enroll("d1").await;
    h.heartbeat(&token, &[]).await;
    assert!(
        h.queen
            .store()
            .set_quarantine("d1", Some("bad disk"))
            .await
            .unwrap()
    );
    h.queue_job(1, "borg-ci-medium-pr", 30);
    h.tick().await;

    assert!(h.leases(&token, &[]).await.is_empty());
    let metrics = h.metrics().await;
    for line in [
        "borg_drones{state=\"fresh\"} 1",
        "borg_drones{state=\"quarantined\"} 1",
        "borg_drone_quarantined{drone=\"d1\"} 1",
        "borg_class_slots{class=\"medium\"} 0",
        "borg_oldest_queued_age_seconds{class=\"medium\",scope=\"pr\"} 30",
    ] {
        assert!(metrics.contains(line), "missing {line}\n{metrics}");
    }
    let status = h.status().await;
    assert_eq!(status["drones"][0]["quarantined"], true);
    assert_eq!(status["drones"][0]["quarantine_reason"], "bad disk");

    h.queen.store().set_quarantine("d1", None).await.unwrap();
    h.tick().await;
    assert_eq!(h.leases(&token, &[]).await.len(), 1);
    assert!(
        h.metrics()
            .await
            .contains("borg_drones{state=\"quarantined\"} 0")
    );
}

#[tokio::test]
async fn rollout_metrics_compare_reported_versions_with_the_target() {
    let h = Harness::start(json!({"rollout_target_version": "2.0.0"})).await;
    let (a, b, c) = (
        h.enroll("d1").await,
        h.enroll("d2").await,
        h.enroll("d3").await,
    );
    h.heartbeat_version(&a, "2.0.0").await;
    h.heartbeat_version(&b, "1.9.0\"x").await;
    h.heartbeat(&c, &[]).await;

    let metrics = h.metrics().await;
    for line in [
        "borg_rollout_target_info{version=\"2.0.0\"} 1",
        "borg_rollout_drones{state=\"current\"} 1",
        "borg_rollout_drones{state=\"outdated\"} 1",
        "borg_rollout_drones{state=\"unknown\"} 1",
        "borg_drone_info{drone=\"d1\",version=\"2.0.0\"} 1",
    ] {
        assert!(metrics.contains(line), "missing {line}\n{metrics}");
    }
    // A quote in a drone-reported version must be escaped, not break the format.
    assert!(metrics.contains("version=\"1.9.0\\\"x\""), "{metrics}");
    assert_eq!(h.status().await["rollout"]["current"], 1);
}

#[tokio::test]
async fn no_rollout_series_without_a_target() {
    let h = Harness::start(json!({})).await;
    let token = h.enroll("d1").await;
    h.heartbeat_version(&token, "1.0.0").await;
    let metrics = h.metrics().await;
    assert!(!metrics.contains("borg_rollout_drones{"), "{metrics}");
    assert!(!metrics.contains("borg_rollout_target_info{"), "{metrics}");
    assert!(metrics.contains("borg_drone_info{drone=\"d1\",version=\"1.0.0\"} 1"));
}

#[tokio::test]
async fn mint_failure_is_reported_and_leaves_no_lease() {
    let h = Harness::start(json!({})).await;
    let token = h.enroll("d1").await;
    h.heartbeat(&token, &[]).await;
    h.queue_job(1, "borg-ci-light", 1);
    h.state().fail_mint = true;
    h.tick().await;
    assert!(h.queen.store().active_leases().await.unwrap().is_empty());
    let err = h.status().await["last_error"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(err.contains("mint failed"), "{err}");
}

#[tokio::test]
async fn github_failure_before_minting_aborts_the_tick() {
    let h = Harness::start(json!({})).await;
    let token = h.enroll("d1").await;
    h.heartbeat(&token, &[]).await;
    h.queue_job(1, "borg-ci-light", 1);
    h.state().fail_runner_list = true;
    assert!(h.queen.tick(OffsetDateTime::now_utc()).await.is_err());
    assert!(h.state().mints.is_empty());
}

#[tokio::test]
async fn warm_schedule_raises_the_floor_only_inside_its_window() {
    use time::macros::datetime;

    // The ticks use a fake clock days from the real heartbeat time.
    let h = Harness::start(
        json!({"drone_stale_secs": 100_000_000, "warm_schedule": {"windows": [
            {"class": "light", "scope": "pr", "count": 2, "start": "08:00", "end": "20:00"}
        ]}}),
    )
    .await;
    let token = h.enroll("d1").await;
    h.heartbeat(&token, &[]).await;

    h.queen.tick(datetime!(2026-10-12 07:59 UTC)).await.unwrap();
    assert!(h.state().mints.is_empty());
    assert_eq!(h.status().await["warm_floors"], json!([]));

    h.queen.tick(datetime!(2026-10-12 08:00 UTC)).await.unwrap();
    assert_eq!(h.state().mints.len(), 2);
    let metrics = h.metrics().await;
    assert!(
        metrics.contains("borg_warm_floor{class=\"light\",scope=\"pr\"} 2"),
        "{metrics}"
    );

    // The window closing does not revoke leases already minted.
    h.queen.tick(datetime!(2026-10-12 20:00 UTC)).await.unwrap();
    assert_eq!(h.state().mints.len(), 2);
}

async fn lease_for(h: &Harness, token: &str, label: &str, id: u64) -> String {
    h.queue_job(id, label, 5);
    h.tick().await;
    h.leases(token, &[]).await[0]["lease_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn post_result(h: &Harness, token: &str, lease: &str, body: Value) -> reqwest::StatusCode {
    h.http
        .post(format!("{}/v1/leases/{lease}/result", h.url))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn telemetry_is_stored_and_reported_per_job_class_and_drone() {
    let h = Harness::start(json!({})).await;
    let token = h.enroll("d1").await;
    h.heartbeat(&token, &[]).await;
    let lease = lease_for(&h, &token, "borg-ci-light-pr", 1).await;

    let status = post_result(
        &h,
        &token,
        &lease,
        json!({"exit_code": 1, "duration_secs": 120, "telemetry": {
            "job_name": "lint", "repo": "example-org/app", "run_id": 9,
            "conclusion": "failure", "failure_class": "host", "queue_wait_secs": 25,
            "steps": [{"name": "setup", "duration_secs": 20, "conclusion": "success"},
                      {"name": "cargo clippy", "duration_secs": 90, "conclusion": "failure"}]}}),
    )
    .await;
    assert_eq!(status, 204);

    let r: Value = h
        .http
        .get(format!("{}/report?since_secs=3600", h.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["jobs"], 1);
    assert_eq!(r["by_job"][0]["job_name"], "lint");
    assert_eq!(r["by_job"][0]["total_secs"], 120);
    assert_eq!(r["by_step"][0]["step"], "cargo clippy");
    assert_eq!(r["queue_wait"][0]["class"], "light");
    assert_eq!(r["queue_wait"][0]["max_secs"], 25);
    assert_eq!(r["drones"][0]["drone"], "d1");
    assert_eq!(r["drones"][0]["host_failures"], 1);
    assert_eq!(r["cancellation"]["cancelled"], 0);
}

#[tokio::test]
async fn results_without_or_with_unusable_telemetry_are_still_accepted() {
    let h = Harness::start(json!({})).await;
    let token = h.enroll("d1").await;
    h.heartbeat(&token, &[]).await;
    let lease = lease_for(&h, &token, "borg-ci-light-pr", 1).await;
    let status = post_result(
        &h,
        &token,
        &lease,
        json!({"exit_code": 0, "duration_secs": 5, "telemetry": {"job_name": "", "repo": "x"}}),
    )
    .await;
    assert_eq!(status, 204);
    assert_eq!(
        post_result(
            &h,
            &token,
            &lease,
            json!({"exit_code": 0, "duration_secs": 5})
        )
        .await,
        204
    );
    let r: Value = h
        .http
        .get(format!("{}/report", h.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["jobs"], 0);
}

#[tokio::test]
async fn old_telemetry_is_pruned_by_the_tick() {
    let h = Harness::start(json!({"telemetry_retention_days": 1})).await;
    let token = h.enroll("d1").await;
    h.heartbeat(&token, &[]).await;
    let lease = lease_for(&h, &token, "borg-ci-light-pr", 1).await;
    post_result(
        &h,
        &token,
        &lease,
        json!({"exit_code": 0, "duration_secs": 5,
               "telemetry": {"job_name": "j", "repo": "o/r", "conclusion": "success"}}),
    )
    .await;
    assert_eq!(
        h.queen.store().telemetry_since(0, 10).await.unwrap().len(),
        1
    );
    // Two days later the one-day retention drops it.
    let later = OffsetDateTime::now_utc() + time::Duration::days(2);
    h.queen.tick(later).await.unwrap();
    assert!(
        h.queen
            .store()
            .telemetry_since(0, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn bids_gate_leases_on_published_pressure_and_expired_bids_are_ignored() {
    let h = Harness::start(json!({})).await;
    let token = h.enroll("d1").await;
    let shy = |expires_at: i64| {
        json!({"bids": [{"class": "light", "scope": "pr", "available_slots": 4,
                         "min_pressure": 1000, "expires_at": expires_at}]})
    };
    let far = OffsetDateTime::now_utc().unix_timestamp() + 3600;

    h.heartbeat_with(&token, shy(far)).await;
    h.queue_job(1, "borg-ci-light-pr", 5);
    h.tick().await;
    // One queued job against a mostly idle host is low pressure: the shy bid
    // stays out, and the queen publishes the pressure it computed.
    let resp = h.heartbeat_with(&token, shy(far)).await;
    assert!(resp["leases"].as_array().unwrap().is_empty(), "{resp}");
    let light = resp["pressure"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["class"] == "light")
        .unwrap();
    assert!(light["pressure"].as_u64().unwrap() < 1000, "{resp}");
    assert!(
        h.metrics()
            .await
            .contains("borg_demand_pressure{class=\"light\"}")
    );

    // The same bid, expired, is ignored and the drone is a legacy drone.
    h.heartbeat_with(&token, shy(1)).await;
    h.tick().await;
    let resp = h.heartbeat_with(&token, shy(1)).await;
    assert_eq!(resp["leases"].as_array().unwrap().len(), 1, "{resp}");
}

#[tokio::test]
async fn homeostasis_raises_warm_floors_under_sustained_queueing_and_stays_off_by_default() {
    let cfg = json!({"drone_stale_secs": 100_000_000, "homeostasis": {
        "enabled": true, "target_secs": 60, "raise_after_secs": 100, "cooldown_secs": 10,
        "max_floor": {"light": 3}}});
    let h = Harness::start(cfg).await;
    let token = h.enroll("d1").await;
    h.heartbeat(&token, &[]).await;
    let base = OffsetDateTime::now_utc();
    // One job queued for 5 minutes at the start: far over the 60s target.
    h.queue_job(1, "borg-ci-light-pr", 300);

    for secs in [0, 50, 100, 110, 220] {
        h.queen
            .tick(base + time::Duration::seconds(secs))
            .await
            .unwrap();
    }
    let status = h.status().await;
    let light_pr = status["homeostasis"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["class"] == "light" && r["scope"] == "pr")
        .unwrap();
    assert_eq!(light_pr["level"], 2, "{light_pr}");
    assert_eq!(light_pr["effective_floor"], 2);
    // Two floors against one queued job: the second is an extra warm runner.
    assert_eq!(h.state().mints.len(), 2);
    let metrics = h.metrics().await;
    assert!(
        metrics.contains("borg_warm_floor_dynamic{class=\"light\",scope=\"pr\"} 2"),
        "{metrics}"
    );
    assert!(metrics.contains("borg_warm_floor{class=\"light\",scope=\"pr\"} 2"));

    // Off by default: the same load changes nothing.
    let off = Harness::start(json!({"drone_stale_secs": 100_000_000})).await;
    let t = off.enroll("d1").await;
    off.heartbeat(&t, &[]).await;
    off.queue_job(1, "borg-ci-light-pr", 300);
    for secs in [0, 130, 260] {
        off.queen
            .tick(base + time::Duration::seconds(secs))
            .await
            .unwrap();
    }
    assert_eq!(off.state().mints.len(), 1);
    assert!(
        off.status().await["homeostasis"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn reclaim_reports_are_audited_once_and_shown_in_status_and_metrics() {
    let h = Harness::start(json!({})).await;
    let token = h.enroll("devbox").await;
    let stats = |entries: Value, total: u64| {
        json!({"reclaim": {
            "enabled": true, "dry_run": false, "reclaimed_bytes_total": total,
            "last_run_at": 100, "last_run_reclaimed_bytes": total,
            "last_run_would_reclaim_bytes": 0, "last_run_errors": 1,
            "usage_pct": 91, "last_run_mode": "pressure",
            "kept": {"busy": 2}, "recent": entries
        }})
    };
    let entries = json!([
        {"seq": 1, "at": 100, "path": "/srv/devdata/cargo-target/old-branch", "kind": "cargo_target",
         "size_bytes": 5_368_709_120u64, "reason": "disk 91% >= 85% high water", "outcome": "deleted"},
        {"seq": 2, "at": 101, "path": "/srv/devdata/worktrees/x", "kind": "worktree",
         "size_bytes": 1_073_741_824u64, "reason": "r", "outcome": "failed", "error": "permission denied"}
    ]);
    h.heartbeat_with(&token, stats(entries.clone(), 5_368_709_120))
        .await;
    // The same entries again (a repeated heartbeat) must not duplicate.
    h.heartbeat_with(&token, stats(entries, 5_368_709_120))
        .await;

    let status = h.status().await;
    assert_eq!(
        status["reclaim"]["reclaimed_bytes_logged"],
        5_368_709_120u64
    );
    let events = status["reclaim"]["events"].as_array().unwrap();
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0]["drone"], "devbox");
    assert_eq!(events[1]["path"], "/srv/devdata/cargo-target/old-branch");
    assert_eq!(events[1]["outcome"], "deleted");
    assert_eq!(status["drones"][0]["reclaim"]["usage_pct"], 91);
    assert_eq!(status["drones"][0]["reclaim"]["recent"], json!([]));

    let metrics = h.metrics().await;
    for line in [
        "borg_reclaim_bytes_total{drone=\"devbox\"} 5368709120",
        "borg_reclaim_disk_usage_percent{drone=\"devbox\"} 91",
        "borg_reclaim_errors{drone=\"devbox\"} 1",
    ] {
        assert!(metrics.contains(line), "missing {line}\n{metrics}");
    }
}

#[tokio::test]
async fn github_job_outcomes_feed_profiles_without_drone_telemetry() {
    let h = Harness::start(json!({"profiles": {"refresh_secs": 1, "min_samples": 1}})).await;
    let token = h.enroll("d1").await;
    h.heartbeat(&token, &[]).await;
    let lease_id = lease_for(&h, &token, "borg-ci-light-pr", 71).await;
    let lease = h.queen.store().lease(&lease_id).await.unwrap().unwrap();
    let end = OffsetDateTime::now_utc();
    let start = end - time::Duration::seconds(30);
    let created = start - time::Duration::seconds(12);
    h.state().jobs = vec![json!({
        "id": 71, "run_id": 1, "name": "lint (shard 2)",
        "status": "completed", "conclusion": "success", "labels": ["borg-ci-light-pr"],
        "runner_id": lease.runner_id, "runner_name": lease.runner_name,
        "created_at": created.format(&time::format_description::well_known::Rfc3339).unwrap(),
        "started_at": start.format(&time::format_description::well_known::Rfc3339).unwrap(),
        "completed_at": end.format(&time::format_description::well_known::Rfc3339).unwrap(),
        "steps": [{"name": "cargo check", "number": 1, "conclusion": "success",
            "started_at": start.format(&time::format_description::well_known::Rfc3339).unwrap(),
            "completed_at": end.format(&time::format_description::well_known::Rfc3339).unwrap()}]
    })];
    h.tick().await;
    let rows = h.queen.store().telemetry_since(0, 100).await.unwrap();
    assert_eq!(
        rows.len(),
        1,
        "GitHub completion must close the learning loop"
    );
    assert_eq!(rows[0].lease_id, lease_id);
    assert_eq!(rows[0].duration_secs, 30);
    assert_eq!(rows[0].telemetry.queue_wait_secs, Some(12));
    assert_eq!(rows[0].telemetry.steps[0].duration_secs, 30);
    // The profile refresh has its own bounded cadence.
    h.queen
        .tick(end + time::Duration::seconds(2))
        .await
        .unwrap();
    assert_eq!(h.queen.snapshot().profiles[0].jobs, 1);
    h.tick().await;
    assert_eq!(
        h.queen.store().telemetry_since(0, 100).await.unwrap(),
        rows,
        "repeated observations must preserve the completion timestamp and one sample"
    );
}

#[tokio::test]
async fn observed_running_job_is_collected_after_workflow_leaves_the_queue() {
    let h = Harness::start(json!({})).await;
    let token = h.enroll("d1").await;
    h.heartbeat(&token, &[]).await;
    let lease_id = lease_for(&h, &token, "borg-ci-light-pr", 72).await;
    let lease = h.queen.store().lease(&lease_id).await.unwrap().unwrap();
    let start = OffsetDateTime::now_utc() - time::Duration::seconds(45);
    h.state().jobs = vec![json!({
        "id": 72, "run_id": 1, "name": "rust-test", "status": "in_progress",
        "conclusion": null, "labels": ["borg-ci-light-pr"],
        "runner_id": lease.runner_id, "runner_name": lease.runner_name,
        "created_at": start.format(&time::format_description::well_known::Rfc3339).unwrap(),
        "started_at": start.format(&time::format_description::well_known::Rfc3339).unwrap(),
        "completed_at": null, "steps": []
    })];
    h.tick().await;
    assert_eq!(h.queen.store().pending_jobs().await.unwrap().len(), 1);
    assert!(
        h.queen
            .store()
            .telemetry_since(0, 100)
            .await
            .unwrap()
            .is_empty()
    );
    h.state().queued_runs.clear();
    h.state().jobs[0]["status"] = json!("completed");
    h.state().jobs[0]["conclusion"] = json!("failure");
    h.state().jobs[0]["completed_at"] = json!(
        OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap()
    );
    h.tick().await;
    let rows = h.queen.store().telemetry_since(0, 100).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].telemetry.conclusion,
        borg::telemetry::Conclusion::Failure
    );
    assert_eq!(
        rows[0].telemetry.failure_class,
        borg::telemetry::FailureClass::Unknown,
        "GitHub failure alone must never quarantine a host"
    );
    assert!(h.queen.store().pending_jobs().await.unwrap().is_empty());
}

#[tokio::test]
async fn github_telemetry_requires_exact_runner_identity_and_preserves_drone_evidence() {
    let h = Harness::start(json!({})).await;
    let token = h.enroll("d1").await;
    h.heartbeat(&token, &[]).await;
    let lease_id = lease_for(&h, &token, "borg-ci-light-pr", 73).await;
    let lease = h.queen.store().lease(&lease_id).await.unwrap().unwrap();
    let start = OffsetDateTime::now_utc() - time::Duration::seconds(60);
    let end = start + time::Duration::seconds(30);
    let mut job = json!({
        "id": 73, "run_id": 1, "name": "untrusted name", "status": "completed", "conclusion": "success",
        "runner_id": lease.runner_id + 1, "runner_name": lease.runner_name,
        "created_at": start.format(&time::format_description::well_known::Rfc3339).unwrap(),
        "started_at": start.format(&time::format_description::well_known::Rfc3339).unwrap(),
        "completed_at": end.format(&time::format_description::well_known::Rfc3339).unwrap()
    });
    h.state().jobs = vec![job.clone()];
    h.tick().await;
    assert!(
        h.queen
            .store()
            .telemetry_since(0, 100)
            .await
            .unwrap()
            .is_empty()
    );
    job["runner_id"] = json!(lease.runner_id);
    job["runner_name"] = json!("other-runner");
    h.state().jobs = vec![job.clone()];
    h.tick().await;
    assert!(
        h.queen
            .store()
            .telemetry_since(0, 100)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        post_result(
            &h,
            &token,
            &lease_id,
            json!({
                "exit_code": 1, "duration_secs": 60,
                "telemetry": {"job_name": "drone evidence", "repo": "example-org/app", "run_id": 1,
                    "conclusion": "failure", "failure_class": "host"}
            })
        )
        .await,
        reqwest::StatusCode::NO_CONTENT
    );
    job["runner_name"] = json!(lease.runner_name);
    h.state().jobs = vec![job];
    h.tick().await;
    let rows = h.queen.store().telemetry_since(0, 100).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].telemetry.job_name, "drone evidence");
    assert_eq!(
        rows[0].telemetry.failure_class,
        borg::telemetry::FailureClass::Host
    );
}

fn running_feedback_job(lease: &borg::store::LeaseRow, id: u64) -> Value {
    let start = OffsetDateTime::now_utc() - time::Duration::seconds(45);
    json!({
        "id": id, "run_id": 1, "name": "rust-test", "status": "in_progress",
        "conclusion": null, "runner_id": lease.runner_id, "runner_name": lease.runner_name,
        "created_at": start.format(&time::format_description::well_known::Rfc3339).unwrap(),
        "started_at": start.format(&time::format_description::well_known::Rfc3339).unwrap(),
        "completed_at": null
    })
}

#[tokio::test]
async fn slow_feedback_does_not_delay_admission_or_overwrite_concurrent_drone_evidence() {
    let h = Harness::start(json!({})).await;
    let token = h.enroll("d1").await;
    h.heartbeat(&token, &[]).await;
    let lease_id = lease_for(&h, &token, "borg-ci-light-pr", 80).await;
    let lease = h.queen.store().lease(&lease_id).await.unwrap().unwrap();
    h.state().jobs = vec![running_feedback_job(&lease, 80)];
    h.tick().await;
    let gate = std::sync::Arc::new(tokio::sync::Notify::new());
    {
        let mut state = h.state();
        state.jobs[0]["status"] = json!("completed");
        state.jobs[0]["conclusion"] = json!("success");
        state.jobs[0]["completed_at"] = json!(
            OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap()
        );
        state.queued_runs.clear();
        state.job_gate = Some(gate.clone());
    }
    // A second workflow is queued while the old one has left the active list.
    h.queue_job(81, "borg-ci-medium-pr", 5);
    // Completed history is absent from the active workflow's jobs response.
    h.state().hide_completed_jobs = true;
    let queen = h.queen.clone();
    let tick = tokio::spawn(async move { queen.tick(OffsetDateTime::now_utc()).await });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while h.state().job_requests == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        h.state().mints.len(),
        2,
        "admission must precede a slow completion lookup"
    );
    assert_eq!(
        post_result(
            &h,
            &token,
            &lease_id,
            json!({
                "exit_code": 1, "duration_secs": 60,
                "telemetry": {"job_name": "drone evidence", "repo": "example-org/app", "run_id": 1,
                    "conclusion": "failure", "failure_class": "host"}
            })
        )
        .await,
        reqwest::StatusCode::NO_CONTENT
    );
    gate.notify_one();
    tick.await.unwrap().unwrap();
    let rows = h.queen.store().telemetry_since(0, 100).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].telemetry.failure_class,
        borg::telemetry::FailureClass::Host
    );
    assert_eq!(rows[0].telemetry.job_name, "drone evidence");
}

#[tokio::test]
async fn terminal_unusable_or_deleted_jobs_are_not_retried() {
    for deleted in [false, true] {
        let h = Harness::start(json!({})).await;
        let token = h.enroll("d1").await;
        h.heartbeat(&token, &[]).await;
        let id = lease_for(&h, &token, "borg-ci-light-pr", 82).await;
        let lease = h.queen.store().lease(&id).await.unwrap().unwrap();
        h.state().jobs = vec![running_feedback_job(&lease, 82)];
        h.tick().await;
        h.state().queued_runs.clear();
        if deleted {
            h.state().jobs.clear();
        } else {
            h.state().jobs[0]["status"] = json!("completed");
            h.state().jobs[0]["conclusion"] = json!("cancelled");
            // No completion timestamp: no duration may be invented.
        }
        h.tick().await;
        h.tick().await;
        assert_eq!(h.state().job_requests, 1);
        assert!(h.queen.store().pending_jobs().await.unwrap().is_empty());
        assert!(
            h.queen
                .store()
                .telemetry_since(0, 100)
                .await
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn a_new_queen_recovers_completion_from_file_backed_bindings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queen.sqlite");
    let h = Harness::start_file(json!({}), &path).await;
    let token = h.enroll("d1").await;
    h.heartbeat(&token, &[]).await;
    let id = lease_for(&h, &token, "borg-ci-light-pr", 83).await;
    let lease = h.queen.store().lease(&id).await.unwrap().unwrap();
    h.state().jobs = vec![running_feedback_job(&lease, 83)];
    h.tick().await;
    let mut job = h.state().jobs[0].clone();
    job["status"] = json!("completed");
    job["conclusion"] = json!("success");
    job["completed_at"] = json!(
        OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap()
    );
    let restarted = Harness::start_file(json!({}), &path).await;
    restarted.state().runners = h.state().runners.clone();
    restarted.state().jobs = vec![job];
    restarted.tick().await;
    restarted.tick().await;
    assert_eq!(restarted.state().job_requests, 1);
    assert_eq!(
        restarted
            .queen
            .store()
            .telemetry_since(0, 100)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn a_timed_out_feedback_lookup_is_bounded_and_retried_later() {
    let h = Harness::start(json!({})).await;
    let token = h.enroll("d1").await;
    h.heartbeat(&token, &[]).await;
    let id = lease_for(&h, &token, "borg-ci-light-pr", 84).await;
    let lease = h.queen.store().lease(&id).await.unwrap().unwrap();
    h.state().jobs = vec![running_feedback_job(&lease, 84)];
    h.tick().await;
    let gate = std::sync::Arc::new(tokio::sync::Notify::new());
    h.state().queued_runs.clear();
    h.state().job_gate = Some(gate.clone());
    tokio::time::timeout(std::time::Duration::from_secs(3), h.tick())
        .await
        .unwrap();
    assert_eq!(h.state().job_requests, 1);
    assert_eq!(h.queen.store().pending_jobs().await.unwrap().len(), 1);
    h.state().job_gate = None;
    h.state().jobs[0]["status"] = json!("completed");
    h.state().jobs[0]["conclusion"] = json!("success");
    h.state().jobs[0]["completed_at"] = json!(
        OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap()
    );
    gate.notify_one();
    h.tick().await;
    assert_eq!(h.state().job_requests, 2);
    assert_eq!(
        h.queen.store().telemetry_since(0, 100).await.unwrap().len(),
        1
    );
}
