mod support;

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use borg::drone::Drone;
use borg::drone::client::QueenClient;
use borg::drone::config::DroneConfig;
use borg::drone::exec::{Exec, Output};
use borg::probe::Host;
use borg::secret::Secret;
use serde_json::{Value, json};
use support::Harness;
use tokio::task::JoinSet;

/// A 30 GiB, 16 vCPU Linux host with a working docker.
struct FixedHost;

impl Host for FixedHost {
    fn read_to_string(&self, path: &Path) -> Option<String> {
        (path == Path::new("/proc/meminfo")).then(|| "MemTotal:       31457280 kB\n".to_owned())
    }
    fn exists(&self, _path: &Path) -> bool {
        false
    }
    fn run(&self, program: &str, _args: &[&str]) -> Option<String> {
        (program == "docker").then(|| "27.0".to_owned())
    }
    fn os(&self) -> &'static str {
        "linux"
    }
    fn arch(&self) -> &'static str {
        "x86_64"
    }
    fn available_parallelism(&self) -> Option<u32> {
        Some(16)
    }
}

/// Records host commands instead of running them.
#[derive(Default)]
struct RecordingExec(Mutex<Vec<String>>);

impl Exec for RecordingExec {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<Output> {
        self.0
            .lock()
            .unwrap()
            .push(format!("{program} {}", args.join(" ")));
        Ok(Output {
            success: true,
            stdout: String::new(),
        })
    }
}

struct Rig {
    h: Harness,
    dir: tempfile::TempDir,
    token: String,
    exec: Arc<RecordingExec>,
}

impl Rig {
    async fn new() -> Self {
        let h = Harness::start(json!({})).await;
        let token = h.enroll("d1").await;
        Self {
            h,
            dir: tempfile::tempdir().unwrap(),
            token,
            exec: Arc::default(),
        }
    }

    fn path(&self, p: &str) -> PathBuf {
        self.dir.path().join(p)
    }

