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
//!   result is reused by every heartbeat in between.
//!
//! Whether the binary matches the fleet's is not decided here: a drone does
//! not know the fleet's build. The queen exports every drone's build in
//! `borg_drone_info` and alerts on a drone behind the fleet.

use serde::{Deserialize, Serialize};

use crate::drone::exec::Exec;
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
    let out = exec.run(
        "timeout",
        &[
            "120",
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
        Ok(o) if o.success && o.stdout.trim().ends_with("ok") => None,
        Ok(o) if o.stdout.contains("dns-fail") => Some(failure(
            "container_network",
            format!(
                "a container on the default bridge cannot resolve {}",
                s.probe_host
            ),
        )),
        Ok(o) if o.stdout.contains("connect-fail") => Some(failure(
            "container_network",
            format!("a container on the default bridge cannot connect to {target}"),
        )),
        Ok(_) => Some(failure(
            "container_network",
            format!(
                "the probe container ({}) did not run to completion",
                s.probe_image
            ),
        )),
        Err(e) => Some(failure(
            "container_network",
            format!("could not start the probe container: {e}"),
        )),
    }
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
            ("", false, "did not run to completion"),
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
