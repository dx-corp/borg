//! Jobs whose runner host was lost: classification through the queen's tick,
//! the one automatic rerun, its caps, and what is never re-run.

use std::cell::Cell;

use crate::support;

use borg::allocator::HostCaps;
use borg::hostlost::{HostLostRow, Outcome, PassReport};
use borg::store::{LeaseRow, LeaseStatus};
use serde_json::{Value, json};
use support::{Harness, ORG};
use time::OffsetDateTime;

/// Two drones on a clock the test moves: `d1` takes the leases, `d2` only
/// heartbeats (so the queen is never deaf) until a test lets it accept work.
struct Fleet {
    h: Harness,
    d1_token: String,
    now: Cell<i64>,
}

fn at(t: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(t).unwrap()
}

fn pr_run(id: u64, head_sha: &str, pr_head: Option<&str>) -> Value {
    let prs: Vec<Value> = pr_head
        .iter()
        .map(|sha| json!({"number": 1, "head": {"sha": sha}}))
        .collect();
    json!({
        "id": id, "workflow_id": 1, "run_number": id, "status": "completed",
        "event": "pull_request", "head_branch": "feature", "head_sha": head_sha,
        "pull_requests": prs
    })
}

impl Fleet {
    async fn start(extra: Value) -> Self {
        let h = Harness::start(extra).await;
        let d1_token = h.enroll("d1").await;
        h.enroll("d2").await;
        // Like GitHub, list a run's own jobs only.
        h.state().jobs_by_run = true;
        Self {
            h,
            d1_token,
            now: Cell::new(OffsetDateTime::now_utc().unix_timestamp()),
        }
    }

    fn now(&self) -> i64 {
        self.now.get()
    }

    fn advance(&self, secs: i64) -> i64 {
        self.now.set(self.now.get() + secs);
        self.now.get()
    }

    async fn heard(&self, drone: &str, accepting: bool) {
        let store = self.h.queen.store();
        let now = self.now();
        store
            .note_silence(drone, now, None, self.h.queen.config().drone_stale_secs)
            .await
            .unwrap();
        store
            .record_heartbeat(
                drone,
                &support::capacity(),
                &HostCaps::default(),
                accepting,
                &[],
                now,
            )
            .await
            .unwrap();
    }

    async fn tick(&self) {
        self.h.queen.tick(at(self.now())).await.unwrap();
    }

    /// `d1` picks up `job` of `run` and the queen sees it running.
    async fn running(&self, job: u64, run: u64) -> LeaseRow {
        self.heard("d1", true).await;
        self.heard("d2", false).await;
        self.h.state().queued_runs = vec![run];
        self.set_job(json!({
            "id": job, "run_id": run, "status": "queued", "labels": ["borg-ci-light-pr"],
            "created_at": "2026-01-01T00:00:00Z"
        }));
        self.tick().await;
        let lease = self
            .h
            .queen
            .store()
            .active_leases()
            .await
            .unwrap()
            .into_iter()
            .rfind(|l| l.state == LeaseStatus::Pending)
            .expect("a lease for the queued job");
        assert_eq!(lease.drone, "d1");
        self.h.set_runner(&lease.runner_name, "online", true);
        self.set_job(json!({
            "id": job, "run_id": run, "name": format!("merge-gate {job}"),
            "status": "in_progress", "conclusion": null, "labels": ["borg-ci-light-pr"],
            "runner_id": lease.runner_id, "runner_name": lease.runner_name,
            "created_at": "2026-01-01T00:00:00Z", "started_at": "2026-01-01T00:00:10Z",
            "completed_at": null, "steps": []
        }));
        self.advance(10);
        self.heard("d1", true).await;
        self.heard("d2", false).await;
        self.tick().await;
        lease
    }

    fn set_job(&self, job: Value) {
        let mut st = self.h.state();
        st.jobs.retain(|j| j["id"] != job["id"]);
        st.jobs.push(job);
    }

    /// GitHub ends `job` as `conclusion` and drops its runner. The running
    /// step concludes `step` (`null` for a step that never finished).
    fn github_ends(&self, lease: &LeaseRow, job: u64, conclusion: &str, step: Value) {
        let mut st = self.h.state();
        st.runners
            .retain(|r| r["name"] != lease.runner_name.as_str());
        for j in st.jobs.iter_mut().filter(|j| j["id"] == job) {
            j["status"] = json!("completed");
            j["conclusion"] = json!(conclusion);
            j["completed_at"] = json!("2026-01-01T00:30:00Z");
            j["steps"] = json!([
                {"name": "Set up job", "number": 1, "conclusion": "success"},
                {"name": "cargo clippy", "number": 2, "conclusion": step},
            ]);
        }
    }

