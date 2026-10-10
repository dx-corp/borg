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
