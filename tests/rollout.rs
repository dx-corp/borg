mod support;

use borg::classes::{Class, Scope};
use borg::store::{LeaseRow, LeaseStatus};
use borg::telemetry::{Conclusion, FailureClass, JobTelemetry};
use serde_json::{Value, json};
use support::Harness;
use time::OffsetDateTime;

struct Rig {
    h: Harness,
    tokens: Vec<(String, String)>,
    base: i64,
    n: u64,
}

fn gated() -> Value {
    json!({
        "drone_stale_secs": 1_000_000_000,
        "rollout_target_version": "2.0.0",
        "rollout": {
            "previous_version": "1.0.0", "canary_jobs": 2, "waves_pct": [50, 100],
            "wave_jobs": 2, "soak_secs": 60, "update_timeout_secs": 300,
            "min_baseline_jobs": 100
        }
    })
}

impl Rig {
    async fn new(cfg: Value) -> Self {
        let h = Harness::start(cfg).await;
        let mut tokens = Vec::new();
        for d in ["l1", "l2", "m1"] {
            tokens.push((d.to_owned(), h.enroll(d).await));
        }
        let rig = Self {
            h,
            tokens,
            base: OffsetDateTime::now_utc().unix_timestamp(),
            n: 0,
        };
        for d in ["l1", "l2", "m1"] {
            rig.beat(d, "1.0.0").await;
        }
        rig
    }

    fn token(&self, d: &str) -> &str {
        &self.tokens.iter().find(|(n, _)| n == d).unwrap().1
    }

    /// Heartbeat as `d` running `version`; returns the response.
    async fn beat(&self, d: &str, version: &str) -> Value {
        let mut cap = serde_json::to_value(support::capacity()).unwrap();
        if d.starts_with('m') {
            cap["os"] = json!("macos");
        }
        self.h
            .heartbeat_with(self.token(d), json!({"capacity": cap, "version": version}))
            .await
    }

    async fn desired(&self, d: &str, running: &str) -> Option<String> {
        self.beat(d, running).await["desired_version"]
            .as_str()
            .map(str::to_owned)
    }

    async fn job(&mut self, d: &str, offset: i64, ok: bool) {
        self.n += 1;
        let lease = LeaseRow {
            id: format!("lease-{}", self.n),
            drone: d.to_owned(),
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
            run_id: 9000 + self.n,
            conclusion: if ok {
                Conclusion::Success
            } else {
                Conclusion::Failure
            },
            failure_class: if ok {
                FailureClass::Unknown
            } else {
                FailureClass::Host
            },
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

    async fn tick(&self, offset: i64) {
        self.h
            .queen
            .tick(OffsetDateTime::from_unix_timestamp(self.base + offset).unwrap())
            .await
            .unwrap();
    }

    async fn phase(&self) -> String {
        self.h.status().await["rollout"]["gate"]["state"]["phase"]
            .as_str()
            .unwrap_or("none")
            .to_owned()
    }
}

#[tokio::test]
async fn a_rollout_goes_canary_then_waves_then_complete_with_the_right_drones_told() {
    let mut r = Rig::new(gated()).await;
    // Before the first pass the queen gives no instruction.
    assert_eq!(r.desired("l1", "1.0.0").await, None);

    r.tick(0).await;
    assert_eq!(r.phase().await, "canary");
    assert_eq!(r.desired("l1", "1.0.0").await.as_deref(), Some("2.0.0"));
    assert_eq!(r.desired("m1", "1.0.0").await.as_deref(), Some("2.0.0"));
    assert_eq!(r.desired("l2", "1.0.0").await.as_deref(), Some("1.0.0"));

    // The canaries update and each completes two jobs.
    r.beat("l1", "2.0.0").await;
    r.beat("m1", "2.0.0").await;
    for d in ["l1", "m1"] {
        r.job(d, 10, true).await;
        r.job(d, 11, true).await;
    }
    r.tick(20).await;
    assert_eq!(r.phase().await, "wave");

    // Soak not over: still wave 1.
    r.job("l1", 30, true).await;
    r.job("m1", 31, true).await;
    r.tick(50).await;
    let st = r.h.status().await;
    assert_eq!(st["rollout"]["gate"]["state"]["wave"], 0);
    assert!(
        st["rollout"]["gate"]["waiting"]
            .as_str()
            .unwrap()
            .contains("soaking")
    );

    // Soaked, enough jobs: wave 2 tells everyone.
    r.tick(100).await;
    assert_eq!(r.desired("l2", "1.0.0").await.as_deref(), Some("2.0.0"));
    r.beat("l2", "2.0.0").await;
    r.job("l2", 110, true).await;
    r.job("l2", 111, true).await;
    r.tick(200).await;
    assert_eq!(r.phase().await, "complete");
    let log = r.h.status().await["rollout"]["log"].clone();
    assert!(log[0][1].as_str().unwrap().contains("complete"), "{log}");
    let metrics = r.h.metrics().await;
    assert!(
        metrics.contains("borg_rollout_phase{phase=\"complete\"} 1"),
        "{metrics}"
    );

    // The state survives a restart: a fresh pass keeps it complete.
    r.tick(300).await;
    assert_eq!(r.phase().await, "complete");
}

#[tokio::test]
async fn host_failures_on_a_canary_halt_the_rollout_and_roll_every_drone_back() {
    let mut r = Rig::new(gated()).await;
    r.tick(0).await;
    r.beat("l1", "2.0.0").await;
    r.beat("m1", "2.0.0").await;
    for i in 0..3 {
        r.job("l1", 10 + i, true).await;
        r.job("m1", 10 + i, false).await;
    }
    r.tick(30).await;
    assert_eq!(r.phase().await, "halted");
    for d in ["l1", "m1", "l2"] {
        assert_eq!(r.desired(d, "2.0.0").await.as_deref(), Some("1.0.0"), "{d}");
    }
    let st = r.h.status().await;
    assert!(
        st["rollout"]["gate"]["state"]["halted_reason"]
            .as_str()
            .unwrap()
            .contains("m1")
    );
    assert!(
        r.h.metrics()
            .await
            .contains("borg_rollout_phase{phase=\"halted\"} 1")
    );

    // Halted stays halted, even with a clean later history.
    r.job("m1", 40, true).await;
    r.tick(100).await;
    assert_eq!(r.phase().await, "halted");
}

#[tokio::test]
async fn without_a_previous_version_the_target_is_only_reported_and_everyone_is_told_it() {
    let r = Rig::new(json!({
        "drone_stale_secs": 1_000_000_000,
        "rollout_target_version": "2.0.0"
    }))
    .await;
    r.tick(0).await;
    assert_eq!(r.desired("l2", "1.0.0").await.as_deref(), Some("2.0.0"));
    assert_eq!(r.phase().await, "none");

    let none = Rig::new(json!({"drone_stale_secs": 1_000_000_000})).await;
    none.tick(0).await;
    assert_eq!(none.desired("l1", "1.0.0").await, None);
}
