//! The drone's [`ReclaimHost`]: build-box reclaim against the real filesystem,
//! git and process table (Linux). Policy and audit live in [`crate::reclaim`].
//!
//! Every probe and deletion runs as the owner of the directory (`setpriv`, the
//! same capability the drone uses for its job user), so git never writes files
//! owned by another user into a checkout, and the busy check sees the owner's
//! processes. Every command runs under `timeout`; a failure or a timeout makes
//! the state unknown, which keeps the directory. Global git config is ignored
//! (`GIT_CONFIG_GLOBAL=/dev/null`) and git takes no optional locks, so probing
//! never rewrites an index.
//!
//! Roots are grouped by filesystem and each group gets its own cycle, because
//! a host can keep worktrees and build output on different volumes.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::drone::config::DroneConfig;
use crate::drone::docker_reclaim::{self, DockerCli};
use crate::drone::exec::Exec;
use crate::drone::slots::{Slots, slot_dir};
use crate::reclaim::{
    Candidate, Cycle, Disk, Kind, Mode, ReclaimHost, ReclaimSettings, ReclaimStats, Root, Safety,
    run_cycle,
};

/// Seconds a single probe or listing command may take.
const PROBE_SECS: u32 = 300;
/// Seconds a deletion may take.
const DELETE_SECS: u32 = 3600;
/// Audit entries sent per heartbeat.
const RECENT_PER_HEARTBEAT: usize = 200;

/// Directory names under each home that hold worktrees on build hosts.
const HOME_WORKTREE_DIRS: &[&str] = &[
    "worktrees",
    "agent-worktrees",
    "claude-worktrees",
    "agent-work",
    ".codex-worktrees",
    ".grok/worktrees",
];
/// Directory names under each home that hold cargo target directories.
const HOME_TARGET_DIRS: &[&str] = &[
    "slot-targets",
    "cargo-targets",
    ".cargo-targets",
    ".cache",
    "t",
];
/// Shared data-volume roots used by the build hosts.
const SHARED_WORKTREE_DIRS: &[&str] = &["/srv/devdata/worktrees", "/srv/devdata/agent-runs"];
const SHARED_TARGET_DIRS: &[&str] = &[
    "/srv/devdata/cargo-target",
    "/srv/devdata/slots",
    "/srv/devdata/worktrees",
];

/// The roots a drone reclaims when its config lists none: the worktree and
/// cargo-target locations under every home directory and the shared data
/// volume, as far as they exist. `exists` is the filesystem check.
fn default_roots(
    homes: &[PathBuf],
    ci_slots: &[PathBuf],
    namespaces: &[PathBuf],
    exists: impl Fn(&Path) -> bool,
) -> Vec<Root> {
    let mut out = Vec::new();
    let mut push = |path: PathBuf, kind: Kind| {
        if exists(&path) && !out.iter().any(|r: &Root| r.path == path && r.kind == kind) {
            out.push(Root { path, kind });
        }
    };
    for home in homes {
        for d in HOME_WORKTREE_DIRS {
            push(home.join(d), Kind::Worktree);
        }
        for d in HOME_TARGET_DIRS {
            push(home.join(d), Kind::CargoTarget);
        }
    }
    for d in SHARED_WORKTREE_DIRS {
        push(PathBuf::from(d), Kind::Worktree);
    }
    for d in SHARED_TARGET_DIRS {
        push(PathBuf::from(d), Kind::CargoTarget);
    }
    // CI jobs keep weekly cargo targets per cache slot; they are regenerable
    // and were never reclaimed, so drones filled their disks with them. The
    // same slots keep a tool home (`mise-home`: rustup, cargo, mise tools),
    // 3-11 GB each on internal-runner-burst-01 on 2026-11-01.
    for d in ci_slots {
        push(d.clone(), Kind::CargoTarget);
        push(d.clone(), Kind::CacheDir);
        // And each whole slot, least recently used first, once the finer
        // pieces are not enough (borg-deploy-2 reached 99% on 2026-11-09
        // with twelve slots all used within the hour, none eligible).
        push(d.clone(), Kind::CacheSlot);
    }
    // A job namespace's other entries (`mono-publish/platform-api`,
    // `mono-publish/bazel`, `mono-ci/hawk-shared`: 131 GB on borg-deploy-2
    // that no root covered) are slots of that cache too.
    for ns in namespaces {
        push(ns.clone(), Kind::CacheSlot);
    }
    out
}

