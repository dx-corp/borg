use crate::support;

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
    assert_eq!(
        (r.uncancellable, r.raced, r.cancelled, r.errors),
        (1, 0, 0, 0),
        "{r:?}"
    );
}

#[tokio::test]
async fn the_run_s_own_pull_request_head_needs_no_pulls_api() {
    // The queen's App may lack pull-request read access: with the head in the
    // run's entry the pulls API is never asked.
    let h = Harness::start(enabled(false, 20)).await;
    h.state().pr_runs.push(json!({
        "id": 1, "status": "queued", "event": "pull_request", "head_sha": "old",
        "pull_requests": [{"number": 10, "head": {"sha": "new"}}]
    }));
    h.state().pr_runs.push(json!({
        "id": 2, "status": "queued", "event": "pull_request", "head_sha": "new",
        "pull_requests": [{"number": 10, "head": {"sha": "new"}}]
    }));
    let r = h.janitor_pass().await;
    assert_eq!((r.superseded, r.cancelled, r.errors), (1, 1, 0), "{r:?}");
    assert_eq!(h.state().cancelled, [1]);
}

#[tokio::test]
async fn runs_of_closed_pull_requests_are_cancelled_for_listed_workflows_only() {
    let h = Harness::start(with(json!({
        "cancel_running": true,
        "closed_pr_workflows": [".github/workflows/pr-gates.yml", "platform-ci"]
    })))
    .await;
    let closed = |id: u64, status: &str, path: &str, name: &str, repo: &str| {
        json!({"id": id, "status": status, "event": "pull_request", "head_sha": "a",
            "head_branch": "feat/x", "path": path, "name": name,
            "head_repository": {"full_name": repo}, "pull_requests": []})
    };
    {
        let mut st = h.state();
        st.pr_runs.push(closed(
            1,
            "queued",
            ".github/workflows/pr-gates.yml",
            "pr-gates",
            "example-org/app",
        ));
        st.pr_runs.push(closed(
            2,
            "in_progress",
            ".github/workflows/platform-ci.yml",
            "platform-ci",
            "example-org/app",
        ));
        // Not listed: a teardown that runs on the closed event.
        st.pr_runs.push(closed(
            3,
            "queued",
            ".github/workflows/teardown.yml",
            "teardown",
            "example-org/app",
        ));
        // A fork's run never names a pull request either.
        st.pr_runs.push(closed(
            4,
            "queued",
            ".github/workflows/pr-gates.yml",
            "pr-gates",
            "fork/r",
        ));
    }
    let r = h.janitor_pass().await;
    assert_eq!(
        (r.closed_pr, r.cancelled, r.cancelled_running),
        (2, 1, 1),
        "{r:?}"
    );
    let mut cancelled = h.state().cancelled.clone();
    cancelled.sort_unstable();
    assert_eq!(cancelled, [1, 2]);
}

#[tokio::test]
async fn older_queued_main_pushes_are_cancelled_and_running_ones_kept() {
    let h = Harness::start(with(json!({"superseded_push_branches": ["main"]}))).await;
    let push = |id: u64, status: &str, sha: &str, branch: &str| {
        json!({"id": id, "workflow_id": 1, "status": status, "event": "push",
            "head_sha": sha, "head_branch": branch, "pull_requests": []})
    };
    {
        let mut st = h.state();
        st.pr_runs.push(push(1, "queued", "a", "main"));
        st.pr_runs.push(push(2, "queued", "b", "main"));
        st.pr_runs.push(push(3, "queued", "c", "main"));
        st.pr_runs.push(push(4, "queued", "a", "feat"));
        st.pr_runs.push(push(5, "queued", "b", "feat"));
    }
    let r = h.janitor_pass().await;
    assert_eq!((r.superseded_push, r.cancelled), (2, 2), "{r:?}");
    let mut cancelled = h.state().cancelled.clone();
    cancelled.sort_unstable();
    assert_eq!(cancelled, [1, 2]);
}

