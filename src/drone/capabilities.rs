//! Bounded job-user execution proofs, distinct from service-user inventory.
//! No probe installs tools, pulls an image, changes host policy or accesses the
//! network. Every child command (including cleanup) has a hard timeout and
//! discards output before it reaches the daemon's process boundary.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;

use crate::drone::config::DroneConfig;
use crate::drone::exec::Exec;
use crate::envcontract::{Capability, CapabilityFact, CheckState};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProbeSettings {
    /// GNU-compatible timeout, or `gtimeout` where coreutils uses that name.
    pub timeout_bin: String,
    pub timeout_secs: u64,
    pub bwrap_bin: String,
    /// An already-loaded image containing `/bin/true`. Never pulled by Borg.
    /// Absent: Docker execution is unknown, even if `docker info` works.
    pub docker_image: Option<String>,
}

impl Default for ProbeSettings {
    fn default() -> Self {
        Self {
            timeout_bin: "timeout".into(),
            timeout_secs: 5,
            bwrap_bin: "bwrap".into(),
            docker_image: None,
        }
    }
}

impl ProbeSettings {
    pub fn validate(&self) -> Result<(), String> {
        let binary_ok = |s: &str| {
            !s.is_empty()
                && s.len() <= 256
                && !s.starts_with('-')
                && !s.chars().any(|c| c.is_control() || c.is_whitespace())
        };
        if !binary_ok(&self.timeout_bin) || !binary_ok(&self.bwrap_bin) {
            return Err(
                "capability_probe binaries must be nonempty executable names or paths".into(),
            );
        }
        if !(1..=10).contains(&self.timeout_secs) {
            return Err("capability_probe.timeout_secs must be between 1 and 10".into());
        }
        if self.docker_image.as_ref().is_some_and(|s| {
            s.is_empty()
                || s.len() > 256
                || s.starts_with('-')
                || !s.bytes().all(|b| {
                    b.is_ascii_alphanumeric()
                        || matches!(b, b'/' | b'.' | b'_' | b'-' | b':' | b'@')
                })
        }) {
            return Err("capability_probe.docker_image must be an image reference".into());
        }
        Ok(())
    }
}

/// Keep a trusted supervisor alive while the smoke runs. Its TERM trap kills
/// the isolated group immediately: some timeout implementations kill only the
/// direct child on their final KILL escalation. The grouped Exec boundary is
/// mandatory so signaling group zero cannot affect the drone's ambient group.
/// Redirection happens before any smoke output can reach SystemExec.
const QUIET_EXEC: &str = r#"exec >/dev/null 2>&1
trap 'kill -KILL 0' TERM HUP INT
"$@" &
child=$!
wait "$child"
status=$?
trap - TERM HUP INT
exit "$status""#;
const TTL_SECS: i64 = 60;

fn bounded(exec: &dyn Exec, settings: &ProbeSettings, program: &str, args: &[&str]) -> CheckState {
    let seconds = format!("{}s", settings.timeout_secs);
    let mut argv = vec![
        "--signal=TERM",
        "--kill-after=2s",
        &seconds,
        "sh",
        "-c",
        QUIET_EXEC,
        "sh",
        program,
    ];
    argv.extend_from_slice(args);
    match exec.run_grouped(&settings.timeout_bin, &argv) {
        Ok(out) if out.success => CheckState::Pass,
        Ok(_) => CheckState::Fail,
        Err(_) => CheckState::Unknown,
    }
}

const WRITE_PROBE: &str = r#"mkdir -m 700 -- "$1" || exit 1
trap 'rm -rf -- "$1"' EXIT HUP INT TERM
printf borg >"$1/proof" && test "$(cat "$1/proof")" = borg"#;

fn write_probe(exec: &dyn Exec, settings: &ProbeSettings, root: &Path, nonce: &str) -> CheckState {
    let path = root.join(format!(".borg-capability-{nonce}"));
    let Some(path) = path.to_str() else {
        return CheckState::Unknown;
    };
    let state = bounded(exec, settings, "sh", &["-c", WRITE_PROBE, "sh", path]);
    // Also clean after a timeout/failed shell. Exact generated child only;
    // neither work nor cache root itself can be passed to removal.
    let cleanup = bounded(exec, settings, "rm", &["-rf", "--", path]);
    if state == CheckState::Pass && cleanup != CheckState::Pass {
        CheckState::Fail
    } else {
        state
    }
}