    /// Both drones heartbeat every 30 seconds for `secs`.
    async fn both_heard_for(&self, secs: i64) {
        for _ in 0..secs / 30 {
            self.advance(30);
            self.heard("d1", true).await;
            self.heard("d2", false).await;
        }
    }

    /// `d1` stops heartbeating under `job`; ten minutes later GitHub fails
    /// the job with its step unfinished and the queen's tick ends the lease.
    async fn lose(&self, job: u64, run: u64) -> LeaseRow {
        let lease = self.running(job, run).await;
        self.advance(600);
        self.heard("d2", false).await;
        self.github_ends(&lease, job, "failure", Value::Null);
        self.tick().await;
        lease
    }

    async fn pass(&self) -> PassReport {
        self.h
            .queen
            .host_lost()
            .unwrap()
            .pass(self.h.queen.store(), self.now())
            .await
            .unwrap()
    }

    async fn rows(&self) -> Vec<HostLostRow> {
        self.h.queen.store().host_lost_since(0, 100).await.unwrap()
    }

    fn reruns(&self) -> Vec<u64> {
        self.h.state().reruns.clone()
    }
}

#[tokio::test]
async fn a_job_on_a_lost_host_is_rerun_once_and_the_drone_is_held() {
    let f = Fleet::start(json!({})).await;
    f.h.state().pr_runs.push(pr_run(7, "sha", Some("sha")));
    let lease = f.lose(71, 7).await;

    let rows = f.rows().await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let lost = &rows[0];
    assert_eq!(
        (lost.lease_id.as_str(), lost.drone.as_str()),
        (lease.id.as_str(), "d1")
    );
    assert_eq!(lost.repo.as_deref(), Some(format!("{ORG}/app").as_str()));
    assert_eq!((lost.run_id, lost.job_id), (Some(7), Some(71)));
    assert_eq!(lost.silent_secs, 600);
    assert_eq!(lost.outcome, None);

    // Inside the backoff nothing is asked of GitHub.
    let r = f.pass().await;
    assert_eq!((r.checked, r.waiting, r.rerun), (1, 1, 0), "{r:?}");
    assert_eq!(f.h.state().run_reads, 0);
    assert!(f.reruns().is_empty());

    f.advance(60);
    let r = f.pass().await;
    assert_eq!((r.checked, r.rerun, r.errors), (1, 1, 0), "{r:?}");
    assert_eq!(f.reruns(), [71]);
    assert_eq!(f.rows().await[0].outcome, Some(Outcome::Rerun));

    // One rerun per job attempt: later passes find nothing to do.
    f.advance(600);
    let r = f.pass().await;
    assert_eq!((r.checked, r.rerun), (0, 0), "{r:?}");
    assert_eq!(f.reruns(), [71]);

    let status = f.h.status().await;
    assert_eq!(status["host_lost"]["total"]["rerun"], json!(1));
    assert!(
        status["health_events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["event"] == "host_lost" && e["drone"] == "d1"),
        "{status}"
    );
    assert!(f.h.metrics().await.contains("borg_host_lost_rerun_total 1"));

    let report: Value =
        f.h.http
            .get(format!("{}/report?since_secs=86400", f.h.url))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    assert_eq!(report["host_lost"]["lost"], json!(1));
    assert_eq!(report["host_lost"]["rerun"], json!(1));
    let d1 = report["drones"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["drone"] == "d1")
        .expect("the lost drone is reported");
    assert_eq!(
        (&d1["host_lost"], &d1["auto_reruns"]),
        (&json!(1), &json!(1))
    );
}

#[tokio::test]
async fn a_suspect_drone_gets_no_lease_until_its_cool_down_ends() {
    let f = Fleet::start(json!({})).await;
    f.lose(71, 7).await;
    let lost_at = f.now();
    let cooldown = i64::try_from(f.h.queen.config().host_lost.suspect_cooldown_secs).unwrap();
    let d1 = |rows: Vec<borg::store::DroneRow>| rows.into_iter().find(|d| d.name == "d1").unwrap();
    assert_eq!(
        d1(f.h.queen.store().drones().await.unwrap()).suspect_until,
        Some(lost_at + cooldown)
    );

    // Both drones are back and accepting: the next job goes to d2.
    f.advance(30);
    f.heard("d1", true).await;
    f.heard("d2", true).await;
    f.h.state().queued_runs = vec![8];
    f.set_job(json!({
        "id": 72, "run_id": 8, "status": "queued", "labels": ["borg-ci-light-pr"],
        "created_at": "2026-01-01T00:00:00Z"
    }));
    f.tick().await;
    let granted = |leases: Vec<LeaseRow>| -> Vec<String> {
        leases
            .into_iter()
            .filter(|l| l.state == LeaseStatus::Pending)
            .map(|l| l.drone)
            .collect()
    };
    let drones = granted(f.h.queen.store().active_leases().await.unwrap());
    assert!(
        !drones.is_empty() && drones.iter().all(|d| d == "d2"),
        "{drones:?}"
    );

    // After the cool-down d1 is schedulable again.
    f.advance(cooldown);
    assert!(!d1(f.h.queen.store().drones().await.unwrap()).suspect(f.now()));
}

#[tokio::test]
async fn a_job_that_ends_while_its_drone_is_heard_is_not_host_lost() {
    let f = Fleet::start(json!({})).await;
    let lease = f.running(71, 7).await;
    // The job fails with the host alive; the queen ends the lease before the
    // drone's result arrives.
    f.both_heard_for(600).await;
    f.github_ends(&lease, 71, "failure", json!("failure"));
    f.tick().await;
    assert!(f.rows().await.is_empty());
    f.advance(120);
    let r = f.pass().await;
    assert_eq!((r.checked, r.rerun), (0, 0), "{r:?}");
    assert!(f.reruns().is_empty());
}

#[tokio::test]
async fn a_failed_step_or_a_cancellation_on_a_silent_drone_is_never_rerun() {
    for (conclusion, step, reason) in [
        // The job's own step failed before the drone went quiet.
        ("failure", json!("failure"), "job_failed"),
        // A person, a concurrency group or the supersede janitor cancelled it.
        ("cancelled", json!("cancelled"), "job_cancelled"),
        ("success", json!("success"), "job_not_failed"),
    ] {
        let f = Fleet::start(json!({})).await;
        f.h.state().pr_runs.push(pr_run(7, "sha", Some("sha")));
        let lease = f.running(71, 7).await;
        f.advance(600);
        f.heard("d2", false).await;
        f.github_ends(&lease, 71, conclusion, step);
        f.tick().await;
        assert_eq!(
            f.rows().await.len(),
            1,
            "{conclusion}: classified by silence"
        );

        f.advance(120);
        let r = f.pass().await;
        assert_eq!((r.skipped, r.rerun), (1, 0), "{conclusion}: {r:?}");
        assert!(f.reruns().is_empty(), "{conclusion}");
        let row = &f.rows().await[0];
        assert_eq!(row.outcome, Some(Outcome::Skipped), "{conclusion}");
        assert_eq!(row.reason.as_deref(), Some(reason));
    }
}

#[tokio::test]
async fn a_pull_request_whose_head_moved_or_closed_is_not_rerun() {
    for pr_head in [Some("newer-sha"), None] {
        let f = Fleet::start(json!({})).await;
        f.h.state().pr_runs.push(pr_run(7, "sha", pr_head));
        f.lose(71, 7).await;
        f.advance(120);
        let r = f.pass().await;
        assert_eq!((r.skipped, r.rerun), (1, 0), "{pr_head:?}: {r:?}");
        assert!(f.reruns().is_empty());
        assert_eq!(f.rows().await[0].reason.as_deref(), Some("head_changed"));
    }
}

#[tokio::test]
async fn a_push_run_with_a_newer_run_of_its_workflow_is_not_rerun() {
    let push = |id: u64| {
        json!({
            "id": id, "workflow_id": 1, "run_number": id, "status": "completed",
            "event": "push", "head_branch": "main", "head_sha": format!("sha-{id}"),
            "pull_requests": []
        })
    };
    // The newest push run is re-run.
    let f = Fleet::start(json!({})).await;
    f.h.state().pr_runs.push(push(7));
    f.lose(71, 7).await;
    f.advance(120);
    assert_eq!(f.pass().await.rerun, 1);
    assert_eq!(f.reruns(), [71]);

    // One a newer push replaced is not.
    let f = Fleet::start(json!({})).await;
    f.h.state().pr_runs.push(push(7));
    f.h.state().pr_runs.push(push(8));
    f.lose(71, 7).await;
    f.advance(120);
    let r = f.pass().await;
    assert_eq!((r.skipped, r.rerun), (1, 0), "{r:?}");
    assert_eq!(f.rows().await[0].reason.as_deref(), Some("superseded"));
}

#[tokio::test]
async fn a_run_gets_at_most_its_hourly_reruns() {
    // No cool-down, so d1 keeps taking (and losing) the run's jobs.
    let f = Fleet::start(json!({"host_lost": {"suspect_cooldown_secs": 0}})).await;
    let cap = f.h.queen.config().host_lost.max_reruns_per_run_per_hour;
    f.h.state().pr_runs.push(pr_run(7, "sha", Some("sha")));
    let mut expected = Vec::new();
    for n in 0..cap {
        let job = 71 + u64::from(n);
        f.lose(job, 7).await;
        // The backoff doubles with each rerun the run already had.
        let backoff = f.h.queen.config().host_lost.backoff_for(n);
        f.advance(backoff - 1);
        assert_eq!(f.pass().await.rerun, 0, "rerun {n} waits its backoff");
        f.advance(1);
        assert_eq!(f.pass().await.rerun, 1, "rerun {n}");
        expected.push(job);
    }
    assert_eq!(f.reruns(), expected);

    // The next loss in the same hour is left alone for good.
    let over = 71 + u64::from(cap);
    f.lose(over, 7).await;
    f.advance(900);
    let r = f.pass().await;
    assert_eq!((r.skipped, r.rerun), (1, 0), "{r:?}");
    assert_eq!(f.reruns(), expected);
    let rows = f.rows().await;
    let capped = rows.iter().find(|r| r.job_id == Some(over)).unwrap();
    assert_eq!(capped.reason.as_deref(), Some("run_cap"));

    // Another run is unaffected.
    f.h.state().pr_runs.push(pr_run(9, "sha", Some("sha")));
    f.lose(99, 9).await;
    f.advance(60);
    assert_eq!(f.pass().await.rerun, 1);
}

#[tokio::test]
async fn the_fleet_wide_hourly_cap_defers_reruns_until_the_hour_passes() {
    let f = Fleet::start(json!({
        "host_lost": {"suspect_cooldown_secs": 0, "max_reruns_per_hour": 1}
    }))
    .await;
    for run in [7, 8] {
        f.h.state().pr_runs.push(pr_run(run, "sha", Some("sha")));
    }
    f.lose(71, 7).await;
    f.advance(60);
    assert_eq!(f.pass().await.rerun, 1);
    f.lose(81, 8).await;
    f.advance(60);
    let r = f.pass().await;
    assert_eq!((r.waiting, r.rerun), (1, 0), "{r:?}");
    assert_eq!(f.reruns(), [71]);
    // An hour after the first rerun the second goes through.
    f.advance(3600);
    assert_eq!(f.pass().await.rerun, 1);
    assert_eq!(f.reruns(), [71, 81]);
}

#[tokio::test]
async fn a_job_someone_already_reran_is_left_alone() {
    let f = Fleet::start(json!({})).await;
    f.h.state().pr_runs.push(pr_run(7, "sha", Some("sha")));
    f.lose(71, 7).await;
    // A person re-ran it: the run's newest attempt lists a new job id.
    f.h.state().jobs.retain(|j| j["id"] != 71);
    f.set_job(json!({
        "id": 7100, "run_id": 7, "name": "merge-gate 71", "status": "in_progress",
        "conclusion": null, "labels": [], "created_at": "2026-01-01T00:40:00Z", "steps": []
    }));
    f.advance(120);
    let r = f.pass().await;
    assert_eq!((r.skipped, r.rerun), (1, 0), "{r:?}");
    assert_eq!(f.rows().await[0].reason.as_deref(), Some("already_rerun"));
    assert!(f.reruns().is_empty());
}

#[tokio::test]
async fn a_run_still_in_progress_waits_and_a_refused_rerun_is_terminal() {
    let f = Fleet::start(json!({})).await;
    let mut run = pr_run(7, "sha", Some("sha"));
    run["status"] = json!("in_progress");
    f.h.state().pr_runs.push(run);
    f.lose(71, 7).await;
    f.advance(120);
    let r = f.pass().await;
    assert_eq!((r.waiting, r.rerun), (1, 0), "{r:?}");

    f.h.state().fresh_status.insert(7, "completed".into());
    f.h.state().rerun_status.insert(71, 500);
    let r = f.pass().await;
    assert_eq!((r.errors, r.rerun), (1, 0), "a 500 is retried: {r:?}");
    assert_eq!(f.rows().await[0].outcome, None);

    f.h.state().rerun_status.insert(71, 409);
    let r = f.pass().await;
    assert_eq!((r.skipped, r.rerun), (1, 0), "{r:?}");
    assert_eq!(f.rows().await[0].reason.as_deref(), Some("refused"));
    assert_eq!(f.pass().await.checked, 0);
}

#[tokio::test]
async fn a_result_from_the_drone_withdraws_the_record() {
    let f = Fleet::start(json!({})).await;
    f.h.state().pr_runs.push(pr_run(7, "sha", Some("sha")));
    let lease = f.lose(71, 7).await;
    assert_eq!(f.rows().await[0].outcome, None);

    // The drone was only cut off from the queen: its result arrives late.
    let status =
        f.h.http
            .post(format!("{}/v1/leases/{}/result", f.h.url, lease.id))
            .bearer_auth(&f.d1_token)
            .json(&json!({"exit_code": 1, "duration_secs": 30}))
            .send()
            .await
            .unwrap()
            .status();
    assert_eq!(status, reqwest::StatusCode::NO_CONTENT);
    let drones = f.h.queen.store().drones().await.unwrap();
    assert!(
        drones.iter().all(|d| d.suspect_until.is_none()),
        "a drone that reports is no longer suspect"
    );
    f.advance(120);
    let r = f.pass().await;
    assert_eq!((r.checked, r.rerun), (0, 0), "{r:?}");
    assert_eq!(f.rows().await[0].reason.as_deref(), Some("drone_reported"));
}

impl Fleet {
    /// `d1` reports the result of `lease` to the queen.
    async fn d1_reports(&self, lease: &LeaseRow) {
        let status = self
            .h
            .http
            .post(format!("{}/v1/leases/{}/result", self.h.url, lease.id))
            .bearer_auth(&self.d1_token)
            .json(&json!({"exit_code": 1, "duration_secs": 30}))
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, reqwest::StatusCode::NO_CONTENT);
    }

    async fn d1_suspect(&self) -> bool {
        let drones = self.h.queen.store().drones().await.unwrap();
        drones
            .iter()
            .any(|d| d.name == "d1" && d.suspect_until.is_some())
    }
}