#[tokio::test]
async fn a_superseded_main_push_with_a_running_job_is_kept() {
    // GitHub shows a run as queued while any job waits for a runner, so a
    // publish run building one image while its siblings wait reads `queued`.
    let h = Harness::start(with(json!({"superseded_push_branches": ["main"]}))).await;
    let push = |id: u64, sha: &str| {
        json!({"id": id, "workflow_id": 1, "status": "queued", "event": "push",
            "head_sha": sha, "head_branch": "main", "pull_requests": []})
    };
    {
        let mut st = h.state();
        st.pr_runs.push(push(1, "a"));
        st.pr_runs.push(push(2, "b"));
        st.jobs
            .push(json!({"id": 10, "run_id": 1, "status": "in_progress"}));
        st.jobs
            .push(json!({"id": 11, "run_id": 1, "status": "queued"}));
    }
    let r = h.janitor_pass().await;
    assert_eq!(
        (r.superseded_push, r.started_push, r.cancelled),
        (1, 1, 0),
        "{r:?}"
    );
    assert!(h.state().cancelled.is_empty());
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
async fn a_queued_duplicate_with_a_running_job_is_kept() {
    // GitHub's workflow stays queued while a sibling job waits for a runner.
    // cancel_running must never opt same-head duplicate cleanup into killing it.
    for cancel_running in [false, true] {
        let h = Harness::start(with(json!({
            "dedupe_same_sha": true, "cancel_running": cancel_running
        })))
        .await;
        h.state().pulls.insert(10, "sha".into());
        h.add_run(4, "queued", "pull_request", "sha", Some(10));
        h.add_run(9, "queued", "pull_request", "sha", Some(10));
        h.add_run_job(9, "active", "in_progress", Some("runner-1"));
        h.add_run_job(9, "waiting", "queued", None);

        let r = h.janitor_pass().await;
        assert_eq!(
            (r.duplicates, r.dedup_cancelled, r.errors, r.raced),
            (1, 0, 0, 1),
            "{r:?}"
        );
        assert!(h.state().cancelled.is_empty());
    }
}

#[tokio::test]
async fn an_unreadable_duplicate_job_response_preserves_the_run() {
    let h = Harness::start(with(json!({"dedupe_same_sha": true}))).await;
    h.state().pulls.insert(10, "sha".into());
    h.add_run(4, "queued", "pull_request", "sha", Some(10));
    h.add_run(9, "queued", "pull_request", "sha", Some(10));
    // An invalid provider response cannot establish that no job is running.
    h.state()
        .jobs
        .push(json!({"id": 90, "run_id": 9, "status": false}));

    let r = h.janitor_pass().await;
    assert_eq!((r.dedup_cancelled, r.errors), (0, 1), "{r:?}");
    assert!(h.state().cancelled.is_empty());
}

#[tokio::test]
async fn keeping_an_active_duplicate_leaves_the_cancel_budget_for_an_idle_one() {
    let h = Harness::start(with(json!({
        "dedupe_same_sha": true, "max_cancels_per_pass": 1
    })))
    .await;
    h.state().pulls.insert(10, "sha".into());
    for id in [4, 6, 9] {
        h.add_run(id, "queued", "pull_request", "sha", Some(10));
    }
    h.add_run_job(6, "active", "in_progress", Some("runner-1"));

    let r = h.janitor_pass().await;
    assert_eq!((r.dedup_cancelled, r.deferred, r.raced), (1, 0, 1), "{r:?}");
    assert_eq!(h.state().cancelled, [9]);
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
    h.add_run(6, "in_progress", "repository_dispatch", "sha", None);
    h.add_run(7, "queued", "repository_dispatch", "sha", None);
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
    assert!(status["janitor"]["since"].as_i64().unwrap() > 0);
    assert!(status["janitor"]["last_pass_at"].as_i64().unwrap() > 0);
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

const PROMOTE: &str = ".github/workflows/promote-runtime-images.yml";
const EVENT: &str = "platform_runtime_images_published";

fn branch_run(
    id: u64,
    status: &str,
    path: &str,
    branch: &str,
    sha: &str,
    created: &str,
) -> serde_json::Value {
    json!({"id": id, "status": status, "event": "pull_request", "head_sha": sha,
        "head_branch": branch, "path": path, "name": path,
        "head_repository": {"full_name": "example-org/app"}, "pull_requests": [],
        "created_at": created})
}

fn pull(number: u64, state: &str, sha: &str, closed_at: Option<&str>) -> serde_json::Value {
    json!({"number": number, "state": state, "head": {"sha": sha}, "closed_at": closed_at})
}

#[tokio::test]
async fn runs_of_closed_or_moved_pull_requests_are_cancelled_for_any_workflow() {
    // No closed_pr_workflows: the verdict comes from the branch's pull requests.
    let h = Harness::start(with(json!({"cancel_running": true}))).await;
    {
        let mut st = h.state();
        // Merged after the review run was created: the run is waste.
        st.pr_runs.push(branch_run(
            1,
            "queued",
            "review.yml",
            "feat/a",
            "a1",
            "2026-10-23T10:00:00Z",
        ));
        st.pulls_by_head.insert(
            "feat/a".into(),
            vec![pull(10, "closed", "a1", Some("2026-10-23T10:00:05Z"))],
        );
        // The `closed` event's own handler: created after the close, kept.
        st.pr_runs.push(branch_run(
            2,
            "queued",
            "teardown.yml",
            "feat/a",
            "a1",
            "2026-10-23T10:00:06Z",
        ));
        // Same second as the close: ambiguous, kept.
        st.pr_runs.push(branch_run(
            3,
            "queued",
            "review.yml",
            "feat/a",
            "a1",
            "2026-10-23T10:00:05Z",
        ));
        // Open at another head: superseded, even though GitHub listed no pull request.
        st.pr_runs.push(branch_run(
            4,
            "in_progress",
            "review.yml",
            "feat/b",
            "b1",
            "2026-10-23T10:00:00Z",
        ));
        st.pulls_by_head
            .insert("feat/b".into(), vec![pull(11, "open", "b2", None)]);
        // Reopened: an open pull request at this head keeps the run.
        st.pr_runs.push(branch_run(
            5,
            "queued",
            "review.yml",
            "feat/c",
            "c1",
            "2026-10-23T10:00:00Z",
        ));
        st.pulls_by_head.insert(
            "feat/c".into(),
            vec![
                pull(12, "open", "c1", None),
                pull(9, "closed", "c0", Some("2026-10-22T10:00:00Z")),
            ],
        );
        // No pull request on the branch at all, and a run without a creation time: kept.
        st.pr_runs.push(branch_run(
            6,
            "queued",
            "review.yml",
            "feat/d",
            "d1",
            "2026-10-23T10:00:00Z",
        ));
        st.pr_runs
            .push(branch_run(7, "queued", "review.yml", "feat/a", "a1", ""));
        // A fork's run is never looked up.
        let mut fork = branch_run(
            8,
            "queued",
            "review.yml",
            "feat/a",
            "a1",
            "2026-10-23T10:00:00Z",
        );
        fork["head_repository"] = json!({"full_name": "fork/app"});
        st.pr_runs.push(fork);
    }
    let r = h.janitor_pass().await;
    assert_eq!(
        (
            r.considered,
            r.closed_pr,
            r.superseded,
            r.cancelled,
            r.cancelled_running,
            r.errors
        ),
        (7, 1, 1, 1, 1, 0),
        "{r:?}"
    );
    let mut cancelled = h.state().cancelled.clone();
    cancelled.sort_unstable();
    assert_eq!(cancelled, [1, 4]);
}

#[tokio::test]
async fn a_refused_pull_request_read_is_one_error_and_keeps_every_run() {
    let h = Harness::start(with(json!({}))).await;
    h.require_mirror_for_pulls();
    {
        let mut st = h.state();
        for id in 1..=3 {
            st.pr_runs.push(branch_run(
                id,
                "queued",
                "review.yml",
                &format!("feat/{id}"),
                "a1",
                "2026-10-23T10:00:00Z",
            ));
            st.pulls_by_head.insert(
                format!("feat/{id}"),
                vec![pull(id, "closed", "a1", Some("2026-10-23T10:00:05Z"))],
            );
        }
    }
    let r = h.janitor_pass().await;
    assert_eq!((r.considered, r.cancelled, r.errors), (0, 0, 1), "{r:?}");
    assert!(h.state().cancelled.is_empty());

    // With compare_auth the same reads go through with the mirror token.
    let mut cfg = with(json!({}));
    cfg["supersede_janitor"]["compare_auth"] = json!({"app_id": "12345", "repositories": ["app"]});
    let h = Harness::start(cfg).await;
    h.require_mirror_for_pulls();
    {
        let mut st = h.state();
        for id in 1..=3 {
            st.pr_runs.push(branch_run(
                id,
                "queued",
                "review.yml",
                &format!("feat/{id}"),
                "a1",
                "2026-10-23T10:00:00Z",
            ));
            st.pulls_by_head.insert(
                format!("feat/{id}"),
                vec![pull(id, "closed", "a1", Some("2026-10-23T10:00:05Z"))],
            );
        }
    }
    let r = h.janitor_pass().await;
    assert_eq!(
        (r.considered, r.closed_pr, r.cancelled, r.errors),
        (3, 3, 3, 0),
        "{r:?}"
    );
}

#[tokio::test]
async fn a_refused_compare_is_one_error_per_pass() {
    let h = Harness::start(promotions(false, json!({}))).await;
    h.state().compare_needs_contents = true;
    for (id, sha) in [(1, 'a'), (2, 'b'), (3, 'c')] {
        h.add_promotion_run(promo(id, "queued", None, sha, "memory", 3600));
        ahead(&h, sha, 'f');
    }
    h.add_promotion_run(promo(9, "in_progress", None, 'f', "memory", 600));
    let r = h.janitor_pass().await;
    assert_eq!(
        (r.promotion_considered, r.promotion_cancelled, r.errors),
        (3, 0, 1),
        "{r:?}"
    );
}

fn sha(c: char) -> String {
    c.to_string().repeat(40)
}

fn ago(secs: i64) -> String {
    (time::OffsetDateTime::now_utc() - time::Duration::seconds(secs))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}

fn promo(
    id: u64,
    status: &str,
    conclusion: Option<&str>,
    c: char,
    services: &str,
    age: i64,
) -> serde_json::Value {
    json!({
        "id": id, "workflow_id": 7, "path": PROMOTE, "status": status,
        "conclusion": conclusion, "event": "repository_dispatch",
        "head_sha": "main", "pull_requests": [],
        "created_at": ago(age),
        "display_title": format!(
            "Promote runtime images sha-{} services={services} dex-governance=false source-event={EVENT} source-run=1",
            sha(c)
        ),
    })
}

fn promotions(dry_run: bool, extra: serde_json::Value) -> serde_json::Value {
    let mut block = json!({
        "repo": "o/k8s", "workflow": PROMOTE, "dry_run": dry_run,
        "source_repos": {EVENT: "o/mono"}, "sweep_secs": 0,
    });
    block
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    json!({"supersede_janitor": {"dry_run": false, "superseded_promotions": [block]}})
}

fn ahead(h: &Harness, base: char, head: char) {
    h.state()
        .compares
        .insert(format!("{}...{}", sha(base), sha(head)), "ahead".into());
}

#[tokio::test]
async fn a_pending_promotion_yields_to_a_started_newer_run_of_its_services() {
    let h = Harness::start(promotions(false, json!({}))).await;
    h.add_promotion_run(promo(1, "queued", None, 'a', "memory", 3600));
    h.add_promotion_run(promo(2, "in_progress", None, 'b', "memory,ingest", 600));
    // Other service set, and a newer run that has not started: both kept.
    h.add_promotion_run(promo(3, "queued", None, 'c', "grid", 3600));
    h.add_promotion_run(promo(4, "queued", None, 'd', "grid", 60));
    // Same services but not a promotion: never examined.
    h.add_run(5, "queued", "workflow_run", "main", None);
    ahead(&h, 'a', 'b');
    ahead(&h, 'c', 'd');

    let r = h.janitor_pass().await;
    assert_eq!(
        (r.promotion_superseded, r.promotion_cancelled, r.errors),
        (1, 1, 0),
        "{r:?}"
    );
    assert_eq!(h.state().cancelled, [1]);
}

#[tokio::test]
async fn an_unproven_successor_never_cancels() {
    let h = Harness::start(promotions(false, json!({}))).await;
    // The compare answers 404: ancestry is unproven, the run is kept.
    h.add_promotion_run(promo(1, "queued", None, 'a', "memory", 3600));
    h.add_promotion_run(promo(2, "in_progress", None, 'b', "memory", 600));
    let r = h.janitor_pass().await;
    assert_eq!((r.promotion_cancelled, r.errors), (0, 1), "{r:?}");

    // A newer run behind the older one is not a successor either.
    let h = Harness::start(promotions(false, json!({}))).await;
    h.add_promotion_run(promo(1, "queued", None, 'a', "memory", 3600));
    h.add_promotion_run(promo(2, "in_progress", None, 'b', "memory", 600));
    h.state()
        .compares
        .insert(format!("{}...{}", sha('a'), sha('b')), "behind".into());
    let r = h.janitor_pass().await;
    assert_eq!((r.promotion_cancelled, r.errors), (0, 0), "{r:?}");
    assert!(h.state().cancelled.is_empty());
}

#[tokio::test]
async fn an_event_without_a_source_repository_is_kept() {
    let h = Harness::start(promotions(false, json!({"source_repos": {}}))).await;
    h.add_promotion_run(promo(1, "queued", None, 'a', "memory", 3600));
    h.add_promotion_run(promo(2, "in_progress", None, 'b', "memory", 600));
    let r = h.janitor_pass().await;
    assert_eq!(
        (r.promotion_unprovable, r.promotion_cancelled),
        (1, 0),
        "{r:?}"
    );
}

#[tokio::test]
async fn an_applied_run_is_only_cancelled_when_stale_and_then_forced() {
    let week = 7 * 24 * 3600;
    let h = Harness::start(promotions(false, json!({"force_cancel_after_mins": 0}))).await;
    // Applied, waiting on verification for a week; a newer run succeeded.
    h.add_promotion_run(promo(1, "in_progress", None, 'a', "memory", week));
    h.add_run_job(
        1,
        "apply-platform-production",
        "completed",
        Some("promotion-gce-01-1"),
    );
    h.add_run_job(
        1,
        "verify-direct-promotion",
        "in_progress",
        Some("gha-runner-pve50-e"),
    );
    // Applied but young: the pending rule never covers an applied run.
    h.add_promotion_run(promo(2, "queued", None, 'a', "ingest", 600));
    h.add_run_job(
        2,
        "apply-platform-production",
        "completed",
        Some("promotion-gce-01-2"),
    );
    h.add_promotion_run(promo(
        3,
        "completed",
        Some("success"),
        'b',
        "memory,ingest",
        300,
    ));
    h.add_promotion_run(promo(4, "in_progress", None, 'c', "ingest", 60));
    ahead(&h, 'a', 'b');
    ahead(&h, 'a', 'c');
    h.state().stuck_on_cancel.push(1);

    let r = h.janitor_pass().await;
    assert_eq!(
        (
            r.promotion_stale,
            r.promotion_superseded,
            r.promotion_cancelled
        ),
        (1, 0, 1),
        "{r:?}"
    );
    assert_eq!(h.state().cancelled, [1]);

    // The cancel did not take: the next pass force-cancels and does not
    // cancel it again.
    let r = h.janitor_pass().await;
    assert_eq!((r.force_cancelled, r.promotion_cancelled), (1, 0), "{r:?}");
    assert_eq!(h.state().force_cancelled, [1]);
    assert_eq!(h.state().cancelled, [1]);
}

#[tokio::test]
async fn a_running_mutation_job_is_never_interrupted() {
    let h = Harness::start(promotions(false, json!({}))).await;
    h.add_promotion_run(promo(1, "in_progress", None, 'a', "memory", 7 * 24 * 3600));
    h.add_run_job(
        1,
        "rollback-verified-bad",
        "in_progress",
        Some("promotion-gce-01-1"),
    );
    h.add_promotion_run(promo(2, "completed", Some("success"), 'b', "memory", 300));
    ahead(&h, 'a', 'b');
    let r = h.janitor_pass().await;
    assert_eq!((r.promotion_cancelled, r.promotion_stale), (0, 0), "{r:?}");
}

#[tokio::test]
async fn a_nameless_queued_run_without_jobs_is_dropped_after_the_ghost_age() {
    let h = Harness::start(promotions(false, json!({}))).await;
    let mut old = promo(1, "queued", None, 'a', "memory", 15 * 24 * 3600);
    old["display_title"] = json!(EVENT);
    h.add_promotion_run(old);
    let mut with_jobs = promo(2, "queued", None, 'a', "memory", 15 * 24 * 3600);
    with_jobs["display_title"] = json!(EVENT);
    h.add_promotion_run(with_jobs);
    h.add_run_job(2, "resolve-lane-contract", "queued", None);
    let mut young = promo(3, "queued", None, 'a', "memory", 3600);
    young["display_title"] = json!(EVENT);
    h.add_promotion_run(young);
    let r = h.janitor_pass().await;
    assert_eq!((r.promotion_ghost, r.promotion_cancelled), (1, 1), "{r:?}");
    assert_eq!(h.state().cancelled, [1]);
}

#[tokio::test]
async fn dry_run_promotion_rules_cancel_nothing() {
    let h = Harness::start(promotions(true, json!({}))).await;
    h.add_promotion_run(promo(1, "queued", None, 'a', "memory", 3600));
    h.add_promotion_run(promo(2, "in_progress", None, 'b', "memory", 600));
    ahead(&h, 'a', 'b');
    let r = h.janitor_pass().await;
    assert_eq!((r.would_cancel, r.promotion_cancelled), (1, 0), "{r:?}");
    assert!(h.state().cancelled.is_empty());
}

#[tokio::test]
async fn compares_use_the_contents_credential_when_the_janitor_token_cannot_read() {
    // Without compare_auth the 403 keeps the run (unproven) and counts an error.
    let h = Harness::start(promotions(false, json!({}))).await;
    h.state().compare_needs_contents = true;
    h.add_promotion_run(promo(1, "queued", None, 'a', "memory", 3600));
    h.add_promotion_run(promo(2, "in_progress", None, 'b', "memory", 600));
    ahead(&h, 'a', 'b');
    let r = h.janitor_pass().await;
    assert_eq!((r.promotion_cancelled, r.errors), (0, 1), "{r:?}");

    let mut cfg = promotions(false, json!({}));
    cfg["supersede_janitor"]["compare_auth"] = json!({"app_id": "12345", "repositories": ["mono"]});
    let h = Harness::start(cfg).await;
    h.state().compare_needs_contents = true;
    h.add_promotion_run(promo(1, "queued", None, 'a', "memory", 3600));
    h.add_promotion_run(promo(2, "in_progress", None, 'b', "memory", 600));
    ahead(&h, 'a', 'b');
    let r = h.janitor_pass().await;
    assert_eq!((r.promotion_cancelled, r.errors), (1, 0), "{r:?}");
    let st = h.state();
    assert_eq!(st.compare_tokens, [support::MIRROR_TOKEN]);
    assert_eq!(
        st.scoped_requests[0]["permissions"],
        json!({"contents": "read", "pull_requests": "read"})
    );
    assert_eq!(st.scoped_requests[0]["repositories"], json!(["mono"]));
}