/// `<cache_root>/<namespace>` directories jobs create for themselves
/// (`platform-ci.yml` writes under `$BORG_CACHE_ROOT/mono-ci`, publish under
/// `mono-publish`): every directory under `cache_root` that is not one of
/// the drone's own managed cache kinds or its git mirrors.
fn namespace_roots(cfg: &DroneConfig) -> Vec<PathBuf> {
    let managed: BTreeSet<&str> = crate::drone::caches::kinds::Kind::ALL
        .iter()
        .map(|k| k.name())
        .chain(std::iter::once("git"))
        .collect();
    let mut out: Vec<PathBuf> = std::fs::read_dir(&cfg.cache_root)
        .map(|it| {
            it.filter_map(Result::ok)
                .filter(|e| e.path().is_dir())
                .filter(|e| {
                    let name = e.file_name();
                    let name = name.to_string_lossy();
                    !name.starts_with('.') && !managed.contains(name.as_ref())
                })
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// Subdirectories of a static runner user's home that its tools fill and
/// re-create on demand.
const RUNNER_USER_CACHES: &[&str] = &[".cache/go-build", ".npm", ".pnpm-store", "go/pkg/mod/cache"];

/// The roots a host's static GitHub runners give reclaim: each runner's
/// `_work` checkouts, its tool cache and `_diag` logs, the caches in its
/// user's home, and `/tmp` once there is any runner at all (internal-runner-
/// burst-01 reached 98% on 2026-11-09 with 6.9 GB of `_work/_tool`, 4.8 GB of
/// `go-build` and 5.6 GB of `_diag` per runner, none of them a root).
fn static_runner_roots(runners: &[crate::drone::credwipe::StaticRunner]) -> Vec<Root> {
    let mut out = Vec::new();
    for r in runners {
        out.push(Root {
            path: r.root.join("_work"),
            kind: Kind::RunnerWork,
        });
        out.push(Root {
            path: r.root.join("_work/_tool"),
            kind: Kind::RunnerToolCache,
        });
        out.push(Root {
            path: r.root.join("_diag"),
            kind: Kind::RunnerDiag,
        });
        if let Some(home) = &r.home {
            for c in RUNNER_USER_CACHES {
                out.push(Root {
                    path: home.join(c),
                    kind: Kind::RunnerUserCache,
                });
            }
        }
    }
    if !runners.is_empty() {
        out.push(Root {
            path: PathBuf::from("/tmp"),
            kind: Kind::TmpEntry,
        });
    }
    out
}

/// The CI cache slot key of lease slot `index`: `platform-ci.yml`
/// (`SET_CARGO_TARGET_DIR`) keys a job's retained build output by
/// `sha256(RUNNER_TOOL_CACHE)`, and the runner's tool cache is `_work/_tool`
/// under the runner root, which borg clones into the lease's slot directory.
/// So the slot a lease holds is `<cache_root>/<namespace>/slots/<this key>`,
/// and a reused lease slot keeps its cache slot. Checked against
/// borg-deploy-2's twelve slot directories on 2026-11-09.
pub(crate) fn cache_slot_key(work_root: &Path, index: usize) -> String {
    let tool_cache = format!("{}/_work/_tool", slot_dir(work_root, index).display());
    hex::encode(Sha256::digest(tool_cache.as_bytes()))
}

/// The slot (`<...>/slots/<key>`) that contains `path`, if any: the path up
/// to and including the component after a `slots` component.
fn slot_containing(path: &Path) -> Option<PathBuf> {
    let mut acc = PathBuf::new();
    let mut after_slots = false;
    for c in path.components() {
        acc.push(c);
        if after_slots {
            return Some(acc);
        }
        after_slots = c.as_os_str() == "slots";
    }
    None
}

/// The slot key of the slot that contains `path`, if any.
fn slot_key_of(path: &Path) -> Option<String> {
    slot_containing(path).and_then(|s| s.file_name().map(|n| n.to_string_lossy().into_owned()))
}

/// Name of the file a drone touches in a slot when a lease that holds it
/// starts and ends; its modification time is the slot's last use
/// (`NEWEST_SLOT` reads it by the same name).
const SLOT_USE_STAMP: &str = ".borg-last-used";

/// `<cache_root>/<namespace>/slots` directories: the per-job CI cache slots
/// whose `cargo-target/<week>` directories reclaim may remove. Namespaces are
/// stable, so new slots under them are found on every scan.
fn ci_slot_roots(cfg: &DroneConfig) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(&cfg.cache_root)
        .map(|it| {
            it.filter_map(Result::ok)
                .map(|e| e.path().join("slots"))
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// Home directories on this host: every directory under `/home` plus the job
/// user's home.
fn host_homes(cfg: &DroneConfig) -> Vec<PathBuf> {
    let mut homes: Vec<PathBuf> = std::fs::read_dir("/home")
        .map(|it| {
            it.filter_map(Result::ok)
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default();
    if let Some(u) = &cfg.job_user {
        homes.push(u.home.clone());
    }
    homes.sort();
    homes.dedup();
    homes
}

/// Runs reclaim commands, as the owner of the path they act on.
pub struct Commands {
    exec: Arc<dyn Exec>,
    setpriv: String,
    self_uid: Option<String>,
}

impl Commands {
    pub(crate) fn new(exec: Arc<dyn Exec>, setpriv: String) -> Self {
        // Real uid of this process (`Uid:` line of /proc/self/status).
        let self_uid = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|t| {
                t.lines()
                    .find_map(|l| l.strip_prefix("Uid:"))
                    .and_then(|v| v.split_whitespace().next())
                    .map(str::to_owned)
            });
        Self {
            exec,
            setpriv,
            self_uid,
        }
    }

    /// `uid:gid` of `path`, or of its nearest ancestor the drone may stat: a
    /// private home (mode 750) hides everything below it from the drone, and
    /// its owner is the one who can see inside. A wrong guess only makes the
    /// owner's commands fail, which keeps the directory.
    fn owner(&self, path: &Path) -> Result<(String, String), String> {
        let mut at = Some(path);
        while let Some(p) = at {
            if let Ok(o) = self.stat_owner(p) {
                return Ok(o);
            }
            at = p.parent();
        }
        Err(format!("stat {}: no readable ancestor", path.display()))
    }

    fn stat_owner(&self, path: &Path) -> Result<(String, String), String> {
        let p = path.to_string_lossy();
        let out = self
            .exec
            .run("stat", &["-c", "%u:%g", "--", &p])
            .map_err(|e| format!("stat {p}: {e}"))?;
        if !out.success {
            return Err(format!("stat {p} failed"));
        }
        match out.stdout.split_once(':') {
            Some((u, g)) if !u.is_empty() && !g.is_empty() => Ok((u.to_owned(), g.to_owned())),
            _ => Err(format!("stat {p}: unexpected {:?}", out.stdout)),
        }
    }

    /// Run `argv` as the owner of `path`, under a timeout. `Ok` carries
    /// stdout of a successful run; a non-zero exit or a timeout is `Err`.
    fn run_as_owner(&self, path: &Path, secs: u32, argv: &[&str]) -> Result<String, String> {
        self.run_as_owner_of(path, secs, argv)
    }

    /// Run `argv` as the owner of `anchor` (not of the paths in `argv`):
    /// for paths below a directory whose owner is trusted while the paths
    /// themselves may be symlinks a job planted.
    fn run_as_owner_of(&self, anchor: &Path, secs: u32, argv: &[&str]) -> Result<String, String> {
        let (uid, gid) = self.owner(anchor)?;
        let secs = secs.to_string();
        let mut full: Vec<&str> = vec!["-k", "10", &secs];
        if self.self_uid.as_deref() != Some(uid.as_str()) {
            full.extend([
                self.setpriv.as_str(),
                "--reuid",
                &uid,
                "--regid",
                &gid,
                "--clear-groups",
                "--inh-caps=-all",
                "--ambient-caps=-all",
                "--bounding-set=-all",
                "--",
            ]);
        }
        full.extend([
            "env",
            "GIT_CONFIG_GLOBAL=/dev/null",
            "GIT_CONFIG_NOSYSTEM=1",
            "GIT_OPTIONAL_LOCKS=0",
            "GIT_TERMINAL_PROMPT=0",
        ]);
        full.extend_from_slice(argv);
        let out = self
            .exec
            .run("timeout", &full)
            .map_err(|e| format!("{}: {e}", argv.join(" ")))?;
        if out.success {
            Ok(out.stdout)
        } else {
            Err(format!("`{}` failed or timed out", argv.join(" ")))
        }
    }

    /// `sh -c script _ path` as the owner of `anchor`.
    pub(crate) fn sh_as(
        &self,
        anchor: &Path,
        path: &Path,
        secs: u32,
        script: &str,
    ) -> Result<String, String> {
        let p = path.to_string_lossy();
        self.run_as_owner_of(anchor, secs, &["sh", "-c", script, "reclaim", &p])
    }

    /// The `uid` of `path`'s owner (or nearest stat-able ancestor's).
    pub(crate) fn owner_uid(&self, path: &Path) -> Result<String, String> {
        self.owner(path).map(|(u, _)| u)
    }

    pub(crate) fn exec(&self) -> &dyn Exec {
        self.exec.as_ref()
    }

    /// `sh -c script _ path` as the owner of `path`.
    fn sh(&self, path: &Path, secs: u32, script: &str) -> Result<String, String> {
        let p = path.to_string_lossy();
        self.run_as_owner(path, secs, &["sh", "-c", script, "reclaim", &p])
    }

    fn git(&self, wt: &Path, args: &[&str]) -> Result<String, String> {
        let p = wt.to_string_lossy();
        let mut argv = vec!["git", "-c", "core.bare=false", "-C", &p];
        argv.extend_from_slice(args);
        self.run_as_owner(wt, PROBE_SECS, &argv)
    }

    fn disk(&self, path: &Path) -> Result<(Disk, String), String> {
        let p = path.to_string_lossy();
        let out = self
            .exec
            .run("df", &["-Pk", "--", &p])
            .map_err(|e| format!("df {p}: {e}"))?;
        // Inside a private home (mode 700, as the static runner homes on
        // internal-runner-burst-01) the drone cannot even stat the path, so
        // `df` prints nothing; ask as the owner, like every other probe.
        let stdout = if out.success && out.stdout.lines().nth(1).is_some() {
            out.stdout
        } else {
            self.run_as_owner(path, PROBE_SECS, &["df", "-Pk", "--", &p])?
        };
        Self::parse_df(&stdout)
    }

    fn parse_df(stdout: &str) -> Result<(Disk, String), String> {
        let line = stdout.lines().nth(1).ok_or("df: no output")?;
        let f: Vec<&str> = line.split_whitespace().collect();
        let num = |i: usize| -> Result<u64, String> {
            f.get(i)
                .and_then(|v| v.parse::<u64>().ok())
                .map(|k| k.saturating_mul(1024))
                .ok_or_else(|| format!("df: unparsable {line:?}"))
        };
        let mount = f.get(5).ok_or("df: no mount point")?.to_string();
        Ok((
            Disk {
                total_bytes: num(1)?,
                free_bytes: num(3)?,
            },
            mount,
        ))
    }
}

/// Newest modification time (unix secs) of anything at most three levels
/// below `$1`, skipping dependency and build trees inside worktrees.
const NEWEST_WORKTREE: &str = r#"find "$1" -maxdepth 3 \( -name target -o -name node_modules -o -name .git \) -prune -o -printf '%T@\n' 2>/dev/null | sort -n | tail -1"#;
const NEWEST_ANY: &str = r#"find "$1" -maxdepth 3 -printf '%T@\n' 2>/dev/null | sort -n | tail -1"#;
/// Worktrees: directories one or two levels below the root that hold `.git`.
const LIST_WORKTREES: &str =
    r#"find "$1" -mindepth 2 -maxdepth 3 -name .git -printf '%h\n' 2>/dev/null; echo .done"#;
/// Cargo target directories (cargo writes `.rustc_info.json` at the root of
/// every target directory) one or two levels below the root.
/// Target directories (those holding `.rustc_info.json`) up to three levels
/// below a root; CI slot caches keep them at `<slots>/<slot>/cargo-target/<week>`.
const LIST_TARGETS: &str = r#"find "$1" -mindepth 2 -maxdepth 4 -name .rustc_info.json -printf '%h\n' 2>/dev/null; echo .done"#;
/// `incremental/` directories of a target directory (host and per-triple).
/// A static runner's checkouts: `_work/<repo>`. The runner's own `_tool`,
/// `_actions`, `_temp`, `_update` and `_PipelineMapping` stay.
const LIST_RUNNER_WORK: &str = r#"find "$1" -mindepth 1 -maxdepth 1 -type d ! -name '_*' -printf '%p\n' 2>/dev/null; echo .done"#;
/// Build-cache slots: each directory under a `<ns>/slots` root, or each entry
/// of a namespace root other than its `slots` directory.
const LIST_SLOTS: &str = r#"find "$1" -mindepth 1 -maxdepth 1 -type d ! -name slots ! -name '.*' -printf '%p\n' 2>/dev/null; echo .done"#;
/// A slot's last use: the drone's stamp when it has one, else the newest
/// modification time inside it.
const NEWEST_SLOT: &str = r#"if [ -e "$1/.borg-last-used" ]; then stat -c %Y -- "$1/.borg-last-used"; else find "$1" -maxdepth 3 -printf '%T@\n' 2>/dev/null | sort -n | tail -1; fi"#;
/// A root that is itself the candidate (a runner's tool cache, a user cache,
/// a `_diag` directory).
const LIST_SELF: &str = r#"[ -d "$1" ] && printf '%s\n' "$1"; echo .done"#;
/// Regenerable temp entries CI tools leave in `/tmp`. An allowlist, not
/// everything old: a `/tmp` entry of unknown origin may be a socket
/// directory or a lock a daemon owns.
const LIST_TMP: &str = r#"find "$1" -mindepth 1 -maxdepth 1 \( -type d \( -name 'go-build*' -o -name 'getter*' -o -name 'trivy-*' -o -name 'rustdoctest*' -o -name 'maestro-pack-smoke-*' -o -name 'cargo-install*' -o -name '.*.node-gyp' \) -o -type f -name 'actions-runner-linux-x64-*.tar.gz' \) -printf '%p\n' 2>/dev/null; echo .done"#;
/// `find` over a `_diag` directory's files older than `age_secs`, ending in
/// `action` (`-printf '%s\n'` to size them, `-delete` to remove them).
fn diag_old_files(age_secs: u64, action: &str) -> String {
    format!(r#"find "$1" -type f -mmin +{} {action}"#, age_secs / 60)
}

/// Age of the oldest log file in a `_diag` directory (unix secs of its
/// modification), so the candidate's idle age says whether anything is
/// old enough to go.
const DIAG_OLDEST: &str = r#"find "$1" -type f -printf '%T@\n' 2>/dev/null | sort -n | head -1"#;
/// Per-slot tool homes: `<slots>/<slot>/mise-home`.
const LIST_CACHE_DIRS: &str = r#"find "$1" -mindepth 2 -maxdepth 2 -type d -name mise-home -printf '%p\n' 2>/dev/null; echo .done"#;
const LIST_INCREMENTAL: &str = r#"find "$1" -mindepth 2 -maxdepth 3 -type d -name incremental -prune -printf '%p\n' 2>/dev/null; echo .done"#;
/// Every cwd and open file of the processes the caller may inspect.
const OPEN_PATHS: &str = r#"find /proc -mindepth 2 -maxdepth 3 \( -path '/proc/[0-9]*/cwd' -o -path '/proc/[0-9]*/fd/*' \) -printf '%l\n' 2>/dev/null; echo .done"#;

fn listed(out: &str) -> Result<Vec<PathBuf>, String> {
    let mut lines: Vec<&str> = out.lines().collect();
    if lines.pop() != Some(".done") {
        return Err("listing did not complete".into());
    }
    let mut paths: Vec<PathBuf> = lines
        .into_iter()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect();
    paths.sort();
    paths.dedup();
    // A directory inside another candidate is part of it.
    let mut out: Vec<PathBuf> = Vec::new();
    for p in paths {
        if !out.iter().any(|o| p.starts_with(o)) {
            out.push(p);
        }
    }
    Ok(out)
}

/// The live [`ReclaimHost`] for one filesystem's roots.
pub struct LiveHost<'a> {
    cmd: &'a Commands,
    /// Any path on the filesystem the group shares.
    anchor: PathBuf,
    now: i64,
    /// Below this idle age a non-slot candidate is never eligible, so its size
    /// is not measured. Critical mode can evict a cache slot at any age.
    size_floor_secs: u64,
    sizes: &'a Mutex<HashMap<PathBuf, (i64, u64)>>,
    /// Common git dir of each worktree probed, for `worktree prune`.
    common: Mutex<HashMap<PathBuf, PathBuf>>,
    /// The queen's disk admission floor (0 when unknown).
    floor: u64,
    /// Leases the drone is running now: a CI slot's tool home or a shared
    /// namespace cache is busy while any lease runs (its tools execute from
    /// there without holding a file open, so the process scan cannot see them).
    leases: &'a Mutex<HashSet<String>>,
    /// Authoritative lease-slot ownership, read again at every deletion probe.
    slots: &'a Slots,
    work_root: &'a Path,
    /// `_diag` log files older than this are what a `runner_diag` candidate
    /// measures and removes.
    diag_max_age_secs: u64,
}

impl LiveHost<'_> {
    fn newest(&self, path: &Path, kind: Kind) -> Option<i64> {
        let script = match kind {
            Kind::Worktree => NEWEST_WORKTREE,
            Kind::CacheSlot => NEWEST_SLOT,
            _ => NEWEST_ANY,
        };
        let out = self.cmd.sh(path, PROBE_SECS, script).ok()?;
        out.trim()
            .split('.')
            .next()
            .and_then(|s| s.parse::<i64>().ok())
    }

    fn candidate(
        &self,
        path: PathBuf,
        kind: Kind,
        within: Option<PathBuf>,
        cache: Option<PathBuf>,
    ) -> Option<Candidate> {
        if kind == Kind::RunnerDiag {
            return self.diag_candidate(path);
        }
        let newest = self.newest(&path, kind)?;
        let idle_secs = u64::try_from(self.now.saturating_sub(newest)).unwrap_or(0);
        let size_bytes = if kind != Kind::CacheSlot && idle_secs < self.size_floor_secs {
            0
        } else {
            let cached = self
                .sizes
                .lock()
                .ok()
                .and_then(|m| m.get(&path).copied())
                .filter(|(at, _)| *at == newest)
                .map(|(_, s)| s);
            match cached {
                Some(s) => s,
                None => {
                    let out = self
                        .cmd
                        .sh(&path, PROBE_SECS, r#"du -sxk -- "$1" | cut -f1"#)
                        .ok()?;
                    let s = out.trim().parse::<u64>().ok()?.saturating_mul(1024);
                    if let Ok(mut m) = self.sizes.lock() {
                        m.insert(path.clone(), (newest, s));
                    }
                    s
                }
            }
        };
        Some(Candidate {
            path,
            kind,
            size_bytes,
            idle_secs,
            within,
            cache,
        })
    }

    /// A `_diag` directory: idle is the age of its oldest log file and size
    /// the bytes of the files old enough to go. No file: no candidate.
    fn diag_candidate(&self, path: PathBuf) -> Option<Candidate> {
        let oldest: i64 = self
            .cmd
            .sh(&path, PROBE_SECS, DIAG_OLDEST)
            .ok()?
            .trim()
            .split('.')
            .next()?
            .parse()
            .ok()?;
        let idle_secs = u64::try_from(self.now.saturating_sub(oldest)).unwrap_or(0);
        let size_bytes = if idle_secs < self.diag_max_age_secs {
            0
        } else {
            let out = self
                .cmd
                .sh(
                    &path,
                    PROBE_SECS,
                    &diag_old_files(self.diag_max_age_secs, "-printf '%s\\n'"),
                )
                .ok()?;
            out.lines()
                .filter_map(|l| l.trim().parse::<u64>().ok())
                .sum()
        };
        Some(Candidate {
            path,
            kind: Kind::RunnerDiag,
            size_bytes,
            idle_secs,
            within: None,
            cache: None,
        })
    }

    /// Whether `path` lies inside a slot a running lease holds now.
    fn in_held_slot(&self, path: &Path) -> Result<bool, String> {
        let assignments = self
            .slots
            .try_assignments()
            .ok_or_else(|| "slot ownership unavailable".to_owned())?;
        let Some(key) = slot_key_of(path) else {
            return Ok(false);
        };
        Ok(assignments
            .into_iter()
            .any(|(index, _)| cache_slot_key(self.work_root, index) == key))
    }

    fn busy(&self, path: &Path) -> Result<bool, String> {
        let out = self.cmd.sh(path, PROBE_SECS, OPEN_PATHS)?;
        let mut lines: Vec<&str> = out.lines().collect();
        if lines.pop() != Some(".done") {
            return Err("process scan did not complete".into());
        }
        Ok(lines
            .iter()
            .any(|l| Path::new(l.trim_end_matches(" (deleted)")).starts_with(path)))
    }

    /// Whether the owner of `path` (a static runner's user) runs a job now.
    fn runner_job_running(&self, path: &Path) -> Option<bool> {
        let uid = self.cmd.owner_uid(path).ok()?;
        if uid == "0" || !uid.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let out = self
            .cmd
            .exec()
            .run(
                "sh",
                &[
                    "-c",
                    r#"pgrep -u "$1" -x Runner.Worker >/dev/null; echo $?"#,
                    "reclaim",
                    &uid,
                ],
            )
            .ok()?;
        match out.stdout.trim() {
            "0" => Some(true),
            "1" => Some(false),
            _ => None,
        }
    }

    fn probe_worktree(&self, path: &Path, s: &mut Safety) -> Result<(), String> {
        if !path.join(".git").exists() {
            s.not_git = true;
            return Ok(());
        }
        let rp = self.cmd.git(
            path,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--show-toplevel",
                "--git-dir",
                "--git-common-dir",
            ],
        )?;
        let parts: Vec<&str> = rp.lines().collect();
        let [top, git_dir, common] = parts[..] else {
            return Err(format!("rev-parse: unexpected {rp:?}"));
        };
        if Path::new(top) != path {
            s.not_git = true;
            return Ok(());
        }
        if git_dir == common {
            s.protected = Some("main checkout".into());
            return Ok(());
        }
        if Path::new(git_dir).join("locked").exists() {
            s.protected = Some("locked worktree".into());
            return Ok(());
        }
        if !self.cmd.git(path, &["status", "--porcelain"])?.is_empty() {
            s.dirty = true;
            return Ok(());
        }
        if !self
            .cmd
            .git(path, &["rev-list", "-n", "1", "HEAD", "--not", "--remotes"])?
            .is_empty()
        {
            s.unpushed = true;
            return Ok(());
        }
        if let Ok(mut m) = self.common.lock() {
            m.insert(path.to_owned(), PathBuf::from(common));
        }
        Ok(())
    }
}

impl ReclaimHost for LiveHost<'_> {
    fn disk(&self) -> Result<Disk, String> {
        self.cmd.disk(&self.anchor).map(|(d, _)| d)
    }

    fn admission_floor_bytes(&self) -> u64 {
        self.floor
    }

    fn scan(&self, roots: &[Root]) -> Result<Vec<Candidate>, String> {
        let mut out = Vec::new();
        for root in roots {
            let script = match root.kind {
                Kind::Worktree => LIST_WORKTREES,
                Kind::CacheDir => LIST_CACHE_DIRS,
                Kind::RunnerWork => LIST_RUNNER_WORK,
                Kind::CacheSlot => LIST_SLOTS,
                Kind::RunnerToolCache | Kind::RunnerUserCache | Kind::RunnerDiag => LIST_SELF,
                Kind::TmpEntry => LIST_TMP,
                _ => LIST_TARGETS,
            };
            // A slot's cache is its namespace: the root itself, or the
            // parent of a `slots` root.
            let cache = (root.kind == Kind::CacheSlot).then(|| {
                if root.path.file_name().is_some_and(|n| n == "slots") {
                    root.path
                        .parent()
                        .map_or_else(|| root.path.clone(), Path::to_path_buf)
                } else {
                    root.path.clone()
                }
            });
            let found = match self
                .cmd
                .sh(&root.path, PROBE_SECS, script)
                .and_then(|o| listed(&o))
            {
                Ok(found) => found,
                Err(e) => {
                    tracing::warn!("reclaim: cannot list {}: {e}", root.path.display());
                    continue;
                }
            };
            for p in found {
                if root.kind == Kind::CargoTarget {
                    let incr = self
                        .cmd
                        .sh(&p, PROBE_SECS, LIST_INCREMENTAL)
                        .and_then(|o| listed(&o))
                        .unwrap_or_default();
                    for i in incr {
                        if let Some(c) =
                            self.candidate(i, Kind::CargoIncremental, Some(p.clone()), None)
                        {
                            out.push(c);
                        }
                    }
                }
                // A target or tool home inside a slot is within that slot,
                // so the slot's own deletion is credited net of it.
                let within = (root.kind != Kind::CacheSlot)
                    .then(|| slot_containing(&p).filter(|s| *s != p))
                    .flatten();
                if let Some(c) = self.candidate(p, root.kind, within, cache.clone()) {
                    out.push(c);
                }
            }
        }
        Ok(out)
    }

    fn probe(&self, c: &Candidate) -> Safety {
        let mut s = Safety::default();
        if !c.path.exists() {
            s.unknown = Some("gone".into());
            return s;
        }
        if c.kind == Kind::RunnerDiag {
            // Only log files older than the age go; the runner writes a new
            // log per job and holds only the current ones open.
            return s;
        }
        if matches!(
            c.kind,
            Kind::RunnerWork | Kind::RunnerToolCache | Kind::RunnerUserCache
        ) {
            // The runner's job may sit between steps with no process inside
            // `_work`; its `Runner.Worker` lives for the whole job.
            match self.runner_job_running(&c.path) {
                Some(false) => {}
                Some(true) => {
                    s.busy = true;
                    return s;
                }
                None => {
                    s.unknown = Some("cannot tell whether the runner has a job".into());
                    return s;
                }
            }
        }
        match self.in_held_slot(&c.path) {
            Ok(false) => {}
            Ok(true) => {
                s.busy = true;
                return s;
            }
            Err(e) => {
                s.unknown = Some(e);
                return s;
            }
        }
        if c.kind == Kind::CacheDir || (c.kind == Kind::CacheSlot && slot_key_of(&c.path).is_none())
        {
            match self.leases.lock() {
                Ok(l) if l.is_empty() => {}
                Ok(_) => {
                    s.busy = true;
                    return s;
                }
                Err(_) => {
                    s.unknown = Some("lease table unavailable".into());
                    return s;
                }
            }
        }
        match self.busy(&c.path) {
            Ok(true) => {
                s.busy = true;
                return s;
            }
            Ok(false) => {}
            Err(e) => {
                s.unknown = Some(e);
                return s;
            }
        }
        if c.kind == Kind::Worktree
            && let Err(e) = self.probe_worktree(&c.path, &mut s)
        {
            s.unknown = Some(e);
        }
        s
    }

    fn delete(&self, c: &Candidate) -> Result<u64, String> {
        if c.kind == Kind::RunnerDiag {
            self.cmd.sh(
                &c.path,
                DELETE_SECS,
                &diag_old_files(self.diag_max_age_secs, "-delete"),
            )?;
            return Ok(c.size_bytes);
        }
        let common = if c.kind == Kind::Worktree {
            Some(
                self.common
                    .lock()
                    .ok()
                    .and_then(|m| m.get(&c.path).cloned())
                    .ok_or("worktree was not probed")?,
            )
        } else {
            None
        };
        self.cmd.sh(&c.path, DELETE_SECS, r#"rm -rf -- "$1""#)?;
        if let Some(common) = common {
            // Drop the worktree's admin entry; the commits stay in the repo.
            let _ = self.cmd.run_as_owner(
                &common,
                PROBE_SECS,
                &[
                    "git",
                    "--git-dir",
                    &common.to_string_lossy(),
                    "worktree",
                    "prune",
                ],
            );
        }
        if let Ok(mut m) = self.sizes.lock() {
            m.remove(&c.path);
        }
        Ok(c.size_bytes)
    }
}

fn severity(m: Mode) -> u8 {
    match m {
        Mode::Idle => 0,
        Mode::Hygiene => 1,
        Mode::Pressure => 2,
        Mode::Critical => 3,
    }
}

/// Fold one filesystem's cycle into the combined cycle of a run.
fn merge(into: &mut Option<Cycle>, c: Cycle) {
    let Some(acc) = into else {
        *into = Some(c);
        return;
    };
    if severity(c.mode) > severity(acc.mode) {
        acc.mode = c.mode;
    }
    acc.usage_pct_before = acc.usage_pct_before.max(c.usage_pct_before);
    acc.usage_pct_after = acc.usage_pct_after.max(c.usage_pct_after);
    // Report the tightest filesystem: the least free space above (or the
    // most below) its target of twice the floor.
    let headroom = |c: &Cycle| i128::from(c.free_bytes_before) - 2 * i128::from(c.floor_bytes);
    if headroom(&c) < headroom(&*acc) {
        acc.free_bytes_before = c.free_bytes_before;
        acc.floor_bytes = c.floor_bytes;
    }
    acc.records.extend(c.records);
    for (k, v) in c.kept {
        *acc.kept.entry(k).or_default() += v;
    }
    for (k, v) in c.evicted {
        *acc.evicted.entry(k).or_default() += v;
    }
    acc.reclaimed_bytes = acc.reclaimed_bytes.saturating_add(c.reclaimed_bytes);
    acc.would_reclaim_bytes = acc
        .would_reclaim_bytes
        .saturating_add(c.would_reclaim_bytes);
    acc.errors += c.errors;
}

/// The drone's reclaim loop and the stats its heartbeat reports.
pub struct Reclaimer {
    settings: ReclaimSettings,
    roots: Vec<Root>,
    /// The drone's own job filesystems: always measured, so every drone
    /// reports usage and a recent cycle even with no build roots.
    anchors: Vec<PathBuf>,
    cmd: Commands,
    docker_bin: String,
    /// Leases the drone is running (shared with the drone).
    leases: Arc<Mutex<HashSet<String>>>,
    /// The lease slots (shared with the drone): which are in use says which
    /// cache slots a live lease holds.
    slots: Arc<Slots>,
    work_root: PathBuf,
    cache_root: PathBuf,
    /// The queen's disk admission floor (shared with the drone).
    floor: Arc<AtomicU64>,
    stats: Mutex<ReclaimStats>,
    seq: Mutex<u64>,
    sizes: Mutex<HashMap<PathBuf, (i64, u64)>>,
    /// Woken when disk admission is blocked, so a cycle runs now instead of
    /// at the next tick.
    kick: tokio::sync::Notify,
    /// When the last cycle finished (unix secs); a kick within
    /// [`KICK_MIN_GAP_SECS`] of it is folded into the next tick.
    last_run: AtomicU64,
}

/// Shortest gap between a kicked cycle and the one before it.
const KICK_MIN_GAP_SECS: u64 = 60;

impl Reclaimer {
    /// `None` when reclaim is off or the host is not Linux.
    pub(super) fn new(
        cfg: &DroneConfig,
        exec: Arc<dyn Exec>,
        leases: Arc<Mutex<HashSet<String>>>,
        slots: Arc<Slots>,
        floor: Arc<AtomicU64>,
    ) -> Option<Arc<Self>> {
        let settings = cfg.reclaim.clone();
        if !settings.enabled {
            return None;
        }
        if !cfg!(target_os = "linux") {
            tracing::info!("reclaim: not supported on this OS; off");
            return None;
        }
        let setpriv = cfg
            .job_user
            .as_ref()
            .map_or_else(|| "setpriv".to_owned(), |u| u.priv_bin.clone());
        let cmd = Commands::new(exec, setpriv);
        let roots = if settings.roots.is_empty() {
            // Checked as the directory's owner: a private home hides its
            // build directories from the drone itself.
            let mut roots = default_roots(
                &host_homes(cfg),
                &ci_slot_roots(cfg),
                &namespace_roots(cfg),
                |p| cmd.sh(p, PROBE_SECS, r#"test -d "$1""#).is_ok(),
            );
            // Static GitHub runners sharing the host (internal-runner-burst-01
            // ran eight, 14-21 GB of `_work` each on 2026-11-01).
            if settings.static_runner_work {
                let runners = crate::drone::credwipe::static_runners(&cmd);
                for r in static_runner_roots(&runners) {
                    if cmd.sh(&r.path, PROBE_SECS, r#"test -d "$1""#).is_ok() {
                        roots.push(r);
                    }
                }
            }
            roots
        } else {
            settings.roots.clone()
        };
        // Audit sequence numbers must grow across drone restarts, because the
        // queen ignores a (drone, seq) it already has.
        let seq = u64::try_from(time::OffsetDateTime::now_utc().unix_timestamp())
            .unwrap_or(0)
            .saturating_mul(1000);
        tracing::info!(
            "reclaim: on (dry_run={}, docker={}), roots: {}",
            settings.dry_run,
            settings.docker.enabled,
            roots
                .iter()
                .map(|r| format!("{}={}", r.kind.as_str(), r.path.display()))
                .collect::<Vec<_>>()
                .join(", ")
        );
        Some(Arc::new(Self {
            stats: Mutex::new(ReclaimStats {
                enabled: true,
                dry_run: settings.dry_run,
                ..ReclaimStats::default()
            }),
            settings,
            roots,
            anchors: vec![cfg.work_root.clone(), cfg.cache_root.clone()],
            cmd,
            docker_bin: cfg.docker_bin.clone(),
            leases,
            slots,
            work_root: cfg.work_root.clone(),
            cache_root: cfg.cache_root.clone(),
            floor,
            seq: Mutex::new(seq),
            sizes: Mutex::new(HashMap::new()),
            kick: tokio::sync::Notify::new(),
            last_run: AtomicU64::new(0),
        }))
    }

    /// Run a cycle now (disk admission is blocked): the drone takes no job
    /// until free space is back above the floor, and the next tick may be
    /// ten minutes away.
    pub(super) fn kick(&self) {
        self.kick.notify_one();
    }

    /// Record that lease slot `index` is in use now: the drone touches the
    /// use stamp of the cache slot that lease holds under every namespace,
    /// as the slot's owner. Called when a lease starts and when it ends, so
    /// the stamp is the slot's last use and LRU eviction has a clock that
    /// does not depend on what the job happened to write.
    pub(super) async fn note_slot_use(self: &Arc<Self>, index: usize) {
        let me = Arc::clone(self);
        let _ = tokio::task::spawn_blocking(move || me.stamp_slot(index)).await;
    }

    fn stamp_slot(&self, index: usize) {
        let key = cache_slot_key(&self.work_root, index);
        let Ok(namespaces) = std::fs::read_dir(&self.cache_root) else {
            return;
        };
        for ns in namespaces.filter_map(Result::ok) {
            let slot = ns.path().join("slots").join(&key);
            if !slot.is_dir() {
                continue;
            }
            if let Err(e) = self.cmd.sh(
                &slot,
                PROBE_SECS,
                &format!(r#"touch -- "$1/{SLOT_USE_STAMP}""#),
            ) {
                tracing::debug!("reclaim: slot use stamp {}: {e}", slot.display());
            }
        }
    }

    /// Stats for the next heartbeat, with the audit entries not yet sent.
    pub(super) fn heartbeat_stats(&self) -> Option<ReclaimStats> {
        let mut s = self.stats.lock().ok()?;
        let mut out = s.clone();
        (out.recent, out.recent_ext) = s.drain_recent(RECENT_PER_HEARTBEAT);
        Some(out)
    }

    /// One reclaim run over every filesystem the roots live on.
    fn run_once(&self) {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let mut groups: BTreeMap<String, (PathBuf, Vec<Root>)> = BTreeMap::new();
        for r in &self.roots {
            match self.cmd.disk(&r.path) {
                Ok((_, mount)) => groups
                    .entry(mount)
                    .or_insert_with(|| (r.path.clone(), Vec::new()))
                    .1
                    .push(r.clone()),
                Err(e) => tracing::warn!("reclaim: {}: {e}", r.path.display()),
            }
        }
        for a in &self.anchors {
            if let Ok((_, mount)) = self.cmd.disk(a) {
                groups
                    .entry(mount)
                    .or_insert_with(|| (a.clone(), Vec::new()));
            }
        }
        let mut combined: Option<Cycle> = None;
        let mut mounts = BTreeSet::new();
        for (mount, (anchor, roots)) in groups {
            let host = LiveHost {
                cmd: &self.cmd,
                anchor,
                now,
                size_floor_secs: self.settings.critical_min_age_secs,
                sizes: &self.sizes,
                common: Mutex::new(HashMap::new()),
                floor: self.floor.load(Ordering::Relaxed),
                leases: &self.leases,
                slots: &self.slots,
                work_root: &self.work_root,
                diag_max_age_secs: self.settings.runner_diag_max_age_secs,
            };
            let settings = ReclaimSettings {
                roots,
                ..self.settings.clone()
            };
            let Ok(mut seq) = self.seq.lock() else { return };
            match run_cycle(&host, &settings, now, &mut seq) {
                Ok(c) => {
                    for r in &c.records {
                        tracing::info!(
                            "reclaim {:?}: {} ({}): {}{}",
                            r.outcome,
                            r.path,
                            r.kind.as_str(),
                            r.reason,
                            r.error
                                .as_deref()
                                .map(|e| format!("; {e}"))
                                .unwrap_or_default()
                        );
                    }
                    tracing::info!(
                        "reclaim cycle {mount}: mode={:?} usage {}% -> {}%, deleted {} MiB, would delete {} MiB, errors {}, kept {:?}",
                        c.mode,
                        c.usage_pct_before,
                        c.usage_pct_after,
                        c.reclaimed_bytes >> 20,
                        c.would_reclaim_bytes >> 20,
                        c.errors,
                        c.kept
                    );
                    mounts.insert(mount);
                    merge(&mut combined, c);
                }
                Err(e) => tracing::warn!("reclaim cycle {mount} failed: {e}"),
            }
        }
        if let (Some(c), Ok(mut stats)) = (combined, self.stats.lock()) {
            stats.absorb(&self.settings, now, &c);
        }
        if self.settings.docker.enabled {
            self.run_docker(now);
        }
        self.last_run.store(
            u64::try_from(time::OffsetDateTime::now_utc().unix_timestamp()).unwrap_or(0),
            Ordering::Relaxed,
        );
    }

    /// The Docker tier, on the filesystem holding Docker's data root.
    fn run_docker(&self, now: i64) {
        let cli = DockerCli {
            exec: self.cmd.exec(),
            docker: &self.docker_bin,
        };
        if !docker_reclaim::available(&cli) {
            return;
        }
        let root = docker_reclaim::root_path(&cli);
        let Ok((disk, mount)) = self.cmd.disk(&root) else {
            return;
        };
        let held: HashSet<String> = match self.leases.lock() {
            Ok(l) => l.clone(),
            Err(_) => return,
        };
        let input = docker_reclaim::Inputs {
            settings: &self.settings,
            disk,
            floor_bytes: self.floor.load(Ordering::Relaxed),
            held: &held,
            now,
        };
        let Ok(mut seq) = self.seq.lock() else { return };
        let measure = || self.cmd.disk(&root).ok().map(|(d, _)| d);
        match docker_reclaim::run(&cli, &input, measure, &mut seq) {
            Ok(c) => {
                for r in &c.records {
                    tracing::info!(
                        "reclaim {:?}: {} ({}): {}{}",
                        r.outcome,
                        r.path,
                        r.kind.as_str(),
                        r.reason,
                        r.error
                            .as_deref()
                            .map(|e| format!("; {e}"))
                            .unwrap_or_default()
                    );
                }
                tracing::info!(
                    "reclaim docker {mount}: mode={:?} usage {}% -> {}%, freed {} MiB, would free {} MiB, errors {}, kept {:?}",
                    c.mode,
                    c.usage_pct_before,
                    c.usage_pct_after,
                    c.reclaimed_bytes >> 20,
                    c.would_reclaim_bytes >> 20,
                    c.errors,
                    c.kept
                );
                if let Ok(mut stats) = self.stats.lock() {
                    stats.absorb_docker(&c);
                }
            }
            Err(e) => tracing::warn!("reclaim docker {mount} failed: {e}"),
        }
    }

    /// Run a cycle shortly after start and then every `interval_secs`, or
    /// sooner when [`Self::kick`] says admission is blocked.
    pub(super) fn spawn(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let me = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let mut every =
                tokio::time::interval(Duration::from_secs(me.settings.interval_secs.max(60)));
            every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                // Both arms are cancel-safe: a tick or a notification that
                // loses the race is kept for the next iteration.
                let kicked = tokio::select! {
                    _ = every.tick() => false,
                    () = me.kick.notified() => true,
                };
                if kicked {
                    let now = u64::try_from(time::OffsetDateTime::now_utc().unix_timestamp())
                        .unwrap_or(0);
                    if now.saturating_sub(me.last_run.load(Ordering::Relaxed)) < KICK_MIN_GAP_SECS {
                        continue;
                    }
                }
                let run = Arc::clone(&me);
                if let Err(e) = tokio::task::spawn_blocking(move || run.run_once()).await {
                    tracing::warn!("reclaim run panicked: {e}");
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_roots_cover_existing_home_and_shared_build_dirs_only() {
        let present: BTreeSet<PathBuf> = [
            "/home/dev/worktrees",
            "/home/dev/.cache",
            "/home/dev/slot-targets",
            "/srv/devdata/worktrees",
            "/srv/devdata/slots",
            "/var/lib/borg-job/cache/mono-ci/slots",
            "/var/lib/borg-job/cache/mono-ci",
            "/var/lib/borg-job/cache/mono-publish",
        ]
        .into_iter()
        .map(PathBuf::from)
        .collect();
        let roots = default_roots(
            &[PathBuf::from("/home/dev")],
            &[PathBuf::from("/var/lib/borg-job/cache/mono-ci/slots")],
            &[
                PathBuf::from("/var/lib/borg-job/cache/mono-ci"),
                PathBuf::from("/var/lib/borg-job/cache/mono-publish"),
            ],
            |p| present.contains(p),
        );
        let got: Vec<(String, &str)> = roots
            .iter()
            .map(|r| (r.path.display().to_string(), r.kind.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("/home/dev/worktrees".to_owned(), "worktree"),
                ("/home/dev/slot-targets".to_owned(), "cargo_target"),
                ("/home/dev/.cache".to_owned(), "cargo_target"),
                ("/srv/devdata/worktrees".to_owned(), "worktree"),
                ("/srv/devdata/slots".to_owned(), "cargo_target"),
                ("/srv/devdata/worktrees".to_owned(), "cargo_target"),
                (
                    "/var/lib/borg-job/cache/mono-ci/slots".to_owned(),
                    "cargo_target"
                ),
                (
                    "/var/lib/borg-job/cache/mono-ci/slots".to_owned(),
                    "cache_dir"
                ),
                (
                    "/var/lib/borg-job/cache/mono-ci/slots".to_owned(),
                    "cache_slot"
                ),
                ("/var/lib/borg-job/cache/mono-ci".to_owned(), "cache_slot"),
                (
                    "/var/lib/borg-job/cache/mono-publish".to_owned(),
                    "cache_slot"
                ),
            ]
        );
    }

    /// The key `platform-ci.yml` derives from `RUNNER_TOOL_CACHE`
    /// (`sha256sum` of the path, no newline), as the slot directories on
    /// borg-deploy-2 showed for lease slots 0 and 11 (work root
    /// `/var/lib/borg-job/work`).
    #[test]
    fn cache_slot_key_is_the_ci_workflow_s_sha256_of_the_runner_tool_cache() {
        let work = Path::new("/var/lib/borg-job/work");
        assert_eq!(
            cache_slot_key(work, 0),
            "717a4f03b9a1252141ee84e5c48ef23629e03521a7dc5f21f579e422f0a904b4"
        );
        assert_eq!(
            cache_slot_key(work, 11),
            "a91e0ad44e01ff286fa99de3ddddc5901e9fd4cd044294c502fe7ea7f513ecf3"
        );
        let slot = PathBuf::from("/var/lib/borg-job/cache/mono-ci/slots/abc");
        assert_eq!(
            slot_containing(&slot.join("cargo-target/2026-W46")),
            Some(slot.clone())
        );
        assert_eq!(slot_containing(&slot), Some(slot.clone()));
        assert_eq!(slot_key_of(&slot.join("mise-home")).as_deref(), Some("abc"));
        assert_eq!(
            slot_containing(Path::new("/var/lib/borg-job/cache/mono-ci/slots")),
            None
        );
        assert_eq!(
            slot_containing(Path::new("/var/lib/borg-job/cache/mono-publish/bazel")),
            None
        );
    }

    /// Each slot under a `slots` root and each entry of a namespace root is a
    /// candidate; the `slots` directory itself and dot entries are not. A
    /// slot's last use is its stamp when the drone left one, else what it
    /// holds.
    #[test]
    fn slot_listing_and_last_use_follow_the_stamp() {
        use crate::drone::exec::SystemExec;
        let tmp = tempfile::tempdir().unwrap();
        let ns = tmp.path().canonicalize().unwrap().join("mono-ci");
        for d in [
            "slots/aaa/cargo-target/2026-W46/debug",
            "slots/bbb/mise-home",
            "slots/.hidden",
            "hawk-shared/2026-W46",
            "tools/sccache",
        ] {
            std::fs::create_dir_all(ns.join(d)).unwrap();
        }
        let cmd = Commands::new(Arc::new(SystemExec), "setpriv".into());
        let slots = listed(&cmd.sh(&ns.join("slots"), 30, LIST_SLOTS).unwrap()).unwrap();
        assert_eq!(slots, vec![ns.join("slots/aaa"), ns.join("slots/bbb")]);
        let entries = listed(&cmd.sh(&ns, 30, LIST_SLOTS).unwrap()).unwrap();
        assert_eq!(entries, vec![ns.join("hawk-shared"), ns.join("tools")]);
        // Without a stamp the newest file inside dates the slot.
        let fresh = cmd.sh(&ns.join("slots/aaa"), 30, NEWEST_SLOT).unwrap();
        let fresh: i64 = fresh.trim().split('.').next().unwrap().parse().unwrap();
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        assert!((now - fresh).abs() < 120, "{fresh} vs {now}");
        // With a stamp, the stamp is the last use, whatever the files say.
        let stamp = ns.join("slots/aaa").join(SLOT_USE_STAMP);
        std::fs::write(&stamp, "").unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        std::fs::File::options()
            .write(true)
            .open(&stamp)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let stamped = cmd.sh(&ns.join("slots/aaa"), 30, NEWEST_SLOT).unwrap();
        assert_eq!(stamped.trim(), "1700000000");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fresh_cache_slots_are_sized_once_and_critical_stops_at_the_target() {
        use crate::drone::exec::{Output, SystemExec};
        use std::sync::atomic::AtomicUsize;

        struct CountDu(AtomicUsize);
        impl Exec for CountDu {
            fn run(&self, program: &str, args: &[&str]) -> std::io::Result<Output> {
                if args.iter().any(|a| a.contains("du -sxk")) {
                    self.0.fetch_add(1, Ordering::Relaxed);
                }
                SystemExec.run(program, args)
            }
        }

        struct TestHost<'a> {
            live: LiveHost<'a>,
            disk: Disk,
            floor: u64,
        }
        impl ReclaimHost for TestHost<'_> {
            fn disk(&self) -> Result<Disk, String> {
                Ok(self.disk)
            }
            fn admission_floor_bytes(&self) -> u64 {
                self.floor
            }
            fn scan(&self, roots: &[Root]) -> Result<Vec<Candidate>, String> {
                self.live.scan(roots)
            }
            fn probe(&self, c: &Candidate) -> Safety {
                self.live.probe(c)
            }
            fn delete(&self, c: &Candidate) -> Result<u64, String> {
                self.live.delete(c)
            }
        }

        const MIB: u64 = 1 << 20;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let slots_root = root.join("mono-ci/slots");
        for name in ["a", "b", "c"] {
            let slot = slots_root.join(name);
            std::fs::create_dir_all(&slot).unwrap();
            std::fs::write(slot.join("data"), vec![b'x'; (4 * MIB) as usize]).unwrap();
        }
        let exec = Arc::new(CountDu(AtomicUsize::new(0)));
        let cmd = Commands::new(exec.clone(), "setpriv".into());
        let sizes = Mutex::new(HashMap::new());
        let leases = Mutex::new(HashSet::new());
        let slots = Slots::new(4);
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let live = LiveHost {
            cmd: &cmd,
            anchor: root,
            now,
            size_floor_secs: 3600,
            sizes: &sizes,
            common: Mutex::new(HashMap::new()),
            floor: 0,
            leases: &leases,
            slots: &slots,
            work_root: Path::new("/w"),
            diag_max_age_secs: 86_400,
        };
        let roots = [Root {
            path: slots_root.clone(),
            kind: Kind::CacheSlot,
        }];
        let found = live.scan(&roots).unwrap();
        assert_eq!(found.len(), 3);
        assert!(found.iter().all(|c| c.idle_secs < 3600));
        assert!(found.iter().all(|c| c.size_bytes > 0));
        live.scan(&roots).unwrap();
        assert_eq!(exec.0.load(Ordering::Relaxed), 3, "repeat scans reuse du");

        let settings = ReclaimSettings {
            roots: roots.to_vec(),
            high_water_pct: 100,
            low_water_pct: 99,
            critical_pct: 100,
            idle_max_age_secs: 0,
            ..ReclaimSettings::default()
        };
        let ordered = crate::reclaim::order(&settings, Mode::Critical, 0, &found);
        let target_bytes = ordered
            .iter()
            .take(2)
            .map(|(c, _)| c.size_bytes)
            .sum::<u64>();
        let host = TestHost {
            live,
            disk: Disk {
                total_bytes: target_bytes * 4,
                free_bytes: target_bytes - 2,
            },
            floor: target_bytes - 1,
        };
        let cycle = run_cycle(&host, &settings, now, &mut 0).unwrap();
        assert_eq!(cycle.mode, Mode::Critical);
        assert_eq!(
            cycle.records.len(),
            2,
            "the measured target needs two slots"
        );
        assert_eq!(cycle.reclaimed_bytes, target_bytes);
        assert_eq!(std::fs::read_dir(slots_root).unwrap().count(), 1);
        assert_eq!(exec.0.load(Ordering::Relaxed), 3);
    }

    /// Each static runner gives its checkouts, tool cache, diagnostics and
    /// the caches of its user's home; `/tmp` once for the host; a runner
    /// with no known home gives no user caches; no runner, no roots.
    #[test]
    fn static_runner_roots_cover_each_runner_and_tmp_once() {
        use crate::drone::credwipe::StaticRunner;
        let runners = [
            StaticRunner {
                unit: "actions.runner.dx-corp.gce-internal-01-1.service".into(),
                root: "/home/runner1/actions-runner".into(),
                user: Some("runner1".into()),
                home: Some("/home/runner1".into()),
            },
            StaticRunner {
                unit: "actions.runner.dx-corp.gce-internal-01-2.service".into(),
                root: "/home/runner2/actions-runner".into(),
                user: None,
                home: None,
            },
        ];
        let got: Vec<(String, &str)> = static_runner_roots(&runners)
            .iter()
            .map(|r| (r.path.display().to_string(), r.kind.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (
                    "/home/runner1/actions-runner/_work".to_owned(),
                    "runner_work"
                ),
                (
                    "/home/runner1/actions-runner/_work/_tool".to_owned(),
                    "runner_tool_cache"
                ),
                (
                    "/home/runner1/actions-runner/_diag".to_owned(),
                    "runner_diag"
                ),
                (
                    "/home/runner1/.cache/go-build".to_owned(),
                    "runner_user_cache"
                ),
                ("/home/runner1/.npm".to_owned(), "runner_user_cache"),
                ("/home/runner1/.pnpm-store".to_owned(), "runner_user_cache"),
                (
                    "/home/runner1/go/pkg/mod/cache".to_owned(),
                    "runner_user_cache"
                ),
                (
                    "/home/runner2/actions-runner/_work".to_owned(),
                    "runner_work"
                ),
                (
                    "/home/runner2/actions-runner/_work/_tool".to_owned(),
                    "runner_tool_cache"
                ),
                (
                    "/home/runner2/actions-runner/_diag".to_owned(),
                    "runner_diag"
                ),
                ("/tmp".to_owned(), "tmp_entry"),
            ]
        );
        assert!(static_runner_roots(&[]).is_empty());
    }

    /// A `_diag` candidate is dated by its oldest log, sized by the logs
    /// older than the age, and its deletion removes only those: the current
    /// log stays. `/tmp` offers only the regenerable CI entries.
    #[test]
    fn diag_logs_go_by_age_only_and_tmp_offers_regenerable_entries_only() {
        use crate::drone::exec::SystemExec;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let diag = root.join("actions-runner/_diag");
        std::fs::create_dir_all(&diag).unwrap();
        std::fs::write(diag.join("Runner_old.log"), vec![b'x'; 1000]).unwrap();
        std::fs::write(diag.join("Worker_now.log"), vec![b'y'; 500]).unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        std::fs::File::options()
            .write(true)
            .open(diag.join("Runner_old.log"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        let cmd = Commands::new(Arc::new(SystemExec), "setpriv".into());
        let sizes = Mutex::new(HashMap::new());
        let leases = Mutex::new(HashSet::new());
        let slots = Slots::new(4);
        let host = LiveHost {
            cmd: &cmd,
            anchor: root.clone(),
            now: time::OffsetDateTime::now_utc().unix_timestamp(),
            size_floor_secs: 0,
            sizes: &sizes,
            common: Mutex::new(HashMap::new()),
            floor: 0,
            leases: &leases,
            slots: &slots,
            work_root: Path::new("/w"),
            diag_max_age_secs: 86_400,
        };
        let found = host
            .scan(&[Root {
                path: diag.clone(),
                kind: Kind::RunnerDiag,
            }])
            .unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        let c = &found[0];
        assert_eq!(
            (c.path.clone(), c.kind, c.size_bytes),
            (diag.clone(), Kind::RunnerDiag, 1000)
        );
        assert!(
            c.idle_secs > 86_400,
            "dated by the oldest log: {}",
            c.idle_secs
        );
        assert!(host.probe(c).blocker(c.kind).is_none(), "never busy");
        assert_eq!(host.delete(c).unwrap(), 1000);
        assert!(!diag.join("Runner_old.log").exists());
        assert!(
            diag.join("Worker_now.log").exists(),
            "the current log stays"
        );
        assert!(diag.exists(), "the directory stays");

        let t = root.join("tmp");
        for d in [
            "go-build123",
            "trivy-abc",
            "systemd-private-x",
            "tmux-1000",
            ".X11-unix",
        ] {
            std::fs::create_dir_all(t.join(d)).unwrap();
        }
        std::fs::write(t.join("actions-runner-linux-x64-2.3.tar.gz"), "").unwrap();
        std::fs::write(t.join("notes.txt"), "").unwrap();
        let listed_entries = listed(&cmd.sh(&t, 30, LIST_TMP).unwrap()).unwrap();
        assert_eq!(
            listed_entries,
            vec![
                t.join("actions-runner-linux-x64-2.3.tar.gz"),
                t.join("go-build123"),
                t.join("trivy-abc")
            ]
        );
    }

    /// A lease that starts while a long scan is paused owns its slot by the
    /// immediate pre-delete probe, even with no process inside the slot.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_slot_guard_acquired_after_scan_is_busy_at_the_predelete_probe() {
        use crate::drone::exec::SystemExec;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let work_root = Path::new("/w");
        let slots_root = root.join("mono-ci/slots");
        let key = cache_slot_key(work_root, 0);
        let slot = slots_root.join(&key);
        let other = slots_root.join(cache_slot_key(work_root, 1));
        std::fs::create_dir_all(slot.join("cargo-target/2026-W46")).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let cmd = Commands::new(Arc::new(SystemExec), "setpriv".into());
        let sizes = Mutex::new(HashMap::new());
        let leases = Mutex::new(HashSet::new());
        let slots = Slots::new(4);
        let host = LiveHost {
            cmd: &cmd,
            anchor: root,
            now: time::OffsetDateTime::now_utc().unix_timestamp(),
            size_floor_secs: 0,
            sizes: &sizes,
            common: Mutex::new(HashMap::new()),
            floor: 0,
            leases: &leases,
            slots: &slots,
            work_root,
            diag_max_age_secs: 86_400,
        };
        let found = host
            .scan(&[Root {
                path: slots_root,
                kind: Kind::CacheSlot,
            }])
            .unwrap();
        let held = found.iter().find(|c| c.path == slot).unwrap();
        let unrelated = found.iter().find(|c| c.path == other).unwrap();

        let pause = slots.acquire("paused-lease").unwrap();
        assert_eq!(pause.index(), 0);
        leases.lock().unwrap().insert("paused-lease".into());
        assert_eq!(
            host.probe(held),
            Safety {
                busy: true,
                ..Safety::default()
            }
        );
        let target = Candidate {
            path: slot.join("cargo-target/2026-W46"),
            kind: Kind::CargoTarget,
            size_bytes: 1,
            idle_secs: 0,
            within: None,
            cache: None,
        };
        assert!(host.probe(&target).busy);
        let free = host.probe(unrelated);
        assert!(!free.busy && free.unknown.is_none(), "{free:?}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_shared_publish_cache_is_busy_throughout_any_lease() {
        use crate::drone::exec::SystemExec;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let namespace = root.join("mono-publish");
        let publish = namespace.join("platform-api");
        std::fs::create_dir_all(publish.join("cargo-target")).unwrap();
        let cmd = Commands::new(Arc::new(SystemExec), "setpriv".into());
        let sizes = Mutex::new(HashMap::new());
        let leases = Mutex::new(HashSet::new());
        let slots = Slots::new(1);
        let host = LiveHost {
            cmd: &cmd,
            anchor: root,
            now: time::OffsetDateTime::now_utc().unix_timestamp(),
            size_floor_secs: 0,
            sizes: &sizes,
            common: Mutex::new(HashMap::new()),
            floor: 0,
            leases: &leases,
            slots: &slots,
            work_root: Path::new("/w"),
            diag_max_age_secs: 86_400,
        };
        let found = host
            .scan(&[Root {
                path: namespace,
                kind: Kind::CacheSlot,
            }])
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, publish);

        leases.lock().unwrap().insert("active-lease".into());
        assert_eq!(
            host.probe(&found[0]),
            Safety {
                busy: true,
                ..Safety::default()
            },
            "an active lease is busy without an open process in the cache"
        );
        leases.lock().unwrap().clear();
        let idle = host.probe(&found[0]);
        assert!(!idle.busy && idle.unknown.is_none(), "{idle:?}");
    }

    #[test]
    fn target_listing_finds_ci_slot_weekly_targets() {
        let root = std::env::temp_dir().join(format!("borg-reclaim-slots-{}", std::process::id()));
        let week = root.join("slot-a/cargo-target/2026-W43");
        std::fs::create_dir_all(week.join("debug")).unwrap();
        std::fs::write(week.join(".rustc_info.json"), "{}").unwrap();
        std::fs::create_dir_all(root.join("slot-a/mise-home/x")).unwrap();
        // The production path: Commands::sh through the reviewed SystemExec.
        let cmd = Commands::new(Arc::new(crate::drone::exec::SystemExec), "setpriv".into());
        let out = cmd.sh(&root, 30, LIST_TARGETS).unwrap();
        let found = listed(&out).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(found, vec![week]);
    }

    #[test]
    fn runner_work_listing_offers_checkouts_never_the_runner_s_own_dirs() {
        let root = std::env::temp_dir().join(format!("borg-reclaim-work-{}", std::process::id()));
        for d in [
            "k8s/k8s",
            "mono/mono",
            "_tool/node",
            "_actions/a",
            "_temp",
            "_update",
            "_PipelineMapping/x",
        ] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let cmd = Commands::new(Arc::new(crate::drone::exec::SystemExec), "setpriv".into());
        let out = cmd.sh(&root, 30, LIST_RUNNER_WORK).unwrap();
        let found = listed(&out).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(found, vec![root.join("k8s"), root.join("mono")]);
    }

    #[test]
    fn df_inside_a_private_home_is_asked_as_the_owner() {
        use crate::drone::exec::Output;
        struct PrivateHome(Mutex<Vec<String>>);
        impl Exec for PrivateHome {
            fn run(&self, program: &str, args: &[&str]) -> std::io::Result<Output> {
                let call = format!("{program} {}", args.join(" "));
                self.0.lock().unwrap().push(call.clone());
                let ok = |stdout: &str| {
                    Ok(Output {
                        success: true,
                        stdout: stdout.to_owned(),
                    })
                };
                if program == "stat" {
                    return ok("1006:1006");
                }
                // As the drone: no permission, no output. As the owner (via
                // timeout/setpriv): the filesystem.
                if program == "df" {
                    return Ok(Output {
                        success: false,
                        stdout: String::new(),
                    });
                }
                if call.contains(" df -Pk -- /home/runner6/actions-runner/_work") {
                    return ok(
                        "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/sda1 300000000 180000000 120000000 60% /",
                    );
                }
                ok("")
            }
        }
        let exec = Arc::new(PrivateHome(Mutex::new(Vec::new())));
        let cmd = Commands::new(Arc::clone(&exec) as Arc<dyn Exec>, "setpriv".into());
        let (disk, mount) = cmd
            .disk(Path::new("/home/runner6/actions-runner/_work"))
            .unwrap();
        assert_eq!(mount, "/");
        assert_eq!(disk.free_bytes, 120_000_000 * 1024);
        assert!(
            exec.0
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.starts_with("timeout ") && c.contains("--reuid 1006"))
        );
    }

    #[test]
    fn owner_falls_back_to_the_nearest_ancestor_the_drone_may_stat() {
        use crate::drone::exec::Output;
        struct PrivateHome;
        impl Exec for PrivateHome {
            fn run(&self, program: &str, args: &[&str]) -> std::io::Result<Output> {
                assert_eq!(program, "stat");
                let visible = args.last() == Some(&"/home/u");
                Ok(Output {
                    success: visible,
                    stdout: if visible {
                        "1000:1000".into()
                    } else {
                        String::new()
                    },
                })
            }
        }
        let cmd = Commands::new(Arc::new(PrivateHome), "setpriv".into());
        assert_eq!(
            cmd.owner(Path::new("/home/u/worktrees/x")).unwrap(),
            ("1000".to_owned(), "1000".to_owned())
        );
    }

    #[test]
    fn listings_must_complete_and_collapse_nested_candidates() {
        assert!(listed("/a/x\n").is_err());
        assert_eq!(
            listed("/a/x\n/a/x/target\n/a/y\n.done").unwrap(),
            [PathBuf::from("/a/x"), PathBuf::from("/a/y")]
        );
        assert!(listed(".done").unwrap().is_empty());
    }

    #[test]
    fn merged_cycles_take_the_worst_mode_and_sum_bytes() {
        let cyc = |mode, before, bytes| Cycle {
            mode,
            usage_pct_before: before,
            usage_pct_after: before,
            free_bytes_before: u64::from(100 - before) << 30,
            floor_bytes: 10 << 30,
            records: Vec::new(),
            evicted: BTreeMap::new(),
            kept: BTreeMap::from([("busy".to_owned(), 1)]),
            reclaimed_bytes: bytes,
            would_reclaim_bytes: 0,
            errors: 0,
        };
        let mut acc = None;
        merge(&mut acc, cyc(Mode::Hygiene, 60, 5));
        merge(&mut acc, cyc(Mode::Pressure, 90, 7));
        let acc = acc.unwrap();
        assert_eq!(acc.mode, Mode::Pressure);
        assert_eq!(acc.usage_pct_before, 90);
        // The 10 GiB free filesystem is the tighter one (target 20 GiB).
        assert_eq!(acc.free_bytes_before, 10 << 30);
        assert_eq!(acc.floor_bytes, 10 << 30);
        assert_eq!(acc.reclaimed_bytes, 12);
        assert_eq!(acc.kept["busy"], 2);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn live_host_keeps_dirty_and_unpushed_worktrees_and_deletes_a_clean_pushed_one() {
        use crate::drone::exec::SystemExec;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let dir = dir.to_string_lossy();
            let mut command = vec![
                "GIT_CONFIG_GLOBAL=/dev/null",
                "git",
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-C",
                dir.as_ref(),
            ];
            command.extend_from_slice(args);
            let output = SystemExec.run("env", &command).unwrap();
            assert!(output.success, "git {args:?}: {}", output.stdout);
        };
        let origin = root.join("origin.git");
        std::fs::create_dir(&origin).unwrap();
        git(&origin, &["init", "-q", "--bare"]);
        let main = root.join("main");
        std::fs::create_dir(&main).unwrap();
        git(&main, &["init", "-q", "-b", "main"]);
        git(&main, &["commit", "-q", "--allow-empty", "-m", "c1"]);
        git(
            &main,
            &["remote", "add", "origin", &origin.to_string_lossy()],
        );
        git(&main, &["push", "-q", "origin", "main"]);
        let wts = root.join("wts");
        std::fs::create_dir(&wts).unwrap();
        for name in ["clean", "dirty", "unpushed"] {
            git(
                &main,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "--detach",
                    &wts.join(name).to_string_lossy(),
                    "origin/main",
                ],
            );
        }
        std::fs::write(wts.join("dirty/new.txt"), "x").unwrap();
        git(
            &wts.join("unpushed"),
            &["commit", "-q", "--allow-empty", "-m", "local"],
        );

        let cmd = Commands::new(Arc::new(SystemExec), "setpriv".into());
        let sizes = Mutex::new(HashMap::new());
        let leases = Mutex::new(HashSet::new());
        let slots = Slots::new(4);
        let host = LiveHost {
            cmd: &cmd,
            anchor: root.clone(),
            now: time::OffsetDateTime::now_utc().unix_timestamp() + 30 * 86_400,
            size_floor_secs: 0,
            sizes: &sizes,
            common: Mutex::new(HashMap::new()),
            floor: 0,
            leases: &leases,
            slots: &slots,
            work_root: Path::new("/w"),
            diag_max_age_secs: 86_400,
        };
        let roots = [Root {
            path: wts.clone(),
            kind: Kind::Worktree,
        }];
        let cands = host.scan(&roots).unwrap();
        assert_eq!(cands.len(), 3, "{cands:?}");
        let by = |n: &str| {
            cands
                .iter()
                .find(|c| c.path == wts.join(n))
                .unwrap()
                .clone()
        };
        assert!(host.probe(&by("dirty")).dirty);
        assert!(host.probe(&by("unpushed")).unpushed);
        let main_cand = Candidate {
            path: main.clone(),
            kind: Kind::Worktree,
            size_bytes: 0,
            idle_secs: 0,
            within: None,
            cache: None,
        };
        assert_eq!(
            host.probe(&main_cand).protected.as_deref(),
            Some("main checkout")
        );
        let clean = by("clean");
        assert_eq!(host.probe(&clean).blocker(Kind::Worktree), None);
        host.delete(&clean).unwrap();
        assert!(!clean.path.exists());
        // `git worktree prune` dropped the worktree's admin entry.
        assert!(!main.join(".git/worktrees/clean").exists());
    }
}