#[tokio::test]
async fn a_result_that_arrives_while_the_pass_reads_github_stops_the_rerun() {
    let f = Fleet::start(json!({})).await;
    f.h.state().pr_runs.push(pr_run(7, "sha", Some("sha")));
    let lease = f.lose(71, 7).await;
    f.advance(120);

    // The pass has taken the record and is reading the run when the drone's
    // result arrives.
    let reached = std::sync::Arc::new(tokio::sync::Notify::new());
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    f.h.state().hold_run_read = Some((reached.clone(), release.clone()));
    let queen = f.h.queen.clone();
    let now = f.now();
    let pass = tokio::spawn(async move {
        queen
            .host_lost()
            .unwrap()
            .pass(queen.store(), now)
            .await
            .unwrap()
    });
    reached.notified().await;
    f.d1_reports(&lease).await;
    release.notify_one();

    let r = pass.await.unwrap();
    assert_eq!(r.rerun, 0, "{r:?}");
    assert!(
        f.reruns().is_empty(),
        "nothing is re-run once the drone reported"
    );
    let row = &f.rows().await[0];
    assert_eq!(row.outcome, Some(Outcome::Skipped));
    assert_eq!(row.reason.as_deref(), Some("drone_reported"));
    assert!(!f.d1_suspect().await);
}