fn docker_probe(exec: &dyn Exec, cfg: &DroneConfig, nonce: &str) -> CheckState {
    let Some(image) = cfg.capability_probe.docker_image.as_deref() else {
        return CheckState::Unknown;
    };
    let name = format!("borg-capability-{nonce}");
    let state = bounded(
        exec,
        &cfg.capability_probe,
        &cfg.docker_bin,
        &[
            "run",
            "--pull=never",
            "--network=none",
            "--read-only",
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
            "--pids-limit=16",
            "--memory=64m",
            "--cpus=0.1",
            "--rm",
            "--name",
            &name,
            "--entrypoint=/bin/true",
            image,
        ],
    );
    // `--rm` cleans normal completion. An interrupted client can leave the
    // daemon running its container: always attempt bounded exact-name removal.
    let _ = bounded(
        exec,
        &cfg.capability_probe,
        &cfg.docker_bin,
        &["rm", "-f", "-v", &name],
    );
    state
}

fn probe(exec: &dyn Exec, cfg: &DroneConfig, now: i64) -> BTreeMap<Capability, CapabilityFact> {
    let nonce = format!("{:016x}", rand::random::<u64>());
    let sandbox = bounded(
        exec,
        &cfg.capability_probe,
        &cfg.capability_probe.bwrap_bin,
        &[
            "--unshare-user",
            "--unshare-pid",
            "--unshare-net",
            "--unshare-ipc",
            "--unshare-uts",
            "--die-with-parent",
            "--new-session",
            "--ro-bind",
            "/",
            "/",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--",
            "/bin/true",
        ],
    );
    [
        (Capability::Sandbox, sandbox),
        (Capability::Docker, docker_probe(exec, cfg, &nonce)),
        (
            Capability::WorkWrite,
            write_probe(exec, &cfg.capability_probe, &cfg.work_root, &nonce),
        ),
        (
            Capability::CacheWrite,
            write_probe(exec, &cfg.capability_probe, &cfg.cache_root, &nonce),
        ),
    ]
    .into_iter()
    .map(|(kind, state)| {
        (
            kind,
            CapabilityFact {
                observed_at: now,
                state,
            },
        )
    })
    .collect()
}

#[derive(Clone, Default)]
pub struct CapabilityCache(Arc<Mutex<Cached>>);

#[derive(Default)]
struct Cached {
    attempted_at: Option<i64>,
    refreshing: bool,
    facts: BTreeMap<Capability, CapabilityFact>,
}

/// Owns the single-flight reservation until the blocking probe and cleanup
/// finish, including when its task panics. No lock is held during commands.
struct Refresh(CapabilityCache);

impl Refresh {
    fn complete(self, mut facts: BTreeMap<Capability, CapabilityFact>) {
        if let Ok(mut cache) = self.0.0.lock() {
            for (kind, fact) in &mut facts {
                if fact.state == CheckState::Unknown
                    && let Some(old) = cache.facts.get(kind)
                {
                    *fact = old.clone();
                }
            }
            cache.facts = facts;
        }
    }
}

impl Drop for Refresh {
    fn drop(&mut self) {
        if let Ok(mut cache) = self.0.0.lock() {
            cache.refreshing = false;
        }
    }
}

impl CapabilityCache {
    fn begin(&self, now: i64) -> Option<Refresh> {
        let mut cache = self.0.lock().ok()?;
        if cache.refreshing
            || cache
                .attempted_at
                .is_some_and(|at| (0..TTL_SECS).contains(&now.saturating_sub(at)))
        {
            return None;
        }
        cache.refreshing = true;
        cache.attempted_at = Some(now);
        Some(Refresh(self.clone()))
    }

    /// Snapshot only: a slow smoke test never delays a heartbeat. Empty or
    /// aged proofs still fail required admission until a refresh completes.
    pub fn snapshot(&self) -> BTreeMap<Capability, CapabilityFact> {
        self.0
            .lock()
            .map(|cache| cache.facts.clone())
            .unwrap_or_default()
    }

    /// Schedule at most one refresh, once per minute, under the drone's
    /// existing task/drain lifecycle. The caller stops scheduling on drain.
    pub fn refresh_if_due(
        &self,
        exec: Arc<dyn Exec>,
        cfg: Arc<DroneConfig>,
        now: i64,
        tasks: &mut JoinSet<()>,
    ) {
        let Some(refresh) = self.begin(now) else {
            return;
        };
        tasks.spawn_blocking(move || {
            refresh.complete(probe(exec.as_ref(), &cfg, now));
        });
    }

