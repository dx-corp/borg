//! Build-box reclaim: disk-pressure driven cleanup of abandoned git
//! worktrees and cargo build directories on hosts where people and agents
//! build by hand.
//!
//! This module is the *policy* and the *report*. It is pure apart from the
//! [`ReclaimHost`] trait, which the drone implements against the real
//! filesystem, git and process table; tests use a fake host. A cycle is
//! [`run_cycle`].
//!
//! What it removes, cheapest first (tiers):
//! 1. `incremental/` directories under cargo target directories (regenerable);
//! 2. whole cargo target directories;
//! 3. regenerable cache directories (per-slot CI tool homes);
//! 4. git worktrees.
//!
//! Docker state (unused images, stopped containers, build cache, dangling
//! volumes) is a separate tier with its own keep rules; see
//! [`DockerReclaimSettings`] and `drone::docker_reclaim`.
//!
//! When it runs:
//! * **Pressure.** Disk usage at or above `high_water_pct` starts reclaiming;
//!   it frees the oldest eligible items first until usage is projected at or
//!   below `low_water_pct`. An item is eligible once it has been untouched for
//!   `min_age_secs` (6 hours). At or above `critical_pct` the age drops to
//!   `critical_min_age_secs`.
//! * **Hygiene.** Below the high-water mark only items untouched for
//!   `idle_max_age_secs` (7 days; 0 turns it off) are removed.
//! * **Admission.** Free space below the queen's disk-admission floor means
//!   the drone takes no jobs, whatever the percentage: that counts as
//!   critical (a 291 GB disk at 80% still has the 10 GiB floor free, but a
//!   small one does not).
//!
//! What it never removes (checked immediately before each deletion, so state
//! that changed since the scan is respected; an unreadable or unknown state
//! counts as unsafe):
//! * anything a process is using: any process with its working directory or an
//!   open file inside it;
//! * a worktree with uncommitted changes, **including untracked files**;
//! * a worktree with commits that are on no remote-tracking branch;
//! * a worktree that is the main checkout or locked (stashes live in the
//!   repository, not the linked worktree, so removing one never loses them);
//! * a worktree that is not a git checkout.
//!
//! It is **on by default** on every drone: with no `roots` configured the
//! drone reclaims the standard worktree and cargo-target locations it finds
//! (see `drone::reclaim::default_roots`). Every deletion, real or would-be,
//! becomes a [`DeletionRecord`] (path, size, reason, outcome) that
//! the drone logs and reports to the queen in its heartbeat.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// What a candidate directory is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// `incremental/` inside a cargo target directory.
    CargoIncremental,
    /// A whole cargo target directory.
    CargoTarget,
    /// A git worktree.
    Worktree,
    /// A regenerable cache directory (a CI slot's tool home).
    CacheDir,
    /// A container image no container uses.
    DockerImage,
    /// A stopped container.
    DockerContainer,
    /// Build cache records (one record per prune).
    DockerBuildCache,
    /// A dangling volume.
    DockerVolume,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CargoIncremental => "cargo_incremental",
            Self::CargoTarget => "cargo_target",
            Self::Worktree => "worktree",
            Self::CacheDir => "cache_dir",
            Self::DockerImage => "docker_image",
            Self::DockerContainer => "docker_container",
            Self::DockerBuildCache => "docker_build_cache",
            Self::DockerVolume => "docker_volume",
        }
    }

    /// Kinds every queen understands. Records of other kinds travel in
    /// [`ReclaimStats::recent_ext`], which a queen built before they existed
    /// ignores instead of rejecting the whole heartbeat.
    pub fn is_wire_v1(self) -> bool {
        matches!(
            self,
            Self::CargoIncremental | Self::CargoTarget | Self::Worktree
        )
    }
}

/// A directory the drone scans for candidates: each child is one candidate
/// (cargo target roots also offer the `incremental/` directories inside).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Root {
    pub path: PathBuf,
    /// `worktree` or `cargo_target` (an `incremental` root is not a thing).
    pub kind: Kind,
}

/// Per-host settings (the `reclaim` block of the drone config).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReclaimSettings {
    /// On by default; a host may opt out.
    pub enabled: bool,
    /// Log and report what would be deleted without deleting anything.
    pub dry_run: bool,
    /// Empty: the drone's default roots.
    pub roots: Vec<Root>,
    /// Start reclaiming at this disk usage (percent).
    pub high_water_pct: u8,
    /// Reclaim until usage is projected at or below this (percent).
    pub low_water_pct: u8,
    /// At or above this usage the minimum age drops to
    /// `critical_min_age_secs`.
    pub critical_pct: u8,
    pub min_age_secs: u64,
    pub critical_min_age_secs: u64,
    /// Below high water, remove items untouched this long; 0 = never.
    pub idle_max_age_secs: u64,
    /// At most this many deletions per cycle.
    pub max_deletions_per_cycle: u32,
    /// Seconds between cycles.
    pub interval_secs: u64,
    /// The Docker tier.
    pub docker: DockerReclaimSettings,
}