#[tokio::test]
async fn a_result_after_the_record_was_decided_still_ends_the_cool_down() {
    // Re-run first, then the drone reports.
    let f = Fleet::start(json!({})).await;
    f.h.state().pr_runs.push(pr_run(7, "sha", Some("sha")));
    let lease = f.lose(71, 7).await;
    f.advance(120);
    assert_eq!(f.pass().await.rerun, 1);
    assert!(f.d1_suspect().await);
    f.d1_reports(&lease).await;
    assert!(!f.d1_suspect().await, "after a rerun");
    assert_eq!(
        f.rows().await[0].outcome,
        Some(Outcome::Rerun),
        "the decision stands"
    );

    // Skipped first (the job's own step failed), then the drone reports.
    let f = Fleet::start(json!({})).await;
    f.h.state().pr_runs.push(pr_run(7, "sha", Some("sha")));
    let lease = f.running(71, 7).await;
    f.advance(600);
    f.heard("d2", false).await;
    f.github_ends(&lease, 71, "failure", json!("failure"));
    f.tick().await;
    f.advance(120);
    assert_eq!(f.pass().await.skipped, 1);
    assert!(f.d1_suspect().await);
    f.d1_reports(&lease).await;
    assert!(!f.d1_suspect().await, "after a skip");
    assert_eq!(f.rows().await[0].reason.as_deref(), Some("job_failed"));
}

