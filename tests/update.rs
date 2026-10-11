//! `borg update` against a fake manifest/artifact server, a fake service
//! manager and a fake health probe.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use borg::drone::exec::{Exec, Output};
use borg::update::health::{HealthProbe, HealthRecord};
use borg::update::manifest::{self, Artifact, Manifest, ManifestError, RunnerRelease};
use borg::update::service::ServiceManager;
use borg::update::swap::{self, SwapError};
use borg::update::{Options, Outcome, UpdateError};
use sha2::{Digest, Sha256};

const NEW_BINARY: &[u8] = b"#!/bin/sh\necho new borg\n";
const RUNNER_TARBALL: &[u8] = b"fake runner tarball";

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[derive(Clone)]
struct Served {
    hits: Arc<AtomicU32>,
}

async fn binary(State(s): State<Served>) -> Vec<u8> {
    s.hits.fetch_add(1, Ordering::SeqCst);
    NEW_BINARY.to_vec()
}

/// Serves `/borg` (the new binary) and `/missing` (404).
async fn serve() -> (String, Served) {
    let served = Served {
        hits: Arc::default(),
    };
    let app = Router::new()
        .route("/borg", get(binary))
        .route("/missing", get(|| async { StatusCode::NOT_FOUND }))
        .with_state(served.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (url, served)
}

fn manifest(base: &str, version: &str, binary_sha: &str, runner: Option<(&str, &str)>) -> Manifest {
    let mut targets = BTreeMap::new();
    targets.insert(
        "linux-x64-musl".to_owned(),
        Artifact {
            url: format!("{base}/borg"),
            sha256: binary_sha.to_owned(),
        },
    );
    Manifest {
        schema: manifest::SCHEMA,
        version: version.to_owned(),
        targets,
        runner: runner.map(|(v, s)| RunnerRelease {
            version: v.to_owned(),
            sha256: BTreeMap::from([("linux-x64".to_owned(), s.to_owned())]),
        }),
    }
}

/// Records what the service was asked to do, and what the installed binary
/// looked like at that moment.
struct FakeService {
    bin: PathBuf,
    events: Arc<Mutex<Vec<String>>>,
    fail_stop: bool,
    /// Make the nth start (1-based) fail.
    fail_start_number: Option<usize>,
    starts: AtomicU32,
}

impl FakeService {
    fn log(&self, what: &str) {
        let content = fs::read_to_string(&self.bin).unwrap_or_default();
        self.events
            .lock()
            .unwrap()
            .push(format!("{what}:{}", content.trim()));
    }
}

impl ServiceManager for FakeService {
    fn stop(&self) -> Result<(), String> {
        self.log("stop");
        if self.fail_stop {
            Err("unit refused to stop".into())
        } else {
            Ok(())
        }
    }
    fn start(&self) -> Result<(), String> {
        self.log("start");
        let nth = self.starts.fetch_add(1, Ordering::SeqCst) as usize + 1;
        if self.fail_start_number == Some(nth) {
            Err("failed to start".into())
        } else {
            Ok(())
        }
    }
}

/// Healthy for `version` only if `healthy_for` says so.
struct FakeHealth {
    healthy_for: Option<String>,
}

impl HealthProbe for FakeHealth {
    fn current(&self) -> Option<HealthRecord> {
        self.healthy_for.clone().map(|version| HealthRecord {
            version,
            at: 1,
            pid: 1,
        })
    }
}

struct Rig {
    dir: tempfile::TempDir,
    bin: PathBuf,
    events: Arc<Mutex<Vec<String>>>,
}

impl Rig {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("borg");
        fs::write(&bin, "v1\n").unwrap();
        Self {
            dir,
            bin,
            events: Arc::default(),
        }
    }

    fn service(&self) -> FakeService {
        FakeService {
            bin: self.bin.clone(),
            events: Arc::clone(&self.events),
            fail_stop: false,
            fail_start_number: None,
            starts: AtomicU32::new(0),
        }
    }

    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }

    fn options<'a>(&'a self, current: &'a str) -> Options<'a> {
        Options {
            bin: &self.bin,
            current_version: current,
            requested_version: None,
            check: false,
            runner_only: false,
            allow_insecure: true,
            health_timeout: Duration::from_millis(300),
            poll: Duration::from_millis(10),
            host_os: "linux",
            host_arch: "x86_64",
            runner_dir: None,
            runner_owner: None,
        }
    }

    fn leftovers(&self) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(self.dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "borg" && n != "borg.prev" && n != "jobhome")
            .collect();
        v.sort();
        v
    }
}