/// The Docker tier (the `reclaim.docker` block). On by default.
///
/// Keep rules, checked immediately before each removal: an image used by
/// any remaining container, the newest image of each repository, and
/// anything labelled `borg.keep` stay; a container or volume of a lease the
/// drone is running is busy and stays; a volume is removed only when it is
/// dangling and either anonymous or created by a borg lease, never a named
/// volume someone made on purpose. Nothing uses `-f` or `system prune`.
///
/// Ages: below high water each kind goes once it is older than its
/// `*_max_age_secs` (0 = never by age alone); under pressure the shared
/// `min_age_secs`, when critical `critical_min_age_secs`, whichever is
/// younger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DockerReclaimSettings {
    pub enabled: bool,
    /// Unused images older than this (since built or last pulled/tagged).
    pub image_max_age_secs: u64,
    /// Stopped containers that finished this long ago.
    pub stopped_container_max_age_secs: u64,
    /// Build cache records unused this long (`builder prune --filter until`).
    pub build_cache_max_age_secs: u64,
    /// Build cache kept regardless of age (`--reserved-space`, or
    /// `--keep-storage` on older buildx); 0 when critical.
    pub build_cache_keep_bytes: u64,
    /// Remove dangling anonymous or lease-created volumes.
    pub prune_dangling_volumes: bool,
    /// Dangling volumes created this long ago.
    pub volume_max_age_secs: u64,
}

impl Default for DockerReclaimSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            image_max_age_secs: 24 * 3600,
            stopped_container_max_age_secs: 6 * 3600,
            build_cache_max_age_secs: 24 * 3600,
            build_cache_keep_bytes: 10 << 30,
            prune_dangling_volumes: true,
            volume_max_age_secs: 24 * 3600,
        }
    }
}

impl DockerReclaimSettings {
    /// The minimum age for a kind whose hygiene age is `hygiene_secs`.
    /// `None`: nothing of that kind is eligible in this mode.
    pub fn min_age(&self, s: &ReclaimSettings, mode: Mode, hygiene_secs: u64) -> Option<u64> {
        let by_mode = match mode {
            Mode::Critical => Some(s.critical_min_age_secs),
            Mode::Pressure => Some(s.min_age_secs),
            Mode::Hygiene | Mode::Idle => None,
        };
        let hygiene = (hygiene_secs > 0).then_some(hygiene_secs);
        match (by_mode, hygiene) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

impl Default for ReclaimSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            dry_run: false,
            roots: Vec::new(),
            high_water_pct: 85,
            low_water_pct: 70,
            critical_pct: 95,
            min_age_secs: 6 * 3600,
            critical_min_age_secs: 3600,
            idle_max_age_secs: 7 * 24 * 3600,
            max_deletions_per_cycle: 50,
            interval_secs: 600,
            docker: DockerReclaimSettings::default(),
        }
    }
}

impl ReclaimSettings {
    /// The block installs wrote before reclaim was on by default: off, dry
    /// run, no roots, default thresholds.
    pub(crate) fn is_legacy_default(&self) -> bool {
        *self
            == Self {
                enabled: false,
                dry_run: true,
                ..Self::default()
            }
    }

    pub fn validate(&self) -> Result<(), String> {
        if !(self.low_water_pct < self.high_water_pct
            && self.high_water_pct <= self.critical_pct
            && self.critical_pct <= 100
            && self.low_water_pct > 0)
        {
            return Err(
                "reclaim needs 0 < low_water_pct < high_water_pct <= critical_pct <= 100".into(),
            );
        }
        if self.critical_min_age_secs > self.min_age_secs {
            return Err("reclaim critical_min_age_secs may not exceed min_age_secs".into());
        }
        if self.max_deletions_per_cycle == 0 || self.interval_secs == 0 {
            return Err(
                "reclaim max_deletions_per_cycle and interval_secs must be positive".into(),
            );
        }
        for r in &self.roots {
            if !r.path.is_absolute() {
                return Err("reclaim root paths must be absolute".into());
            }
            if !matches!(r.kind, Kind::Worktree | Kind::CargoTarget) {
                return Err("reclaim roots are worktree or cargo_target".into());
            }
        }
        Ok(())
    }
}

/// A directory that could be reclaimed, as the scan found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: PathBuf,
    pub kind: Kind,
    pub size_bytes: u64,
    /// Seconds since anything in it was modified or used.
    pub idle_secs: u64,
    /// For an `incremental/`, the target directory that contains it.
    pub within: Option<PathBuf>,
}

