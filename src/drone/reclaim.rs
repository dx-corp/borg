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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::drone::config::DroneConfig;
use crate::drone::exec::Exec;
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
    // and were never reclaimed, so drones filled their disks with them.
    for d in ci_slots {
        push(d.clone(), Kind::CargoTarget);
    }
    out
}

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
    fn new(exec: Arc<dyn Exec>, setpriv: String) -> Self {
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
        let (uid, gid) = self.owner(path)?;
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
        let line = out.stdout.lines().nth(1).ok_or("df: no output")?;
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
    /// Below this idle age a candidate is never eligible, so its size is not
    /// measured.
    size_floor_secs: u64,
    sizes: &'a Mutex<HashMap<PathBuf, (i64, u64)>>,
    /// Common git dir of each worktree probed, for `worktree prune`.
    common: Mutex<HashMap<PathBuf, PathBuf>>,
}

impl LiveHost<'_> {
    fn newest(&self, path: &Path, worktree: bool) -> Option<i64> {
        let script = if worktree {
            NEWEST_WORKTREE
        } else {
            NEWEST_ANY
        };
        let out = self.cmd.sh(path, PROBE_SECS, script).ok()?;
        out.trim()
            .split('.')
            .next()
            .and_then(|s| s.parse::<i64>().ok())
    }

    fn candidate(&self, path: PathBuf, kind: Kind, within: Option<PathBuf>) -> Option<Candidate> {
        let newest = self.newest(&path, kind == Kind::Worktree)?;
        let idle_secs = u64::try_from(self.now.saturating_sub(newest)).unwrap_or(0);
        let size_bytes = if idle_secs < self.size_floor_secs {
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
        })
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

    fn scan(&self, roots: &[Root]) -> Result<Vec<Candidate>, String> {
        let mut out = Vec::new();
        for root in roots {
            let script = match root.kind {
                Kind::Worktree => LIST_WORKTREES,
                _ => LIST_TARGETS,
            };
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
                        if let Some(c) = self.candidate(i, Kind::CargoIncremental, Some(p.clone()))
                        {
                            out.push(c);
                        }
                    }
                }
                if let Some(c) = self.candidate(p, root.kind, None) {
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
    acc.records.extend(c.records);
    for (k, v) in c.kept {
        *acc.kept.entry(k).or_default() += v;
    }
    acc.reclaimed_bytes = acc.reclaimed_bytes.saturating_add(c.reclaimed_bytes);
    acc.would_reclaim_bytes = acc
        .would_reclaim_bytes
        .saturating_add(c.would_reclaim_bytes);
    acc.errors += c.errors;
}

/// Where the container runtime keeps images (the root filesystem on the
/// drones); `df` here measures the filesystem image pulls fill.
const DOCKER_ROOT: &str = "/var/lib/docker";
/// `docker image ls` rows: repository, full image id, creation time.
const IMAGE_FORMAT: &str = "{{.Repository}}\t{{.ID}}\t{{.CreatedAt}}";

