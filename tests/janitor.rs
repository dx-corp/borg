mod support;

use serde_json::json;
use support::Harness;

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