/// What the safety probe found. Anything the probe could not determine must be
/// reported as unknown, which blocks deletion.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Safety {
    /// A process has its cwd or an open file inside.
    pub busy: bool,
    /// Worktree: uncommitted changes including untracked files.
    pub dirty: bool,
    /// Worktree: commits reachable from no remote-tracking branch.
    pub unpushed: bool,
    /// Worktree: not a git checkout.
    pub not_git: bool,
    /// Worktree: main checkout or locked.
    pub protected: Option<String>,
    /// The probe failed or could not tell.
    pub unknown: Option<String>,
}

impl Safety {
    /// Why this candidate must be kept, if it must.
    pub fn blocker(&self, kind: Kind) -> Option<String> {
        if let Some(why) = &self.unknown {
            return Some(format!("unknown: {why}"));
        }
        if self.busy {
            return Some("busy".into());
        }
        if kind == Kind::Worktree {
            if self.not_git {
                return Some("not a git checkout".into());
            }
            if self.dirty {
                return Some("uncommitted changes".into());
            }
            if self.unpushed {
                return Some("unpushed commits".into());
            }
            if let Some(why) = &self.protected {
                return Some(why.clone());
            }
        }
        None
    }
}

/// Disk state of the filesystem holding the roots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Disk {
    pub total_bytes: u64,
    pub free_bytes: u64,
}

impl Disk {
    fn used(&self) -> u64 {
        self.total_bytes.saturating_sub(self.free_bytes)
    }

    /// Usage in whole percent, rounded down.
    pub fn usage_pct(&self) -> u8 {
        if self.total_bytes == 0 {
            return 0;
        }
        u8::try_from(u128::from(self.used()) * 100 / u128::from(self.total_bytes)).unwrap_or(100)
    }
}

/// The filesystem, git and process table, as the drone provides them.
pub trait ReclaimHost {
    fn disk(&self) -> Result<Disk, String>;
    /// Free bytes below which the drone takes no jobs (the queen's disk
    /// admission floor); 0 when unknown.
    fn admission_floor_bytes(&self) -> u64 {
        0
    }
    /// List candidates under the configured roots.
    fn scan(&self, roots: &[Root]) -> Result<Vec<Candidate>, String>;
    /// Probe a candidate immediately before it is deleted.
    fn probe(&self, c: &Candidate) -> Safety;
    /// Delete a candidate; returns the bytes freed.
    fn delete(&self, c: &Candidate) -> Result<u64, String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Deleted,
    WouldDelete,
    Failed,
}

/// One audit entry: a deletion (or would-be deletion) and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeletionRecord {
    /// Per-drone sequence number; the queen ignores repeats.
    pub seq: u64,
    pub at: i64,
    pub path: String,
    pub kind: Kind,
    pub size_bytes: u64,
    pub reason: String,
    pub outcome: Outcome,
    /// Set for `failed`.
    #[serde(default)]
    pub error: Option<String>,
}

/// Why a cycle ran and what it concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Below high water and no idle hygiene candidates due.
    Idle,
    Hygiene,
    Pressure,
    Critical,
}

/// The result of one cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cycle {
    pub mode: Mode,
    pub usage_pct_before: u8,
    /// Projected after the deletions (exact when not a dry run).
    pub usage_pct_after: u8,
    pub records: Vec<DeletionRecord>,
    /// Candidates kept, by reason.
    pub kept: BTreeMap<String, u32>,
    pub reclaimed_bytes: u64,
    pub would_reclaim_bytes: u64,
    pub errors: u32,
}

/// Reported to the queen in every heartbeat.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReclaimStats {
    pub enabled: bool,
    pub dry_run: bool,
    /// Bytes really deleted since the drone started.
    pub reclaimed_bytes_total: u64,
    pub last_run_at: Option<i64>,
    pub last_run_reclaimed_bytes: u64,
    /// What the last run would have deleted (dry run) .
    pub last_run_would_reclaim_bytes: u64,
    pub last_run_errors: u32,
    /// Disk usage the last run saw.
    pub usage_pct: Option<u8>,
    pub last_run_mode: Option<Mode>,
    /// Candidates the last run kept, by reason.
    pub kept: BTreeMap<String, u32>,
    /// Audit entries created since the previous heartbeat; the queen stores
    /// them and ignores any it already has (by `seq`). Only
    /// [`Kind::is_wire_v1`] kinds.
    #[serde(default)]
    pub recent: Vec<DeletionRecord>,
    /// Audit entries of the newer kinds (cache dirs, Docker), kept apart so a
    /// queen that predates them still accepts the heartbeat.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_ext: Vec<DeletionRecord>,
    /// Bytes the Docker tier freed since the drone started (also counted in
    /// `reclaimed_bytes_total`).
    #[serde(default)]
    pub docker_reclaimed_bytes_total: u64,
    /// Bytes the last Docker tier run freed.
    #[serde(default)]
    pub last_run_docker_reclaimed_bytes: u64,
}

