//! Host probe: what this machine can lend to the fleet.
//!
//! Every host interaction goes through the [`Host`] trait so detection logic
//! is tested against fake filesystems and command outputs.

pub mod system;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::drone::exec::Exec;
use crate::envcontract::{EnvFacts, PROBE_TOOLS, TOOL_MISSING};

/// What kind of machine the drone runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostKind {
    Wsl,
    Lxc,
    Vm,
    Bare,
}

/// Read-only view of the machine. Reads and commands return `None` on any
/// failure; the probe degrades field by field rather than failing.
pub trait Host {
    fn read_to_string(&self, path: &Path) -> Option<String>;
    fn exists(&self, path: &Path) -> bool;
    /// Run `program` and return trimmed stdout if it exited 0.
    fn run(&self, program: &str, args: &[&str]) -> Option<String>;
    /// Operating system, as in `std::env::consts::OS` (`linux`, `macos`).
    fn os(&self) -> &'static str;
    /// CPU architecture, as in `std::env::consts::ARCH`.
    fn arch(&self) -> &'static str;
    /// CPUs this process may use (honours cgroup quotas).
    fn available_parallelism(&self) -> Option<u32>;
}

/// The capacity record a drone reports to the queen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capacity {
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub kind: HostKind,
    pub vcpu: u32,
    pub mem_mib: u64,
    /// Free space on the filesystem holding the work directory.
    pub free_disk_mib: Option<u64>,
    /// `docker info` reached a daemon as the service user. This is inventory;
    /// job-user execution proof lives in `EnvFacts::capabilities`.
    pub docker_usable: bool,
    pub mise_version: Option<String>,
    pub git_version: Option<String>,
}

/// Directory whose filesystem free space is reported.
const DISK_PROBE_PATH: &str = "/";

pub fn probe(host: &dyn Host) -> Capacity {
    probe_at(host, Path::new(DISK_PROBE_PATH))
}

/// Capacity of the filesystem actually used for leases.
pub fn probe_at(host: &dyn Host, work_root: &Path) -> Capacity {
    Capacity {
        hostname: hostname(host),
        os: host.os().to_owned(),
        arch: host.arch().to_owned(),
        kind: host_kind(host),
        vcpu: host.available_parallelism().unwrap_or(1).max(1),
        mem_mib: mem_mib(host).unwrap_or(0),
        free_disk_mib: free_disk_mib_at(host, work_root),
        docker_usable: host
            .run("docker", &["info", "--format", "{{.ServerVersion}}"])
            .is_some(),
        mise_version: host.run("mise", &["--version"]),
        git_version: host.run("git", &["--version"]),
    }
}

/// Build-environment facts that need no tooling: memory available and CPU
/// pressure. Sent on every heartbeat; the admission rule reads the pressure.
pub fn env_facts(host: &dyn Host, now: i64) -> EnvFacts {
    EnvFacts {
        observed_at: now,
        mem_available_mib: mem_available_mib(host),
        cpu_psi_some_avg10_pm: cpu_psi_pm(host),
        load_per_cpu_pm: load_per_cpu_pm(host),
        ..EnvFacts::default()
    }
}

/// The 1-minute load average per usable CPU, in permille.
fn load_per_cpu_pm(host: &dyn Host) -> Option<u32> {
    let load = host
        .read_to_string(Path::new("/proc/loadavg"))?
        .split_whitespace()
        .next()?
        .parse::<f64>()
        .ok()
        .filter(|l| l.is_finite() && *l >= 0.0)?;
    let cpus = f64::from(host.available_parallelism()?.max(1));
    Some((load / cpus * 1000.0).round().min(f64::from(u32::MAX)) as u32)
}

/// How long a tools probe is reused before the tools are looked up again.
const TOOLS_TTL_SECS: i64 = 60;
/// Longest version string reported for a tool.
const TOOL_VERSION_MAX: usize = 80;

