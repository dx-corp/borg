//! Per-lease memory limits through cgroup v2.
//!
//! The queen admits leases by their class's memory, but nothing stopped one
//! job from using far more: a single heavy rustc using 13 to 28.5 GiB OOM-killed
//! everything in its container 16 times in a week (gha-heavy-pve50-1). Each
//! lease now runs in its own child cgroup of the drone's service cgroup, with
//! `memory.max` from the grant (the class size plus the queen's headroom) and
//! `memory.high` a little below it for back-pressure. A job over its limit is
//! OOM-killed alone; the drone reads `memory.events` and reports `oom_killed`.
//!
//! The drone runs as an unprivileged systemd service, so it needs the cgroup
//! delegated (`Delegate=yes` in its unit, which `borg drone install` writes).
//! cgroup v2 allows controllers only on cgroups without processes of their
//! own, so the drone first moves itself into a `drone` leaf of its service
//! cgroup, then enables the memory controller for the leaf cgroups beside it.
//! Without delegation, cgroup v2 or the memory controller, leases run without
//! a limit, as before, and the drone says so once at startup.
//!
//! A lease's runner is started through `sh`, which writes itself into the
//! lease cgroup before it `exec`s the runner, so every process the job starts
//! is inside the limit from its first instruction.
//!
//! Docker containers a job starts are not covered: dockerd creates them in
//! its own cgroup, not under the lease's, so their memory is outside
//! `memory.max` (see `docs/operations/borg.md`).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The memory limit of one lease, from its grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LeaseMemory {
    /// `memory.max`: the job is OOM-killed above this.
    pub max_bytes: u64,
    /// `memory.high`: the kernel throttles and reclaims above this.
    pub high_bytes: u64,
}

/// The leaf cgroup the drone process itself lives in.
pub(crate) const DRONE_LEAF: &str = "drone";

/// Where the drone creates lease cgroups: its own service cgroup.
#[derive(Debug, Clone)]
pub struct LeaseCgroups {
    base: PathBuf,
}

/// The cgroup v2 path in `/proc/self/cgroup` contents (the `0::` line).
pub fn unified_path(proc_self_cgroup: &str) -> Option<&str> {
    proc_self_cgroup
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(str::trim)
}

/// The `oom_kill` count in `memory.events` contents.
pub fn oom_kills(memory_events: &str) -> u64 {
    memory_events
        .lines()
        .find_map(|l| l.strip_prefix("oom_kill "))
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0)
}

impl LeaseCgroups {
    /// The service cgroup under `cgroup_root` (normally `/sys/fs/cgroup`) for
    /// a process whose `/proc/self/cgroup` reads `proc_self_cgroup`, if it
    /// offers the memory controller. A drone already moved into its leaf (a
    /// restart within the same service) maps back to the service cgroup.
    pub fn detect(cgroup_root: &Path, proc_self_cgroup: &str) -> io::Result<Self> {
        let rel = unified_path(proc_self_cgroup)
            .ok_or_else(|| io::Error::other("not on the cgroup v2 unified hierarchy"))?;
        let mut base = cgroup_root.join(rel.trim_start_matches('/'));
        if base.file_name().is_some_and(|n| n == DRONE_LEAF) {
            base.pop();
        }
        let controllers = fs::read_to_string(base.join("cgroup.controllers"))?;
        if !controllers.split_whitespace().any(|c| c == "memory") {
            return Err(io::Error::other(format!(
                "the memory controller is not delegated to {}",
                base.display()
            )));
        }
        Ok(Self { base })
    }

    /// The running system's lease cgroups, ready for use.
    pub fn system() -> io::Result<Self> {
        let own = fs::read_to_string("/proc/self/cgroup")?;
        let cgroups = Self::detect(Path::new("/sys/fs/cgroup"), &own)?;
        cgroups.prepare(std::process::id())?;
        Ok(cgroups)
    }

    /// Move process `pid` (the drone) into the `drone` leaf and enable the
    /// memory controller for the service cgroup's children.
    pub fn prepare(&self, pid: u32) -> io::Result<()> {
        let leaf = self.base.join(DRONE_LEAF);
        fs::create_dir_all(&leaf)?;
        fs::write(leaf.join("cgroup.procs"), pid.to_string())?;
        fs::write(self.base.join("cgroup.subtree_control"), "+memory")
    }

    /// A fresh cgroup for `lease_id` with `limit` applied. A leftover of the
    /// same lease (a drone restart mid-lease) is reused.
    pub fn create(&self, lease_id: &str, limit: LeaseMemory) -> io::Result<LeaseCgroup> {
        let dir = self.base.join(format!("lease-{lease_id}"));
        fs::create_dir_all(&dir)?;
        let cgroup = LeaseCgroup { dir };
        // High first: a max below an existing high is rejected on some kernels.
        fs::write(cgroup.dir.join("memory.high"), limit.high_bytes.to_string())?;
        fs::write(cgroup.dir.join("memory.max"), limit.max_bytes.to_string())?;
        Ok(cgroup)
    }
}

/// One lease's cgroup.
#[derive(Debug)]
pub struct LeaseCgroup {
    dir: PathBuf,
}

