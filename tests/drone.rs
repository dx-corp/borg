use crate::support;

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
    fn run_grouped(&self, program: &str, args: &[&str]) -> io::Result<Output> {
        self.run(program, args)
    }

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
        Self::with_queen(json!({})).await
    }

    async fn with_queen(queen: Value) -> Self {
        let h = Harness::start(queen).await;
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
    let diag = rig
        .h
        .queen
        .store()
        .lease_host(&lease_id, 30.0)
        .await
        .unwrap()
        .expect("host samples stored with the result");
    assert_eq!(diag.drone, "d1");
    assert_eq!(diag.exit_code, Some(0));
    let (start, end) = (diag.start.unwrap(), diag.end.unwrap());
    assert_eq!((start.cpus, start.active_leases), (Some(16), 1));
    assert!(end.at >= start.at);
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

/// Runs everything except docker, which is only recorded.
struct AllButDocker(RecordingExec);

impl Exec for AllButDocker {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<Output> {
        if program == "docker" {
            self.0.run(program, args)
        } else {
            SystemExec.run(program, args)
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
        Arc::new(borg::drone::fsops::LocalFs),
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
    let work = fs::canonicalize(rig.path("work")).unwrap();
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

/// A stand-in for `setpriv` that records its arguments, then runs the command
/// after `--` as the current user.
fn fake_setpriv(dir: &Path, log: &Path) -> PathBuf {
    let path = dir.join("fake-setpriv");
    fs::write(
        &path,
        format!(
            "#!/bin/sh\necho \"$@\" >> {}\nwhile [ \"$1\" != \"--\" ]; do shift; done\nshift\nexec \"$@\"\n",
            log.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[tokio::test]
async fn with_a_job_user_the_runner_and_its_file_operations_go_through_setpriv() {
    let rig = Rig::new().await;
    let log = rig.path("setpriv.log");
    let setpriv = fake_setpriv(rig.dir.path(), &log);
    rig.install_runner(
        "printf '%s' \"$ACTIONS_RUNNER_INPUT_JITCONFIG\" > $OUT/jit.txt\necho \"$HOME|$USER\" > $OUT/who.txt\necho \"$@\" > $OUT/args.txt",
    );
    let drone = rig.drone_with(
        json!({"job_user": {
            "name": "borg-job",
            "home": rig.path("job-home"),
            "priv_bin": setpriv,
        }}),
        Arc::new(AllButDocker(RecordingExec::default())),
    );
    let mut tasks = JoinSet::new();
    drone.heartbeat_once(true, &mut tasks).await.unwrap();
    rig.h.queue_job(1, "borg-ci-light", 1);
    rig.h.tick().await;
    assert_eq!(drone.heartbeat_once(true, &mut tasks).await.unwrap(), 1);
    drain(&mut tasks).await;

    let calls = fs::read_to_string(&log).unwrap();
    let prefix = "--reuid borg-job --regid borg-job --init-groups --inh-caps=-all --ambient-caps=-all --bounding-set=-all --";
    // File contents passed as arguments can span lines; only count call starts.
    let lines: Vec<&str> = calls.lines().collect();
    let starts: Vec<&&str> = lines.iter().filter(|l| l.starts_with("--reuid")).collect();
    assert!(starts.len() >= 5, "{calls}");
    assert!(
        starts.iter().all(|l| l.starts_with(prefix)),
        "every call drops to the job user: {calls}"
    );
    let has = |needle: &str| lines.iter().any(|l| l.contains(needle));
    assert!(
        has("cp -al"),
        "the runner install is cloned as the job user: {calls}"
    );
    assert!(
        has("/work/slot-0/run.sh"),
        "run.sh is started through setpriv: {calls}"
    );
    assert!(
        has("rm -rf -- ") && has("/work/slot-0"),
        "cleanup removes the slot as the job user: {calls}"
    );

    let name = rig.h.state().mints[0]["name"].as_str().unwrap().to_owned();
    assert_eq!(
        fs::read_to_string(rig.path("out/jit.txt")).unwrap(),
        format!("jit-{name}")
    );
    assert_eq!(
        fs::read_to_string(rig.path("out/args.txt")).unwrap().trim(),
        ""
    );
    assert_eq!(
        fs::read_to_string(rig.path("out/who.txt")).unwrap().trim(),
        format!("{}|borg-job", rig.path("job-home").display()),
        "the job sees the job user's home"
    );
    assert!(!rig.path("work/slot-0").exists());
}

#[tokio::test]
async fn extra_labels_reach_the_minted_runner_but_class_labels_are_dropped() {
    let rig = Rig::new().await;
    rig.install_runner("true");
    let drone = rig.drone(json!({"extra_labels": ["on-prem", "legacy-lane"]}));
    let mut tasks = JoinSet::new();
    drone.heartbeat_once(true, &mut tasks).await.unwrap();
    rig.h.queue_job(1, "borg-ci-light-pr", 1);
    rig.h.tick().await;
    let labels: Vec<String> = rig.h.state().mints[0]["labels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        labels,
        [
            "self-hosted",
            "Linux",
            "X64",
            "on-prem",
            "legacy-lane",
            "borg-ci-light",
            "borg-ci-light-pr"
        ]
    );
    drain(&mut tasks).await;

    // A drone cannot smuggle a class label past the queen: a PR runner stays PR-scoped.
    let req = json!({"capacity": support::capacity(), "extra_labels": ["borg-ci-light", "borg-ci-heavy-pr", "ok-label"]});
    let r = rig
        .h
        .http
        .post(format!("{}/v1/heartbeat", rig.h.url))
        .bearer_auth(&rig.token)
        .json(&req)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    rig.h.state().jobs.clear();
    rig.h.state().mints.clear();
    rig.h.queue_job(2, "borg-ci-medium-pr", 1);
    rig.h.tick().await;
    let labels: Vec<String> = rig.h.state().mints[0]["labels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        labels,
        [
            "self-hosted",
            "Linux",
            "X64",
            "ok-label",
            "borg-ci-medium",
            "borg-ci-medium-pr"
        ]
    );
}

#[tokio::test]
async fn runner_env_and_hooks_are_exported_to_every_lease() {
    let rig = Rig::new().await;
    rig.install_runner(
        "echo \"$HTTP_PROXY|$ACTIONS_RUNNER_HOOK_JOB_STARTED|$ACTIONS_RUNNER_HOOK_JOB_COMPLETED\" > $OUT/env.txt",
    );
    let drone = rig.drone(json!({
        "runner_env": {"HTTP_PROXY": "http://proxy.example.test:3128"},
        "job_started_hook": "/etc/borg/hooks/start.sh",
        "job_completed_hook": "/etc/borg/hooks/done.sh",
    }));
    let mut tasks = JoinSet::new();
    drone.heartbeat_once(true, &mut tasks).await.unwrap();
    rig.h.queue_job(1, "borg-ci-light", 1);
    rig.h.tick().await;
    drone.heartbeat_once(true, &mut tasks).await.unwrap();
    drain(&mut tasks).await;
    assert_eq!(
        fs::read_to_string(rig.path("out/env.txt")).unwrap().trim(),
        "http://proxy.example.test:3128|/etc/borg/hooks/start.sh|/etc/borg/hooks/done.sh"
    );
}

fn request_version(dir: &Path) -> Option<String> {
    let v: Value = serde_json::from_slice(&fs::read(dir.join("update-request")).ok()?).ok()?;
    v["version"].as_str().map(str::to_owned)
}

#[tokio::test]
async fn an_idle_drone_asks_for_the_version_the_queen_wants_and_takes_no_lease() {
    let rig = Rig::with_queen(json!({"rollout_target_version": "9.9.9"})).await;
    rig.install_runner("exit 0");
    let state = rig.path("state");
    fs::create_dir_all(&state).unwrap();
    let drone = rig.drone(json!({"state_dir": state}));
    let mut tasks = JoinSet::new();

    drone.heartbeat_once(true, &mut tasks).await.unwrap();
    assert_eq!(request_version(&state).as_deref(), Some("9.9.9"));
    assert!(
        state.join("health.json").exists(),
        "health evidence is written"
    );

    rig.h.queue_job(1, "borg-ci-light-pr", 3);
    rig.h.tick().await;
    assert_eq!(
        drone.heartbeat_once(true, &mut tasks).await.unwrap(),
        0,
        "no new lease while an update is pending"
    );
}

#[tokio::test]
async fn capability_heartbeat_proves_job_user_execution_and_reuses_probe_timestamps() {
    let rig = Rig::new().await;
    let drone = rig.drone(json!({
        "job_user": {"name": "borg-job", "home": "/var/lib/borg-job"},
        "capability_probe": {"docker_image": "busybox:local"}
    }));
    let mut tasks = JoinSet::new();
    drone.heartbeat_once(true, &mut tasks).await.unwrap();
    drain(&mut tasks).await;
    // The next heartbeat publishes the asynchronously completed proof.
    drone.heartbeat_once(true, &mut tasks).await.unwrap();
    let first = rig.h.queen.store().drones().await.unwrap();
    let facts = &first[0].env_facts.as_ref().unwrap().capabilities;
    for name in borg::envcontract::Capability::ALL {
        assert_eq!(facts[&name].state, borg::envcontract::CheckState::Pass);
        assert!(facts[&name].observed_at > 0);
    }
    let calls = rig.exec.0.lock().unwrap().clone();
    let probes: Vec<_> = calls
        .iter()
        .filter(|c| c.contains("--kill-after=2s"))
        .collect();
    assert_eq!(probes.len(), 7);
    assert!(probes.iter().all(
        |c| c.starts_with("setpriv --reuid borg-job --regid borg-job")
            && c.contains("--bounding-set=-all -- timeout")
    ));
    drone.heartbeat_once(true, &mut tasks).await.unwrap();
    let second = rig.h.queen.store().drones().await.unwrap();
    assert_eq!(facts, &second[0].env_facts.as_ref().unwrap().capabilities);
    assert_eq!(
        rig.exec
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.contains("--kill-after=2s"))
            .count(),
        7
    );
}

/// Deliberately blocks only capability commands; inventory/tool probes stay
/// fast. Releasing the gate lets the full smoke and cleanup sequence finish.
#[derive(Default)]
struct GatedCapabilityExec {
    released: (Mutex<bool>, std::sync::Condvar),
    started: tokio::sync::Notify,
    commands: std::sync::atomic::AtomicUsize,
}

impl GatedCapabilityExec {
    fn release(&self) {
        *self.released.0.lock().unwrap() = true;
        self.released.1.notify_all();
    }
}

impl Exec for GatedCapabilityExec {
    fn run_grouped(&self, program: &str, args: &[&str]) -> io::Result<Output> {
        self.run(program, args)
    }

    fn run(&self, _program: &str, args: &[&str]) -> io::Result<Output> {
        if args.contains(&"--kill-after=2s") {
            self.commands
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.started.notify_one();
            let mut released = self.released.0.lock().unwrap();
            while !*released {
                released = self.released.1.wait(released).unwrap();
            }
        }
        Ok(Output {
            success: true,
            stdout: String::new(),
        })
    }
}

/// A failing assertion must also release the blocking thread, so a regression
/// fails promptly instead of hanging the Tokio runtime's shutdown.
struct ReleaseCapabilityGate(Arc<GatedCapabilityExec>);
impl Drop for ReleaseCapabilityGate {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[tokio::test]
async fn blocked_capability_refresh_does_not_block_heartbeats_or_duplicate_probes() {
    let rig = Rig::with_queen(json!({
        "env_contract": {
            "enabled": true,
            "checks": {"capabilities": "required"},
            "required_capabilities": ["sandbox"]
        }
    }))
    .await;
    let exec = Arc::new(GatedCapabilityExec::default());
    let release = ReleaseCapabilityGate(Arc::clone(&exec));
    let drone = rig.drone_with(
        json!({
            "capability_probe": {"timeout_secs": 10, "docker_image": "busybox:local"}
        }),
        exec.clone(),
    );
    let mut tasks = JoinSet::new();
    tokio::time::timeout(
        Duration::from_secs(30),
        drone.heartbeat_once(true, &mut tasks),
    )
    .await
    .expect("heartbeat must not wait for smoke completion")
    .unwrap();
    tokio::time::timeout(Duration::from_secs(30), exec.started.notified())
        .await
        .expect("smoke should start in background");
    assert_eq!(exec.commands.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        tasks.len(),
        1,
        "unfinished refresh belongs to the drain task set"
    );
    let stored = rig.h.queen.store().drones().await.unwrap();
    assert!(
        stored[0].last_heartbeat.is_some(),
        "heartbeat reached the queen while the smoke was blocked"
    );
    assert!(
        stored[0]
            .env_facts
            .as_ref()
            .unwrap()
            .capabilities
            .is_empty()
    );
    assert_eq!(
        rig.h.status().await["drones"][0]["contract"]["admission"]["light"]["admit"],
        false
    );
    tokio::time::timeout(
        Duration::from_secs(30),
        drone.heartbeat_once(true, &mut tasks),
    )
    .await
    .expect("later heartbeat must also remain independent of the refresh")
    .unwrap();
    assert_eq!(exec.commands.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(tasks.len(), 1, "refresh is single-flight");
    tokio::time::timeout(
        Duration::from_secs(30),
        drone.heartbeat_once(false, &mut tasks),
    )
    .await
    .expect("draining heartbeat must not wait for the refresh")
    .unwrap();
    assert_eq!(tasks.len(), 1);
    drop(release);
    tokio::time::timeout(Duration::from_secs(30), drain(&mut tasks))
        .await
        .expect("drain joins refresh and its cleanup");
    assert_eq!(exec.commands.load(std::sync::atomic::Ordering::SeqCst), 7);
    drone.heartbeat_once(false, &mut tasks).await.unwrap();
    let stored = rig.h.queen.store().drones().await.unwrap();
    assert_eq!(
        stored[0].env_facts.as_ref().unwrap().capabilities[&borg::envcontract::Capability::Sandbox]
            .state,
        borg::envcontract::CheckState::Pass
    );
}

#[tokio::test]
async fn draining_drones_do_not_start_capability_refreshes() {
    let rig = Rig::new().await;
    let drone = rig.drone(json!({}));
    let mut tasks = JoinSet::new();
    drone.heartbeat_once(false, &mut tasks).await.unwrap();
    assert!(tasks.is_empty());
    assert!(
        !rig.exec
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.contains("--kill-after=2s"))
    );
}
