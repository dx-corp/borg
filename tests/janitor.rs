mod support;

use serde_json::json;
use support::Harness;

fn with(flags: serde_json::Value) -> serde_json::Value {
    let mut j = json!({"dry_run": false});
    j.as_object_mut()
        .unwrap()
        .extend(flags.as_object().unwrap().clone());
    json!({"supersede_janitor": j})
}

fn enabled(dry_run: bool, max: u32) -> serde_json::Value {
    json!({"supersede_janitor": {"dry_run": dry_run, "max_cancels_per_pass": max}})
}

#[tokio::test]
async fn cancels_only_queued_runs_of_superseded_heads_and_is_idempotent() {
    let h = Harness::start(enabled(false, 20)).await;
    h.queue_pr_run(1, 10, "old", "new");
    h.queue_pr_run(2, 10, "new", "new");
    h.queue_pr_run(3, 11, "old11", "new11");

    let r = h.janitor_pass().await;
    assert_eq!(
        (r.considered, r.superseded, r.cancelled),
        (3, 2, 2),
        "{r:?}"
    );
    let mut cancelled = h.state().cancelled.clone();
    cancelled.sort_unstable();
    assert_eq!(cancelled, [1, 3]);

    // A second pass finds nothing left to do.
    let r = h.janitor_pass().await;
    assert_eq!(
        (r.considered, r.superseded, r.cancelled),
        (1, 0, 0),
        "{r:?}"
    );
    assert_eq!(h.state().cancelled.len(), 2);
}

#[tokio::test]
async fn dry_run_cancels_nothing() {
    let h = Harness::start(json!({"supersede_janitor": {}})).await;
    h.queue_pr_run(1, 10, "old", "new");
    let r = h.janitor_pass().await;
    assert_eq!((r.would_cancel, r.cancelled), (1, 0), "{r:?}");
    assert!(h.state().cancelled.is_empty());
}

#[tokio::test]
async fn a_run_that_started_after_listing_is_left_alone() {
    let h = Harness::start(enabled(false, 20)).await;
    h.queue_pr_run(1, 10, "old", "new");
    h.state().fresh_status.insert(1, "in_progress".into());
    let r = h.janitor_pass().await;
    assert_eq!((r.raced, r.cancelled), (1, 0), "{r:?}");
    assert!(h.state().cancelled.is_empty());
}

#[tokio::test]
async fn an_already_finished_run_answering_409_is_not_an_error() {
    let h = Harness::start(enabled(false, 20)).await;
    h.queue_pr_run(1, 10, "old", "new");
    h.state().uncancellable.push(1);
    let r = h.janitor_pass().await;
    assert_eq!((r.raced, r.cancelled, r.errors), (1, 0, 0), "{r:?}");
}

#[tokio::test]
async fn the_per_pass_cap_defers_the_rest_to_later_passes() {
    let h = Harness::start(enabled(false, 2)).await;
    for id in 1..=5 {
        h.queue_pr_run(id, 10 + id, "old", "new");
    }
    let r = h.janitor_pass().await;
    assert_eq!((r.cancelled, r.deferred), (2, 3), "{r:?}");
    let r = h.janitor_pass().await;
    assert_eq!((r.cancelled, r.deferred), (2, 1), "{r:?}");
    let r = h.janitor_pass().await;
    assert_eq!((r.cancelled, r.deferred), (1, 0), "{r:?}");
    assert_eq!(h.state().cancelled.len(), 5);
}

#[tokio::test]
async fn runs_without_exactly_one_pull_request_are_never_touched() {
    let h = Harness::start(enabled(false, 20)).await;
    {
        let mut st = h.state();
        st.pulls.insert(10, "new".into());
        st.pr_runs
            .push(json!({"id": 1, "status": "queued", "event": "pull_request",
            "head_sha": "old", "pull_requests": []}));
        st.pr_runs
            .push(json!({"id": 2, "status": "queued", "event": "pull_request",
            "head_sha": "old", "pull_requests": [{"number": 10}, {"number": 11}]}));
        st.pr_runs
            .push(json!({"id": 3, "status": "queued", "event": "merge_group",
            "head_sha": "old", "pull_requests": [{"number": 10}]}));
    }
    let r = h.janitor_pass().await;
    assert_eq!((r.considered, r.cancelled), (0, 0), "{r:?}");
}

#[tokio::test]
async fn an_unknown_pull_request_is_an_error_not_a_cancellation() {
    let h = Harness::start(enabled(false, 20)).await;
    h.queue_pr_run(1, 10, "old", "new");
    h.state().pulls.clear();
    let r = h.janitor_pass().await;
    assert_eq!((r.errors, r.cancelled), (1, 0), "{r:?}");
}

#[tokio::test]
async fn the_janitor_uses_its_own_token() {
    // The fake accepts only the janitor token on these endpoints; a pass that
    // used the queen's installation token would see 401s and cancel nothing.
    let h = Harness::start(enabled(false, 20)).await;
    h.queue_pr_run(1, 10, "old", "new");
    assert_eq!(h.janitor_pass().await.cancelled, 1);
}