#[tokio::test]
async fn a_failed_rerun_request_after_the_drone_reported_is_not_retried() {
    let f = Fleet::start(json!({})).await;
    f.h.state().pr_runs.push(pr_run(7, "sha", Some("sha")));
    let lease = f.lose(71, 7).await;
    f.advance(120);
    // The rerun request fails with a retryable error: the record returns to
    // undecided and the next pass tries again.
    f.h.state().rerun_status.insert(71, 500);
    let r = f.pass().await;
    assert_eq!((r.errors, r.rerun), (1, 0), "{r:?}");
    assert_eq!(f.rows().await[0].outcome, None);
    // Counted as nothing: the claim is released.
    assert_eq!(
        f.h.queen
            .store()
            .host_lost_reruns(None, i64::MIN)
            .await
            .unwrap(),
        0
    );
    f.d1_reports(&lease).await;
    f.h.state().rerun_status.remove(&71);
    let r = f.pass().await;
    assert_eq!((r.checked, r.rerun), (0, 0), "{r:?}");
    assert_eq!(f.rows().await[0].reason.as_deref(), Some("drone_reported"));
}

#[tokio::test]
async fn a_result_during_the_rerun_request_keeps_a_failed_request_from_returning() {
    let f = Fleet::start(json!({})).await;
    let lease = f.lose(71, 7).await;
    let store = f.h.queen.store();
    let now = f.now();
    // The pass claimed the record and is asking GitHub when the result lands.
    assert!(
        store
            .decide_host_lost(&lease.id, Outcome::Rerun, None, now)
            .await
            .unwrap()
    );
    assert!(store.withdraw_host_lost(&lease.id, now).await.unwrap());
    assert_eq!(f.rows().await[0].outcome, Some(Outcome::Rerun));
    // The request fails: the record does not return to the pass.
    store
        .settle_host_lost_claim(&lease.id, None, now)
        .await
        .unwrap();
    let row = &f.rows().await[0];
    assert_eq!(row.outcome, Some(Outcome::Skipped));
    assert_eq!(row.reason.as_deref(), Some("drone_reported"));
    assert!(store.pending_host_lost(10).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_drone_back_from_a_silence_under_the_job_is_host_lost() {
    let f = Fleet::start(json!({})).await;
    let lease = f.running(71, 7).await;
    // d1 restarts: five silent minutes, then it heartbeats again without the job.
    f.advance(300);
    f.heard("d1", true).await;
    f.heard("d2", false).await;
    f.both_heard_for(240).await;
    f.github_ends(&lease, 71, "failure", Value::Null);
    f.tick().await;
    let rows = f.rows().await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].silent_secs, 300);
}

