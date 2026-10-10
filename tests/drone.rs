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
use borg::drone::exec::{Exec, Output, SystemExec};
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
        self.drone_with(extra, Arc::clone(&self.exec) as Arc<dyn Exec>)
    }

    fn drone_with(&self, extra: Value, exec: Arc<dyn Exec>) -> Drone {
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
        // Never let a test write the real user's ~/.bazelrc.
        if cfg["caches"].is_null() {
            cfg["caches"] = json!({});
        }
        if cfg["caches"]["bazel"].is_null() {
            cfg["caches"]["bazel"] = json!({});
        }
        cfg["caches"]["bazel"]["bazelrc"] = json!(self.path("bazelrc"));
        let cfg = DroneConfig::from_json(&cfg.to_string()).unwrap();
        let client = QueenClient::new(&cfg, Secret::new(self.token.clone())).unwrap();
        Drone::new(cfg, client, Arc::new(FixedHost), exec)
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
        !rig.path("work/slot-0").exists(),
        "slot dir removed after the job"
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

/// Real `git` for mirror work; everything else is recorded, not executed.
struct GitOnlyExec(RecordingExec);

impl Exec for GitOnlyExec {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<Output> {
        if program == "git" {
            SystemExec.run(program, args)
        } else {
            self.0.run(program, args)
        }
    }
}

fn git(args: &[&str]) -> String {
    let out = SystemExec.run("git", args).unwrap();
    assert!(out.success, "git {args:?} failed");
    out.stdout
}

/// A bare origin at `<base>/example-org/app.git` with one commit on `main`;
/// returns (base, work clone dir).
fn origin(root: &Path) -> (PathBuf, PathBuf) {
    let base = root.join("origin");
    let bare = base.join("example-org/app.git");
    let work = root.join("work-clone");
    fs::create_dir_all(bare.parent().unwrap()).unwrap();
    git(&[
        "init",
        "--bare",
        "--quiet",
        "-b",
        "main",
        bare.to_str().unwrap(),
    ]);
    git(&["init", "--quiet", "-b", "main", work.to_str().unwrap()]);
    git(&[
        "-C",
        work.to_str().unwrap(),
        "remote",
        "add",
        "origin",
        bare.to_str().unwrap(),
    ]);
    commit(&work, "one");
    (base, work)
}

fn commit(work: &Path, msg: &str) -> String {
    let w = work.to_str().unwrap();
    git(&[
        "-C",
        w,
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@example.test",
        "commit",
        "--quiet",
        "--allow-empty",
        "-m",
        msg,
    ]);
    git(&["-C", w, "push", "--quiet", "origin", "main"]);
    git(&["-C", w, "rev-parse", "HEAD"])
}

#[tokio::test]
async fn jobs_share_cache_dirs_and_a_git_mirror_that_survive_cleanup() {
    let rig = Rig::new().await;
    let (base, _work) = origin(rig.dir.path());
    rig.install_runner(
        "printf '%s' \"$BORG_GIT_MIRROR_EXAMPLE_ORG_APP\" > $OUT/mirror-$BORG_LEASE_ID.txt\n\
         git --git-dir \"$BORG_GIT_MIRROR_EXAMPLE_ORG_APP\" rev-parse main > $OUT/head-$BORG_LEASE_ID.txt\n\
         echo \"$SCCACHE_DIR|$CARGO_HOME|$SCCACHE_CACHE_SIZE\" > $OUT/caches-$BORG_LEASE_ID.txt\n\
         echo \"$UV_CACHE_DIR|$GOCACHE|$GOMODCACHE|$MISE_DATA_DIR|$PNPM_STORE_DIR|$BAZELISK_HOME\" > $OUT/more-$BORG_LEASE_ID.txt\n\
         echo \"$CARGO_INSTALL_ROOT\" > $OUT/install-$BORG_LEASE_ID.txt\n\
         touch \"$SCCACHE_DIR/entry-$BORG_LEASE_ID\"",
    );
    let exec = Arc::new(GitOnlyExec(RecordingExec::default()));
    let drone = rig.drone_with(
        json!({"caches": {
            "git": {"mirrors": ["example-org/app"], "remote_base": base.to_str().unwrap()},
            "sccache": {"max_size": "7G"},
        }}),
        exec,
    );
    let mut tasks = JoinSet::new();
    drone.heartbeat_once(true, &mut tasks).await.unwrap();

    for job in [1, 2] {
        rig.h.state().jobs.clear();
        rig.h.queue_job(job, "borg-ci-light", 1);
        rig.h.tick().await;
        assert_eq!(drone.heartbeat_once(true, &mut tasks).await.unwrap(), 1);
        drain(&mut tasks).await;
    }

    let read = |prefix: &str| -> Vec<String> {
        let mut found: Vec<String> = fs::read_dir(rig.path("out"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(prefix))
            .map(|e| fs::read_to_string(e.path()).unwrap().trim().to_owned())
            .collect();
        found.sort();
        found
    };
    let cache = rig.path("cache");
    let mirror = cache.join("git/example-org/app.git").display().to_string();
    assert_eq!(
        read("mirror-"),
        [mirror.clone(), mirror],
        "both leases see the mirror"
    );
    let caches = read("caches-");
    assert_eq!(caches.len(), 2);
    assert_eq!(
        caches[0], caches[1],
        "same cache paths and size for every lease"
    );
    assert_eq!(
        caches[0],
        format!("{0}/sccache|{0}/cargo|7G", cache.display())
    );
    let more = read("more-");
    assert_eq!(more[0], more[1]);
    assert_eq!(
        more[0],
        format!(
            "{0}/uv|{0}/go/build|{0}/go/mod|{0}/mise|{0}/pnpm|{0}/bazel/bazelisk",
            cache.display()
        )
    );
    let installs = read("install-");
    let slot_install = format!("{}/slot-0/cargo-install", rig.path("work").display());
    assert_eq!(
        installs,
        [slot_install.clone(), slot_install],
        "cargo install roots live in the lease's slot, never in the shared CARGO_HOME"
    );
    let bazelrc = fs::read_to_string(rig.path("bazelrc")).unwrap();
    assert!(bazelrc.contains(&format!("--disk_cache={}/bazel/disk", cache.display())));
    let heads = read("head-");
    assert_eq!(heads[0], heads[1]);
    assert_eq!(heads[0].len(), 40, "mirror serves the origin commit");

    assert!(
        cache.join("git/example-org/app.git/HEAD").exists(),
        "mirror survives cleanup"
    );
    let entries = fs::read_dir(cache.join("sccache")).unwrap().count();
    assert_eq!(
        entries, 2,
        "compile cache entries from both leases survive cleanup"
    );
    assert_eq!(fs::read_dir(rig.path("work")).unwrap().count(), 0);
}

#[tokio::test]
async fn the_mirror_timer_picks_up_new_commits() {
    let rig = Rig::new().await;
    let (base, work) = origin(rig.dir.path());
    let json_cfg = json!({
        "queen_url": "http://127.0.0.1:1", "allow_insecure": true,
        "runner_dir": rig.path("runner"), "work_root": rig.path("work"), "cache_root": rig.path("cache"),
        "caches": {
            "git": {"mirrors": ["example-org/app"], "remote_base": base.to_str().unwrap(), "interval_secs": 1},
            "bazel": {"enabled": false},
        },
    });
    let cfg = Arc::new(DroneConfig::from_json(&json_cfg.to_string()).unwrap());
    let caches = Arc::new(borg::drone::caches::Caches::new(
        cfg,
        Arc::new(SystemExec),
        Arc::new(FixedHost),
    ));
    let timer = caches.spawn_background();
    let mirror = rig.path("cache/git/example-org/app.git");
    let head = |expected: &str| {
        let m = mirror.to_str().unwrap().to_owned();
        let expected = expected.to_owned();
        async move {
            for _ in 0..100 {
                let out = SystemExec.run("git", &["--git-dir", &m, "rev-parse", "main"]);
                if out.is_ok_and(|o| o.success && o.stdout == expected) {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            false
        }
    };
    let first = git(&["-C", work.to_str().unwrap(), "rev-parse", "HEAD"]);
    assert!(head(&first).await, "initial clone");
    let second = commit(&work, "two");
    assert!(head(&second).await, "timer refresh fetched the new commit");
    timer.abort();

    // A job can borrow the mirror's objects.
    let clone = rig.path("job-clone");
    git(&[
        "clone",
        "--quiet",
        "--reference",
        mirror.to_str().unwrap(),
        base.join("example-org/app.git").to_str().unwrap(),
        clone.to_str().unwrap(),
    ]);
    assert!(clone.join(".git/objects/info/alternates").exists());
}

#[tokio::test]
async fn leases_get_distinct_slots_while_concurrent_and_the_same_path_when_a_slot_is_reused() {
    let rig = Rig::new().await;
    rig.install_runner(
        "pwd > $OUT/pwd-$BORG_LEASE_ID.txt\necho $BORG_SLOT > $OUT/slot-$BORG_LEASE_ID.txt\nsleep 1",
    );
    let drone = rig.drone(json!({}));
    let mut tasks = JoinSet::new();
    drone.heartbeat_once(true, &mut tasks).await.unwrap();

    rig.h.queue_job(1, "borg-ci-light", 1);
    rig.h.queue_job(2, "borg-ci-light", 1);
    rig.h.tick().await;
    assert_eq!(drone.heartbeat_once(true, &mut tasks).await.unwrap(), 2);
    let during = drone.slot_assignments();
    assert_eq!(
        during.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
        [0, 1],
        "distinct slots while concurrent"
    );
    drain(&mut tasks).await;
    assert!(
        drone.slot_assignments().is_empty(),
        "slots are released after cleanup"
    );

    let read_all = |prefix: &str| -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = fs::read_dir(rig.path("out"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(prefix))
            .map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                (
                    name,
                    fs::read_to_string(e.path()).unwrap().trim().to_owned(),
                )
            })
            .collect();
        v.sort();
        v
    };
    let first_pwds: Vec<String> = read_all("pwd-").into_iter().map(|(_, p)| p).collect();
    let work = rig.path("work");
    let mut want = vec![
        work.join("slot-0").display().to_string(),
        work.join("slot-1").display().to_string(),
    ];
    let mut got = first_pwds.clone();
    got.sort();
    want.sort();
    assert_eq!(got, want);

    // A later lease alone takes slot 0 again, at exactly the same path.
    rig.h.state().jobs.clear();
    rig.h.queue_job(3, "borg-ci-light", 1);
    rig.h.tick().await;
    assert_eq!(drone.heartbeat_once(true, &mut tasks).await.unwrap(), 1);
    drain(&mut tasks).await;
    let all = read_all("pwd-");
    assert_eq!(all.len(), 3);
    let slot_files = read_all("slot-");
    assert_eq!(
        slot_files.iter().filter(|(_, s)| s == "0").count(),
        2,
        "slot 0 was used twice"
    );
    assert_eq!(fs::read_dir(rig.path("work")).unwrap().count(), 0);
}
