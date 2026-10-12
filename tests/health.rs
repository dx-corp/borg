use crate::support;

use borg::classes::{Class, Scope};
use borg::store::{LeaseRow, LeaseStatus};
use borg::telemetry::{Conclusion, FailureClass, JobTelemetry};
use serde_json::{Value, json};
use support::Harness;
use time::OffsetDateTime;

fn at(t: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(t).unwrap()
}

struct Fleet {
    h: Harness,
    tokens: Vec<(String, String)>,
    base: i64,
    n: u64,
}

impl Fleet {
    async fn new(drones: &[&str], extra: Value) -> Self {
        let mut cfg = json!({"drone_stale_secs": 1_000_000_000});
        for (k, v) in extra.as_object().cloned().unwrap_or_default() {
            cfg[k] = v;
        }
        let h = Harness::start(cfg).await;
        let mut tokens = Vec::new();
        for d in drones {
            let token = h.enroll(d).await;
            h.heartbeat(&token, &[]).await;
            tokens.push(((*d).to_owned(), token));
        }
        Self {
            h,
            tokens,
            base: OffsetDateTime::now_utc().unix_timestamp(),
            n: 0,
        }
    }

    /// Record a finished job on `drone` at `base + offset`.
    async fn job(&mut self, drone: &str, offset: i64, conclusion: Conclusion, class: FailureClass) {
        self.n += 1;
        let lease = LeaseRow {
            id: format!("lease-{}", self.n),
            drone: drone.to_owned(),
            class: Class::Light,
            scope: Scope::Pr,
            labels: vec![],
            runner_name: "r".into(),
            runner_id: 1,
            jit_config: None,
            state: LeaseStatus::Done,
            created_at: 0,
            expires_at: 0,
        };
        let t = JobTelemetry {
            job_name: "job".into(),
            repo: "example-org/app".into(),
            run_id: 1000 + self.n,
            conclusion,
            failure_class: class,
            queue_wait_secs: None,
            steps: vec![],
        };
        self.h
            .queen
            .store()
            .record_telemetry(&lease, &t, 10, self.base + offset)
            .await
            .unwrap();
    }

    /// A successful job on `drone` that took `secs`.
    async fn job_taking(&mut self, drone: &str, offset: i64, secs: u64) {
        self.n += 1;
        let lease = LeaseRow {
            id: format!("lease-{}", self.n),
            drone: drone.to_owned(),
            class: Class::Light,
            scope: Scope::Pr,
            labels: vec![],
            runner_name: "r".into(),
            runner_id: 1,
            jit_config: None,
            state: LeaseStatus::Done,
            created_at: 0,
            expires_at: 0,
        };
        let t = JobTelemetry {
            job_name: format!("job (shard {offset})"),
            repo: "example-org/app".into(),
            run_id: 5000 + self.n,
            conclusion: Conclusion::Success,
            failure_class: FailureClass::Unknown,
            queue_wait_secs: None,
            steps: vec![],
        };
        self.h
            .queen
            .store()
            .record_telemetry(&lease, &t, secs, self.base + offset)
            .await
            .unwrap();
    }

    async fn host_failures(&mut self, drone: &str, count: usize, offset: i64) {
        for _ in 0..count {
            self.job(drone, offset, Conclusion::Failure, FailureClass::Host)
                .await;
        }
    }

    async fn tick(&self, offset: i64) {
        self.h.queen.tick(at(self.base + offset)).await.unwrap();
    }

    async fn health(&self, drone: &str) -> Value {
        let status = self.h.status().await;
        status["drones"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["name"] == drone)
            .unwrap()["health"]
            .clone()
    }

    fn token(&self, drone: &str) -> &str {
        &self.tokens.iter().find(|(n, _)| n == drone).unwrap().1
    }
}

#[tokio::test]
async fn corroborated_host_failures_quarantine_a_drone_and_leases_move_away() {
    let mut f = Fleet::new(&["d1", "d2"], json!({})).await;
    f.host_failures("d1", 3, 0).await;
    f.tick(10).await;

    let h = f.health("d1").await;
    assert_eq!(h["state"], "quarantined", "{h}");
    assert_eq!(h["source"], "auto");
    assert_eq!(h["corroborating_runs"], 3);
    assert_eq!(f.health("d2").await["state"], "healthy");

    // The audit trail keeps the evidence.
    let events = f.h.status().await["health_events"].clone();
    assert_eq!(events[0]["event"], "auto_quarantine");
    assert_eq!(events[0]["evidence"].as_array().unwrap().len(), 3);

    // Ties go to d1 by name, so a lease landing on d2 shows d1 is held back.
    f.h.queue_job(1, "borg-ci-light-pr", 5);
    f.tick(11).await;
    assert!(f.h.leases(f.token("d1"), &[]).await.is_empty());
    assert_eq!(f.h.leases(f.token("d2"), &[]).await.len(), 1);

    let metrics = f.h.metrics().await;
    assert!(
        metrics.contains("borg_drone_health_state{drone=\"d1\",state=\"quarantined\"} 1"),
        "{metrics}"
    );
    assert!(metrics.contains("borg_drones{state=\"quarantined\"} 1"));
}