struct NoExec;
impl Exec for NoExec {
    fn run(&self, program: &str, _: &[&str]) -> io::Result<Output> {
        panic!("unexpected command {program}");
    }
}

#[tokio::test]
async fn updates_drain_first_then_swap_then_confirm_health_and_keep_the_previous_binary() {
    let (base, served) = serve().await;
    let rig = Rig::new();
    let m = manifest(&base, "0.5.0", &sha(NEW_BINARY), None);
    let out = borg::update::run(
        &rig.options("0.4.0"),
        &m,
        &rig.service(),
        &FakeHealth {
            healthy_for: Some("0.5.0".into()),
        },
        &NoExec,
    )
    .await
    .unwrap();

    assert_eq!(
        out,
        Outcome::Updated {
            from: "0.4.0".into(),
            to: "0.5.0".into(),
            runner_updated: false
        }
    );
    // The drain (stop) saw the old binary; the restart saw the new one.
    assert_eq!(rig.events(), ["stop:v1", "start:#!/bin/sh\necho new borg"]);
    assert_eq!(fs::read(&rig.bin).unwrap(), NEW_BINARY);
    assert_eq!(
        fs::read_to_string(swap::prev_path(&rig.bin)).unwrap(),
        "v1\n",
        "borg.prev keeps the old binary"
    );
    assert_eq!(served.hits.load(Ordering::SeqCst), 1);
    assert!(rig.leftovers().is_empty(), "{:?}", rig.leftovers());
}

#[tokio::test]
async fn a_checksum_mismatch_refuses_the_update_before_anything_is_drained() {
    let (base, _) = serve().await;
    let rig = Rig::new();
    let m = manifest(&base, "0.5.0", &sha(b"something else"), None);
    let err = borg::update::run(
        &rig.options("0.4.0"),
        &m,
        &rig.service(),
        &FakeHealth { healthy_for: None },
        &NoExec,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, UpdateError::Swap(SwapError::Checksum { .. })),
        "{err}"
    );
    assert!(
        rig.events().is_empty(),
        "the service was never touched: {:?}",
        rig.events()
    );
    assert_eq!(fs::read_to_string(&rig.bin).unwrap(), "v1\n");
    assert!(
        rig.leftovers().is_empty(),
        "no partial download is left: {:?}",
        rig.leftovers()
    );
}

#[tokio::test]
async fn a_failed_health_check_rolls_back_and_restarts_the_old_binary() {
    let (base, _) = serve().await;
    let rig = Rig::new();
    let m = manifest(&base, "0.5.0", &sha(NEW_BINARY), None);
    // The new version never reports healthy.
    let out = borg::update::run(
        &rig.options("0.4.0"),
        &m,
        &rig.service(),
        &FakeHealth {
            healthy_for: Some("0.4.0".into()),
        },
        &NoExec,
    )
    .await
    .unwrap();
    match out {
        Outcome::RolledBack { from, to, reason } => {
            assert_eq!((from.as_str(), to.as_str()), ("0.4.0", "0.5.0"));
            assert!(reason.contains("no health evidence"), "{reason}");
        }
        other => panic!("expected a rollback, got {other:?}"),
    }
    assert_eq!(
        rig.events(),
        [
            "stop:v1",
            "start:#!/bin/sh\necho new borg",
            "stop:#!/bin/sh\necho new borg",
            "start:v1"
        ],
        "drain, swap, start, then stop the bad version, restore, start"
    );
    assert_eq!(fs::read_to_string(&rig.bin).unwrap(), "v1\n");
}

#[tokio::test]
async fn a_service_that_will_not_start_is_rolled_back_too() {
    let (base, _) = serve().await;
    let rig = Rig::new();
    let m = manifest(&base, "0.5.0", &sha(NEW_BINARY), None);
    let mut service = rig.service();
    service.fail_start_number = Some(1); // only the first start fails; the rollback's start works
    let out = borg::update::run(
        &rig.options("0.4.0"),
        &m,
        &service,
        &FakeHealth {
            healthy_for: Some("0.5.0".into()),
        },
        &NoExec,
    )
    .await
    .unwrap();
    assert!(
        matches!(&out, Outcome::RolledBack { reason, .. } if reason.contains("did not start")),
        "{out:?}"
    );
    assert_eq!(fs::read_to_string(&rig.bin).unwrap(), "v1\n");
}

