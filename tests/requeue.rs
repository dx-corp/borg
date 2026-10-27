use crate::support;

use serde_json::json;
use support::Harness;
use time::OffsetDateTime;

fn config(dry_run: bool, max: u32) -> serde_json::Value {
    json!({
        "preemption": {"after_secs": 600, "max_per_pass": 1, "min_victim_age_secs": 60},
        "supersede_janitor": {"dry_run": dry_run, "max_cancels_per_pass": max}
    })
}

fn now() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

async fn harness(dry_run: bool, max: u32) -> Harness {
    let h = Harness::start(config(dry_run, max)).await;
    h.enroll("d1").await;
    h
}

async fn requeued_at(h: &Harness, lease: &str) -> Option<i64> {
    h.queen
        .store()
        .preemptions(50)
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.lease_id == lease)
        .unwrap()
        .requeued_at
}

#[tokio::test]
async fn a_completed_runs_job_is_rerun_and_marked() {
    let h = harness(false, 20).await;
    h.add_run(7, "completed", "pull_request", "sha", None);
    h.preempted("l1", 7, 70, now()).await;

    let r = h.requeue_pass().await;
    assert_eq!((r.checked, r.rerun, r.errors), (1, 1, 0), "{r:?}");
    assert_eq!(h.state().reruns, [70]);
    assert!(requeued_at(&h, "l1").await.is_some());

    let r = h.requeue_pass().await;
    assert_eq!((r.checked, r.rerun), (0, 0), "{r:?}");
    assert_eq!(h.state().reruns.len(), 1);

    let status = h.status().await;
    assert_eq!(status["requeue"]["total"]["rerun"], json!(1));
    assert!(h.metrics().await.contains("borg_requeue_rerun_total 1"));
}

#[tokio::test]
async fn an_in_progress_run_is_left_waiting() {
    let h = harness(false, 20).await;
    h.add_run(7, "in_progress", "pull_request", "sha", None);
    h.preempted("l1", 7, 70, now()).await;

    let r = h.requeue_pass().await;
    assert_eq!((r.checked, r.waiting, r.rerun), (1, 1, 0), "{r:?}");
    assert!(h.state().reruns.is_empty());
    assert!(requeued_at(&h, "l1").await.is_none());

    h.state().fresh_status.insert(7, "completed".into());
    let r = h.requeue_pass().await;
    assert_eq!(r.rerun, 1, "{r:?}");
    assert_eq!(h.state().reruns, [70]);
}

#[tokio::test]
async fn a_404_on_rerun_is_terminal() {
    let h = harness(false, 20).await;
    h.add_run(7, "completed", "pull_request", "sha", None);
    h.preempted("l1", 7, 70, now()).await;
    h.state().rerun_status.insert(70, 404);

    let r = h.requeue_pass().await;
    assert_eq!((r.terminal, r.rerun, r.errors), (1, 0, 0), "{r:?}");
    assert!(requeued_at(&h, "l1").await.is_some());
    let r = h.requeue_pass().await;
    assert_eq!(r.checked, 0, "{r:?}");
}

#[tokio::test]
async fn other_rerun_errors_are_retried_next_pass() {
    let h = harness(false, 20).await;
    h.add_run(7, "completed", "pull_request", "sha", None);
    h.preempted("l1", 7, 70, now()).await;
    h.state().rerun_status.insert(70, 500);

    let r = h.requeue_pass().await;
    assert_eq!((r.errors, r.rerun, r.terminal), (1, 0, 0), "{r:?}");
    assert!(requeued_at(&h, "l1").await.is_none());

    h.state().rerun_status.clear();
    let r = h.requeue_pass().await;
    assert_eq!(r.rerun, 1, "{r:?}");
}

#[tokio::test]
async fn quota_refusal_keeps_preempted_job_eligible_without_repeated_requests() {
    let h = harness(false, 20).await;
    h.add_run(7, "completed", "pull_request", "sha", None);
    h.preempted("l1", 7, 70, now()).await;
    let path = format!("/repos/{}/app/actions/jobs/70/rerun", support::ORG);
    h.state().http_replies.insert(
        path.clone(),
        (
            403,
            vec![
                ("x-ratelimit-remaining".into(), "0".into()),
                ("x-ratelimit-reset".into(), (now() + 3600).to_string()),
            ],
            "{\"message\":\"API rate limit exceeded\"}".into(),
        ),
    );

    let report = h.requeue_pass().await;
    assert_eq!((report.errors, report.terminal, report.rerun), (1, 0, 0));
    assert!(requeued_at(&h, "l1").await.is_none());
    let requests = h.state().http_requests.clone();
    tokio::time::timeout(std::time::Duration::from_millis(200), h.requeue_pass())
        .await
        .expect("quota cooldown must return promptly");
    assert_eq!(
        h.state().http_requests,
        requests,
        "cooldown must suppress all HTTP requests"
    );
}

#[tokio::test]
async fn dry_run_makes_no_calls_and_marks_nothing() {
    let h = harness(true, 20).await;
    h.add_run(7, "completed", "pull_request", "sha", None);
    h.preempted("l1", 7, 70, now()).await;

    let r = h.requeue_pass().await;
    assert_eq!((r.checked, r.rerun), (1, 0), "{r:?}");
    assert_eq!(h.state().run_reads, 0);
    assert!(h.state().reruns.is_empty());
    assert!(requeued_at(&h, "l1").await.is_none());
}

#[tokio::test]
async fn the_per_pass_cap_limits_reruns() {
    let h = harness(false, 2).await;
    for i in 1..=5u64 {
        h.add_run(i, "completed", "pull_request", "sha", None);
        h.preempted(
            &format!("l{i}"),
            i,
            i * 10,
            now() - 100 + i64::try_from(i).unwrap(),
        )
        .await;
    }
    let r = h.requeue_pass().await;
    assert_eq!(r.rerun, 2, "{r:?}");
    assert_eq!(h.state().reruns, [10, 20], "oldest first");
    let r = h.requeue_pass().await;
    assert_eq!(r.rerun, 2, "{r:?}");
    let r = h.requeue_pass().await;
    assert_eq!(r.rerun, 1, "{r:?}");
    assert_eq!(h.state().reruns.len(), 5);
}

#[tokio::test]
async fn a_row_older_than_24_hours_is_marked_stale_without_a_call() {
    let h = harness(false, 20).await;
    h.add_run(7, "completed", "pull_request", "sha", None);
    h.preempted("l1", 7, 70, now() - 25 * 3600).await;

    let r = h.requeue_pass().await;
    assert_eq!((r.checked, r.stale, r.rerun), (1, 1, 0), "{r:?}");
    assert_eq!(h.state().run_reads, 0);
    assert!(h.state().reruns.is_empty());
    assert!(requeued_at(&h, "l1").await.is_some());
}

#[tokio::test]
async fn the_requeuer_needs_both_preemption_and_the_janitor() {
    let h = Harness::start(json!({"supersede_janitor": {}})).await;
    assert!(h.queen.requeuer().is_none());
    let h = Harness::start(json!({
        "preemption": {"after_secs": 600, "max_per_pass": 1, "min_victim_age_secs": 60}
    }))
    .await;
    assert!(h.queen.requeuer().is_none());
}