#[tokio::test]
async fn code_failures_and_a_single_host_failure_never_suppress_a_host() {
    let mut f = Fleet::new(&["d1", "d2"], json!({})).await;
    for _ in 0..6 {
        f.job("d1", 0, Conclusion::Failure, FailureClass::Code)
            .await;
    }
    f.host_failures("d1", 1, 0).await;
    f.tick(10).await;
    assert_eq!(f.health("d1").await["state"], "healthy");
    f.host_failures("d1", 1, 0).await;
    f.tick(11).await;
    // Two corroborated failures only recommend.
    assert_eq!(f.health("d1").await["state"], "recommended");
    assert!(
        !f.h.status().await["drones"][0]["quarantined"]
            .as_bool()
            .unwrap()
    );
}

#[tokio::test]
async fn cooldown_probation_and_a_health_proof_return_a_host_to_service() {
    let mut f = Fleet::new(&["d1", "d2"], json!({})).await;
    f.host_failures("d1", 3, 0).await;
    f.tick(10).await;
    assert_eq!(f.health("d1").await["state"], "quarantined");

    // Inside the cooldown (default 1800s) nothing changes.
    f.tick(1000).await;
    assert_eq!(f.health("d1").await["state"], "quarantined");

    // After it, probation: d1 is schedulable again, one lease at a time.
    f.tick(1900).await;
    f.tick(1901).await;
    assert_eq!(f.health("d1").await["state"], "probation");
    f.h.queue_job(1, "borg-ci-light-pr", 5);
    f.tick(1902).await;
    assert_eq!(f.h.leases(f.token("d1"), &[]).await.len(), 1);

    // Two successful jobs after probation began prove it healthy.
    f.job("d1", 1950, Conclusion::Success, FailureClass::Unknown)
        .await;
    f.tick(2000).await;
    assert_eq!(f.health("d1").await["state"], "probation");
    f.job("d1", 2010, Conclusion::Success, FailureClass::Unknown)
        .await;
    f.tick(2020).await;
    f.tick(2021).await;
    assert_eq!(f.health("d1").await["state"], "healthy");
    let events: Vec<String> = f.h.status().await["health_events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["event"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(events, ["release", "probation_start", "auto_quarantine"]);

    // The failures that caused the first quarantine do not count again.
    f.tick(2100).await;
    assert_eq!(f.health("d1").await["state"], "healthy");
}

#[tokio::test]
async fn a_host_failure_on_probation_requarantines_with_a_longer_cooldown() {
    let mut f = Fleet::new(&["d1", "d2"], json!({})).await;
    f.host_failures("d1", 3, 0).await;
    f.tick(10).await;
    f.tick(1900).await;
    f.tick(1901).await;
    assert_eq!(f.health("d1").await["state"], "probation");

    f.host_failures("d1", 1, 1950).await;
    f.tick(2000).await;
    f.tick(2001).await;
    let h = f.health("d1").await;
    assert_eq!(
        (h["state"].as_str(), h["strikes"].as_u64()),
        (Some("quarantined"), Some(1)),
        "{h}"
    );
    // The second cooldown is twice the first (3600s from the requarantine).
    assert_eq!(h["probation_at"], f.base + 2000 + 3600);
}

#[tokio::test]
async fn operator_quarantine_is_never_lifted_by_the_cooldown() {
    let f = Fleet::new(&["d1", "d2"], json!({})).await;
    f.h.queen
        .store()
        .set_quarantine("d1", Some("rack move"))
        .await
        .unwrap();
    f.tick(1_000_000).await;
    let h = f.health("d1").await;
    assert_eq!(
        (h["state"].as_str(), h["source"].as_str()),
        (Some("quarantined"), Some("operator"))
    );
}

#[tokio::test]
async fn fleet_wide_host_failures_only_recommend() {
    let mut f = Fleet::new(&["d1", "d2", "d3", "d4"], json!({})).await;
    f.host_failures("d1", 3, 0).await;
    f.host_failures("d2", 1, 0).await;
    f.job("d3", 0, Conclusion::Success, FailureClass::Unknown)
        .await;
    f.tick(10).await;
    assert_eq!(f.health("d1").await["state"], "recommended");
    assert_eq!(f.h.status().await["health_systemic"], true);
    assert!(f.h.metrics().await.contains("borg_health_systemic 1"));
}

#[tokio::test]
async fn automatic_quarantine_can_be_switched_off() {
    let mut f = Fleet::new(&["d1", "d2"], json!({"health": {"auto_quarantine": false}})).await;
    f.host_failures("d1", 4, 0).await;
    f.tick(10).await;
    let h = f.health("d1").await;
    assert_eq!(h["state"], "recommended");
    assert!(h["note"].as_str().unwrap().contains("off"));
}

#[tokio::test]
async fn learned_profiles_steer_new_leases_to_the_faster_host() {
    // d1 would win every tie by name; d2 has proven faster on the same family.
    let mut f = Fleet::new(&["d1", "d2"], json!({})).await;
    for i in 0..8 {
        f.job_taking("d1", i, 200 + i.unsigned_abs()).await;
        f.job_taking("d2", i, 100 + i.unsigned_abs()).await;
    }
    f.tick(10).await;
    let status = f.h.status().await;
    let p2 = status["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["drone"] == "d2" && p["class"] == "light")
        .unwrap();
    assert!(p2["speed_permille"].as_u64().unwrap() > 1100, "{p2}");
    assert_eq!(p2["families"][0]["family"], "job");
    assert!(
        f.h.metrics()
            .await
            .contains("borg_drone_profile_speed{drone=\"d2\",class=\"light\"}")
    );

    f.h.queue_job(1, "borg-ci-light-pr", 5);
    f.tick(20).await;
    assert!(f.h.leases(f.token("d1"), &[]).await.is_empty());
    assert_eq!(f.h.leases(f.token("d2"), &[]).await.len(), 1);
}
