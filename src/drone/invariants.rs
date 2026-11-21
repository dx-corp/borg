//! Host invariants: what must hold on a drone's host before it takes a lease.
//! While any fails the drone accepts nothing, logs the reason once, and
//! reports the failures to the queen (`EnvFacts::host_invariant_failures`),
//! which exports `borg_drone_host_invariant_failing{drone,check}` for paging.
//!
//! Checks:
//! * `ip_forward`: a Docker host forwards IPv4 (containers have no network
//!   otherwise; internal-runner-burst-01 from 2026-10-23 to 11-02);
//! * `docker_daemon`: Docker is installed and its daemon answers;
//! * `disk`: the job filesystems are above the queen's admission floor;
//! * `container_network`: end to end, a container on the default bridge
//!   resolves a name and opens a TCP connection to it. This one starts a
//!   container, so it runs at startup and every `interval_secs` and its
//!   result is reused by every heartbeat in between. A failed probe is run
//!   again after `RETRY_SECS`, so one probe that did not finish costs the
//!   drone a minute of leases, not a whole interval; it takes none meanwhile.
//!   A probe that did not finish says why in its detail: timed out or the
//!   exit code (125 is docker itself: an image pull or daemon error) and the
//!   first lines of stderr, within [`MAX_DETAIL_CHARS`]. The image is pulled
//!   with `--pull missing` from wherever `probe_image` names. The default
//!   `busybox:1.37` is Docker Hub, unless the daemon has a registry mirror;
//!   a host that cannot reach Docker Hub, or shares a rate-limited egress
//!   address, needs `probe_image` set to a mirror it can reach.
//!
//! Whether the binary matches the fleet's is not decided here: a drone does
//! not know the fleet's build. The queen exports every drone's build in
//! `borg_drone_info` and alerts on a drone behind the fleet.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::drone::exec::{Diagnosed, Exec};
use crate::envcontract::InvariantFailure;
use crate::probe::{self, Host};

/// The `host_invariants` block of the drone config. On by default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InvariantSettings {
    pub(crate) enabled: bool,
    /// Run the end-to-end container network probe.
    pub(crate) container_network: bool,
    /// Image the probe runs; needs `sh`, `nslookup` and `nc` (busybox).
    probe_image: String,
    /// Name the probe resolves and connects to.
    probe_host: String,
    probe_port: u16,
    /// Seconds between container network probes.
    pub(crate) interval_secs: u64,
}

impl Default for InvariantSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            container_network: true,
            probe_image: "busybox:1.37".to_owned(),
            probe_host: "github.com".to_owned(),
            probe_port: 443,
            interval_secs: 600,
        }
    }
}

impl InvariantSettings {
    pub(crate) fn validate(&self) -> Result<(), String> {
        let plain = |s: &str| {
            !s.is_empty()
                && s.len() <= 200
                && s.bytes().all(|b| {
                    b.is_ascii_alphanumeric()
                        || matches!(b, b'.' | b'-' | b'_' | b':' | b'/' | b'@')
                })
        };
        if !plain(&self.probe_image) || !plain(&self.probe_host) {
            return Err("host_invariants probe_image and probe_host must be plain names".into());
        }
        if self.interval_secs == 0 || self.probe_port == 0 {
            return Err("host_invariants interval_secs and probe_port must be positive".into());
        }
        Ok(())
    }
}

fn failure(check: &str, detail: impl Into<String>) -> InvariantFailure {
    InvariantFailure {
        check: check.to_owned(),
        detail: detail.into(),
    }
}

/// The checks cheap enough for every heartbeat. `docker_usable` is the
/// capacity probe's `docker info` result; `disk_blocked` is the admission
/// verdict.
pub(crate) fn fast_checks(
    host: &dyn Host,
    docker_usable: bool,
    disk_blocked: bool,
) -> Vec<InvariantFailure> {
    let mut out = Vec::new();
    let docker_installed = host.run("docker", &["--version"]).is_some();
    if docker_installed && !docker_usable {
        out.push(failure(
            "docker_daemon",
            "docker is installed but its daemon does not answer `docker info`",
        ));
    }
    if docker_usable && probe::ipv4_forwarding(host) == Some(false) {
        out.push(failure(
            "ip_forward",
            "/proc/sys/net/ipv4/ip_forward is 0, so containers have no network; \
             `borg drone install` writes /etc/sysctl.d/99-borg-docker-forwarding.conf, \
             or run `sysctl -w net.ipv4.ip_forward=1` and persist it",
        ));
    }
    if disk_blocked {
        out.push(failure(
            "disk",
            "a job filesystem is below the queen's disk admission floor",
        ));
    }
    out
}