#[tokio::test]
async fn a_drain_that_fails_changes_nothing() {
    let (base, _) = serve().await;
    let rig = Rig::new();
    let m = manifest(&base, "0.5.0", &sha(NEW_BINARY), None);
    let mut service = rig.service();
    service.fail_stop = true;
    let err = borg::update::run(
        &rig.options("0.4.0"),
        &m,
        &service,
        &FakeHealth { healthy_for: None },
        &NoExec,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, UpdateError::Drain(_)), "{err}");
    assert_eq!(fs::read_to_string(&rig.bin).unwrap(), "v1\n");
    assert!(rig.leftovers().is_empty());
}

#[tokio::test]
async fn updating_to_the_current_version_does_nothing() {
    let (base, served) = serve().await;
    let rig = Rig::new();
    let m = manifest(&base, "0.5.0", &sha(NEW_BINARY), None);
    let out = borg::update::run(
        &rig.options("0.5.0"),
        &m,
        &rig.service(),
        &FakeHealth { healthy_for: None },
        &NoExec,
    )
    .await
    .unwrap();
    assert_eq!(
        out,
        Outcome::UpToDate {
            version: "0.5.0".into()
        }
    );
    assert!(rig.events().is_empty());
    assert_eq!(served.hits.load(Ordering::SeqCst), 0, "no download");
}

#[tokio::test]
async fn check_reports_what_would_change_and_changes_nothing() {
    let (base, served) = serve().await;
    let rig = Rig::new();
    let m = manifest(&base, "0.5.0", &sha(NEW_BINARY), None);
    let mut opts = rig.options("0.4.0");
    opts.check = true;
    let out = borg::update::run(
        &opts,
        &m,
        &rig.service(),
        &FakeHealth { healthy_for: None },
        &NoExec,
    )
    .await
    .unwrap();
    assert_eq!(
        out,
        Outcome::Available {
            current: "0.4.0".into(),
            target: "0.5.0".into(),
            runner: None
        }
    );
    assert!(rig.events().is_empty() && served.hits.load(Ordering::SeqCst) == 0);
    assert_eq!(fs::read_to_string(&rig.bin).unwrap(), "v1\n");

    let up_to_date = borg::update::run(
        &{
            let mut o = rig.options("0.5.0");
            o.check = true;
            o
        },
        &m,
        &rig.service(),
        &FakeHealth { healthy_for: None },
        &NoExec,
    )
    .await
    .unwrap();
    assert_eq!(
        up_to_date,
        Outcome::UpToDate {
            version: "0.5.0".into()
        }
    );
}

#[tokio::test]
async fn a_requested_version_must_match_the_manifest() {
    let (base, _) = serve().await;
    let rig = Rig::new();
    let m = manifest(&base, "0.5.0", &sha(NEW_BINARY), None);
    let mut opts = rig.options("0.4.0");
    opts.requested_version = Some("0.4.9");
    let err = borg::update::run(
        &opts,
        &m,
        &rig.service(),
        &FakeHealth { healthy_for: None },
        &NoExec,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, UpdateError::VersionMismatch { .. }), "{err}");
    assert!(rig.events().is_empty());
}

#[tokio::test]
async fn a_host_without_a_released_binary_is_a_clear_error() {
    let (base, _) = serve().await;
    let rig = Rig::new();
    let m = manifest(&base, "0.5.0", &sha(NEW_BINARY), None);
    let mut opts = rig.options("0.4.0");
    opts.host_os = "linux";
    opts.host_arch = "riscv64";
    let err = borg::update::run(
        &opts,
        &m,
        &rig.service(),
        &FakeHealth { healthy_for: None },
        &NoExec,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, UpdateError::UnsupportedHost(_)), "{err}");
}

struct Hooked {
    calls: Mutex<Vec<String>>,
}

impl Exec for Hooked {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<Output> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("{program} {}", args.join(" ")));
        match program {
            "curl" => fs::write(args[2], RUNNER_TARBALL)?,
            "tar" => fs::write(Path::new(args[3]).join("run.sh"), "#!/bin/sh\n")?,
            _ => {}
        }
        Ok(Output {
            success: true,
            stdout: String::new(),
        })
    }
}