/// The first line of a tool's `--version` output, trimmed and clipped to
/// `TOOL_VERSION_MAX` characters; `"unknown"` when there is no output.
pub fn tool_version_line(output: &str) -> String {
    let line = output.lines().map(str::trim).find(|l| !l.is_empty());
    match line {
        Some(l) => l.chars().take(TOOL_VERSION_MAX).collect(),
        None => "unknown".to_owned(),
    }
}

/// For each tool in [`PROBE_TOOLS`], its version line, or `"missing"` when the
/// shell cannot find it on `PATH`. `exec` runs as the job user, so this is the
/// `PATH` a job gets. A tool that exists but prints no version is `"unknown"`.
pub fn tool_versions(exec: &dyn Exec) -> BTreeMap<String, String> {
    PROBE_TOOLS
        .iter()
        .map(|&tool| {
            let found = exec
                .run(
                    "sh",
                    &["-c", "command -v \"$1\" >/dev/null 2>&1", "sh", tool],
                )
                .is_ok_and(|o| o.success);
            let value = if found {
                // `go` has no `--version` flag.
                let args: &[&str] = if tool == "go" {
                    &["version"]
                } else {
                    &["--version"]
                };
                match exec.run(tool, args) {
                    Ok(o) if o.success => tool_version_line(&o.stdout),
                    _ => "unknown".to_owned(),
                }
            } else {
                TOOL_MISSING.to_owned()
            };
            (tool.to_owned(), value)
        })
        .collect()
}

/// The last tools probe, reused for `TOOLS_TTL_SECS`.
#[derive(Clone, Default)]
pub struct ToolsCache(Arc<Mutex<Option<ToolsProbe>>>);

/// When the tools were probed, and what was found.
type ToolsProbe = (i64, BTreeMap<String, String>);

impl ToolsCache {
    /// The cached tools if they are under a minute old, else a fresh probe.
    pub fn get(&self, exec: &dyn Exec, now: i64) -> Option<BTreeMap<String, String>> {
        let mut slot = self.0.lock().ok()?;
        if let Some((at, tools)) = slot.as_ref()
            && (0..TOOLS_TTL_SECS).contains(&(now - at))
        {
            return Some(tools.clone());
        }
        let tools = tool_versions(exec);
        *slot = Some((now, tools.clone()));
        Some(tools)
    }
}

pub(crate) fn mem_available_mib(host: &dyn Host) -> Option<u64> {
    let meminfo = host.read_to_string(Path::new("/proc/meminfo"))?;
    let kib = meminfo
        .lines()
        .find_map(|l| l.strip_prefix("MemAvailable:"))?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()?;
    Some(kib / 1024)
}

/// The higher of the host-wide and the cgroup `cpu some avg10`, in permille.
/// Inside an LXC container `/proc/pressure/cpu` is the host's or the
/// container's depending on lxcfs, so both files are read.
fn cpu_psi_pm(host: &dyn Host) -> Option<u16> {
    ["/proc/pressure/cpu", "/sys/fs/cgroup/cpu.pressure"]
        .iter()
        .filter_map(|path| parse_psi_some_avg10_pm(&host.read_to_string(Path::new(path))?))
        .max()
}

/// `some avg10=1.23 ...` (percent) as permille, clamped to 1000.
fn parse_psi_some_avg10_pm(text: &str) -> Option<u16> {
    let line = text.lines().find(|l| l.starts_with("some "))?;
    let pct = line
        .split_whitespace()
        .find_map(|f| f.strip_prefix("avg10="))?
        .parse::<f64>()
        .ok()?;
    pct.is_finite()
        .then(|| (pct * 10.0).round().clamp(0.0, 1000.0) as u16)
}

