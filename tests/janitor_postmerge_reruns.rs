use crate::support::Harness;
use serde_json::{Value, json};

const WORKFLOW: &str = ".github/workflows/semgrep.yml";
const CREATED: &str = "2026-11-01T21:01:32Z";
const CLOSED: &str = "2026-11-01T21:04:31Z";
const RESTARTED: &str = "2026-11-02T05:09:02Z";

fn settings(listed: bool) -> Value {
    json!({"supersede_janitor": {
        "dry_run": false, "cancel_running": true,
        "closed_pr_workflows": if listed { vec![WORKFLOW] } else { vec![] }
    }})
}

fn rerun(id: u64, status: &str) -> Value {
    json!({"id": id, "status": status, "event": "pull_request",
        "head_sha": "qualified-head", "head_branch": "fix/projection",
        "path": WORKFLOW, "name": "semgrep", "pull_requests": [],
        "head_repository": {"full_name": "example-org/app"},
        "created_at": CREATED, "run_attempt": 2, "run_started_at": RESTARTED})
}

fn closed_pull(head: &str) -> Value {
    json!({"number": 19530, "state": "closed",
        "head": {"sha": head, "repo": {"full_name": "example-org/app"}},
        "closed_at": CLOSED})
}

#[tokio::test]
async fn requested_postmerge_reruns_survive_closed_pr_cleanup() {
    for listed in [false, true] {
        let h = Harness::start(settings(listed)).await;
        {
            let mut st = h.state();
            st.pr_runs.push(rerun(1, "queued"));
            st.pr_runs.push(rerun(2, "in_progress"));
            st.pulls_by_head
                .insert("fix/projection".into(), vec![closed_pull("qualified-head")]);
        }
        let result = h.janitor_pass().await;
        assert_eq!(result.errors, 0, "listed={listed}: {result:?}");
        assert!(
            h.state().cancelled.is_empty(),
            "an exact-head attempt requested after merge must run: listed={listed}: {result:?}"
        );
    }
}

#[tokio::test]
async fn rerun_exemption_requires_exact_head_and_complete_attempt_proof() {
    let h = Harness::start(settings(true)).await;
    {
        let mut st = h.state();
        st.pulls_by_head
            .insert("fix/projection".into(), vec![closed_pull("qualified-head")]);
        for (id, field, value) in [
            (1, "run_attempt", json!(1)),
            (2, "run_attempt", json!(0)),
            (3, "run_started_at", json!(CREATED)),
            (4, "run_started_at", json!(CLOSED)),
            (5, "run_started_at", json!("invalid")),
            (6, "run_started_at", Value::Null),
            (7, "head_sha", json!("superseded-head")),
            (8, "created_at", json!("invalid")),
        ] {
            let mut run = rerun(id, "queued");
            run[field] = value;
            st.pr_runs.push(run);
        }
    }
    let result = h.janitor_pass().await;
    assert_eq!(result.cancelled, 8, "{result:?}");
    assert_eq!(h.state().cancelled.len(), 8);
}

#[tokio::test]
async fn a_same_owner_branch_and_sha_cannot_substitute_another_repository() {
    for repository in [Value::Null, json!({"full_name": "example-org/fork"})] {
        let h = Harness::start(settings(true)).await;
        {
            let mut st = h.state();
            st.pr_runs.push(rerun(1, "queued"));
            let mut pull = closed_pull("qualified-head");
            pull["head"]["repo"] = repository;
            st.pulls_by_head.insert("fix/projection".into(), vec![pull]);
        }
        let result = h.janitor_pass().await;
        assert_eq!(result.cancelled, 1, "{result:?}");
    }
}

#[tokio::test]
async fn an_attempt_restarted_after_listing_is_not_cancelled() {
    let h = Harness::start(settings(true)).await;
    {
        let mut st = h.state();
        let mut run = rerun(1, "queued");
        run["run_attempt"] = json!(1);
        run["run_started_at"] = json!(CREATED);
        st.pr_runs.push(run);
        st.pulls_by_head
            .insert("fix/projection".into(), vec![closed_pull("qualified-head")]);
        st.fresh_attempts.insert(1, (2, RESTARTED.into()));
    }
    let result = h.janitor_pass().await;
    assert_eq!((result.raced, result.cancelled), (1, 0), "{result:?}");
    assert!(h.state().cancelled.is_empty());
}