#[tokio::test]
async fn runner_only_updates_the_runner_without_touching_the_binary_or_the_service() {
    let (base, served) = serve().await;
    let rig = Rig::new();
    let runner_dir = rig.dir.path().join("jobhome/runner");
    // A newer binary is available too, but --runner-only leaves it alone.
    let m = manifest(
        &base,
        "0.5.0",
        &sha(NEW_BINARY),
        Some(("2.338.0", &sha(RUNNER_TARBALL))),
    );
    let mut opts = rig.options("0.4.0");
    opts.runner_only = true;
    opts.runner_dir = Some(&runner_dir);
    opts.runner_owner = Some("borg-job");
    let exec = Hooked {
        calls: Mutex::default(),
    };
    let out = borg::update::run(
        &opts,
        &m,
        &rig.service(),
        &FakeHealth { healthy_for: None },
        &exec,
    )
    .await
    .unwrap();

    assert_eq!(
        out,
        Outcome::RunnerUpdated {
            version: "2.338.0".into()
        }
    );
    assert!(
        rig.events().is_empty(),
        "no drain or restart: {:?}",
        rig.events()
    );
    assert_eq!(
        served.hits.load(Ordering::SeqCst),
        0,
        "the borg binary was not downloaded"
    );
    assert_eq!(fs::read_to_string(&rig.bin).unwrap(), "v1\n");
    let marker = fs::read_to_string(runner_dir.join(".borg-runner-version")).unwrap();
    assert_eq!(marker, format!("2.338.0 {}\n", sha(RUNNER_TARBALL)));
    assert!(
        exec.calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("chown -R borg-job:borg-job"))
    );

    // Idempotent: the same manifest again has nothing to do.
    let again = borg::update::run(
        &opts,
        &m,
        &rig.service(),
        &FakeHealth { healthy_for: None },
        &exec,
    )
    .await
    .unwrap();
    assert_eq!(
        again,
        Outcome::UpToDate {
            version: "0.4.0".into()
        }
    );
}

#[tokio::test]
async fn a_binary_update_also_brings_the_runner_up_to_date() {
    let (base, _) = serve().await;
    let rig = Rig::new();
    let runner_dir = rig.dir.path().join("jobhome/runner");
    let m = manifest(
        &base,
        "0.5.0",
        &sha(NEW_BINARY),
        Some(("2.338.0", &sha(RUNNER_TARBALL))),
    );
    let mut opts = rig.options("0.4.0");
    opts.runner_dir = Some(&runner_dir);
    let out = borg::update::run(
        &opts,
        &m,
        &rig.service(),
        &FakeHealth {
            healthy_for: Some("0.5.0".into()),
        },
        &Hooked {
            calls: Mutex::default(),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        out,
        Outcome::Updated {
            from: "0.4.0".into(),
            to: "0.5.0".into(),
            runner_updated: true
        }
    );
    assert!(runner_dir.join("run.sh").exists());
}

#[tokio::test]
async fn a_bad_runner_checksum_aborts_before_the_service_is_drained() {
    let (base, _) = serve().await;
    let rig = Rig::new();
    let runner_dir = rig.dir.path().join("jobhome/runner");
    let m = manifest(
        &base,
        "0.5.0",
        &sha(NEW_BINARY),
        Some(("2.338.0", &"0".repeat(64))),
    );
    let mut opts = rig.options("0.4.0");
    opts.runner_dir = Some(&runner_dir);
    let err = borg::update::run(
        &opts,
        &m,
        &rig.service(),
        &FakeHealth { healthy_for: None },
        &Hooked {
            calls: Mutex::default(),
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(err, UpdateError::Runner(_)), "{err}");
    assert!(rig.events().is_empty());
    assert!(
        rig.leftovers().is_empty(),
        "the verified binary download is cleaned up too: {:?}",
        rig.leftovers()
    );
}

#[tokio::test]
async fn manifests_are_fetched_over_http_only_when_allowed() {
    let (base, _) = serve().await;
    assert!(matches!(
        manifest::fetch(&format!("{base}/missing"), true).await,
        Err(ManifestError::Status { status: 404, .. })
    ));
    assert!(matches!(
        manifest::fetch(&format!("{base}/borg"), false).await,
        Err(ManifestError::Insecure(_))
    ));
}