#[tokio::test]
async fn a_running_job_whose_runner_stays_registered_offline_is_host_lost() {
    let f = Fleet::start(json!({})).await;
    let lease = f.running(71, 7).await;
    // d1 goes quiet; GitHub keeps its runner registered but offline and idle,
    // so the queen ends the lease past its TTL rather than on deregistration.
    f.advance(600);
    f.heard("d2", false).await;
    f.h.set_runner(&lease.runner_name, "offline", false);
    f.tick().await;
    let rows = f.rows().await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].lease_id, lease.id);
    let drones = f.h.queen.store().drones().await.unwrap();
    assert!(
        drones
            .iter()
            .any(|d| d.name == "d1" && d.suspect_until.is_some())
    );
}

#[tokio::test]
async fn a_queen_that_hears_no_drone_at_all_classifies_nothing() {
    let f = Fleet::start(json!({})).await;
    let lease = f.running(71, 7).await;
    // Ten minutes with no heartbeat from anyone: the queen's network, not d1.
    f.advance(600);
    f.github_ends(&lease, 71, "failure", Value::Null);
    f.tick().await;
    assert!(f.rows().await.is_empty());
    let drones = f.h.queen.store().drones().await.unwrap();
    assert!(drones.iter().all(|d| d.suspect_until.is_none()));
}

#[tokio::test]
async fn dry_run_reports_what_it_would_rerun_and_calls_nothing() {
    let f = Fleet::start(json!({"host_lost": {"dry_run": true}})).await;
    f.h.state().pr_runs.push(pr_run(7, "sha", Some("sha")));
    f.lose(71, 7).await;
    f.advance(120);
    let r = f.pass().await;
    assert_eq!((r.would_rerun, r.rerun), (1, 0), "{r:?}");
    assert!(f.reruns().is_empty());
    assert_eq!(f.rows().await[0].outcome, None);
}

#[tokio::test]
async fn rerun_off_still_classifies_and_holds_the_drone() {
    let f = Fleet::start(json!({"host_lost": {"rerun": false}})).await;
    f.h.state().pr_runs.push(pr_run(7, "sha", Some("sha")));
    f.lose(71, 7).await;
    f.advance(120);
    let r = f.pass().await;
    assert_eq!((r.skipped, r.rerun), (1, 0), "{r:?}");
    assert_eq!(f.rows().await[0].reason.as_deref(), Some("rerun_off"));
    assert_eq!(
        f.h.state().run_reads,
        0,
        "no GitHub read when reruns are off"
    );
    let drones = f.h.queen.store().drones().await.unwrap();
    assert!(
        drones
            .iter()
            .any(|d| d.name == "d1" && d.suspect_until.is_some())
    );
}

