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
//! The job's processes live in a `job` leaf of the lease cgroup, so the
//! lease cgroup itself can hold children: with the per-lease Docker endpoint
//! ([`crate::drone::dockerproxy`]) dockerd creates the job's containers
//! beside that leaf, inside the lease's `memory.max`. Without the endpoint,
//! containers run in dockerd's own cgroup, outside the limit.
//!
//! The same shell also raises its `oom_score_adj` one step above every lease
//! still running ([`OomOrder`]), so when the whole host runs short of memory
//! the kernel OOM killer and earlyoom lean toward the newest job and never the
//! drone. The kernel also weighs memory use, so that is a hint, not an order:
//! with `host_memory_guard_mib` set, the drone itself kills the newest lease's
//! cgroup when the host's available memory falls below it, before either
//! killer runs. Raising one's own score needs no privilege, so this works from
//! the drone's unprivileged service and survives the job user switch; leases
//! without a cgroup get the score too ([`score_wrap`]).

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
/// The leaf of a lease cgroup its job's processes live in.
pub(crate) const JOB_LEAF: &str = "job";

/// `oom_score_adj` of a lease started while no other lease runs.
pub(crate) const LEASE_OOM_BASE: i32 = 100;
/// How much higher each newer lease scores than the newest one running.
pub(crate) const LEASE_OOM_STEP: i32 = 25;
/// The kernel's ceiling for `oom_score_adj`.
const OOM_SCORE_ADJ_MAX: i32 = 1000;

/// Where the drone creates lease cgroups: its own service cgroup.
#[derive(Debug, Clone)]
pub struct LeaseCgroups {
    /// The cgroup v2 mount (`/sys/fs/cgroup`).
    root: PathBuf,
    base: PathBuf,
    /// `/proc`, or a stand-in in tests.
    proc_root: PathBuf,
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
        Ok(Self {
            root: cgroup_root.to_path_buf(),
            base,
            proc_root: PathBuf::from("/proc"),
        })
    }

    #[cfg(test)]
    pub(crate) fn with_proc_root(mut self, proc_root: &Path) -> Self {
        self.proc_root = proc_root.to_path_buf();
        self
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

    /// A fresh cgroup for `lease_id` with `limit` applied, and its empty
    /// `job` leaf; its processes run with `oom_score_adj` (from
    /// [`OomOrder::assign`]). A leftover of the same lease (a drone restart
    /// mid-lease) is reused.
    pub fn create(
        &self,
        lease_id: &str,
        limit: LeaseMemory,
        oom_score_adj: i32,
    ) -> io::Result<LeaseCgroup> {
        let dir = self.base.join(format!("lease-{lease_id}"));
        fs::create_dir_all(dir.join(JOB_LEAF))?;
        let rel = dir
            .strip_prefix(&self.root)
            .map(|r| format!("/{}", r.to_string_lossy()))
            .unwrap_or_default();
        let cgroup = LeaseCgroup {
            dir,
            rel,
            oom_score_adj,
            self_oom_score_adj: self.proc_root.join("self/oom_score_adj"),
        };
        // High first: a max below an existing high is rejected on some kernels.
        fs::write(cgroup.dir.join("memory.high"), limit.high_bytes.to_string())?;
        fs::write(cgroup.dir.join("memory.max"), limit.max_bytes.to_string())?;
        // Containers placed beside the job leaf get their own memory
        // accounting (and `docker run --memory` works there). Refused only
        // while processes sit in the lease cgroup itself (a lease an older
        // drone started), which leaves containers charged to the lease.
        if let Err(e) = fs::write(cgroup.dir.join("cgroup.subtree_control"), "+memory") {
            tracing::debug!("lease {lease_id}: no memory controller for children: {e}");
        }
        Ok(cgroup)
    }

    /// Kill every process of `lease_id`'s cgroup at once (`cgroup.kill`).
    /// Whether the kill was delivered.
    /// Only a cgroup with processes in it (`populated 1` in `cgroup.events`)
    /// counts: an empty one, a lease still setting up, is left alone.
    pub fn kill(&self, lease_id: &str) -> bool {
        let dir = self.base.join(format!("lease-{lease_id}"));
        let populated = fs::read_to_string(dir.join("cgroup.events"))
            .is_ok_and(|e| e.lines().any(|l| l.trim() == "populated 1"));
        populated && fs::write(dir.join("cgroup.kill"), "1").is_ok()
    }
}