    /// Synchronous test driver for the same reservation/completion path.
    #[cfg(test)]
    fn get(
        &self,
        exec: &dyn Exec,
        cfg: &DroneConfig,
        now: i64,
    ) -> BTreeMap<Capability, CapabilityFact> {
        if let Some(refresh) = self.begin(now) {
            refresh.complete(probe(exec, cfg, now));
        }
        self.snapshot()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drone::exec::Output;
    use crate::drone::jobuser::{JobExec, JobUser};
    use std::io;

    #[derive(Default)]
    struct FakeExec {
        calls: Mutex<Vec<(String, Vec<String>)>>,
        fail: Mutex<Option<String>>,
        unavailable: Mutex<bool>,
    }

    impl Exec for FakeExec {
        fn run_grouped(&self, program: &str, args: &[&str]) -> io::Result<Output> {
            self.run(program, args)
        }

        fn run(&self, program: &str, args: &[&str]) -> io::Result<Output> {
            self.calls
                .lock()
                .unwrap()
                .push((program.into(), args.iter().map(|a| (*a).into()).collect()));
            if *self.unavailable.lock().unwrap() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "timeout unavailable",
                ));
            }
            let success = !self
                .fail
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|p| args.contains(&p.as_str()));
            Ok(Output {
                success,
                stdout: "ignored and never reported".into(),
            })
        }
    }

    fn config() -> DroneConfig {
        DroneConfig::from_json(r#"{"queen_url":"https://queen.example.test","runner_dir":"/r","work_root":"/w","cache_root":"/c","capability_probe":{"docker_image":"busybox@sha256:abcd"}}"#).unwrap()
    }

    #[test]
    fn an_executor_without_group_isolation_reports_unknown_before_starting_a_smoke() {
        struct Ungrouped;
        impl Exec for Ungrouped {
            fn run(&self, _program: &str, _args: &[&str]) -> io::Result<Output> {
                panic!("unsafe supervisor must not start in an ambient group");
            }
        }
        assert_eq!(
            bounded(&Ungrouped, &ProbeSettings::default(), "sh", &["-c", "true"]),
            CheckState::Unknown
        );
    }

    #[test]
    fn proofs_use_timeouts_quiet_output_isolation_and_exact_cleanup() {
        let fake = FakeExec::default();
        let facts = CapabilityCache::default().get(&fake, &config(), 100);
        assert!(
            facts
                .values()
                .all(|f| f.state == CheckState::Pass && f.observed_at == 100)
        );
        let calls = fake.calls.lock().unwrap();
        assert_eq!(calls.len(), 7);
        for (program, args) in calls.iter() {
            assert_eq!(program, "timeout");
            assert_eq!(
                &args[..7],
                [
                    "--signal=TERM",
                    "--kill-after=2s",
                    "5s",
                    "sh",
                    "-c",
                    QUIET_EXEC,
                    "sh"
                ]
            );
        }
        let docker = calls
            .iter()
            .find(|(_, a)| a.iter().any(|x| x == "run"))
            .unwrap();
        for argument in [
            "--pull=never",
            "--network=none",
            "--read-only",
            "--cap-drop=ALL",
            "--entrypoint=/bin/true",
        ] {
            assert!(docker.1.iter().any(|a| a == argument));
        }
        let name = &docker.1[docker.1.iter().position(|a| a == "--name").unwrap() + 1];
        assert!(calls.iter().any(|(_, a)| a.ends_with(&[
            "docker".into(),
            "rm".into(),
            "-f".into(),
            "-v".into(),
            name.clone()
        ])));
        assert!(
            calls
                .iter()
                .any(|(_, a)| a.contains(&"--unshare-user".into())
                    && a.contains(&"--unshare-net".into()))
        );
        for root in ["/w/.borg-capability-", "/c/.borg-capability-"] {
            let path = calls
                .iter()
                .flat_map(|(_, a)| a)
                .find(|a| a.starts_with(root))
                .unwrap();
            assert!(calls.iter().any(|(_, a)| a.ends_with(&[
                "rm".into(),
                "-rf".into(),
                "--".into(),
                path.clone()
            ])));
        }
    }

    #[test]
    fn failed_or_timed_out_commands_are_not_positive_proof_and_cleanup_still_runs() {
        let fake = FakeExec::default();
        *fake.fail.lock().unwrap() = Some("run".into());
        let facts = CapabilityCache::default().get(&fake, &config(), 100);
        assert_eq!(facts[&Capability::Docker].state, CheckState::Fail);
        assert!(
            fake.calls
                .lock()
                .unwrap()
                .iter()
                .any(|(_, a)| a.contains(&"-f".into()))
        );
        // The process boundary represents timeout's nonzero exit as success=false.
        *fake.fail.lock().unwrap() = Some("bwrap".into());
        let facts = CapabilityCache::default().get(&fake, &config(), 100);
        assert_eq!(facts[&Capability::Sandbox].state, CheckState::Fail);
    }

    #[test]
    fn filesystem_write_or_cleanup_failure_is_not_positive_proof() {
        let fake = FakeExec::default();
        *fake.fail.lock().unwrap() = Some(WRITE_PROBE.into());
        let facts = CapabilityCache::default().get(&fake, &config(), 100);
        for capability in [Capability::WorkWrite, Capability::CacheWrite] {
            assert_eq!(facts[&capability].state, CheckState::Fail);
        }
        assert_eq!(
            fake.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, a)| a.contains(&"-rf".into()))
                .count(),
            2
        );
        *fake.fail.lock().unwrap() = Some("-rf".into());
        let facts = CapabilityCache::default().get(&fake, &config(), 100);
        assert_eq!(facts[&Capability::WorkWrite].state, CheckState::Fail);
        assert_eq!(facts[&Capability::CacheWrite].state, CheckState::Fail);
    }

    #[test]
    fn missing_docker_image_is_unknown_without_a_docker_invocation() {
        let fake = FakeExec::default();
        let mut cfg = config();
        cfg.capability_probe.docker_image = None;
        let facts = CapabilityCache::default().get(&fake, &cfg, 100);
        assert_eq!(facts[&Capability::Docker].state, CheckState::Unknown);
        assert!(
            !fake
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|(_, a)| a.contains(&"docker".into()))
        );
    }

    #[test]
    fn cached_proofs_keep_their_time_even_when_refresh_cannot_start() {
        let fake = FakeExec::default();
        let cache = CapabilityCache::default();
        let cfg = config();
        let first = cache.get(&fake, &cfg, 100);
        assert_eq!(cache.get(&fake, &cfg, 159), first);
        assert_eq!(fake.calls.lock().unwrap().len(), 7);
        *fake.unavailable.lock().unwrap() = true;
        assert_eq!(cache.get(&fake, &cfg, 160), first);
        let calls = fake.calls.lock().unwrap().len();
        assert_eq!(cache.get(&fake, &cfg, 161), first);
        assert_eq!(fake.calls.lock().unwrap().len(), calls);
    }

    #[test]
    fn every_smoke_command_crosses_the_existing_job_user_boundary() {
        let inner = Arc::new(FakeExec::default());
        let exec = JobExec::new(
            inner.clone(),
            JobUser {
                name: "borg-job".into(),
                home: "/home/borg-job".into(),
                priv_bin: "setpriv".into(),
            },
        );
        CapabilityCache::default().get(&exec, &config(), 100);
        let calls = inner.calls.lock().unwrap();
        for (program, args) in calls.iter() {
            assert_eq!(program, "setpriv");
            assert!(args.starts_with(&[
                "--reuid".into(),
                "borg-job".into(),
                "--regid".into(),
                "borg-job".into()
            ]));
            assert!(args.contains(&"--bounding-set=-all".into()));
            assert!(args.contains(&"timeout".into()));
        }
    }

    #[test]
    fn a_refresh_keeps_single_flight_past_the_ttl_and_does_not_lock_snapshots() {
        let cache = CapabilityCache::default();
        let refresh = cache.begin(100).unwrap();
        assert!(cache.snapshot().is_empty());
        assert!(
            cache.begin(200).is_none(),
            "slow refresh stays single-flight after the TTL"
        );
        let facts = probe(&FakeExec::default(), &config(), 100);
        refresh.complete(facts.clone());
        assert_eq!(cache.snapshot(), facts);
        assert!(
            cache.begin(200).is_some(),
            "completion releases the reservation"
        );
    }

    #[test]
    fn a_panicking_refresh_releases_its_single_flight_reservation() {
        let cache = CapabilityCache::default();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _refresh = cache.begin(100).unwrap();
            panic!("fake probe panicked outside the cache lock");
        }));
        assert!(panic.is_err());
        assert!(cache.snapshot().is_empty());
        assert!(cache.begin(200).is_some());
    }

    /// Acceptance for the Linux timeout path; a missing timeout binary
    /// is a fixture failure rather than silently skipped subprocess evidence.
    #[cfg(target_os = "linux")]
    fn native_timeout_settings() -> ProbeSettings {
        use crate::drone::exec::SystemExec;

        let settings = ProbeSettings {
            timeout_bin: "/usr/bin/timeout".into(),
            timeout_secs: 1,
            ..ProbeSettings::default()
        };
        let version = SystemExec
            .run(&settings.timeout_bin, &["--version"])
            .expect("Linux capability acceptance requires configured /usr/bin/timeout");
        assert!(version.success, "timeout --version failed: {version:?}");
        settings
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn native_timeout_kills_term_ignoring_descendants_before_their_delayed_write() {
        use crate::drone::exec::SystemExec;
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};

        let settings = native_timeout_settings();
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("term-ignoring-smoke.sh");
        let started = dir.path().join("descendant-started");
        let leaked = dir.path().join("descendant-survived");
        // Even a broken timeout leaves only a finite fixture: its descendant
        // exits after six seconds and the parent then exits. All writes stay
        // under this exact temporary directory, without global process kills.
        std::fs::write(
            &script,
            r#"#!/bin/sh
trap '' TERM
(
    trap '' TERM
    printf started > "$2"
    sleep 6
    printf leaked > "$1"
) &
wait "$!"
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let began = Instant::now();
        let state = bounded(
            &SystemExec,
            &settings,
            script.to_str().unwrap(),
            &[leaked.to_str().unwrap(), started.to_str().unwrap()],
        );
        let elapsed = began.elapsed();
        // Wait past the descendant's original marker deadline before any
        // assertion can unwind and delete its temporary paths. This catches
        // killing just the shell while leaving its descendant alive.
        std::thread::sleep(Duration::from_secs(7));
        assert_eq!(
            state,
            CheckState::Fail,
            "a killed smoke must not be positive proof"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "one-second timeout plus two-second kill fallback must terminate the finite fixture: {elapsed:?}"
        );
        assert!(
            started.exists(),
            "fixture must have started the real descendant"
        );
        assert!(
            !leaked.exists(),
            "timed-out descendant survived to its delayed write"
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn native_filesystem_smokes_write_read_and_remove_only_their_temporary_children() {
        use crate::drone::exec::SystemExec;

        let settings = native_timeout_settings();
        let dir = tempfile::tempdir().unwrap();
        for name in ["work", "cache"] {
            let root = dir.path().join(name);
            std::fs::create_dir(&root).unwrap();
            let keep = root.join("existing-data");
            std::fs::write(&keep, "preserve").unwrap();
            assert_eq!(
                write_probe(&SystemExec, &settings, &root, "native-fixture"),
                CheckState::Pass
            );
            assert_eq!(std::fs::read_to_string(&keep).unwrap(), "preserve");
            assert!(!root.join(".borg-capability-native-fixture").exists());
            assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        }
        let absent = dir.path().join("absent-root");
        assert_eq!(
            write_probe(&SystemExec, &settings, &absent, "native-fixture"),
            CheckState::Fail
        );
        assert!(
            !absent.exists(),
            "a smoke must not create its configured root"
        );
    }

    #[test]
    fn settings_reject_unbounded_or_malformed_commands() {
        assert!(ProbeSettings::default().validate().is_ok());
        for bad in [
            ProbeSettings {
                timeout_secs: 0,
                ..ProbeSettings::default()
            },
            ProbeSettings {
                timeout_secs: 11,
                ..ProbeSettings::default()
            },
            ProbeSettings {
                timeout_bin: "".into(),
                ..ProbeSettings::default()
            },
            ProbeSettings {
                bwrap_bin: "bwrap --help".into(),
                ..ProbeSettings::default()
            },
            ProbeSettings {
                docker_image: Some("--privileged".into()),
                ..ProbeSettings::default()
            },
            ProbeSettings {
                docker_image: Some("busybox latest".into()),
                ..ProbeSettings::default()
            },
        ] {
            assert!(bad.validate().is_err());
        }
    }
}
