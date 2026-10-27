use crate::support;

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

fn job_in_run(run: u64, id: u64, label: &str) -> Value {
    let created = OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    json!({"id": id, "run_id": run, "status": "queued", "labels": [label], "created_at": created})
}

#[tokio::test]
async fn a_5xx_on_one_runs_jobs_skips_that_run_and_the_tick_goes_on() {
    let h = Harness::start(json!({})).await;
    let token = h.enroll("d1").await;
    h.heartbeat(&token, &[]).await;
    {
        let mut st = h.state();
        st.jobs_by_run = true;
        st.queued_runs = vec![1, 2];
        st.jobs = vec![
            job_in_run(1, 11, "borg-ci-light"),
            job_in_run(2, 21, "borg-ci-light"),
        ];
        st.jobs_fail_runs.insert(2, 502);
    }
    h.queen.tick(OffsetDateTime::now_utc()).await.unwrap();
    let s = h.status().await;
    assert!(s["last_error"].is_null(), "{}", s["last_error"]);
    // Run 1's job is still seen and placed; run 2 is read again next poll.
    assert_eq!(h.state().mints.len(), 1, "{}", s["queue"]);
    h.state().jobs_fail_runs.clear();
    h.tick().await;
    assert_eq!(h.state().mints.len(), 2);
}