#[tokio::test]
async fn a_suspect_drone_is_handed_no_lease_minted_before_its_cool_down() {
    let h = Harness::start(json!({})).await;
    let token = h.enroll("d1").await;
    h.heartbeat(&token, &[]).await;
    h.queue_job(10, "borg-ci-light-pr", 5);
    h.tick().await;
    let now = OffsetDateTime::now_utc().unix_timestamp();
    h.queen
        .store()
        .suspect_drone("d1", now + 900)
        .await
        .unwrap();
    let body = h.heartbeat_with(&token, json!({})).await;
    assert_eq!(body["leases"], json!([]), "{body}");
    assert_eq!(body["start_standby"], json!([]));
}

#[tokio::test]
async fn a_pull_request_target_run_is_judged_by_its_pull_request_head_too() {
    for (pr_head, rerun) in [(Some("sha"), 1), (Some("newer-sha"), 0)] {
        let f = Fleet::start(json!({})).await;
        let mut run = pr_run(7, "sha", pr_head);
        run["event"] = json!("pull_request_target");
        f.h.state().pr_runs.push(run);
        f.lose(71, 7).await;
        f.advance(120);
        assert_eq!(f.pass().await.rerun, rerun, "{pr_head:?}");
    }
}

#[tokio::test]
async fn a_result_and_a_released_claim_in_any_order_leave_the_record_terminal() {
    let terminal = |row: &HostLostRow| {
        row.outcome == Some(Outcome::Skipped) && row.reason.as_deref() == Some("drone_reported")
    };
    // Claim, result, failed request; claim, failed request, result; and a
    // claim the queen stopped under, the result, then its release.
    for order in ["result_then_release", "release_then_result", "interrupted"] {
        let f = Fleet::start(json!({})).await;
        let lease = f.lose(71, 7).await;
        let store = f.h.queen.store();
        let now = f.now();
        assert!(store.claim_host_lost(&lease.id, now).await.unwrap());
        match order {
            "result_then_release" => {
                assert!(store.withdraw_host_lost(&lease.id, now).await.unwrap());
                store
                    .settle_host_lost_claim(&lease.id, None, now)
                    .await
                    .unwrap();
            }
            "release_then_result" => {
                store
                    .settle_host_lost_claim(&lease.id, None, now)
                    .await
                    .unwrap();
                assert!(store.withdraw_host_lost(&lease.id, now).await.unwrap());
            }
            _ => {
                assert!(store.withdraw_host_lost(&lease.id, now).await.unwrap());
                assert_eq!(store.release_interrupted_claims(now + 1).await.unwrap(), 1);
            }
        }
        let row = &f.rows().await[0];
        assert!(terminal(row), "{order}: {row:?}");
        assert!(
            store.pending_host_lost(10).await.unwrap().is_empty(),
            "{order}"
        );
    }
}

#[tokio::test]
async fn a_claim_the_queen_stopped_under_is_judged_again_after_the_grace() {
    // The queen stopped after the claim, before the request reached GitHub.
    let f = Fleet::start(json!({})).await;
    f.h.state().pr_runs.push(pr_run(7, "sha", Some("sha")));
    let lease = f.lose(71, 7).await;
    f.advance(120);
    let store = f.h.queen.store();
    assert!(store.claim_host_lost(&lease.id, f.now()).await.unwrap());
    // Still inside the grace: left alone.
    let r = f.pass().await;
    assert_eq!((r.checked, r.rerun), (0, 0), "{r:?}");
    f.advance(601);
    let r = f.pass().await;
    assert_eq!((r.checked, r.rerun), (1, 1), "{r:?}");
    assert_eq!(f.reruns(), [71]);
    assert_eq!(f.rows().await[0].outcome, Some(Outcome::Rerun));
    // A confirmed rerun is never released again.
    f.advance(700);
    let r = f.pass().await;
    assert_eq!((r.checked, r.rerun), (0, 0), "{r:?}");
    assert_eq!(f.reruns(), [71]);

    // The request did reach GitHub before the queen stopped: the newer
    // attempt is found and nothing is re-run twice.
    let f = Fleet::start(json!({})).await;
    f.h.state().pr_runs.push(pr_run(7, "sha", Some("sha")));
    let lease = f.lose(71, 7).await;
    f.advance(120);
    let store = f.h.queen.store();
    assert!(store.claim_host_lost(&lease.id, f.now()).await.unwrap());
    f.h.state().jobs.retain(|j| j["id"] != 71);
    f.set_job(json!({
        "id": 7100, "run_id": 7, "name": "merge-gate 71", "status": "in_progress",
        "conclusion": null, "labels": [], "created_at": "2026-01-01T00:40:00Z", "steps": []
    }));
    f.advance(601);
    let r = f.pass().await;
    assert_eq!((r.skipped, r.rerun), (1, 0), "{r:?}");
    assert_eq!(f.rows().await[0].reason.as_deref(), Some("already_rerun"));
    assert!(f.reruns().is_empty());
}