impl ReclaimStats {
    /// Fold a finished cycle into the running stats.
    pub fn absorb(&mut self, settings: &ReclaimSettings, now: i64, cycle: &Cycle) {
        self.enabled = settings.enabled;
        self.dry_run = settings.dry_run;
        self.reclaimed_bytes_total = self
            .reclaimed_bytes_total
            .saturating_add(cycle.reclaimed_bytes);
        self.last_run_at = Some(now);
        self.last_run_reclaimed_bytes = cycle.reclaimed_bytes;
        self.last_run_would_reclaim_bytes = cycle.would_reclaim_bytes;
        self.last_run_errors = cycle.errors;
        self.usage_pct = Some(cycle.usage_pct_before);
        self.last_run_mode = Some(cycle.mode);
        self.kept = cycle.kept.clone();
        self.push_records(&cycle.records);
    }

    /// Fold a Docker tier run into the stats of the cycle just absorbed: its
    /// bytes count toward the run and the running totals, its errors toward
    /// the run's errors, and its keep reasons join the run's.
    pub fn absorb_docker(&mut self, cycle: &Cycle) {
        self.reclaimed_bytes_total = self
            .reclaimed_bytes_total
            .saturating_add(cycle.reclaimed_bytes);
        self.last_run_reclaimed_bytes = self
            .last_run_reclaimed_bytes
            .saturating_add(cycle.reclaimed_bytes);
        self.last_run_would_reclaim_bytes = self
            .last_run_would_reclaim_bytes
            .saturating_add(cycle.would_reclaim_bytes);
        self.docker_reclaimed_bytes_total = self
            .docker_reclaimed_bytes_total
            .saturating_add(cycle.reclaimed_bytes);
        self.last_run_docker_reclaimed_bytes = cycle.reclaimed_bytes;
        self.last_run_errors = self.last_run_errors.saturating_add(cycle.errors);
        for (k, v) in &cycle.kept {
            *self.kept.entry(format!("docker: {k}")).or_default() += v;
        }
        self.push_records(&cycle.records);
    }

    fn push_records(&mut self, records: &[DeletionRecord]) {
        for r in records {
            if r.kind.is_wire_v1() {
                self.recent.push(r.clone());
            } else {
                self.recent_ext.push(r.clone());
            }
        }
    }

    /// Take the audit entries for the next heartbeat (at most `max` of each
    /// list; the rest stay for the following one): `(recent, recent_ext)`.
    pub fn drain_recent(&mut self, max: usize) -> (Vec<DeletionRecord>, Vec<DeletionRecord>) {
        let n = self.recent.len().min(max);
        let m = self.recent_ext.len().min(max);
        (
            self.recent.drain(..n).collect(),
            self.recent_ext.drain(..m).collect(),
        )
    }
}

fn tier(k: Kind) -> u8 {
    match k {
        Kind::CargoIncremental => 0,
        Kind::CargoTarget => 1,
        Kind::CacheDir => 2,
        Kind::Worktree => 3,
        // The Docker tier orders its own work.
        Kind::DockerImage | Kind::DockerContainer | Kind::DockerBuildCache | Kind::DockerVolume => {
            4
        }
    }
}

fn gib(bytes: u64) -> String {
    format!(
        "{}.{} GiB",
        bytes >> 30,
        ((bytes & ((1 << 30) - 1)) * 10) >> 30
    )
}

fn hours(secs: u64) -> String {
    if secs >= 86_400 {
        format!("{}d", secs / 86_400)
    } else {
        format!("{}h", secs / 3600)
    }
}

/// The mode for this disk state.
pub fn mode_for(s: &ReclaimSettings, disk: &Disk) -> Mode {
    mode_for_floor(s, disk, 0)
}

/// The mode for this disk state when the drone stops taking jobs below
/// `floor_bytes` free: below the floor is critical.
pub fn mode_for_floor(s: &ReclaimSettings, disk: &Disk, floor_bytes: u64) -> Mode {
    let u = disk.usage_pct();
    if u >= s.critical_pct || (floor_bytes > 0 && disk.free_bytes < floor_bytes) {
        Mode::Critical
    } else if u >= s.high_water_pct {
        Mode::Pressure
    } else if s.idle_max_age_secs > 0 {
        Mode::Hygiene
    } else {
        Mode::Idle
    }
}