/// Upper bound on a failure's detail. The queen keeps at most 300 characters
/// of it, so the cause comes first and stderr fills what is left.
pub(crate) const MAX_DETAIL_CHARS: usize = 300;
/// Lines of stderr kept in a failure's detail.
const STDERR_LINES: usize = 3;
/// Seconds the probe may take, image pull included.
const PROBE_TIMEOUT_SECS: u64 = 120;

/// Docker's own progress and usage notes, which say nothing about a cause
/// and would crowd it out of the bounded detail.
fn docker_note(line: &str) -> bool {
    line.starts_with("Unable to find image") || line.starts_with("See 'docker run --help'")
}

/// The first non-empty lines of `stderr` on one line, docker's notes and
/// control characters dropped.
fn stderr_head(stderr: &str) -> String {
    stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !docker_note(l))
        .take(STDERR_LINES)
        .map(|l| {
            let clean: String = l.chars().filter(|c| !c.is_control()).collect();
            // A registry error can name the image as a long URL
            // (`https://.../v2/<repo>/manifests/<digest>`); shorten every long
            // token, not just a literal image repeat, so the cause after it
            // still fits the detail.
            clean
                .split(' ')
                .map(shown_image)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Why a probe that printed no verdict did not finish, from `timeout`'s and
/// `docker run`'s exit codes.
fn why_not_finished(d: &Diagnosed) -> String {
    match d.code {
        // `timeout` exits 124 when the deadline ends the command. 137 is a
        // SIGKILL from anywhere (the kernel's OOM killer, an operator), so it
        // is reported as what it is, not as the deadline.
        Some(124) => format!("timed out after {PROBE_TIMEOUT_SECS}s"),
        Some(137) => "exit 137: process killed (SIGKILL)".into(),
        Some(125) => "exit 125: docker could not run it (image pull or daemon error)".into(),
        Some(126) | Some(127) => format!(
            "exit {}: the image cannot run `sh -c` (no shell in it?)",
            d.code.unwrap_or_default()
        ),
        Some(0) => "exit 0 without the final marker".into(),
        Some(code) => format!("exit {code}"),
        None => "ended by a signal".into(),
    }
}

/// Longest image name shown in a detail; a longer one keeps its start and end.
const MAX_IMAGE_CHARS: usize = 40;

/// `image` as a detail shows it: whole when short, otherwise its first and
/// last characters around `...`, so a long registry path or digest leaves the
/// detail room for stderr.
fn shown_image(image: &str) -> String {
    let n = image.chars().count();
    if n <= MAX_IMAGE_CHARS {
        return image.to_owned();
    }
    let keep = (MAX_IMAGE_CHARS - 3) / 2;
    let head: String = image.chars().take(keep).collect();
    let tail: String = image.chars().skip(n - keep).collect();
    format!("{head}...{tail}")
}

/// `head` plus `: stderr` while it fits, cut to [`MAX_DETAIL_CHARS`].
fn bounded(head: String, stderr: &str) -> String {
    let tail = stderr_head(stderr);
    let full = if tail.is_empty() {
        head
    } else {
        format!("{head}; stderr: {tail}")
    };
    if full.chars().count() <= MAX_DETAIL_CHARS {
        return full;
    }
    let mut cut: String = full.chars().take(MAX_DETAIL_CHARS - 3).collect();
    cut.push_str("...");
    cut
}

/// The end-to-end container network probe. `None` when it passed.
pub(crate) fn container_network(
    exec: &dyn Exec,
    docker: &str,
    s: &InvariantSettings,
) -> Option<InvariantFailure> {
    let port = s.probe_port.to_string();
    // Markers tell a DNS failure from a connect failure; the trailing echo
    // proves the script ran to the end.
    let script = r#"nslookup "$1" >/dev/null 2>&1 || { echo dns-fail; exit 0; }
nc -z -w 5 "$1" "$2" >/dev/null 2>&1 || { echo connect-fail; exit 0; }
echo ok"#;
    let limit = PROBE_TIMEOUT_SECS.to_string();
    let out = exec.run_diagnosed(
        "timeout",
        &[
            &limit,
            docker,
            "run",
            "--rm",
            "--network",
            "bridge",
            "--pull",
            "missing",
            "--label",
            "borg.probe=host-invariants",
            &s.probe_image,
            "sh",
            "-c",
            script,
            "probe",
            &s.probe_host,
            &port,
        ],
    );
    let target = format!("{}:{}", s.probe_host, s.probe_port);
    match out {
        Ok(d) if d.output.success && d.output.stdout.trim().ends_with("ok") => None,
        Ok(d) if d.output.stdout.contains("dns-fail") => Some(failure(
            "container_network",
            format!(
                "a container on the default bridge cannot resolve {}",
                s.probe_host
            ),
        )),
        Ok(d) if d.output.stdout.contains("connect-fail") => Some(failure(
            "container_network",
            format!("a container on the default bridge cannot connect to {target}"),
        )),
        Ok(d) => Some(failure("container_network", {
            // Docker repeats the image in its errors ("Unable to find
            // image '...' locally"): shorten it there too.
            let image = shown_image(&s.probe_image);
            bounded(
                format!(
                    "the probe container ({image}) did not run to completion: {}",
                    why_not_finished(&d)
                ),
                &d.stderr.replace(&s.probe_image, &image),
            )
        })),
        Err(e) => Some(failure(
            "container_network",
            bounded(format!("could not start the probe container: {e}"), ""),
        )),
    }
}

/// Seconds until a failed container network probe is run again.
pub(crate) const RETRY_SECS: u64 = 60;

/// The wait before the next container network probe. After a pass it is
/// `interval_secs` (at least a minute). After a failure it is `RETRY_SECS`,
/// never longer than the interval: the drone takes no lease until a probe
/// passes, so waiting the full interval only extends the outage.
pub(crate) fn next_probe_delay(s: &InvariantSettings, failing: bool) -> Duration {
    let interval = s.interval_secs.max(60);
    Duration::from_secs(if failing {
        interval.min(RETRY_SECS)
    } else {
        interval
    })
}

/// Log a change of the failing set: the reasons when something breaks, a
/// note when everything holds again. Returns whether it changed.
pub(crate) fn log_change(previous: &[InvariantFailure], now: &[InvariantFailure]) -> bool {
    let keys = |v: &[InvariantFailure]| -> Vec<String> {
        let mut k: Vec<String> = v.iter().map(|f| f.check.clone()).collect();
        k.sort();
        k
    };
    if keys(previous) == keys(now) {
        return false;
    }
    if now.is_empty() {
        tracing::info!("host invariants hold again: taking leases");
    } else {
        for f in now {
            tracing::error!(
                "host invariant {} failed, taking no leases: {}",
                f.check,
                f.detail
            );
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::drone::cleanup::testing::FakeExec;
    use crate::drone::exec::Output;

    #[test]
    fn a_long_registry_url_leaves_room_for_the_stderr_cause() {
        let repo = "team/".repeat(20);
        let digest = "0".repeat(64);
        let url =
            format!("https://mirror.example.internal/v2/{repo}busybox/manifests/sha256:{digest}:");
        let stderr =
            format!("docker: Error response from daemon: Head {url} toomanyrequests: rate limit\n");
        let detail = bounded(
            "the probe container (busybox:1.37) did not run to completion: exit 125".into(),
            &stderr,
        );
        assert!(detail.contains("toomanyrequests"), "{detail}");
        assert!(detail.chars().count() <= MAX_DETAIL_CHARS);
    }

    struct H {
        docker: bool,
        forward: Option<&'static str>,
    }

    impl Host for H {
        fn read_to_string(&self, path: &Path) -> Option<String> {
            (path == Path::new("/proc/sys/net/ipv4/ip_forward"))
                .then_some(self.forward?)
                .map(str::to_owned)
        }
        fn exists(&self, _: &Path) -> bool {
            false
        }
        fn run(&self, program: &str, _: &[&str]) -> Option<String> {
            (program == "docker" && self.docker).then(|| "Docker version 29".to_owned())
        }
        fn os(&self) -> &'static str {
            "linux"
        }
        fn arch(&self) -> &'static str {
            "x86_64"
        }
        fn available_parallelism(&self) -> Option<u32> {
            Some(8)
        }
    }

    fn checks(v: &[InvariantFailure]) -> Vec<&str> {
        v.iter().map(|f| f.check.as_str()).collect()
    }

    #[test]
    fn fast_checks_cover_forwarding_daemon_and_disk() {
        let healthy = H {
            docker: true,
            forward: Some("1\n"),
        };
        assert!(fast_checks(&healthy, true, false).is_empty());
        // internal-runner-burst-01, 2026-10-23..11-02.
        let burst01 = H {
            docker: true,
            forward: Some("0\n"),
        };
        assert_eq!(checks(&fast_checks(&burst01, true, false)), ["ip_forward"]);
        // Installed but the daemon is down; forwarding is moot then.
        assert_eq!(
            checks(&fast_checks(&burst01, false, true)),
            ["docker_daemon", "disk"]
        );
        // No docker at all (a macOS drone): nothing to say about it.
        let none = H {
            docker: false,
            forward: None,
        };
        assert!(fast_checks(&none, false, false).is_empty());
    }

    fn probe_key(s: &InvariantSettings) -> String {
        format!(
            "timeout 120 docker run --rm --network bridge --pull missing --label borg.probe=host-invariants {} sh -c {} probe {} {}",
            s.probe_image,
            "nslookup \"$1\" >/dev/null 2>&1 || { echo dns-fail; exit 0; }\nnc -z -w 5 \"$1\" \"$2\" >/dev/null 2>&1 || { echo connect-fail; exit 0; }\necho ok",
            s.probe_host,
            s.probe_port
        )
    }

    #[test]
    fn the_container_probe_tells_dns_from_connect_from_not_running() {
        let s = InvariantSettings::default();
        let key = probe_key(&s);
        let x = FakeExec::default();
        x.on(&key, "ok");
        assert_eq!(container_network(&x, "docker", &s), None);
        for (stdout, success, want) in [
            ("dns-fail", true, "cannot resolve github.com"),
            ("connect-fail", true, "cannot connect to github.com:443"),
            ("", false, "did not run to completion: ended by a signal"),
        ] {
            let x = FakeExec::default();
            x.script.lock().unwrap().insert(
                key.clone(),
                Output {
                    success,
                    stdout: stdout.to_owned(),
                },
            );
            let f = container_network(&x, "docker", &s).unwrap();
            assert_eq!(f.check, "container_network");
            assert!(f.detail.contains(want), "{f:?}");
        }
    }

    fn probe_fails(key: &str, code: Option<i32>, stderr: &str) -> InvariantFailure {
        probe_fails_with(&InvariantSettings::default(), key, code, stderr)
    }

    fn probe_fails_with(
        s: &InvariantSettings,
        key: &str,
        code: Option<i32>,
        stderr: &str,
    ) -> InvariantFailure {
        let x = FakeExec::default();
        x.diagnosed.lock().unwrap().insert(
            key.to_owned(),
            Diagnosed {
                output: Output {
                    success: code == Some(0),
                    stdout: String::new(),
                },
                code,
                stderr: stderr.to_owned(),
            },
        );
        let f = container_network(&x, "docker", s).unwrap();
        assert_eq!(f.check, "container_network");
        assert!(f.detail.chars().count() <= MAX_DETAIL_CHARS, "{f:?}");
        f
    }

    #[test]
    fn a_long_image_name_leaves_room_for_the_stderr_cause() {
        let image = format!(
            "us-central1-docker.pkg.dev/example-ci-project-0001/ci-dockerhub-remote/\
             library/mirrors/busybox-host-invariant-probe@sha256:{}",
            "0123456789abcdef".repeat(4)
        );
        assert!((180..=200).contains(&image.len()), "{}", image.len());
        let s = InvariantSettings {
            probe_image: image.clone(),
            ..InvariantSettings::default()
        };
        assert!(s.validate().is_ok());
        let stderr = format!(
            "Unable to find image '{image}' locally\n\
             docker: Error response from daemon: toomanyrequests: You have reached your \
             unauthenticated pull rate limit.\n"
        );
        let f = probe_fails_with(&s, &probe_key(&s), Some(125), &stderr);
        assert!(f.detail.contains("toomanyrequests"), "{f:?}");
        assert!(f.detail.contains("exit 125"), "{f:?}");
        assert!(!f.detail.contains(&image), "{f:?}");
        // The shortened name keeps the registry start and the digest end.
        let shown = shown_image(&image);
        assert_eq!(shown.chars().count(), MAX_IMAGE_CHARS - 1);
        assert!(shown.starts_with("us-central1-docker"), "{shown}");
        assert!(shown.ends_with("cdef"), "{shown}");
        assert!(f.detail.contains(&format!("({shown})")), "{f:?}");
        // A short name is shown whole.
        assert_eq!(shown_image("busybox:1.37"), "busybox:1.37");
    }

    #[test]
    fn only_the_deadline_is_a_timeout_and_sigkill_is_reported_as_such() {
        let key = probe_key(&InvariantSettings::default());
        let f = probe_fails(&key, Some(124), "");
        assert!(f.detail.ends_with("timed out after 120s"), "{f:?}");
        let f = probe_fails(&key, Some(137), "");
        assert!(
            f.detail.ends_with("exit 137: process killed (SIGKILL)"),
            "{f:?}"
        );
        assert!(!f.detail.contains("timed out"), "{f:?}");
    }

    #[test]
    fn a_probe_that_did_not_finish_says_why_within_the_bound() {
        let key = probe_key(&InvariantSettings::default());
        // Docker Hub unreachable or rate limited: docker run exits 125.
        let pull = "Unable to find image 'busybox:1.37' locally\n\
            docker: Error response from daemon: Get \"https://registry-1.docker.io/v2/\": \
            dial tcp: lookup registry-1.docker.io: i/o timeout.\n\
            See 'docker run --help'.\n";
        let f = probe_fails(&key, Some(125), pull);
        assert!(
            f.detail
                .contains("(busybox:1.37) did not run to completion"),
            "{f:?}"
        );
        assert!(f.detail.contains("exit 125"), "{f:?}");
        assert!(f.detail.contains("image pull or daemon error"), "{f:?}");
        // Docker's notes are dropped; its error is kept, through to the
        // registry it could not reach.
        assert!(
            f.detail.contains(
                "stderr: docker: Error response from daemon: Get \"https://registry-1.docker.io/v2/\""
            ),
            "{f:?}"
        );
        assert!(!f.detail.contains("Unable to find image"), "{f:?}");
        assert!(!f.detail.contains("docker run --help"), "{f:?}");
        assert!(!f.detail.contains('\n'));
        // A pull that hangs: `timeout` ends it with 124.
        let f = probe_fails(&key, Some(124), "");
        assert!(f.detail.ends_with("timed out after 120s"), "{f:?}");
        // The daemon itself refuses.
        let f = probe_fails(
            &key,
            Some(125),
            "docker: Cannot connect to the Docker daemon at unix:///var/run/docker.sock.",
        );
        assert!(
            f.detail.contains("Cannot connect to the Docker daemon"),
            "{f:?}"
        );
        // An image without a shell; another exit code is reported as is.
        assert!(probe_fails(&key, Some(127), "").detail.contains("exit 127"));
        assert!(probe_fails(&key, Some(3), "").detail.ends_with("exit 3"));
        // Only the first lines of stderr are kept, and a flood is cut to the
        // bound with the cause still first.
        let f = probe_fails(&key, Some(125), "one\n\ntwo\nthree\nfour\n");
        assert!(f.detail.ends_with("stderr: one | two | three"), "{f:?}");
        // Many short words: one long token would itself be shortened.
        let flood = format!("{}\u{1b}[31m\n", "xx ".repeat(2000));
        let f = probe_fails(&key, Some(125), &flood);
        assert_eq!(f.detail.chars().count(), MAX_DETAIL_CHARS);
        assert!(f.detail.contains("exit 125"), "{f:?}");
        assert!(f.detail.ends_with("..."));
        assert!(!f.detail.chars().any(char::is_control));
    }

    #[test]
    fn the_system_executor_keeps_exit_code_and_stderr() {
        use crate::drone::exec::SystemExec;
        let d = SystemExec
            .run_diagnosed("sh", &["-c", "echo out; echo err-line >&2; exit 125"])
            .unwrap();
        assert!(!d.output.success);
        assert_eq!(d.output.stdout, "out");
        assert_eq!(d.code, Some(125));
        assert_eq!(d.stderr, "err-line");
    }

    #[test]
    fn a_failed_probe_is_retried_after_a_minute_and_a_pass_waits_the_interval() {
        let s = InvariantSettings::default();
        assert_eq!(s.interval_secs, 600);
        assert_eq!(next_probe_delay(&s, false), Duration::from_secs(600));
        assert_eq!(next_probe_delay(&s, true), Duration::from_secs(RETRY_SECS));
        // The retry never waits longer than the configured interval, and no
        // probe runs more often than once a minute.
        for (interval_secs, pass, fail) in
            [(1, 60, 60), (60, 60, 60), (90, 90, 60), (3600, 3600, 60)]
        {
            let s = InvariantSettings {
                interval_secs,
                ..InvariantSettings::default()
            };
            assert_eq!(next_probe_delay(&s, false), Duration::from_secs(pass));
            assert_eq!(next_probe_delay(&s, true), Duration::from_secs(fail));
        }
    }

    #[test]
    fn a_change_is_logged_once_and_settings_validate() {
        let a = vec![failure("ip_forward", "x")];
        assert!(log_change(&[], &a));
        assert!(!log_change(&a, &[failure("ip_forward", "other words")]));
        assert!(log_change(&a, &[]));
        assert!(InvariantSettings::default().validate().is_ok());
        let bad = InvariantSettings {
            probe_host: "github.com; rm -rf /".into(),
            ..InvariantSettings::default()
        };
        assert!(bad.validate().is_err());
    }
}