impl LeaseCgroup {
    /// `program args...` started so that it runs inside this cgroup: `sh`
    /// writes itself into `cgroup.procs`, then `exec`s the program. If the
    /// write fails the job still runs, without the limit.
    pub fn wrap(&self, program: &Path, args: &[String]) -> (PathBuf, Vec<String>) {
        let mut wrapped = vec![
            "-c".to_owned(),
            r#"{ echo 0 > "$1"; } 2>/dev/null; shift; exec "$@""#.to_owned(),
            "borg-lease".to_owned(),
            self.dir.join("cgroup.procs").to_string_lossy().into_owned(),
            program.to_string_lossy().into_owned(),
        ];
        wrapped.extend(args.iter().cloned());
        (PathBuf::from("/bin/sh"), wrapped)
    }

    /// Whether the kernel OOM-killed a process of this lease.
    pub fn oom_killed(&self) -> bool {
        fs::read_to_string(self.dir.join("memory.events")).is_ok_and(|e| oom_kills(&e) > 0)
    }

    /// Kill whatever is left in the cgroup and remove it. Best effort: a
    /// cgroup that cannot be removed now is reused by the same lease id or
    /// left empty.
    pub fn remove(self) {
        let _ = fs::write(self.dir.join("cgroup.kill"), "1");
        for _ in 0..20 {
            if fs::remove_dir(&self.dir).is_ok() || !self.dir.exists() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    #[cfg(test)]
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory standing in for a delegated service cgroup.
    fn service(root: &Path) -> LeaseCgroups {
        let base = root.join("system.slice/borg-drone.service");
        fs::create_dir_all(&base).unwrap();
        fs::write(base.join("cgroup.controllers"), "cpu memory pids\n").unwrap();
        LeaseCgroups::detect(root, "0::/system.slice/borg-drone.service\n").unwrap()
    }

    #[test]
    fn the_unified_line_and_oom_count_parse() {
        let own = "12:pids:/x\n0::/system.slice/borg-drone.service/drone\n";
        assert_eq!(
            unified_path(own),
            Some("/system.slice/borg-drone.service/drone")
        );
        assert_eq!(unified_path("1:name=systemd:/x\n"), None);
        let events = "low 0\nhigh 12\nmax 3\noom 1\noom_kill 2\noom_group_kill 0\n";
        assert_eq!(oom_kills(events), 2);
        assert_eq!(oom_kills("low 0\n"), 0);
    }

    #[test]
    fn detect_needs_the_memory_controller_and_maps_the_leaf_back() {
        let root = tempfile::tempdir().unwrap();
        let cg = service(root.path());
        // A drone restarted inside its leaf finds the same service cgroup.
        let again =
            LeaseCgroups::detect(root.path(), "0::/system.slice/borg-drone.service/drone").unwrap();
        assert_eq!(again.base, cg.base);
        fs::write(cg.base.join("cgroup.controllers"), "cpu pids\n").unwrap();
        assert!(LeaseCgroups::detect(root.path(), "0::/system.slice/borg-drone.service").is_err());
        assert!(LeaseCgroups::detect(root.path(), "1:name=systemd:/x").is_err());
    }

    #[test]
    fn prepare_moves_the_drone_into_its_leaf_and_enables_memory() {
        let root = tempfile::tempdir().unwrap();
        let cg = service(root.path());
        cg.prepare(4242).unwrap();
        assert_eq!(
            fs::read_to_string(cg.base.join("drone/cgroup.procs")).unwrap(),
            "4242"
        );
        assert_eq!(
            fs::read_to_string(cg.base.join("cgroup.subtree_control")).unwrap(),
            "+memory"
        );
    }

    #[test]
    fn a_lease_cgroup_gets_its_limits_and_reports_an_oom_kill() {
        let root = tempfile::tempdir().unwrap();
        let cg = service(root.path());
        let limit = LeaseMemory {
            max_bytes: 30 << 30,
            high_bytes: 27 << 30,
        };
        let lease = cg.create("l1", limit).unwrap();
        assert_eq!(
            fs::read_to_string(lease.dir().join("memory.max")).unwrap(),
            (30u64 << 30).to_string()
        );
        assert_eq!(
            fs::read_to_string(lease.dir().join("memory.high")).unwrap(),
            (27u64 << 30).to_string()
        );
        assert!(!lease.oom_killed());
        fs::write(lease.dir().join("memory.events"), "oom 1\noom_kill 1\n").unwrap();
        assert!(lease.oom_killed());
    }

    #[test]
    fn the_wrapped_command_joins_the_cgroup_then_runs_the_program() {
        let root = tempfile::tempdir().unwrap();
        let cg = service(root.path());
        let lease = cg
            .create(
                "l2",
                LeaseMemory {
                    max_bytes: 1 << 30,
                    high_bytes: 1 << 29,
                },
            )
            .unwrap();
        let out = root.path().join("out.txt");
        let (program, args) = lease.wrap(
            Path::new("/bin/sh"),
            &["-c".to_owned(), format!("echo ran > {}", out.display())],
        );
        #[allow(
            clippy::disallowed_methods,
            reason = "test: runs /bin/sh through the lease wrapper in a temp dir"
        )]
        let status = std::process::Command::new(program)
            .args(&args)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(fs::read_to_string(&out).unwrap(), "ran\n");
        // The shell wrote itself ("0" means the writer) into cgroup.procs.
        assert_eq!(
            fs::read_to_string(lease.dir().join("cgroup.procs")).unwrap(),
            "0\n"
        );
    }
}