/// Candidates in the order they should be tried, each with its eligibility
/// reason; ineligible candidates are left out. Deterministic.
pub fn order(
    s: &ReclaimSettings,
    mode: Mode,
    usage_pct: u8,
    cands: &[Candidate],
) -> Vec<(Candidate, String)> {
    let min_age = match mode {
        Mode::Critical => s.critical_min_age_secs,
        Mode::Pressure => s.min_age_secs,
        Mode::Hygiene => s.idle_max_age_secs,
        Mode::Idle => return Vec::new(),
    };
    let mut out: Vec<(Candidate, String)> = cands
        .iter()
        .filter(|c| c.idle_secs >= min_age)
        .map(|c| {
            let why = match mode {
                Mode::Hygiene => format!(
                    "idle {} (>= {} hygiene limit)",
                    hours(c.idle_secs),
                    hours(min_age)
                ),
                _ => format!(
                    "disk {usage_pct}% >= {}% high water ({mode:?}); idle {} (>= {})",
                    s.high_water_pct,
                    hours(c.idle_secs),
                    hours(min_age)
                ),
            };
            (c.clone(), why)
        })
        .collect();
    out.sort_by(|(a, _), (b, _)| {
        tier(a.kind)
            .cmp(&tier(b.kind))
            .then(b.idle_secs.cmp(&a.idle_secs))
            .then(b.size_bytes.cmp(&a.size_bytes))
            .then(a.path.cmp(&b.path))
    });
    out
}