#[tokio::test]
async fn running_stale_runs_are_cancelled_only_with_cancel_running() {
    let off = Harness::start(enabled(false, 20)).await;
    off.state().pulls.insert(10, "new".into());
    off.add_run(1, "in_progress", "pull_request", "old", Some(10));
    let r = off.janitor_pass().await;
    assert_eq!((r.considered, r.cancelled_running), (0, 0), "{r:?}");
    assert!(off.state().cancelled.is_empty());

    let on = Harness::start(with(json!({"cancel_running": true}))).await;
    on.state().pulls.insert(10, "new".into());
    on.add_run(1, "in_progress", "pull_request", "old", Some(10));
    on.add_run(2, "in_progress", "pull_request", "new", Some(10));
    on.add_run(3, "in_progress", "pull_request", "old", Some(10));
    on.state().fresh_status.insert(3, "completed".into());
    let r = on.janitor_pass().await;
    assert_eq!(
        (r.superseded, r.cancelled, r.cancelled_running, r.raced),
        (2, 0, 1, 1),
        "{r:?}"
    );
    assert_eq!(on.state().cancelled, [1]);
}

#[tokio::test]
async fn queued_duplicates_keep_the_lowest_id() {
    let h = Harness::start(with(json!({"dedupe_same_sha": true}))).await;
    h.state().pulls.insert(10, "sha".into());
    for id in [9, 4, 6] {
        h.add_run(id, "queued", "pull_request", "sha", Some(10));
    }
    let r = h.janitor_pass().await;
    assert_eq!(
        (r.duplicates, r.dedup_cancelled, r.cancelled),
        (2, 2, 0),
        "{r:?}"
    );
    let mut cancelled = h.state().cancelled.clone();
    cancelled.sort_unstable();
    assert_eq!(cancelled, [6, 9]);
    let r = h.janitor_pass().await;
    assert_eq!(r.duplicates, 0, "{r:?}");
}

#[tokio::test]
async fn a_running_duplicate_is_kept_and_never_cancelled() {
    let h = Harness::start(with(json!({"dedupe_same_sha": true}))).await;
    h.state().pulls.insert(10, "sha".into());
    h.add_run(5, "queued", "pull_request", "sha", Some(10));
    h.add_run(7, "in_progress", "pull_request", "sha", Some(10));
    h.add_run(8, "in_progress", "pull_request", "sha", Some(10));
    h.add_run(9, "queued", "pull_request", "sha", Some(10));
    let r = h.janitor_pass().await;
    assert_eq!((r.duplicates, r.dedup_cancelled), (3, 2), "{r:?}");
    let mut cancelled = h.state().cancelled.clone();
    cancelled.sort_unstable();
    assert_eq!(cancelled, [5, 9]);
}

#[tokio::test]
async fn dispatch_and_merge_group_duplicates_are_untouched() {
    let h = Harness::start(with(json!({"dedupe_same_sha": true}))).await;
    for id in 1..=3 {
        h.add_run(id, "queued", "workflow_dispatch", "sha", None);
    }
    for id in 4..=5 {
        h.add_run(id, "queued", "merge_group", "sha", None);
    }
    let r = h.janitor_pass().await;
    assert_eq!((r.duplicates, r.dedup_cancelled), (0, 0), "{r:?}");
    assert!(h.state().cancelled.is_empty());
}

#[tokio::test]
async fn dry_run_counts_both_kinds_and_calls_no_cancel() {
    let h = Harness::start(json!({"supersede_janitor":
        {"cancel_running": true, "dedupe_same_sha": true}}))
    .await;
    h.state().pulls.insert(10, "new".into());
    h.add_run(1, "in_progress", "pull_request", "old", Some(10));
    h.add_run(2, "queued", "pull_request", "new", Some(10));
    h.add_run(3, "queued", "pull_request", "new", Some(10));
    let r = h.janitor_pass().await;
    assert_eq!(
        (
            r.would_cancel,
            r.cancelled,
            r.cancelled_running,
            r.dedup_cancelled
        ),
        (2, 0, 0, 0),
        "{r:?}"
    );
    assert!(h.state().cancelled.is_empty());
}

#[tokio::test]
async fn totals_reach_status_and_metrics() {
    let h = Harness::start(enabled(false, 20)).await;
    h.queue_pr_run(1, 10, "old", "new");
    h.janitor_pass().await;
    h.janitor_pass().await;
    let status = h.status().await;
    assert_eq!(status["janitor"]["passes"], 2);
    assert_eq!(status["janitor"]["total"]["cancelled"], 1);
    assert_eq!(status["janitor"]["last"]["cancelled"], 0);
    let metrics = h
        .http
        .get(format!("{}/metrics", h.url))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains("borg_janitor_cancelled_total 1"),
        "{metrics}"
    );
}