/// Image ids superseded by a newer image of the same repository, from
/// `IMAGE_FORMAT` rows. CI pulls a new platform image per commit, so old
/// ones pile up; the newest image of every repository is kept, which keeps
/// deliberately pre-pulled CI tool images. Images with no repository are
/// dangling layers and always superseded. `CreatedAt` is
/// `YYYY-MM-DD HH:MM:SS +0000 UTC` on the drones, so it sorts as text.
fn superseded_images(listing: &str) -> Vec<String> {
    let mut by_repo: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
    let mut out = Vec::new();
    for line in listing.lines() {
        let mut f = line.split('\t');
        let (Some(repo), Some(id), Some(created)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        if repo == "<none>" {
            out.push(id.to_owned());
        } else {
            by_repo.entry(repo).or_default().push((created, id));
        }
    }
    for images in by_repo.values_mut() {
        images.sort_unstable();
        images.dedup_by(|a, b| a.1 == b.1);
        images.pop();
        out.extend(images.iter().map(|(_, id)| (*id).to_owned()));
    }
    out.sort_unstable();
    out.dedup();
    out
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
    stats: Mutex<ReclaimStats>,
    seq: Mutex<u64>,
    sizes: Mutex<HashMap<PathBuf, (i64, u64)>>,
}

impl Reclaimer {
    /// `None` when reclaim is off or the host is not Linux.
    pub(super) fn new(cfg: &DroneConfig, exec: Arc<dyn Exec>) -> Option<Arc<Self>> {
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
            default_roots(&host_homes(cfg), &ci_slot_roots(cfg), |p| {
                cmd.sh(p, PROBE_SECS, r#"test -d "$1""#).is_ok()
            })
        } else {
            settings.roots.clone()
        };
        // Audit sequence numbers must grow across drone restarts, because the
        // queen ignores a (drone, seq) it already has.
        let seq = u64::try_from(time::OffsetDateTime::now_utc().unix_timestamp())
            .unwrap_or(0)
            .saturating_mul(1000);
        tracing::info!(
            "reclaim: on (dry_run={}), roots: {}",
            settings.dry_run,
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
            seq: Mutex::new(seq),
            sizes: Mutex::new(HashMap::new()),
        }))
    }

    /// Stats for the next heartbeat, with the audit entries not yet sent.
    pub(super) fn heartbeat_stats(&self) -> Option<ReclaimStats> {
        let mut s = self.stats.lock().ok()?;
        let mut out = s.clone();
        out.recent = s.drain_recent(RECENT_PER_HEARTBEAT);
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
        self.prune_superseded_images();
    }

    /// Above the high-water mark on the image filesystem, removes images a
    /// newer image of the same repository superseded (pve50-vm240 failed
    /// disk admission on 2026-10-25 holding 7 GiB of them). `docker image rm`
    /// without `-f` refuses an image a container still uses, so a running
    /// job's image stays.
    fn prune_superseded_images(&self) {
        let Ok((disk, mount)) = self.cmd.disk(Path::new(DOCKER_ROOT)) else {
            return;
        };
        let usage = disk.usage_pct();
        if usage < self.settings.high_water_pct {
            return;
        }
        let listing = match self.cmd.exec.run(
            &self.docker_bin,
            &["image", "ls", "--no-trunc", "--format", IMAGE_FORMAT],
        ) {
            Ok(out) if out.success => out.stdout,
            _ => return,
        };
        let ids = superseded_images(&listing);
        if ids.is_empty() {
            return;
        }
        if self.settings.dry_run {
            tracing::info!(
                "reclaim images {mount}: usage {usage}%, would remove {} superseded image(s)",
                ids.len()
            );
            return;
        }
        let mut removed = 0usize;
        for id in &ids {
            match self.cmd.exec.run(&self.docker_bin, &["image", "rm", id]) {
                Ok(out) if out.success => removed += 1,
                _ => tracing::info!("reclaim images: kept {id} (in use or referenced elsewhere)"),
            }
        }
        tracing::info!(
            "reclaim images {mount}: usage {usage}%, removed {removed} of {} superseded image(s)",
            ids.len()
        );
    }

    /// Run a cycle shortly after start and then every `interval_secs`.
    pub(super) fn spawn(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let me = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let mut every =
                tokio::time::interval(Duration::from_secs(me.settings.interval_secs.max(60)));
            every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                every.tick().await;
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
        ]
        .into_iter()
        .map(PathBuf::from)
        .collect();
        let roots = default_roots(
            &[PathBuf::from("/home/dev")],
            &[PathBuf::from("/var/lib/borg-job/cache/mono-ci/slots")],
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
            ]
        );
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
    fn superseded_images_keep_the_newest_of_each_repository() {
        let listing = "\
ghcr.io/dx-corp/platform/platform-worker\tsha256:w3\t2026-10-25 18:00:00 +0000 UTC
ghcr.io/dx-corp/platform/platform-worker\tsha256:w1\t2026-10-25 08:00:00 +0000 UTC
ghcr.io/dx-corp/platform/platform-api\tsha256:a2\t2026-10-25 12:00:00 +0000 UTC
ghcr.io/dx-corp/platform/platform-worker\tsha256:w2\t2026-10-25 10:00:00 +0000 UTC
ghcr.io/dx-corp/deploy-feature-flag-ci-tools\tsha256:tools\t2026-08-20 09:00:00 +0000 UTC
ghcr.io/dx-corp/platform/platform-api\tsha256:a1\t2026-10-24 12:00:00 +0000 UTC
<none>\tsha256:dangling\t2026-10-01 00:00:00 +0000 UTC
malformed line
";
        assert_eq!(
            superseded_images(listing),
            vec!["sha256:a1", "sha256:dangling", "sha256:w1", "sha256:w2"]
        );
    }

    #[test]
    fn one_image_per_repository_is_never_superseded() {
        let listing = "repo-a\tsha256:x\t2026-01-01 00:00:00 +0000 UTC\nrepo-b\tsha256:y\t2026-01-01 00:00:00 +0000 UTC\n";
        assert!(superseded_images(listing).is_empty());
        // The same id listed twice for one repository is one image.
        let twice = "repo-a\tsha256:x\t2026-01-01 00:00:00 +0000 UTC\nrepo-a\tsha256:x\t2026-01-01 00:00:00 +0000 UTC\n";
        assert!(superseded_images(twice).is_empty());
    }

    #[test]
    fn merged_cycles_take_the_worst_mode_and_sum_bytes() {
        let cyc = |mode, before, bytes| Cycle {
            mode,
            usage_pct_before: before,
            usage_pct_after: before,
            records: Vec::new(),
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
        let host = LiveHost {
            cmd: &cmd,
            anchor: root.clone(),
            now: time::OffsetDateTime::now_utc().unix_timestamp() + 30 * 86_400,
            size_floor_secs: 0,
            sizes: &sizes,
            common: Mutex::new(HashMap::new()),
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