#[tokio::test]
async fn an_auth_failure_on_one_runs_jobs_still_aborts_the_tick() {
    let h = Harness::start(json!({})).await;
    let token = h.enroll("d1").await;
    h.heartbeat(&token, &[]).await;
    {
        let mut st = h.state();
        st.jobs_by_run = true;
        st.queued_runs = vec![1, 2];
        st.jobs = vec![
            job_in_run(1, 11, "borg-ci-light"),
            job_in_run(2, 21, "borg-ci-light"),
        ];
        st.jobs_fail_runs.insert(2, 401);
    }
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
async fn job_and_run_endpoints_show_the_drone_and_its_host_load() {
    let h = Harness::start(
        json!({"admission": {"psi_soft_pm": 300, "psi_hard_pm": 600, "facts_max_age_secs": 180}}),
    )
    .await;
    let token = h.enroll("shared-box").await;
    h.heartbeat(&token, &[]).await;
    let lease_id = lease_for(&h, &token, "borg-ci-heavy", 4242).await;
    let lease = h.queen.store().lease(&lease_id).await.unwrap().unwrap();
    let start = OffsetDateTime::now_utc() - time::Duration::seconds(45);
    let ts = start
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    h.state().jobs = vec![json!({
        "id": 4242, "run_id": 1, "name": "ui-e2e (shard 3)", "status": "in_progress",
        "conclusion": null, "labels": ["borg-ci-heavy"],
        "runner_id": lease.runner_id, "runner_name": lease.runner_name,
        "created_at": ts, "started_at": ts, "completed_at": null, "steps": []
    })];
    h.tick().await;

    // Seen running, no result yet: the drone is known, samples are not.
    let job: Value = h
        .http
        .get(format!("{}/jobs/4242", h.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(job["drone"], "shared-box");
    // The mock serves this job under queued run 1.
    assert_eq!(job["run_id"], 1);
    assert_eq!(job["job_name"], "ui-e2e (shard 3)");
    assert_eq!(job["start"], Value::Null);
    assert_eq!(job["slow_host"], false);

    let status = post_result(
        &h,
        &token,
        &lease_id,
        json!({"exit_code": 1, "duration_secs": 100, "host": {
            "start": {"at": 1000, "cpus": 16, "load": [77.0, 60.0, 50.0],
                      "cgroup_cpu_psi": {"some_avg10": 54.9, "some_avg60": 52.4},
                      "host_cpu_usage_usec": 0, "drone_cpu_usage_usec": 0,
                      "work_free_mib": 30000, "cache_free_mib": 30000, "active_leases": 2},
            "end": {"at": 1100, "cpus": 16, "host_cpu_usage_usec": 1550000000u64,
                    "drone_cpu_usage_usec": 300000000, "active_leases": 1}}}),
    )
    .await;
    assert_eq!(status, 204);

    let job: Value = h
        .http
        .get(format!("{}/jobs/4242", h.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(job["exit_code"], 1);
    assert_eq!(job["duration_secs"], 100);
    assert_eq!(job["start"]["active_leases"], 2);
    assert_eq!(job["during"]["host_cpu_busy_cores"], 15.5);
    assert_eq!(job["during"]["drone_cpu_busy_cores"], 3.0);
    assert_eq!(job["slow_host"], true);
    let reasons = job["slow_host_reasons"].as_array().unwrap();
    assert!(
        reasons[0]
            .as_str()
            .unwrap()
            .starts_with("cpu pressure 54.9% at start"),
        "{reasons:?}"
    );

    let run: Value = h
        .http
        .get(format!("{}/runs/1", h.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(run["jobs"].as_array().unwrap().len(), 1);
    assert_eq!(run["jobs"][0]["job_id"], 4242);

    let missing = h
        .http
        .get(format!("{}/jobs/1", h.url))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);
    let empty: Value = h
        .http
        .get(format!("{}/runs/2", h.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(empty["jobs"], json!([]));
}

#[tokio::test]
async fn admission_falls_back_to_load_without_psi_and_light_only_sheds_main_heavy() {
    let h = Harness::start(json!({"admission": {
        "psi_soft_pm": 300, "psi_light_only_pm": 500, "psi_hard_pm": 600, "facts_max_age_secs": 180}}))
    .await;
    let token = h.enroll("no-psi").await;
    let now = OffsetDateTime::now_utc().unix_timestamp();
    // No PSI, load 93 on 16 saturated CPUs: estimated 828 permille, closed.
    h.heartbeat_with(
        &token,
        json!({"env_facts": {"observed_at": now, "load_per_cpu_pm": 5838, "cpu_busy_pm": 990}}),
    )
    .await;
    h.queue_job(1, "borg-ci-heavy", 5);
    h.tick().await;
    let s = h.status().await;
    let a = &s["drones"][0]["admission"];
    assert_eq!(a["state"], "closed", "{a}");
    assert_eq!(a["cpu_psi_pm"], Value::Null);
    assert_eq!(a["cpu_load_pm"], 828);
    assert!(
        h.state().mints.is_empty(),
        "an overloaded drone without PSI gets no main heavy"
    );
    assert!(
        h.metrics()
            .await
            .contains("borg_drone_cpu_load_pm{drone=\"no-psi\"} 828")
    );

    // With PSI 540 the drone is light-only: main light placed, main heavy not.
    h.heartbeat_with(
        &token,
        json!({"env_facts": {"observed_at": now, "cpu_psi_some_avg10_pm": 540,
                             "load_per_cpu_pm": 5838, "cpu_busy_pm": 990}}),
    )
    .await;
    h.queue_job(2, "borg-ci-light", 5);
    h.tick().await;
    assert_eq!(
        h.status().await["drones"][0]["admission"]["state"],
        "light_only"
    );
    let minted: Vec<String> = h
        .state()
        .mints
        .iter()
        .map(|m| m["labels"].to_string())
        .collect();
    assert_eq!(minted.len(), 1, "{minted:?}");
    assert!(minted[0].contains("borg-ci-light"), "{minted:?}");
}

#[tokio::test]
async fn admission_reads_the_window_mean_so_one_burst_does_not_close_a_drone() {
    let h = Harness::start(json!({"admission": {
        "psi_soft_pm": 300, "psi_light_only_pm": 500, "psi_hard_pm": 600, "facts_max_age_secs": 300}}))
    .await;
    let token = h.enroll("d1").await;
    let now = OffsetDateTime::now_utc().unix_timestamp();
    // Three quiet heartbeats, then one burst at 700: the window mean is 213.
    for (i, pm) in [50, 60, 40, 700].into_iter().enumerate() {
        let at = now - 170 + i64::try_from(i).unwrap() * 10;
        h.heartbeat_with(
            &token,
            json!({"env_facts": {"observed_at": at, "cpu_psi_some_avg10_pm": pm}}),
        )
        .await;
    }
    h.queue_job(1, "borg-ci-heavy", 5);
    h.tick().await;
    let a = h.status().await["drones"][0]["admission"].clone();
    assert_eq!(a["state"], "open", "{a}");
    assert_eq!(a["cpu_psi_pm"], 213, "{a}");
    assert_eq!(h.state().mints.len(), 1, "the heavy job is placed");
    assert!(
        h.metrics()
            .await
            .contains("borg_drone_cpu_psi_pm{drone=\"d1\"} 213")
    );

    // Sustained pressure closes it; a repeated reading counts once.
    for i in 0..6 {
        h.heartbeat_with(
            &token,
            json!({"env_facts": {"observed_at": now - 50 + i * 10, "cpu_psi_some_avg10_pm": 1000}}),
        )
        .await;
    }
    h.heartbeat_with(
        &token,
        json!({"env_facts": {"observed_at": now, "cpu_psi_some_avg10_pm": 1000}}),
    )
    .await;
    h.queue_job(2, "borg-ci-heavy", 5);
    h.tick().await;
    let a = h.status().await["drones"][0]["admission"].clone();
    assert_eq!(a["state"], "closed", "{a}");
    assert_eq!(a["cpu_psi_pm"], 685, "{a}");
    assert_eq!(h.state().mints.len(), 1, "no new grant on a closed drone");
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

#[tokio::test]
async fn starving_main_demand_preempts_one_running_pr_lease_and_tells_its_drone() {
    let h = Harness::start(json!({
        "preemption": {"after_secs": 600, "max_per_pass": 1, "min_victim_age_secs": 60}
    }))
    .await;
    let token = h.enroll("d1").await;
    h.heartbeat(&token, &[]).await;
    h.queue_job(1, "borg-ci-medium-pr", 1);
    h.queue_job(2, "borg-ci-medium-pr", 1);
    h.tick().await;
    h.set_runner("borg-d1-", "online", true);
    let started = (OffsetDateTime::now_utc() - time::Duration::minutes(10))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let running: Vec<Value> = h
        .state()
        .mints
        .iter()
        .enumerate()
        .map(|(i, m)| {
            json!({"id": 100 + i, "run_id": 9, "status": "in_progress", "labels": [],
                   "created_at": started, "started_at": started, "runner_name": m["name"]})
        })
        .collect();
    h.state().jobs = running;
    h.queue_job(3, "borg-ci-medium", 700);

    h.tick().await;
    h.tick().await;

    let rows = h.queen.store().preemptions(20).await.unwrap();
    assert_eq!(rows.len(), 1, "one cancellation covers the demand");
    assert_eq!((rows[0].run_id, rows[0].job_id / 100), (1, 1));
    let body = h.heartbeat_with(&token, json!({})).await;
    assert_eq!(body["cancel_leases"], json!([rows[0].lease_id]));
    let status = h.status().await;
    assert_eq!(
        status["preemptions"][0]["lease_id"],
        json!(rows[0].lease_id)
    );
    assert!(
        h.metrics()
            .await
            .contains("borg_preemptions_total{from_scope=\"pr\",to_scope=\"main\"} 1")
    );
}

#[tokio::test]
async fn nothing_is_preempted_unless_configured() {
    let h = Harness::start(json!({})).await;
    let token = h.enroll("d1").await;
    h.heartbeat(&token, &[]).await;
    h.queue_job(1, "borg-ci-medium", 5000);
    h.tick().await;
    assert!(h.queen.store().preemptions(20).await.unwrap().is_empty());
    let body = h.heartbeat_with(&token, json!({})).await;
    assert_eq!(body["cancel_leases"], json!([]));
}

fn standby_cfg() -> Value {
    json!({"standby": {"classes": ["medium"], "per_drone": 1}})
}

async fn standby_beat(h: &Harness, token: &str, held: &[&str], active: &[&str]) -> Value {
    h.heartbeat_with(
        token,
        json!({"trust": "deploy-only", "held_standby": held, "active_leases": active}),
    )
    .await
}

#[tokio::test]
async fn standby_runners_are_minted_for_deploy_drones_only_and_never_expire_by_ttl() {
    let mut cfg = standby_cfg();
    cfg["lease_ttl_secs"] = json!(300);
    let h = Harness::start(cfg).await;
    let dep = h.enroll("dep").await;
    let shared = h.enroll("shr").await;
    standby_beat(&h, &dep, &[], &[]).await;
    h.heartbeat(&shared, &[]).await;

    h.tick().await;

    let mints = h.state().mints.clone();
    assert_eq!(mints.len(), 1, "one standby for the deploy-only drone");
    assert!(mints[0]["name"].as_str().unwrap().starts_with("borg-dep-"));
    assert_eq!(
        labels(&mints[0]),
        ["self-hosted", "Linux", "X64", "borg-ci-medium-deploy"],
        "deploy labels only"
    );
    let body = standby_beat(&h, &dep, &[], &[]).await;
    assert_eq!(body["leases"], json!([]), "not an ordinary lease");
    assert_eq!(body["start_standby"], json!([]));
    let grants = body["standby"].as_array().unwrap();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0]["scope"], "deploy");
    let name = mints[0]["name"].as_str().unwrap();
    assert_eq!(grants[0]["jit_config"], format!("jit-{name}"));
    let id = grants[0]["lease_id"].as_str().unwrap().to_owned();
    let shared_body = h.heartbeat_with(&shared, json!({})).await;
    assert_eq!(shared_body["standby"], json!([]));

    // Acknowledged by listing it as held: not resent.
    let body = standby_beat(&h, &dep, &[&id], &[]).await;
    assert_eq!(body["standby"], json!([]));

    // Far beyond the pending-lease TTL the standby is still there, its
    // registration is not deleted and no replacement is minted.
    let runner_id = h.state().runners[0]["id"].as_u64().unwrap();
    let later = OffsetDateTime::now_utc() + time::Duration::seconds(301);
    h.queen.tick(later).await.unwrap();
    assert!(!h.state().deleted.contains(&runner_id));
    assert_eq!(h.state().mints.len(), 1);
    let lease = h.queen.store().lease(&id).await.unwrap().unwrap();
    assert_eq!(lease.state, borg::store::LeaseStatus::Standby);
    // The reaper treats the offline standby registration as owned.
    assert_eq!(h.state().runners.len(), 1);
}

#[tokio::test]
async fn a_lost_standby_config_is_resent_and_a_forgotten_one_is_dropped() {
    let h = Harness::start(standby_cfg()).await;
    let dep = h.enroll("dep").await;
    standby_beat(&h, &dep, &[], &[]).await;
    h.tick().await;
    let id = standby_beat(&h, &dep, &[], &[]).await["standby"][0]["lease_id"]
        .as_str()
        .unwrap()
        .to_owned();
    // The drone lost its copy: the queen sends it again.
    let body = standby_beat(&h, &dep, &[], &[]).await;
    assert_eq!(body["standby"][0]["lease_id"], json!(id));
    // A held id the queen has no standby for is dropped.
    let body = standby_beat(&h, &dep, &[&id, "gone1"], &[]).await;
    assert_eq!(body["drop_standby"], json!(["gone1"]));
}

#[tokio::test]
async fn queued_deploy_demand_starts_the_standby_instead_of_minting_and_a_replacement_follows() {
    let h = Harness::start(standby_cfg()).await;
    let dep = h.enroll("dep").await;
    standby_beat(&h, &dep, &[], &[]).await;
    h.tick().await;
    let id = standby_beat(&h, &dep, &[], &[]).await["standby"][0]["lease_id"]
        .as_str()
        .unwrap()
        .to_owned();

    h.queue_job(1, "borg-ci-medium-deploy", 5);
    h.tick().await;
    assert_eq!(h.state().mints.len(), 1, "the standby absorbs the demand");
    let body = standby_beat(&h, &dep, &[&id], &[]).await;
    assert_eq!(body["start_standby"], json!([id]));
    assert_eq!(body["leases"], json!([]));

    // The drone started it: it reports the id as active and no longer held.
    standby_beat(&h, &dep, &[], &[&id]).await;
    let lease = h.queen.store().lease(&id).await.unwrap().unwrap();
    assert_eq!(lease.state, borg::store::LeaseStatus::Pending);
    assert!(lease.jit_config.is_none());

    // The runner takes the job; the queen marks the lease running and mints
    // a new standby.
    h.set_runner("borg-dep-", "online", true);
    h.state().jobs.clear();
    h.tick().await;
    let lease = h.queen.store().lease(&id).await.unwrap().unwrap();
    assert_eq!(lease.state, borg::store::LeaseStatus::Running);
    assert_eq!(h.state().mints.len(), 2, "replacement standby minted");
    let body = standby_beat(&h, &dep, &[], &[&id]).await;
    assert_eq!(body["start_standby"], json!([]));
    assert_eq!(body["standby"].as_array().unwrap().len(), 1);

    // The job finishes and the ephemeral runner deregisters.
    h.state().runners.retain(|r| r["busy"] != json!(true));
    h.tick().await;
    let lease = h.queen.store().lease(&id).await.unwrap().unwrap();
    assert_eq!(lease.state, borg::store::LeaseStatus::Done);
    assert_eq!(h.state().mints.len(), 2);
}

#[tokio::test]
async fn a_standby_started_during_an_outage_is_reconciled_when_the_queen_returns() {
    let h = Harness::start(standby_cfg()).await;
    let dep = h.enroll("dep").await;
    standby_beat(&h, &dep, &[], &[]).await;
    h.tick().await;
    let id = standby_beat(&h, &dep, &[], &[]).await["standby"][0]["lease_id"]
        .as_str()
        .unwrap()
        .to_owned();
    // The drone fail-opened on its own and the job ran while the queen could
    // not hear it; the runner is gone from GitHub before the next heartbeat.
    h.set_runner("borg-dep-", "online", true);
    h.tick().await;
    assert_eq!(
        h.queen.store().lease(&id).await.unwrap().unwrap().state,
        borg::store::LeaseStatus::Running
    );
    h.state().runners.retain(|r| r["busy"] != json!(true));
    h.tick().await;
    assert_eq!(
        h.queen.store().lease(&id).await.unwrap().unwrap().state,
        borg::store::LeaseStatus::Done
    );
    // The drone, back online, still lists it as held; the queen does not
    // revive it and tells the drone to forget it.
    let body = standby_beat(&h, &dep, &[&id], &[]).await;
    assert_eq!(body["drop_standby"], json!([id]));
}

fn mirror_cfg() -> Value {
    json!({"mirror_auth": {
        "app_id": "12345",
        "private_key_path": "/etc/borg/mirror-app.pem",
        "repositories": ["app"]
    }})
}

#[tokio::test]
async fn the_minter_scopes_the_token_to_contents_read_on_the_named_repositories() {
    let h = Harness::start(json!({})).await;
    let client =
        borg::github::GithubClient::new(&h.gh_url, support::ORG, "12345", h.private_pem.as_bytes())
            .unwrap();
    let (token, expires_at) = client
        .installation_token_scoped(&json!({"contents": "read"}), &["app".to_owned()])
        .await
        .unwrap();
    assert_eq!(token.expose(), support::MIRROR_TOKEN);
    assert!(expires_at > OffsetDateTime::now_utc().unix_timestamp());
    let bodies = h.state().scoped_requests.clone();
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["permissions"]["contents"], "read");
    assert_eq!(bodies[0]["repositories"], json!(["app"]));
    assert_eq!(
        bodies[0]["permissions"].as_object().unwrap().len(),
        1,
        "no other permission is requested"
    );
}

#[tokio::test]
async fn heartbeats_carry_the_mirror_credential_only_when_configured() {
    let off = Harness::start(json!({})).await;
    let token = off.enroll("d1").await;
    let body = off.heartbeat_with(&token, json!({})).await;
    assert!(body.get("mirror_credential").is_none(), "{body}");
    assert_eq!(
        off.status().await["mirror_auth"],
        json!({"configured": false, "expires_at": null})
    );

    let on = Harness::start(mirror_cfg()).await;
    let token = on.enroll("d1").await;
    let body = on.heartbeat_with(&token, json!({})).await;
    let cred = &body["mirror_credential"];
    assert_eq!(cred["token"], support::MIRROR_TOKEN);
    assert_eq!(cred["repositories"], json!(["example-org/app"]));
    let expires_at = cred["expires_at"].as_i64().unwrap();
    assert!(expires_at > OffsetDateTime::now_utc().unix_timestamp());

    let status = on.status().await;
    assert_eq!(
        status["mirror_auth"],
        json!({"configured": true, "expires_at": expires_at})
    );
    assert!(
        !status.to_string().contains(support::MIRROR_TOKEN),
        "/status never shows the token"
    );
}

#[tokio::test]
async fn the_mirror_token_is_cached_until_ten_minutes_of_life_remain() {
    let h = Harness::start(mirror_cfg()).await;
    let token = h.enroll("d1").await;
    for _ in 0..3 {
        h.heartbeat_with(&token, json!({})).await;
    }
    assert_eq!(
        h.state().scoped_requests.len(),
        1,
        "heartbeats reuse the cached token"
    );

    // A token that lives five minutes is always inside the re-mint window.
    let short = Harness::start(mirror_cfg()).await;
    short.state().token_ttl_secs = Some(300);
    let token = short.enroll("d1").await;
    short.heartbeat_with(&token, json!({})).await;
    short.heartbeat_with(&token, json!({})).await;
    assert_eq!(short.state().scoped_requests.len(), 2);
}

#[tokio::test]
async fn job_listing_reads_runs_concurrently_and_keeps_run_order() {
    let h = Harness::start(json!({})).await;
    let created = OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    {
        let mut st = h.state();
        st.jobs_by_run = true;
        st.jobs_delay_ms = 40;
        st.queued_runs = (1..=16).rev().collect();
        st.jobs = (1..=16u64)
            .map(|run| {
                json!({"id": 100 + run, "run_id": run, "status": "queued",
                       "labels": ["borg-ci-light"], "created_at": created})
            })
            .collect();
    }
    let client =
        borg::github::GithubClient::new(&h.gh_url, support::ORG, "12345", h.private_pem.as_bytes())
            .unwrap();
    let jobs = client
        .workflow_jobs(&format!("{}/app", support::ORG))
        .await
        .unwrap();
    assert_eq!(
        jobs.iter().map(|j| (j.run_id, j.id)).collect::<Vec<_>>(),
        (1..=16u64).map(|run| (run, 100 + run)).collect::<Vec<_>>(),
        "every run's jobs, in run-id order"
    );
    let peak = h.state().jobs_max_inflight;
    assert!(peak > 1, "jobs listings overlap (peak {peak})");
    assert!(peak <= 8, "at most eight listings at once (peak {peak})");
}
