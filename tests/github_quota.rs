//! Exercise the real HTTP paths against GitHub's quota headers.
use crate::support::{Harness, JANITOR_TOKEN, ORG};
use borg::{
    config::JanitorSettings,
    github::{GithubClient, GithubError, TokenSource},
    janitor::Janitor,
    mirror_auth::MirrorAuth,
    requeue::Requeuer,
    secret::Secret,
};
use serde_json::json;
use std::{sync::Arc, time::Duration};

fn client(h: &Harness) -> GithubClient {
    GithubClient::new(&h.gh_url, ORG, "12345", h.private_pem.as_bytes()).unwrap()
}
fn settings() -> JanitorSettings {
    serde_json::from_value(json!({"dry_run":false})).unwrap()
}
fn runners_path() -> String {
    format!("/orgs/{ORG}/actions/runners")
}

#[tokio::test]
async fn quota_cooldown_is_shared_by_scheduler_janitor_and_requeuer_but_not_mirror_app() {
    let h = Harness::start(json!({})).await;
    h.enroll("d1").await;
    h.preempted(
        "p1",
        7,
        70,
        time::OffsetDateTime::now_utc().unix_timestamp(),
    )
    .await;
    let scheduler = client(&h);
    let shared = scheduler.rate_limit();
    let cleanup_client = Arc::new(client(&h).with_rate_limit(shared.clone()));
    let janitor =
        Janitor::with_token_source(&h.gh_url, TokenSource::App(cleanup_client), settings())
            .unwrap();
    let requeuer = Requeuer::new(&h.gh_url, Secret::new(JANITOR_TOKEN), &settings())
        .unwrap()
        .with_rate_limit(shared);
    h.state().http_replies.insert(
        runners_path(),
        (
            403,
            vec![("retry-after".into(), "3600".into())],
            "{}".into(),
        ),
    );
    assert!(matches!(
        scheduler.list_runners().await,
        Err(GithubError::RateLimited { .. })
    ));
    let requests = h.state().http_requests.clone();
    tokio::time::timeout(Duration::from_millis(200), async {
        assert!(scheduler.list_runners().await.is_err());
        janitor.pass(&[format!("{ORG}/app")]).await;
        let r = requeuer
            .pass(
                h.queen.store(),
                time::OffsetDateTime::now_utc().unix_timestamp(),
            )
            .await
            .unwrap();
        assert_eq!((r.errors, r.terminal), (1, 0));
    })
    .await
    .expect("cooldown must not wait for quota reset");
    assert_eq!(requests, h.state().http_requests);
    assert!(h.state().mints.is_empty());
    assert!(h.state().cancelled.is_empty());
    assert!(h.state().reruns.is_empty());
    let mirror = MirrorAuth::new(client(&h), ORG, &[format!("{ORG}/app")]);
    mirror
        .credential()
        .await
        .expect("independent contents credential remains available");
}

#[tokio::test]
async fn successful_last_quota_runner_mint_is_not_repeated() {
    let h = Harness::start(json!({})).await;
    let api = client(&h);
    let path = format!("/orgs/{ORG}/actions/runners/generate-jitconfig");
    h.state().http_replies.insert(
        path.clone(),
        (
            201,
            vec![
                ("x-ratelimit-remaining".into(), "0".into()),
                (
                    "x-ratelimit-reset".into(),
                    (time::OffsetDateTime::now_utc().unix_timestamp() + 3600).to_string(),
                ),
            ],
            json!({"runner":{"id":42},"encoded_jit_config":"test-jit"}).to_string(),
        ),
    );
    assert_eq!(
        api.mint_jit_runner("r1", 1, &[], "_work")
            .await
            .unwrap()
            .runner_id,
        42
    );
    assert!(matches!(
        api.mint_jit_runner("r2", 1, &[], "_work").await,
        Err(GithubError::RateLimited { .. })
    ));
    assert_eq!(h.state().http_requests[&path], 1);
}

#[tokio::test]
async fn cooldown_recovers_automatically_and_does_not_reuse_stale_inventory() {
    let h = Harness::start(json!({})).await;
    let api = client(&h);
    let path = runners_path();
    h.state().http_replies.insert(
        path.clone(),
        (429, vec![("retry-after".into(), "1".into())], "{}".into()),
    );
    assert!(matches!(
        api.list_runners().await,
        Err(GithubError::RateLimited { .. })
    ));
    h.state().http_replies.clear();
    h.state()
        .runners
        .push(json!({"id":8,"name":"fresh-runner","status":"online","busy":false}));
    assert!(api.list_runners().await.is_err());
    assert_eq!(h.state().http_requests[&path], 1);
    tokio::time::sleep(Duration::from_secs(2)).await;
    let runners = api.list_runners().await.unwrap();
    assert_eq!(runners[0].name, "fresh-runner");
    assert_eq!(h.state().http_requests[&path], 2);
}