fn hostname(host: &dyn Host) -> String {
    host.read_to_string(Path::new("/proc/sys/kernel/hostname"))
        .or_else(|| host.run("hostname", &[]))
        .map(|h| h.trim().to_owned())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Detect WSL, then LXC, then a hypervisor guest, else bare metal.
pub fn host_kind(host: &dyn Host) -> HostKind {
    if is_wsl(host) {
        HostKind::Wsl
    } else if is_lxc(host) {
        HostKind::Lxc
    } else if is_vm(host) {
        HostKind::Vm
    } else {
        HostKind::Bare
    }
}

fn is_wsl(host: &dyn Host) -> bool {
    host.exists(Path::new("/proc/sys/fs/binfmt_misc/WSLInterop"))
        || host.exists(Path::new("/run/WSL"))
        || host
            .read_to_string(Path::new("/proc/version"))
            .is_some_and(|v| {
                let v = v.to_ascii_lowercase();
                v.contains("microsoft") || v.contains("wsl")
            })
}

fn is_lxc(host: &dyn Host) -> bool {
    if host
        .read_to_string(Path::new("/run/systemd/container"))
        .is_some_and(|c| c.trim().starts_with("lxc"))
    {
        return true;
    }
    host.read_to_string(Path::new("/proc/1/environ"))
        .is_some_and(|env| env.split('\0').any(|kv| kv.starts_with("container=lxc")))
}

fn is_vm(host: &dyn Host) -> bool {
    if host.os() == "macos" {
        return host
            .run("sysctl", &["-n", "kern.hv_vmm_present"])
            .as_deref()
            == Some("1");
    }
    const VM_MARKERS: [&str; 7] = [
        "qemu",
        "kvm",
        "vmware",
        "virtualbox",
        "xen",
        "microsoft corporation",
        "bochs",
    ];
    let dmi = ["product_name", "sys_vendor"].iter().any(|field| {
        host.read_to_string(&Path::new("/sys/class/dmi/id").join(field))
            .is_some_and(|v| {
                let v = v.to_ascii_lowercase();
                VM_MARKERS.iter().any(|m| v.contains(m))
            })
    });
    dmi || host
        .read_to_string(Path::new("/proc/cpuinfo"))
        .is_some_and(|c| {
            c.lines()
                .any(|l| l.starts_with("flags") && l.split_whitespace().any(|f| f == "hypervisor"))
        })
}

/// Total memory in MiB, clamped by any cgroup limit (LXC and containers
/// report the host's `MemTotal` unless lxcfs is mounted).
fn mem_mib(host: &dyn Host) -> Option<u64> {
    let total = if host.os() == "macos" {
        host.run("sysctl", &["-n", "hw.memsize"])?
            .parse::<u64>()
            .ok()
            .map(|bytes| bytes / (1024 * 1024))?
    } else {
        let meminfo = host.read_to_string(Path::new("/proc/meminfo"))?;
        let kib = meminfo
            .lines()
            .find_map(|l| l.strip_prefix("MemTotal:"))?
            .split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()?;
        kib / 1024
    };
    Some(match cgroup_mem_limit_mib(host) {
        Some(limit) if limit < total => limit,
        _ => total,
    })
}

fn cgroup_mem_limit_mib(host: &dyn Host) -> Option<u64> {
    [
        "/sys/fs/cgroup/memory.max",
        "/sys/fs/cgroup/memory/memory.limit_in_bytes",
    ]
    .iter()
    .find_map(|path| {
        host.read_to_string(Path::new(path))?
            .trim()
            .parse::<u64>()
            .ok()
    })
    .map(|bytes| bytes / (1024 * 1024))
}

/// Fresh available blocks on the configured job filesystems (which may differ).
pub fn job_disk_space(
    host: &dyn Host,
    work_root: &Path,
    cache_root: &Path,
) -> (Option<u64>, Option<u64>) {
    (
        free_disk_mib_at(host, work_root),
        free_disk_mib_at(host, cache_root).map(|mib| mib.saturating_mul(1 << 20)),
    )
}

/// Free space, in MiB, on the filesystem holding `path`.
pub fn free_disk_mib_at(host: &dyn Host, path: &Path) -> Option<u64> {
    // POSIX `df -Pk`: header line, then `fs blocks used avail cap mount`.
    let out = host.run("df", &["-Pk", &path.to_string_lossy()])?;
    let avail_kib = out
        .lines()
        .nth(1)?
        .split_whitespace()
        .nth(3)?
        .parse::<u64>()
        .ok()?;
    Some(avail_kib / 1024)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[derive(Default)]
    struct FakeHost {
        files: HashMap<&'static str, &'static str>,
        commands: HashMap<&'static str, &'static str>,
        os: &'static str,
        cpus: Option<u32>,
    }

    impl Host for FakeHost {
        fn read_to_string(&self, path: &Path) -> Option<String> {
            self.files.get(path.to_str()?).map(|s| (*s).to_owned())
        }
        fn exists(&self, path: &Path) -> bool {
            path.to_str().is_some_and(|p| self.files.contains_key(p))
        }
        fn run(&self, program: &str, args: &[&str]) -> Option<String> {
            let key = if program == "df" {
                args.last()
                    .copied()
                    .filter(|path| self.commands.contains_key(path))
                    .unwrap_or(program)
            } else {
                program
            };
            self.commands.get(key).map(|s| (*s).to_owned())
        }
        fn os(&self) -> &'static str {
            self.os
        }
        fn arch(&self) -> &'static str {
            "x86_64"
        }
        fn available_parallelism(&self) -> Option<u32> {
            self.cpus
        }
    }

    #[test]
    fn job_disk_probes_use_each_configured_filesystem_and_preserve_unknown() {
        let host = FakeHost {
            commands: HashMap::from([
                (
                    "/",
                    "fs blocks used available cap mount\nroot 999999999 0 999999999 0% /",
                ),
                (
                    "/work",
                    "fs blocks used available cap mount\nwork 99999999 0 8388608 0% /work",
                ),
                (
                    "/cache",
                    "fs blocks used available cap mount\ncache 99999999 0 52428800 0% /cache",
                ),
            ]),
            ..linux()
        };
        assert_eq!(
            probe_at(&host, Path::new("/work")).free_disk_mib,
            Some(8192)
        );
        assert_eq!(
            job_disk_space(&host, Path::new("/work"), Path::new("/cache")),
            (Some(8192), Some(50 << 30))
        );
        assert_eq!(
            job_disk_space(&host, Path::new("/missing"), Path::new("/cache")),
            (None, Some(50 << 30))
        );
    }

    fn linux() -> FakeHost {
        FakeHost {
            os: "linux",
            cpus: Some(16),
            ..FakeHost::default()
        }
    }

    struct FakeExec {
        calls: Mutex<Vec<String>>,
    }

    impl Exec for FakeExec {
        fn run(&self, program: &str, args: &[&str]) -> std::io::Result<crate::drone::exec::Output> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{program} {}", args.join(" ")));
            let ok = |stdout: &str| {
                Ok(crate::drone::exec::Output {
                    success: true,
                    stdout: stdout.to_owned(),
                })
            };
            let no = || {
                Ok(crate::drone::exec::Output {
                    success: false,
                    stdout: String::new(),
                })
            };
            match (program, args) {
                ("sh", [_, _, _, tool]) => {
                    if ["git", "gh", "go", "jq"].contains(tool) {
                        ok("")
                    } else {
                        no()
                    }
                }
                ("git", ["--version"]) => ok("git version 2.45.0\nbuilt from source"),
                ("gh", ["--version"]) => {
                    ok("\n  gh version 2.50.0 (2026-01-01)  \nhttps://example.test")
                }
                ("go", ["version"]) => ok("go version go1.25.0 linux/amd64"),
                _ => no(),
            }
        }
    }

    #[test]
    fn version_lines_are_the_first_non_empty_line_trimmed_and_clipped() {
        assert_eq!(
            tool_version_line("git version 2.45.0\nmore"),
            "git version 2.45.0"
        );
        assert_eq!(tool_version_line("\n  v22.1.0  \n"), "v22.1.0");
        assert_eq!(tool_version_line("   \n"), "unknown");
        assert_eq!(tool_version_line(&"x".repeat(200)).len(), 80);
    }

    #[test]
    fn tools_are_found_on_path_then_asked_for_their_version() {
        let exec = FakeExec {
            calls: Mutex::default(),
        };
        let t = tool_versions(&exec);
        assert_eq!(t.len(), PROBE_TOOLS.len());
        assert_eq!(t["git"], "git version 2.45.0");
        assert_eq!(t["gh"], "gh version 2.50.0 (2026-01-01)");
        assert_eq!(t["go"], "go version go1.25.0 linux/amd64");
        // On PATH but silent: present, version unknown.
        assert_eq!(t["jq"], "unknown");
        assert_eq!(t["node"], "missing");
        assert_eq!(t["cosign"], "missing");
        // A missing tool is never executed.
        assert!(
            !exec
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.starts_with("node "))
        );
    }

    #[test]
    fn the_tools_probe_is_cached_for_a_minute() {
        let exec = FakeExec {
            calls: Mutex::default(),
        };
        let cache = ToolsCache::default();
        let first = cache.get(&exec, 1000).unwrap();
        let n = exec.calls.lock().unwrap().len();
        assert_eq!(cache.get(&exec, 1059).unwrap(), first);
        assert_eq!(exec.calls.lock().unwrap().len(), n);
        cache.get(&exec, 1060).unwrap();
        assert!(exec.calls.lock().unwrap().len() > n);
    }

    #[test]
    fn load_per_cpu_is_permille_of_usable_cpus() {
        let mut h = linux();
        assert_eq!(load_per_cpu_pm(&h), None);
        h.files
            .insert("/proc/loadavg", "93.40 78.56 69.31 90/3277 3570264\n");
        assert_eq!(load_per_cpu_pm(&h), Some(5838));
        assert_eq!(env_facts(&h, 1).load_per_cpu_pm, Some(5838));
        h.files.insert("/proc/loadavg", "garbage");
        assert_eq!(load_per_cpu_pm(&h), None);
    }

    #[test]
    fn psi_is_the_higher_of_host_and_cgroup_pressure() {
        let mut h = linux();
        h.files.insert(
            "/proc/pressure/cpu",
            "some avg10=12.34 avg60=1.00 avg300=1.00 total=1\nfull avg10=0.00\n",
        );
        assert_eq!(cpu_psi_pm(&h), Some(123));
        h.files.insert(
            "/sys/fs/cgroup/cpu.pressure",
            "some avg10=45.60 avg60=1.00 avg300=1.00 total=1\n",
        );
        assert_eq!(cpu_psi_pm(&h), Some(456));
        assert_eq!(cpu_psi_pm(&linux()), None);
        assert_eq!(parse_psi_some_avg10_pm("full avg10=5.00\n"), None);
        assert_eq!(
            parse_psi_some_avg10_pm("some avg10=100.00 avg60=0\n"),
            Some(1000)
        );
    }

    const MEMINFO_28G: &str = "MemTotal:       29360128 kB\nMemFree: 1 kB\n";

    #[test]
    fn detects_wsl_from_interop_or_kernel_string() {
        let mut h = linux();
        h.files.insert("/proc/sys/fs/binfmt_misc/WSLInterop", "");
        assert_eq!(host_kind(&h), HostKind::Wsl);

        let mut h = linux();
        h.files.insert(
            "/proc/version",
            "Linux version 5.15.0-microsoft-standard-WSL2",
        );
        assert_eq!(host_kind(&h), HostKind::Wsl);
    }

    #[test]
    fn detects_lxc_from_systemd_or_init_environ() {
        let mut h = linux();
        h.files.insert("/run/systemd/container", "lxc\n");
        assert_eq!(host_kind(&h), HostKind::Lxc);

        let mut h = linux();
        h.files
            .insert("/proc/1/environ", "PATH=/bin\0container=lxc\0TERM=linux\0");
        assert_eq!(host_kind(&h), HostKind::Lxc);
    }

    #[test]
    fn detects_vm_from_dmi_or_hypervisor_flag() {
        let mut h = linux();
        h.files.insert("/sys/class/dmi/id/sys_vendor", "QEMU\n");
        assert_eq!(host_kind(&h), HostKind::Vm);

        let mut h = linux();
        h.files.insert(
            "/proc/cpuinfo",
            "processor : 0\nflags : fpu vme hypervisor lm\n",
        );
        assert_eq!(host_kind(&h), HostKind::Vm);
    }

    #[test]
    fn detects_macos_vm_and_bare() {
        let mut h = FakeHost {
            os: "macos",
            cpus: Some(8),
            ..FakeHost::default()
        };
        assert_eq!(host_kind(&h), HostKind::Bare);
        h.commands.insert("sysctl", "1");
        assert_eq!(host_kind(&h), HostKind::Vm);
    }

    #[test]
    fn bare_metal_is_the_fallback_and_wsl_wins_over_vm() {
        assert_eq!(host_kind(&linux()), HostKind::Bare);

        let mut h = linux();
        h.files.insert("/run/WSL", "");
        h.files.insert("/proc/cpuinfo", "flags : hypervisor\n");
        assert_eq!(host_kind(&h), HostKind::Wsl);
    }

    #[test]
    fn memory_comes_from_meminfo() {
        let mut h = linux();
        h.files.insert("/proc/meminfo", MEMINFO_28G);
        assert_eq!(mem_mib(&h), Some(28672));
    }

    #[test]
    fn cgroup_limit_clamps_memory_but_max_does_not() {
        let mut h = linux();
        h.files.insert("/proc/meminfo", MEMINFO_28G);
        h.files.insert("/sys/fs/cgroup/memory.max", "8589934592\n");
        assert_eq!(mem_mib(&h), Some(8192));

        h.files.insert("/sys/fs/cgroup/memory.max", "max\n");
        assert_eq!(mem_mib(&h), Some(28672));
    }

    #[test]
    fn macos_memory_comes_from_sysctl() {
        let mut h = FakeHost {
            os: "macos",
            cpus: Some(8),
            ..FakeHost::default()
        };
        h.commands.insert("sysctl", "17179869184");
        assert_eq!(mem_mib(&h), Some(16384));
    }

    #[test]
    fn free_disk_parses_posix_df() {
        let mut h = linux();
        h.commands.insert(
            "df",
            "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/sda1 100000000 1000000 52428800 2% /",
        );
        assert_eq!(free_disk_mib_at(&h, Path::new("/")), Some(51200));
    }

    #[test]
    fn probe_assembles_capacity_and_degrades_per_field() {
        let mut h = linux();
        h.files.insert("/proc/meminfo", MEMINFO_28G);
        h.files.insert("/proc/sys/kernel/hostname", "host-a\n");
        h.commands.insert("docker", "27.0.1");
        h.commands.insert("git", "git version 2.45.0");

        let c = probe(&h);
        assert_eq!(c.hostname, "host-a");
        assert_eq!((c.vcpu, c.mem_mib), (16, 28672));
        assert!(c.docker_usable);
        assert_eq!(c.git_version.as_deref(), Some("git version 2.45.0"));
        assert_eq!(c.mise_version, None);
        assert_eq!(c.free_disk_mib, None);
        assert_eq!(c.kind, HostKind::Bare);
    }

    #[test]
    fn probe_of_an_empty_host_still_reports_something() {
        let c = probe(&FakeHost {
            os: "linux",
            ..FakeHost::default()
        });
        assert_eq!((c.hostname.as_str(), c.vcpu, c.mem_mib), ("unknown", 1, 0));
        assert!(!c.docker_usable);
    }

    #[test]
    fn capacity_serializes_as_json() {
        let c = probe(&linux());
        let v: serde_json::Value = serde_json::to_value(&c).unwrap();
        assert_eq!(v["kind"], "bare");
        assert_eq!(v["vcpu"], 16);
    }
}