/// Run one cycle. `seq` numbers the audit records (it is advanced). Returns
/// the cycle; nothing is deleted when `settings.dry_run` is true.
pub fn run_cycle(
    host: &dyn ReclaimHost,
    settings: &ReclaimSettings,
    now: i64,
    seq: &mut u64,
) -> Result<Cycle, String> {
    let disk = host.disk()?;
    let before = disk.usage_pct();
    let mode = mode_for_floor(settings, &disk, host.admission_floor_bytes());
    let mut cycle = Cycle {
        mode,
        usage_pct_before: before,
        usage_pct_after: before,
        records: Vec::new(),
        kept: BTreeMap::new(),
        reclaimed_bytes: 0,
        would_reclaim_bytes: 0,
        errors: 0,
    };
    if mode == Mode::Idle {
        return Ok(cycle);
    }
    let cands = host.scan(&settings.roots)?;
    // Under pressure, free until usage is projected at or below low water.
    // Hygiene has no byte target: it removes everything that is old enough.
    let target_free = match mode {
        Mode::Hygiene => u64::MAX,
        _ => disk
            .used()
            .saturating_sub(disk.total_bytes / 100 * u64::from(settings.low_water_pct))
            // Below the admission floor, at least back above it (with the
            // same margin again, so the next job does not drop it back).
            .max(
                host.admission_floor_bytes()
                    .saturating_mul(2)
                    .saturating_sub(disk.free_bytes),
            ),
    };
    let mut freed: u64 = 0;
    // Bytes already freed inside a target directory (its incremental/), so a
    // later deletion of the whole directory is not counted twice.
    let mut freed_within: BTreeMap<PathBuf, u64> = BTreeMap::new();
    let mut deletions = 0u32;

    for (c, eligible) in order(settings, mode, before, &cands) {
        if freed >= target_free || deletions >= settings.max_deletions_per_cycle {
            break;
        }
        // Re-probe now, not at scan time.
        let safety = host.probe(&c);
        if let Some(why) = safety.blocker(c.kind) {
            *cycle.kept.entry(why).or_default() += 1;
            continue;
        }
        let already = freed_within.get(&c.path).copied().unwrap_or(0);
        let size = c.size_bytes.saturating_sub(already);
        let reason = format!(
            "{eligible}; {} {}; tier {}",
            c.kind.as_str(),
            gib(size),
            tier(c.kind) + 1
        );
        *seq += 1;
        let mut rec = DeletionRecord {
            seq: *seq,
            at: now,
            path: c.path.display().to_string(),
            kind: c.kind,
            size_bytes: size,
            reason,
            outcome: Outcome::WouldDelete,
            error: None,
        };
        if settings.dry_run {
            cycle.would_reclaim_bytes = cycle.would_reclaim_bytes.saturating_add(size);
        } else {
            match host.delete(&c) {
                Ok(bytes) => {
                    rec.outcome = Outcome::Deleted;
                    // What the host says it freed, but never more than the
                    // estimate net of what an earlier deletion already took.
                    rec.size_bytes = bytes.min(size);
                    cycle.reclaimed_bytes = cycle.reclaimed_bytes.saturating_add(rec.size_bytes);
                }
                Err(e) => {
                    rec.outcome = Outcome::Failed;
                    rec.error = Some(e);
                    cycle.errors += 1;
                    cycle.records.push(rec);
                    continue;
                }
            }
        }
        let credited = if settings.dry_run {
            size
        } else {
            rec.size_bytes
        };
        freed = freed.saturating_add(credited);
        deletions += 1;
        if let Some(parent) = &c.within {
            *freed_within.entry(parent.clone()).or_default() += credited;
        }
        cycle.records.push(rec);
    }
    let after_used = disk.used().saturating_sub(freed);
    cycle.usage_pct_after = if disk.total_bytes == 0 {
        0
    } else {
        u8::try_from(u128::from(after_used) * 100 / u128::from(disk.total_bytes)).unwrap_or(100)
    };
    Ok(cycle)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    const GIB: u64 = 1 << 30;
    const H: u64 = 3600;

    struct Fake {
        disk: RefCell<Disk>,
        cands: Vec<Candidate>,
        safety: BTreeMap<PathBuf, Safety>,
        fail: Vec<PathBuf>,
        deleted: RefCell<Vec<PathBuf>>,
        probes: RefCell<u32>,
    }

    impl Fake {
        fn new(total_gib: u64, used_gib: u64, cands: Vec<Candidate>) -> Self {
            Self {
                disk: RefCell::new(Disk {
                    total_bytes: total_gib * GIB,
                    free_bytes: (total_gib - used_gib) * GIB,
                }),
                cands,
                safety: BTreeMap::new(),
                fail: Vec::new(),
                deleted: RefCell::new(Vec::new()),
                probes: RefCell::new(0),
            }
        }
    }

    impl ReclaimHost for Fake {
        fn disk(&self) -> Result<Disk, String> {
            Ok(*self.disk.borrow())
        }
        fn scan(&self, _: &[Root]) -> Result<Vec<Candidate>, String> {
            Ok(self.cands.clone())
        }
        fn probe(&self, c: &Candidate) -> Safety {
            *self.probes.borrow_mut() += 1;
            self.safety.get(&c.path).cloned().unwrap_or_default()
        }
        fn delete(&self, c: &Candidate) -> Result<u64, String> {
            if self.fail.contains(&c.path) {
                return Err("permission denied".into());
            }
            self.deleted.borrow_mut().push(c.path.clone());
            Ok(c.size_bytes)
        }
    }

    fn cand(path: &str, kind: Kind, gib: u64, idle_h: u64) -> Candidate {
        Candidate {
            path: PathBuf::from(path),
            kind,
            size_bytes: gib * GIB,
            idle_secs: idle_h * H,
            within: None,
        }
    }

    fn live(settings: ReclaimSettings) -> ReclaimSettings {
        ReclaimSettings {
            enabled: true,
            dry_run: false,
            roots: vec![Root {
                path: "/b".into(),
                kind: Kind::Worktree,
            }],
            ..settings
        }
    }

    fn run(f: &Fake, s: &ReclaimSettings) -> Cycle {
        run_cycle(f, s, 1000, &mut 0).unwrap()
    }

    #[test]
    fn below_high_water_only_week_old_items_go_and_nothing_else() {
        let f = Fake::new(
            100,
            50,
            vec![
                cand("/b/new", Kind::Worktree, 5, 30),
                cand("/b/old", Kind::Worktree, 5, 24 * 8),
            ],
        );
        let c = run(&f, &live(ReclaimSettings::default()));
        assert_eq!(c.mode, Mode::Hygiene);
        assert_eq!(f.deleted.borrow().as_slice(), [PathBuf::from("/b/old")]);
        // Hygiene off: nothing at all below high water.
        let off = ReclaimSettings {
            idle_max_age_secs: 0,
            ..ReclaimSettings::default()
        };
        let f2 = Fake::new(100, 50, vec![cand("/b/old", Kind::Worktree, 5, 24 * 30)]);
        let c = run(&f2, &live(off));
        assert_eq!((c.mode, c.records.len()), (Mode::Idle, 0));
    }

    #[test]
    fn pressure_frees_oldest_first_cheapest_tier_first_until_low_water() {
        // 100 GiB disk at 90%: need to get to 70%, i.e. free 20 GiB.
        let f = Fake::new(
            100,
            90,
            vec![
                cand("/b/wt-old", Kind::Worktree, 8, 200),
                cand("/t/a", Kind::CargoTarget, 12, 10),
                cand("/t/b", Kind::CargoTarget, 12, 50),
                cand("/t/b/incremental", Kind::CargoIncremental, 4, 50),
                cand("/t/young", Kind::CargoTarget, 30, 2),
            ],
        );
        let c = run(&f, &live(ReclaimSettings::default()));
        assert_eq!(c.mode, Mode::Pressure);
        let order: Vec<String> = f
            .deleted
            .borrow()
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        // Incremental first, then target dirs oldest first; stops at 20 GiB;
        // the worktree is never reached; the 2h-old dir is below min age.
        assert_eq!(order, ["/t/b/incremental", "/t/b", "/t/a"]);
        assert!(c.reclaimed_bytes >= 20 * GIB, "{}", c.reclaimed_bytes);
        assert!(c.usage_pct_after <= 70, "{}", c.usage_pct_after);
        assert_eq!(c.usage_pct_before, 90);
    }

    #[test]
    fn a_target_dir_is_not_double_counted_after_its_incremental_went() {
        let mut inc = cand("/t/b/incremental", Kind::CargoIncremental, 4, 50);
        inc.within = Some("/t/b".into());
        let f = Fake::new(100, 90, vec![inc, cand("/t/b", Kind::CargoTarget, 12, 50)]);
        let c = run(&f, &live(ReclaimSettings::default()));
        let sizes: Vec<u64> = c.records.iter().map(|r| r.size_bytes / GIB).collect();
        // 4 GiB for incremental, then 12 - 4 = 8 GiB credited for the rest.
        assert_eq!(sizes, [4, 8]);
        assert_eq!(c.reclaimed_bytes, 12 * GIB);
    }

    #[test]
    fn critical_usage_relaxes_the_minimum_age() {
        let items = vec![cand("/t/recent", Kind::CargoTarget, 10, 2)];
        let f = Fake::new(100, 96, items.clone());
        let c = run(&f, &live(ReclaimSettings::default()));
        assert_eq!(c.mode, Mode::Critical);
        assert_eq!(c.records.len(), 1);
        // At pressure (not critical) the same item is too young.
        let f = Fake::new(100, 90, items);
        assert!(
            run(&f, &live(ReclaimSettings::default()))
                .records
                .is_empty()
        );
    }

    #[test]
    fn unsafe_worktrees_are_never_deleted_and_the_reason_is_counted() {
        let paths = [
            "busy", "dirty", "unpushed", "nongit", "main", "unknown", "ok",
        ];
        let cands: Vec<Candidate> = paths
            .iter()
            .map(|p| cand(&format!("/b/{p}"), Kind::Worktree, 5, 100))
            .collect();
        let mut f = Fake::new(100, 99, cands);
        let set = |f: &mut Fake, p: &str, s: Safety| {
            f.safety.insert(PathBuf::from(format!("/b/{p}")), s);
        };
        set(
            &mut f,
            "busy",
            Safety {
                busy: true,
                ..Safety::default()
            },
        );
        set(
            &mut f,
            "dirty",
            Safety {
                dirty: true,
                ..Safety::default()
            },
        );
        set(
            &mut f,
            "unpushed",
            Safety {
                unpushed: true,
                ..Safety::default()
            },
        );
        set(
            &mut f,
            "nongit",
            Safety {
                not_git: true,
                ..Safety::default()
            },
        );
        set(
            &mut f,
            "main",
            Safety {
                protected: Some("main checkout".into()),
                ..Safety::default()
            },
        );
        set(
            &mut f,
            "unknown",
            Safety {
                unknown: Some("git failed".into()),
                ..Safety::default()
            },
        );
        let c = run(&f, &live(ReclaimSettings::default()));
        assert_eq!(f.deleted.borrow().as_slice(), [PathBuf::from("/b/ok")]);
        assert_eq!(c.kept["busy"], 1);
        assert_eq!(c.kept["uncommitted changes"], 1);
        assert_eq!(c.kept["unpushed commits"], 1);
        assert_eq!(c.kept["not a git checkout"], 1);
        assert_eq!(c.kept["main checkout"], 1);
        assert_eq!(c.kept["unknown: git failed"], 1);
    }

    #[test]
    fn git_state_does_not_block_cargo_directories_but_busy_does() {
        let mut f = Fake::new(
            100,
            99,
            vec![
                cand("/t/a", Kind::CargoTarget, 10, 100),
                cand("/t/b", Kind::CargoTarget, 10, 100),
            ],
        );
        f.safety.insert(
            "/t/a".into(),
            Safety {
                dirty: true,
                not_git: true,
                ..Safety::default()
            },
        );
        f.safety.insert(
            "/t/b".into(),
            Safety {
                busy: true,
                ..Safety::default()
            },
        );
        let c = run(&f, &live(ReclaimSettings::default()));
        assert_eq!(f.deleted.borrow().as_slice(), [PathBuf::from("/t/a")]);
        assert_eq!(c.kept["busy"], 1);
    }

    #[test]
    fn dry_run_deletes_nothing_but_reports_every_would_be_deletion() {
        let f = Fake::new(100, 90, vec![cand("/t/a", Kind::CargoTarget, 25, 100)]);
        let s = ReclaimSettings {
            dry_run: true,
            ..live(ReclaimSettings::default())
        };
        let c = run(&f, &s);
        assert!(f.deleted.borrow().is_empty());
        assert_eq!((c.reclaimed_bytes, c.would_reclaim_bytes), (0, 25 * GIB));
        assert_eq!(c.records[0].outcome, Outcome::WouldDelete);
        assert!(
            c.records[0].reason.contains("90% >= 85% high water"),
            "{}",
            c.records[0].reason
        );
        // Defaults are on and deleting, with the drone's default roots.
        let d = ReclaimSettings::default();
        assert!(d.enabled && !d.dry_run && d.roots.is_empty());
        assert!(d.validate().is_ok());
    }

    #[test]
    fn failures_are_recorded_and_the_cycle_continues_and_the_cap_applies() {
        let mut f = Fake::new(
            100,
            99,
            vec![
                cand("/t/a", Kind::CargoTarget, 5, 100),
                cand("/t/b", Kind::CargoTarget, 5, 90),
                cand("/t/c", Kind::CargoTarget, 5, 80),
            ],
        );
        f.fail.push("/t/a".into());
        let s = ReclaimSettings {
            max_deletions_per_cycle: 2,
            ..live(ReclaimSettings::default())
        };
        let c = run(&f, &s);
        assert_eq!(c.errors, 1);
        assert_eq!(c.records[0].outcome, Outcome::Failed);
        assert_eq!(c.records[0].error.as_deref(), Some("permission denied"));
        // The failure does not count toward the cap: b and c are deleted.
        assert_eq!(f.deleted.borrow().len(), 2);
    }

    #[test]
    fn safety_is_probed_immediately_before_each_deletion_only_for_tried_items() {
        let f = Fake::new(
            100,
            90,
            vec![
                cand("/t/a", Kind::CargoTarget, 25, 100),
                cand("/t/b", Kind::CargoTarget, 25, 90),
            ],
        );
        run(&f, &live(ReclaimSettings::default()));
        // One deletion reached the target; the second item was never probed.
        assert_eq!(*f.probes.borrow(), 1);
    }

    #[test]
    fn records_are_numbered_and_stats_hand_them_over_once() {
        let f = Fake::new(100, 90, vec![cand("/t/a", Kind::CargoTarget, 25, 100)]);
        let s = live(ReclaimSettings::default());
        let mut seq = 41;
        let c = run_cycle(&f, &s, 1000, &mut seq).unwrap();
        assert_eq!(c.records[0].seq, 42);
        let mut stats = ReclaimStats::default();
        stats.absorb(&s, 1000, &c);
        assert_eq!(
            (stats.reclaimed_bytes_total, stats.last_run_at),
            (25 * GIB, Some(1000))
        );
        assert_eq!(stats.drain_recent(100).0.len(), 1);
        assert!(stats.drain_recent(100).0.is_empty());
    }

    #[test]
    fn ordering_is_deterministic_and_settings_validate() {
        let cands = vec![
            cand("/b/y", Kind::Worktree, 1, 50),
            cand("/b/x", Kind::Worktree, 1, 50),
            cand("/t/z", Kind::CargoTarget, 1, 50),
        ];
        let s = ReclaimSettings::default();
        let a = order(&s, Mode::Pressure, 90, &cands);
        assert_eq!(a, order(&s, Mode::Pressure, 90, &cands));
        let paths: Vec<&str> = a.iter().map(|(c, _)| c.path.to_str().unwrap()).collect();
        assert_eq!(paths, ["/t/z", "/b/x", "/b/y"]);

        assert!(live(ReclaimSettings::default()).validate().is_ok());
        for bad in [
            ReclaimSettings {
                low_water_pct: 90,
                ..ReclaimSettings::default()
            },
            ReclaimSettings {
                critical_pct: 80,
                ..ReclaimSettings::default()
            },
            ReclaimSettings {
                critical_min_age_secs: u64::MAX,
                ..ReclaimSettings::default()
            },
            ReclaimSettings {
                max_deletions_per_cycle: 0,
                ..ReclaimSettings::default()
            },
            ReclaimSettings {
                roots: vec![Root {
                    path: "relative".into(),
                    kind: Kind::Worktree,
                }],
                ..ReclaimSettings::default()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn disk_usage_rounds_down_and_handles_empty_disks() {
        let d = Disk {
            total_bytes: 1000,
            free_bytes: 151,
        };
        assert_eq!(d.usage_pct(), 84);
        assert_eq!(
            Disk {
                total_bytes: 0,
                free_bytes: 0
            }
            .usage_pct(),
            0
        );
    }
}