#[tokio::test]
async fn a_late_result_for_one_lease_keeps_the_hold_of_another_lost_lease() {
    let f = Fleet::start(json!({})).await;
    let lease = f.lose(71, 7).await;
    let store = f.h.queen.store();
    // A second lease of d1 was lost too and its drone never reported it.
    let other = LeaseRow {
        id: "other0000001".into(),
        ..lease.clone()
    };
    assert!(store.record_host_lost(&other, f.now(), 600).await.unwrap());
    f.d1_reports(&lease).await;
    assert!(f.d1_suspect().await, "the unreported loss still holds d1");

    // A loss from before the cool-down window holds nothing.
    let f = Fleet::start(json!({})).await;
    let lease = f.lose(71, 7).await;
    let old = LeaseRow {
        id: "old000000001".into(),
        ..lease.clone()
    };
    let cooldown = i64::try_from(f.h.queen.config().host_lost.suspect_cooldown_secs).unwrap();
    assert!(
        f.h.queen
            .store()
            .record_host_lost(&old, f.now() - 2 * cooldown, 600)
            .await
            .unwrap()
    );
    f.d1_reports(&lease).await;
    assert!(!f.d1_suspect().await);
}

#[tokio::test]
async fn a_lease_withheld_from_a_suspect_drone_leaves_its_job_to_a_healthy_drone() {
    let f = Fleet::start(json!({})).await;
    f.heard("d1", true).await;
    f.heard("d2", false).await;
    f.h.state().queued_runs = vec![8];
    f.set_job(json!({
        "id": 72, "run_id": 8, "status": "queued", "labels": ["borg-ci-light-pr"],
        "created_at": "2026-01-01T00:00:00Z"
    }));
    f.tick().await;
    let pending = |leases: Vec<LeaseRow>| -> Vec<String> {
        leases
            .into_iter()
            .filter(|l| {
                l.state == LeaseStatus::Pending
                    && l.class == borg::classes::Class::Light
                    && l.scope == borg::classes::Scope::Pr
            })
            .map(|l| l.drone)
            .collect()
    };
    let store = f.h.queen.store();
    assert_eq!(pending(store.active_leases().await.unwrap()), ["d1"]);

    // d1 enters a cool-down before its heartbeat collects the lease, which it
    // is now never handed; d2 accepts work.
    store.suspect_drone("d1", f.now() + 900).await.unwrap();
    f.advance(30);
    f.heard("d1", true).await;
    f.heard("d2", true).await;
    f.tick().await;
    assert_eq!(
        pending(store.active_leases().await.unwrap()),
        ["d1", "d2"],
        "the job gets a runner on d2"
    );
}

#[tokio::test]
async fn results_for_every_loss_end_the_hold_whatever_the_skip_reason() {
    // Reruns are off: both losses of d1 are skipped as `rerun_off` before
    // either result arrives. The skip reason stays; the results still count.
    let f = Fleet::start(json!({"host_lost": {"rerun": false}})).await;
    let lease = f.lose(71, 7).await;
    let store = f.h.queen.store();
    let other = LeaseRow {
        id: "other0000002".into(),
        ..lease.clone()
    };
    assert!(store.record_host_lost(&other, f.now(), 600).await.unwrap());
    for id in [&lease.id, &other.id] {
        assert!(
            store
                .decide_host_lost(
                    id,
                    Outcome::Skipped,
                    Some(borg::hostlost::Skip::RerunOff),
                    f.now()
                )
                .await
                .unwrap()
        );
    }
    // The other lease's result arrives first: d1 is still held by this one.
    assert!(store.withdraw_host_lost(&other.id, f.now()).await.unwrap());
    assert!(f.d1_suspect().await);
    // Then this one's: every loss is reported, so the hold ends.
    f.d1_reports(&lease).await;
    assert!(!f.d1_suspect().await, "both results arrived");
    let rows = f.rows().await;
    assert!(
        rows.iter()
            .all(|r| r.reason.as_deref() == Some("rerun_off")),
        "{rows:?}"
    );
}