    /// A fake pinned runner install whose run.sh executes `body`.
    fn install_runner(&self, body: &str) {
        let runner = self.path("runner");
        fs::create_dir_all(runner.join("bin")).unwrap();
        let script = runner.join("run.sh");
        fs::write(
            &script,
            format!("#!/bin/sh\nOUT={}\n{body}\n", self.path("out").display()),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        fs::create_dir_all(self.path("out")).unwrap();
    }

    fn drone(&self, extra: Value) -> Drone {
        let mut cfg = json!({
            "queen_url": self.h.url,
            "allow_insecure": true,
            "runner_dir": self.path("runner"),
            "work_root": self.path("work"),
            "cache_root": self.path("cache"),
            "heartbeat_secs": 1,
            "drain_timeout_secs": 30,
        });
        for (k, v) in extra.as_object().cloned().unwrap_or_default() {
            cfg[k] = v;
        }
        let cfg = DroneConfig::from_json(&cfg.to_string()).unwrap();
        let client = QueenClient::new(&cfg, Secret::new(self.token.clone())).unwrap();
        Drone::new(
            cfg,
            client,
            Arc::new(FixedHost),
            Arc::clone(&self.exec) as Arc<dyn Exec>,
        )
    }

    async fn lease_state(&self) -> Vec<String> {
        let s = self.h.status().await;
        let c = &s["classes"]["light"];
        vec![format!(
            "pending={} running={}",
            c["leases_pending"], c["leases_running"]
        )]
    }
}

async fn drain(tasks: &mut JoinSet<()>) {
    while tasks.join_next().await.is_some() {}
}

#[tokio::test]
async fn runs_a_granted_lease_cleans_up_and_reports_the_result() {
    let rig = Rig::new().await;
    rig.install_runner(
        "echo \"$@\" > $OUT/args.txt\nprintf '%s' \"$ACTIONS_RUNNER_INPUT_JITCONFIG\" > $OUT/jit.txt\ntouch junk _diag.log\nexit 0",
    );
    fs::create_dir_all(rig.path("cache/sccache")).unwrap();
    fs::write(rig.path("cache/sccache/entry"), "cached").unwrap();
    let drone = rig.drone(json!({}));
    let mut tasks = JoinSet::new();

    assert_eq!(
        drone.heartbeat_once(true, &mut tasks).await.unwrap(),
        0,
        "registers capacity, nothing granted yet"
    );
    rig.h.queue_job(1, "borg-ci-light-pr", 3);
    rig.h.tick().await;
    assert_eq!(drone.heartbeat_once(true, &mut tasks).await.unwrap(), 1);
    drain(&mut tasks).await;

    let name = rig.h.state().mints[0]["name"].as_str().unwrap().to_owned();
    assert_eq!(
        fs::read_to_string(rig.path("out/jit.txt")).unwrap(),
        format!("jit-{name}")
    );
    assert_eq!(
        fs::read_to_string(rig.path("out/args.txt")).unwrap().trim(),
        "",
        "the JIT config is never an argument"
    );
    let lease_id = name.rsplit('-').next().unwrap().to_owned();
    assert!(
        !rig.path("work").join(&lease_id).exists(),
        "work dir removed after the job"
    );
    assert_eq!(
        fs::read_to_string(rig.path("cache/sccache/entry")).unwrap(),
        "cached",
        "caches survive cleanup"
    );
    assert!(
        rig.h
            .queen
            .store()
            .active_leases()
            .await
            .unwrap()
            .is_empty(),
        "result reported, lease done"
    );
    let calls = rig.exec.0.lock().unwrap().clone();
    assert!(
        calls
            .iter()
            .any(|c| c.contains(&format!("--filter name=borg-{lease_id}"))),
        "{calls:?}"
    );
    assert!(calls.iter().all(|c| !c.contains("prune")));
    assert_eq!(
        drone.heartbeat_once(true, &mut tasks).await.unwrap(),
        0,
        "nothing new to start"
    );
}

#[tokio::test]
async fn a_failing_job_is_still_cleaned_up_and_finished() {
    let rig = Rig::new().await;
    rig.install_runner("exit 3");
    let drone = rig.drone(json!({}));
    let mut tasks = JoinSet::new();
    drone.heartbeat_once(true, &mut tasks).await.unwrap();
    rig.h.queue_job(1, "borg-ci-light", 1);
    rig.h.tick().await;
    drone.heartbeat_once(true, &mut tasks).await.unwrap();
    drain(&mut tasks).await;
    assert!(
        rig.h
            .queen
            .store()
            .active_leases()
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(fs::read_dir(rig.path("work")).unwrap().count(), 0);
}

#[tokio::test]
async fn owner_lease_cap_is_advertised_and_respected() {
    let rig = Rig::new().await;
    rig.install_runner("sleep 1");
    let drone = rig.drone(json!({"max_leases": 1}));
    let mut tasks = JoinSet::new();
    drone.heartbeat_once(true, &mut tasks).await.unwrap();
    rig.h.queue_job(1, "borg-ci-light", 1);
    rig.h.queue_job(2, "borg-ci-light", 1);
    rig.h.tick().await;
    // The drone advertised max_leases = 1, so the queen only granted one.
    assert_eq!(rig.h.state().mints.len(), 1);
    assert_eq!(drone.heartbeat_once(true, &mut tasks).await.unwrap(), 1);
    assert_eq!(
        drone.heartbeat_once(true, &mut tasks).await.unwrap(),
        0,
        "already held: not started twice"
    );
    drain(&mut tasks).await;
    assert_eq!(rig.lease_state().await, ["pending=0 running=0"]);
}

#[tokio::test]
async fn shutdown_stops_taking_leases_and_lets_the_running_job_finish() {
    let rig = Rig::new().await;
    rig.install_runner("touch $OUT/started\nsleep 2\ntouch $OUT/finished");
    fs::create_dir_all(rig.path("work/stale-lease")).unwrap();
    let drone = rig.drone(json!({}));
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let run = tokio::spawn(drone.run(async move {
        let _ = stop_rx.await;
    }));

    // Wait for the first heartbeat to register, then give the queen a job.
    tokio::time::sleep(Duration::from_millis(300)).await;
    rig.h.queue_job(1, "borg-ci-light", 1);
    rig.h.tick().await;
    for _ in 0..50 {
        if rig.path("out/started").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(rig.path("out/started").exists(), "lease started");
    assert!(
        !rig.path("work/stale-lease").exists(),
        "startup sweep removed stale lease dirs"
    );

    stop_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .unwrap()
        .unwrap();
    assert!(
        rig.path("out/finished").exists(),
        "in-flight job ran to completion"
    );
    assert!(
        rig.h
            .queen
            .store()
            .active_leases()
            .await
            .unwrap()
            .is_empty()
    );
}