/// The leases running now in launch order, and the `oom_score_adj` each was
/// given. Scores are handed out under one lock, so leases started from the
/// same heartbeat never tie: each new lease gets one step above the highest
/// score still running, or the base when none runs (the scale restarts
/// whenever the drone is idle). Raising its own score needs no privilege, so
/// the wrapping shell can apply it from the unprivileged drone service; it
/// cannot lower a running lease's, so under constant overlap the scores climb
/// to the kernel's ceiling and tie there. That, and the kernel adding memory
/// use to the score, make the score a fallback hint: the host memory guard
/// ([`OomOrder::newest`], `host_memory_guard_mib`) is what kills the newest
/// lease first deterministically.
#[derive(Debug, Default)]
pub struct OomOrder {
    inner: std::sync::Mutex<OrderState>,
}

#[derive(Debug, Default)]
struct OrderState {
    seq: u64,
    /// Lease id to (launch sequence, score).
    running: std::collections::BTreeMap<String, (u64, i32)>,
}

impl OomOrder {
    /// The score for `lease_id`, starting now; recorded until [`Self::release`].
    pub fn assign(&self, lease_id: &str) -> i32 {
        let Ok(mut st) = self.inner.lock() else {
            return LEASE_OOM_BASE;
        };
        let score = st
            .running
            .iter()
            .filter(|(id, _)| id.as_str() != lease_id)
            .map(|(_, &(_, score))| score)
            .max()
            .map_or(LEASE_OOM_BASE, |h| {
                h.max(LEASE_OOM_BASE - LEASE_OOM_STEP)
                    .saturating_add(LEASE_OOM_STEP)
                    .min(OOM_SCORE_ADJ_MAX)
            });
        st.seq += 1;
        let seq = st.seq;
        st.running.insert(lease_id.to_owned(), (seq, score));
        score
    }

    /// `lease_id` ended.
    pub fn release(&self, lease_id: &str) {
        if let Ok(mut st) = self.inner.lock() {
            st.running.remove(lease_id);
        }
    }

    /// The most recently started lease still running.
    pub fn newest(&self) -> Option<String> {
        self.newest_first().into_iter().next()
    }

    /// The leases running now, most recently started first.
    pub fn newest_first(&self) -> Vec<String> {
        let Ok(st) = self.inner.lock() else {
            return Vec::new();
        };
        let mut ids: Vec<(u64, String)> =
            st.running.iter().map(|(id, v)| (v.0, id.clone())).collect();
        ids.sort_by_key(|&(seq, _)| std::cmp::Reverse(seq));
        ids.into_iter().map(|(_, id)| id).collect()
    }
}

/// `program args...` started through `sh`, which raises its own
/// `oom_score_adj` to `score` (via `self_oom`, `/proc/self/oom_score_adj`)
/// and then `exec`s the program, which inherits it. For leases without a
/// cgroup, so they never inherit a protective score from the drone's unit.
pub fn score_wrap(
    program: &Path,
    args: &[String],
    score: i32,
    self_oom: &Path,
) -> (PathBuf, Vec<String>) {
    let mut wrapped = vec![
        "-c".to_owned(),
        r#"{ echo "$2" > "$1"; } 2>/dev/null; shift 2; exec "$@""#.to_owned(),
        "borg-lease".to_owned(),
        self_oom.to_string_lossy().into_owned(),
        score.to_string(),
        program.to_string_lossy().into_owned(),
    ];
    wrapped.extend(args.iter().cloned());
    (PathBuf::from("/bin/sh"), wrapped)
}

/// Available memory (MiB) in `/proc/meminfo` contents.
pub fn mem_available_mib(meminfo: &str) -> Option<u64> {
    meminfo
        .lines()
        .find_map(|l| l.strip_prefix("MemAvailable:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|kib| kib.parse::<u64>().ok())
        .map(|kib| kib / 1024)
}

/// One lease's cgroup.
#[derive(Debug)]
pub struct LeaseCgroup {
    dir: PathBuf,
    /// `dir` from the cgroup root, as `/proc/<pid>/cgroup` and dockerd's
    /// `CgroupParent` name it (`/system.slice/borg-drone.service/lease-<id>`).
    rel: String,
    /// The score the lease's processes run with; see [`OomOrder`].
    oom_score_adj: i32,
    /// `/proc/self/oom_score_adj` as the wrapping shell sees it.
    self_oom_score_adj: PathBuf,
}

impl LeaseCgroup {
    /// `program args...` started so that it runs inside this cgroup: `sh`
    /// writes itself into `cgroup.procs`, raises its `oom_score_adj`, then
    /// `exec`s the program, which inherits both. If a write fails the job
    /// still runs, without the limit or the score.
    pub fn wrap(&self, program: &Path, args: &[String]) -> (PathBuf, Vec<String>) {
        let mut wrapped = vec![
            "-c".to_owned(),
            r#"{ echo 0 > "$1"; } 2>/dev/null; { echo "$3" > "$2"; } 2>/dev/null; shift 3; exec "$@""#
                .to_owned(),
            "borg-lease".to_owned(),
            self.dir
                .join(JOB_LEAF)
                .join("cgroup.procs")
                .to_string_lossy()
                .into_owned(),
            self.self_oom_score_adj.to_string_lossy().into_owned(),
            self.oom_score_adj.to_string(),
            program.to_string_lossy().into_owned(),
        ];
        wrapped.extend(args.iter().cloned());
        (PathBuf::from("/bin/sh"), wrapped)
    }

    /// The lease cgroup from the cgroup root (`/system.slice/...`).
    pub fn rel_path(&self) -> &str {
        &self.rel
    }

    /// The score the lease's processes run with.
    pub fn oom_score_adj(&self) -> i32 {
        self.oom_score_adj
    }

    /// The memory the lease (its processes and any containers under it)
    /// holds that the kernel cannot simply drop: `memory.current` less the
    /// inactive page cache, the "working set" `docker stats` reports.
    pub fn working_set_bytes(&self) -> Option<u64> {
        working_set(&self.dir)
    }

    /// Whether the kernel OOM-killed a process of this lease.
    pub fn oom_killed(&self) -> bool {
        fs::read_to_string(self.dir.join("memory.events")).is_ok_and(|e| oom_kills(&e) > 0)
    }

    /// Kill whatever is left in the cgroup (the job and any container under
    /// it) and remove it with its children. Best effort: a cgroup that cannot
    /// be removed now is reused by the same lease id or left empty.
    pub fn remove(self) {
        // cgroupfs has no file creation, so this only ever writes the
        // kernel's own interface file (5.14+).
        let kill = self.dir.join("cgroup.kill");
        if kill.exists() {
            let _ = fs::write(kill, "1");
        }
        for _ in 0..20 {
            if remove_tree(&self.dir) || !self.dir.exists() {
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

/// `memory.current` less `inactive_file` from `memory.stat` in `dir`.
pub(crate) fn working_set(dir: &Path) -> Option<u64> {
    let current: u64 = fs::read_to_string(dir.join("memory.current"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let inactive = fs::read_to_string(dir.join("memory.stat"))
        .ok()
        .and_then(|stat| {
            stat.lines()
                .find_map(|l| l.strip_prefix("inactive_file "))
                .and_then(|v| v.trim().parse::<u64>().ok())
        })
        .unwrap_or(0);
    Some(current.saturating_sub(inactive))
}

/// Remove the cgroup directory `dir` after its child cgroups (cgroupfs
/// removes a cgroup with `rmdir`, and only once it has no children).
/// Whether `dir` is gone.
fn remove_tree(dir: &Path) -> bool {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                remove_tree(&entry.path());
            }
        }
    }
    fs::remove_dir(dir).is_ok()
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
        let lease = cg.create("l1", limit, LEASE_OOM_BASE).unwrap();
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
        // The job gets its own leaf; children of the lease get memory.
        assert!(lease.dir().join("job").is_dir());
        assert_eq!(
            fs::read_to_string(lease.dir().join("cgroup.subtree_control")).unwrap(),
            "+memory"
        );
        // Named as dockerd's CgroupParent and /proc/<pid>/cgroup name it.
        assert_eq!(
            lease.rel_path(),
            "/system.slice/borg-drone.service/lease-l1"
        );
        // The working set leaves out the inactive page cache.
        assert_eq!(lease.working_set_bytes(), None);
        fs::write(lease.dir().join("memory.current"), "5000\n").unwrap();
        fs::write(
            lease.dir().join("memory.stat"),
            "anon 3000\nactive_file 800\ninactive_file 1200\n",
        )
        .unwrap();
        assert_eq!(lease.working_set_bytes(), Some(3800));
    }

    #[test]
    fn removal_takes_child_cgroups_first() {
        let root = tempfile::tempdir().unwrap();
        let cg = service(root.path());
        let lease = cg
            .create(
                "l4",
                LeaseMemory {
                    max_bytes: 1 << 30,
                    high_bytes: 1 << 29,
                },
                LEASE_OOM_BASE,
            )
            .unwrap();
        let dir = lease.dir().to_path_buf();
        // A container cgroup dockerd made beside the job leaf. Plain
        // directories stand in for cgroupfs, where rmdir ignores the
        // interface files; drop the files the test wrote so rmdir works.
        fs::create_dir_all(dir.join("abc123")).unwrap();
        for f in ["memory.high", "memory.max", "cgroup.subtree_control"] {
            fs::remove_file(dir.join(f)).unwrap();
        }
        lease.remove();
        assert!(!dir.exists());
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
                LEASE_OOM_BASE,
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
        // The shell wrote itself ("0" means the writer) into the job leaf.
        assert_eq!(
            fs::read_to_string(lease.dir().join("job/cgroup.procs")).unwrap(),
            "0\n"
        );
    }

    #[test]
    fn each_newer_lease_scores_above_every_running_one() {
        let order = OomOrder::default();
        assert_eq!(order.newest(), None);
        // Two leases from one heartbeat never tie.
        assert_eq!(order.assign("a"), LEASE_OOM_BASE);
        assert_eq!(order.assign("b"), LEASE_OOM_BASE + LEASE_OOM_STEP);
        assert_eq!(order.newest().as_deref(), Some("b"));
        // The oldest ends; the next still outranks the newest running one.
        order.release("a");
        assert_eq!(order.assign("c"), LEASE_OOM_BASE + 2 * LEASE_OOM_STEP);
        assert_eq!(order.newest().as_deref(), Some("c"));
        // Reassigning a lease (a retry) does not count it against itself.
        assert_eq!(order.assign("c"), LEASE_OOM_BASE + 2 * LEASE_OOM_STEP);
        order.release("c");
        assert_eq!(order.newest().as_deref(), Some("b"));
        // An idle drone starts the scale over.
        order.release("b");
        assert_eq!(order.assign("d"), LEASE_OOM_BASE);
        // The score never passes the kernel's ceiling.
        for i in 0..60 {
            order.assign(&format!("x{i}"));
        }
        assert_eq!(order.assign("last"), OOM_SCORE_ADJ_MAX);
        assert_eq!(order.newest().as_deref(), Some("last"));
    }

    #[test]
    fn available_memory_parses_and_the_kill_reaches_the_lease_cgroup() {
        let info = "MemTotal:       131072000 kB\nMemFree:  1000 kB\nMemAvailable:    8388608 kB\n";
        assert_eq!(mem_available_mib(info), Some(8192));
        assert_eq!(mem_available_mib("MemTotal: 1 kB\n"), None);
        let root = tempfile::tempdir().unwrap();
        let cg = service(root.path());
        let lease = cg
            .create(
                "k1",
                LeaseMemory {
                    max_bytes: 1 << 30,
                    high_bytes: 1 << 29,
                },
                LEASE_OOM_BASE,
            )
            .unwrap();
        // Empty (still setting up): left alone.
        fs::write(lease.dir().join("cgroup.events"), "populated 0\nfrozen 0\n").unwrap();
        assert!(!cg.kill("k1"));
        assert!(!lease.dir().join("cgroup.kill").exists());
        fs::write(lease.dir().join("cgroup.events"), "populated 1\nfrozen 0\n").unwrap();
        assert!(cg.kill("k1"));
        assert_eq!(
            fs::read_to_string(lease.dir().join("cgroup.kill")).unwrap(),
            "1"
        );
        assert!(!cg.kill("missing"));
    }

    /// The real kernel interface: an unprivileged process may raise its own
    /// score, and what the wrapper `exec`s inherits it.
    #[test]
    #[cfg(target_os = "linux")]
    fn the_score_is_inherited_through_the_real_procfs() {
        let own: i32 = fs::read_to_string("/proc/self/oom_score_adj")
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let score = own
            .max(LEASE_OOM_BASE)
            .saturating_add(7)
            .min(OOM_SCORE_ADJ_MAX);
        let (program, args) = score_wrap(
            Path::new("/bin/sh"),
            &["-c".to_owned(), "cat /proc/self/oom_score_adj".to_owned()],
            score,
            Path::new("/proc/self/oom_score_adj"),
        );
        #[allow(
            clippy::disallowed_methods,
            reason = "test: runs /bin/sh through the score wrapper"
        )]
        let out = std::process::Command::new(program)
            .args(&args)
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            score.to_string()
        );
    }

    #[test]
    fn the_wrapper_raises_its_own_score_before_running_the_job() {
        let root = tempfile::tempdir().unwrap();
        let proc = root.path().join("proc");
        fs::create_dir_all(proc.join("self")).unwrap();
        let cg = service(root.path()).with_proc_root(&proc);
        let lease = cg
            .create(
                "l3",
                LeaseMemory {
                    max_bytes: 1 << 30,
                    high_bytes: 1 << 29,
                },
                LEASE_OOM_BASE,
            )
            .unwrap();
        let (program, args) = lease.wrap(Path::new("/bin/true"), &[]);
        #[allow(
            clippy::disallowed_methods,
            reason = "test: runs /bin/true through the lease wrapper in a temp dir"
        )]
        let status = std::process::Command::new(program)
            .args(&args)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            fs::read_to_string(proc.join("self/oom_score_adj")).unwrap(),
            format!("{LEASE_OOM_BASE}\n")
        );
    }
}
