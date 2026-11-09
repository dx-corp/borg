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
//! 1. `incremental/` directories under cargo target directories (regenerable),
//!    a static runner's old `_diag` logs and regenerable CI temp entries;
//! 2. whole cargo target directories;
//! 3. regenerable cache directories (per-slot CI tool homes) and whole
//!    build-cache slots (`<cache_root>/<namespace>/slots/<slot>` and the
//!    other entries of a job namespace under `cache_root`), least recently
//!    used first;
//! 4. a static GitHub runner's idle checkouts, tool cache and user caches;
//! 5. git worktrees.
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
//!   `critical_min_age_secs`. A build-cache slot is regenerable and its use is
//!   recorded by the drone, so under pressure it is eligible after
//!   `critical_min_age_secs` and when critical at any age: slots go least
//!   recently used first until free space is back at the target, so the disk
//!   never fills while admission still works. A slot a live lease holds is
//!   busy and never goes (`ReclaimHost::probe`).
//! * **Cap.** `cache_slot_cap_gib` (0: none) bounds the slots of one cache
//!   (`<cache_root>/<namespace>`) together; above it the least recently used
//!   slots go in any mode.
//! * **Hygiene.** Below the high-water mark only items untouched for
//!   `idle_max_age_secs` (7 days; 0 turns it off) are removed.
//! * **Admission.** Free space below the queen's disk-admission floor means
//!   the drone takes no jobs, whatever the percentage: that counts as
//!   critical (a 291 GB disk at 80% still has the 10 GiB floor free, but a
//!   small one does not).
//! * **Free-space target.** A box can declare its own floor
//!   (`free_floor_gib`: dev-desktop's is cargo-slot's 25 GiB build floor,
//!   below which cargo-slot refuses to start a build). The effective floor is
//!   the larger of that and the queen's admission floor, and the target is
//!   twice the floor (one floor of headroom, so the next build does not drop
//!   the disk back under it). Free space below the target is pressure, below
//!   the floor critical, whatever the percentage: on an 810 GiB disk 90% use
//!   still leaves 81 GiB, on a 100 GiB disk 80% leaves only 20.
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

use std::collections::{BTreeMap, BTreeSet};
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
    /// A whole build-cache slot: `<cache_root>/<namespace>/slots/<slot>`, or
    /// another entry of a job namespace under `cache_root` (a publish
    /// target directory, a weekly shared cache). Regenerable; evicted least
    /// recently used first.
    CacheSlot,
    /// A checkout in an idle static GitHub runner's `_work`.
    RunnerWork,
    /// A static runner's tool cache (`_work/_tool`: the toolchains
    /// `setup-*` actions download). Regenerable.
    RunnerToolCache,
    /// A cache a static runner's tools keep in its user's home
    /// (`.cache/go-build`, `.npm`, `.pnpm-store`, `go/pkg/mod/cache`).
    RunnerUserCache,
    /// A static runner's `_diag` directory; only its log files older than
    /// `runner_diag_max_age_secs` go, never the directory.
    RunnerDiag,
    /// A regenerable temp entry a CI tool left in `/tmp` (`go-build*`,
    /// `trivy-*`, …).
    TmpEntry,
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
            Self::CacheSlot => "cache_slot",
            Self::RunnerWork => "runner_work",
            Self::RunnerToolCache => "runner_tool_cache",
            Self::RunnerUserCache => "runner_user_cache",
            Self::RunnerDiag => "runner_diag",
            Self::TmpEntry => "tmp_entry",
            Self::DockerImage => "docker_image",
            Self::DockerContainer => "docker_container",
            Self::DockerBuildCache => "docker_build_cache",
            Self::DockerVolume => "docker_volume",
        }
    }

    /// Kinds every queen understands. Records of other kinds travel in
    /// [`ReclaimStats::recent_ext`], which a queen built before they existed
    /// ignores instead of rejecting the whole heartbeat.
    pub(crate) fn is_wire_v1(self) -> bool {
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
    /// This box's own free-space floor in GiB (0: only the queen's admission
    /// floor). Set it to the floor of the build tool the box runs, e.g.
    /// cargo-slot's `CARGO_MIN_FREE_GB` (25) on dev-desktop. Reclaim keeps
    /// free space at or above twice the effective floor ([`Self::floor_bytes`]).
    pub free_floor_gib: u64,
    /// Cap on the build-cache slots of one cache (`<cache_root>/<namespace>`)
    /// together, in GiB; above it the least recently used slots go in any
    /// mode. 0: no cap, slots go only by pressure and hygiene.
    pub cache_slot_cap_gib: u64,
    /// With default roots, also reclaim what the static GitHub runners on
    /// this host leave: their `_work` checkouts, tool caches and user caches
    /// while the runner has no job, their old `_diag` logs, and regenerable
    /// CI temp entries in `/tmp`.
    pub(crate) static_runner_work: bool,
    /// A static runner's `_diag` log files older than this go, in any mode
    /// (0: never). The runner writes a new log per job and never reads an
    /// old one.
    pub runner_diag_max_age_secs: u64,
    /// A regenerable CI temp entry in `/tmp` untouched this long goes below
    /// high water (0: never by age alone; pressure ages still apply).
    pub tmp_max_age_secs: u64,
    /// The Docker tier.
    pub(crate) docker: DockerReclaimSettings,
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
    pub(crate) enabled: bool,
    /// Unused images older than this (since built or last pulled/tagged).
    pub(crate) image_max_age_secs: u64,
    /// Stopped containers that finished this long ago.
    pub(crate) stopped_container_max_age_secs: u64,
    /// Build cache records unused this long (`builder prune --filter until`).
    pub(crate) build_cache_max_age_secs: u64,
    /// Build cache kept regardless of age (`--reserved-space`, or
    /// `--keep-storage` on older buildx); 0 when critical.
    pub(crate) build_cache_keep_bytes: u64,
    /// Remove dangling anonymous or lease-created volumes.
    pub(crate) prune_dangling_volumes: bool,
    /// Dangling volumes created this long ago.
    pub(crate) volume_max_age_secs: u64,
    /// Running CI service containers (GitHub Actions job services, CI
    /// postgres) started this long ago were leaked by a job that ended
    /// without its cleanup and are force-removed; 0 = never. Containers
    /// without CI provenance are never touched by this rule.
    pub(crate) ci_container_max_age_secs: u64,
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
            ci_container_max_age_secs: 24 * 3600,
        }
    }
}

impl DockerReclaimSettings {
    /// The minimum age for a kind whose hygiene age is `hygiene_secs`.
    /// `None`: nothing of that kind is eligible in this mode.
    pub(crate) fn min_age(
        &self,
        s: &ReclaimSettings,
        mode: Mode,
        hygiene_secs: u64,
    ) -> Option<u64> {
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
            free_floor_gib: 0,
            cache_slot_cap_gib: 0,
            static_runner_work: true,
            runner_diag_max_age_secs: 24 * 3600,
            tmp_max_age_secs: 24 * 3600,
            docker: DockerReclaimSettings::default(),
        }
    }
}

impl ReclaimSettings {
    /// The default for a reclaim-only drone on a host where people build by
    /// hand: dry run, Docker tier off, until the host's own block is set.
    pub(crate) fn for_shared_host() -> Self {
        let mut s = Self {
            dry_run: true,
            ..Self::default()
        };
        s.docker.enabled = false;
        s
    }

    /// The effective free-space floor: the larger of this box's own
    /// (`free_floor_gib`) and `admission_floor_bytes` (the queen's; 0 when
    /// unknown). Reclaim's target is twice this.
    pub fn floor_bytes(&self, admission_floor_bytes: u64) -> u64 {
        admission_floor_bytes.max(self.free_floor_gib.saturating_mul(1 << 30))
    }

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
        if self.free_floor_gib > 1024 {
            return Err("reclaim free_floor_gib is at most 1024".into());
        }
        if self.cache_slot_cap_gib > 65_536 {
            return Err("reclaim cache_slot_cap_gib is at most 65536".into());
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
    /// The candidate that contains this one (an `incremental/`'s target
    /// directory, a slot's target or tool home), so bytes freed inside it are
    /// not counted twice when it goes too.
    pub within: Option<PathBuf>,
    /// For a build-cache slot, the cache it belongs to
    /// (`<cache_root>/<namespace>`); `cache_slot_cap_gib` applies per cache.
    pub cache: Option<PathBuf>,
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
    /// Free bytes the cycle saw before deleting anything.
    pub free_bytes_before: u64,
    /// The effective free-space floor it worked to ([`ReclaimSettings::floor_bytes`]).
    pub floor_bytes: u64,
    pub records: Vec<DeletionRecord>,
    /// Candidates kept, by reason.
    pub kept: BTreeMap<String, u32>,
    /// Deletions (or would-be deletions in a dry run), by kind.
    pub evicted: BTreeMap<String, u32>,
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
    /// Free bytes the last run saw (the tightest filesystem it reclaims).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_bytes: Option<u64>,
    /// The effective free-space floor of the last run; reclaim keeps free
    /// space at or above twice this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor_bytes: Option<u64>,
    pub last_run_mode: Option<Mode>,
    /// Candidates the last run kept, by reason.
    pub kept: BTreeMap<String, u32>,
    /// What the last run deleted (or would have, dry run), by kind; the
    /// build-cache slots it evicted show as `cache_slot`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub evicted: BTreeMap<String, u32>,
    /// Audit entries created since the previous heartbeat; the queen stores
    /// them and ignores any it already has (by `seq`). Only
    /// `Kind::is_wire_v1` kinds.
    #[serde(default)]
    pub recent: Vec<DeletionRecord>,
    /// Audit entries of the newer kinds (cache dirs, Docker), kept apart so a
    /// queen that predates them still accepts the heartbeat.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) recent_ext: Vec<DeletionRecord>,
    /// Bytes the Docker tier freed since the drone started (also counted in
    /// `reclaimed_bytes_total`).
    #[serde(default)]
    pub(crate) docker_reclaimed_bytes_total: u64,
    /// Bytes the last Docker tier run freed.
    #[serde(default)]
    pub(crate) last_run_docker_reclaimed_bytes: u64,
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
        self.free_bytes = Some(cycle.free_bytes_before);
        self.floor_bytes = Some(cycle.floor_bytes);
        self.last_run_mode = Some(cycle.mode);
        self.kept = cycle.kept.clone();
        self.evicted = cycle.evicted.clone();
        self.push_records(&cycle.records);
    }

    /// Fold a Docker tier run into the stats of the cycle just absorbed: its
    /// bytes count toward the run and the running totals, its errors toward
    /// the run's errors, and its keep reasons join the run's.
    pub(crate) fn absorb_docker(&mut self, cycle: &Cycle) {
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
        for (k, v) in &cycle.evicted {
            *self.evicted.entry(k.clone()).or_default() += v;
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
        Kind::CargoIncremental | Kind::RunnerDiag | Kind::TmpEntry => 0,
        Kind::CargoTarget => 1,
        Kind::CacheDir | Kind::CacheSlot => 2,
        Kind::RunnerWork | Kind::RunnerToolCache | Kind::RunnerUserCache => 3,
        Kind::Worktree => 4,
        // The Docker tier orders its own work.
        Kind::DockerImage | Kind::DockerContainer | Kind::DockerBuildCache | Kind::DockerVolume => {
            5
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

/// The mode for this disk state with `floor_bytes` the effective free-space
/// floor ([`ReclaimSettings::floor_bytes`]): below the floor is critical,
/// below twice the floor (the target) is pressure.
pub(crate) fn mode_for_floor(s: &ReclaimSettings, disk: &Disk, floor_bytes: u64) -> Mode {
    let u = disk.usage_pct();
    if u >= s.critical_pct || (floor_bytes > 0 && disk.free_bytes < floor_bytes) {
        Mode::Critical
    } else if u >= s.high_water_pct
        || (floor_bytes > 0 && disk.free_bytes < floor_bytes.saturating_mul(2))
    {
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
    order_for(
        s,
        mode,
        &format!("disk {usage_pct}% >= {}% high water", s.high_water_pct),
        cands,
    )
}

/// The minimum idle age for a candidate of `kind` in `mode`; `None` when
/// nothing of that kind is eligible. A build-cache slot is regenerable and
/// its last use is recorded, so under pressure it waits only the critical
/// age and when critical none: the least recently used slots go until free
/// space is back at the target, before admission fails for want of disk.
fn min_age_for(s: &ReclaimSettings, mode: Mode, kind: Kind) -> Option<u64> {
    match (mode, kind) {
        (Mode::Idle, _) => None,
        // Old diagnostics logs go by their own age whatever the disk looks
        // like; nothing reads them.
        (_, Kind::RunnerDiag) => {
            (s.runner_diag_max_age_secs > 0).then_some(s.runner_diag_max_age_secs)
        }
        (Mode::Critical, Kind::CacheSlot) => Some(0),
        (Mode::Pressure, Kind::CacheSlot) => Some(s.critical_min_age_secs),
        (Mode::Critical, _) => Some(s.critical_min_age_secs),
        (Mode::Pressure, _) => Some(s.min_age_secs),
        (Mode::Hygiene, Kind::TmpEntry) => (s.tmp_max_age_secs > 0).then_some(s.tmp_max_age_secs),
        (Mode::Hygiene, _) => Some(s.idle_max_age_secs),
    }
}

/// Oldest first within a tier: the least recently used goes before the
/// more recently used, then the larger before the smaller.
fn lru(a: &Candidate, b: &Candidate) -> std::cmp::Ordering {
    tier(a.kind)
        .cmp(&tier(b.kind))
        .then(b.idle_secs.cmp(&a.idle_secs))
        .then(b.size_bytes.cmp(&a.size_bytes))
        .then(a.path.cmp(&b.path))
}

/// [`order`] with the pressure reason spelled out by the caller (the disk
/// percentage, or free space under the target).
fn order_for(
    s: &ReclaimSettings,
    mode: Mode,
    pressure: &str,
    cands: &[Candidate],
) -> Vec<(Candidate, String)> {
    let mut out: Vec<(Candidate, String)> = cands
        .iter()
        .filter_map(|c| {
            let min_age = min_age_for(s, mode, c.kind)?;
            (c.idle_secs >= min_age).then(|| {
                let why = match (mode, c.kind) {
                    (Mode::Hygiene, _) => format!(
                        "idle {} (>= {} hygiene limit)",
                        hours(c.idle_secs),
                        hours(min_age)
                    ),
                    (_, Kind::CacheSlot) => format!(
                        "{pressure} ({mode:?}); least recently used, idle {}",
                        hours(c.idle_secs)
                    ),
                    _ => format!(
                        "{pressure} ({mode:?}); idle {} (>= {})",
                        hours(c.idle_secs),
                        hours(min_age)
                    ),
                };
                (c.clone(), why)
            })
        })
        .collect();
    out.sort_by(|(a, _), (b, _)| lru(a, b));
    out
}

/// One cycle's deletions: the safety re-probe, the audit record and the
/// byte accounting shared by the pressure pass and the cap pass.
struct Deletions<'a> {
    host: &'a dyn ReclaimHost,
    settings: &'a ReclaimSettings,
    cycle: &'a mut Cycle,
    seq: &'a mut u64,
    now: i64,
    /// Bytes freed (or, dry run, that would be).
    freed: u64,
    /// Bytes already freed inside a containing candidate, so a later deletion
    /// of the whole directory is not counted twice.
    freed_within: BTreeMap<PathBuf, u64>,
    count: u32,
    /// Paths removed this cycle (or that would be), so a later pass does not
    /// try them again or count them as present.
    gone: BTreeSet<PathBuf>,
}

impl Deletions<'_> {
    fn capped(&self) -> bool {
        self.count >= self.settings.max_deletions_per_cycle
    }

    /// Try to remove `c` for `eligible`; the bytes credited when it went (or
    /// would have gone), `None` when it was kept or the deletion failed.
    fn attempt(&mut self, c: &Candidate, eligible: &str) -> Option<u64> {
        if self.gone.iter().any(|g| c.path.starts_with(g)) {
            return None;
        }
        // Re-probe now, not at scan time.
        let safety = self.host.probe(c);
        if let Some(why) = safety.blocker(c.kind) {
            *self.cycle.kept.entry(why).or_default() += 1;
            return None;
        }
        let already = self.freed_within.get(&c.path).copied().unwrap_or(0);
        let size = c.size_bytes.saturating_sub(already);
        let reason = format!(
            "{eligible}; {} {}; tier {}",
            c.kind.as_str(),
            gib(size),
            tier(c.kind) + 1
        );
        *self.seq += 1;
        let mut rec = DeletionRecord {
            seq: *self.seq,
            at: self.now,
            path: c.path.display().to_string(),
            kind: c.kind,
            size_bytes: size,
            reason,
            outcome: Outcome::WouldDelete,
            error: None,
        };
        if self.settings.dry_run {
            self.cycle.would_reclaim_bytes = self.cycle.would_reclaim_bytes.saturating_add(size);
        } else {
            match self.host.delete(c) {
                Ok(bytes) => {
                    rec.outcome = Outcome::Deleted;
                    // What the host says it freed, but never more than the
                    // estimate net of what an earlier deletion already took.
                    rec.size_bytes = bytes.min(size);
                    self.cycle.reclaimed_bytes =
                        self.cycle.reclaimed_bytes.saturating_add(rec.size_bytes);
                }
                Err(e) => {
                    rec.outcome = Outcome::Failed;
                    rec.error = Some(e);
                    self.cycle.errors += 1;
                    self.cycle.records.push(rec);
                    return None;
                }
            }
        }
        let credited = if self.settings.dry_run {
            size
        } else {
            rec.size_bytes
        };
        self.freed = self.freed.saturating_add(credited);
        self.count += 1;
        if let Some(parent) = &c.within {
            *self.freed_within.entry(parent.clone()).or_default() += credited;
        }
        *self
            .cycle
            .evicted
            .entry(c.kind.as_str().to_owned())
            .or_default() += 1;
        self.gone.insert(c.path.clone());
        self.cycle.records.push(rec);
        Some(credited)
    }
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
    let floor = settings.floor_bytes(host.admission_floor_bytes());
    let mode = mode_for_floor(settings, &disk, floor);
    let mut cycle = Cycle {
        mode,
        usage_pct_before: before,
        usage_pct_after: before,
        free_bytes_before: disk.free_bytes,
        floor_bytes: floor,
        records: Vec::new(),
        kept: BTreeMap::new(),
        evicted: BTreeMap::new(),
        reclaimed_bytes: 0,
        would_reclaim_bytes: 0,
        errors: 0,
    };
    let cap = settings.cache_slot_cap_gib.saturating_mul(1 << 30);
    if mode == Mode::Idle && cap == 0 {
        return Ok(cycle);
    }
    let cands = host.scan(&settings.roots)?;
    // Under pressure, free until usage is projected at or below low water
    // (when the percentage is what crossed high water) and free space is
    // back at the target. Hygiene has no byte target: it removes everything
    // that is old enough.
    let pct_target = if before >= settings.high_water_pct {
        disk.used()
            .saturating_sub(disk.total_bytes / 100 * u64::from(settings.low_water_pct))
    } else {
        0
    };
    let target_free = match mode {
        Mode::Hygiene => u64::MAX,
        Mode::Idle => 0,
        _ => pct_target
            // Below the target, at least back to it: the floor plus the
            // same margin again, so the next build does not drop it back.
            .max(floor.saturating_mul(2).saturating_sub(disk.free_bytes)),
    };
    let pressure = if floor > 0 && disk.free_bytes < floor.saturating_mul(2) {
        format!(
            "free {} < {} target (2 x {} floor)",
            gib(disk.free_bytes),
            gib(floor.saturating_mul(2)),
            gib(floor)
        )
    } else {
        format!("disk {before}% >= {}% high water", settings.high_water_pct)
    };
    let mut run = Deletions {
        host,
        settings,
        cycle: &mut cycle,
        seq,
        now,
        freed: 0,
        freed_within: BTreeMap::new(),
        count: 0,
        gone: BTreeSet::new(),
    };
    for (c, eligible) in order_for(settings, mode, &pressure, &cands) {
        if run.freed >= target_free || run.capped() {
            break;
        }
        run.attempt(&c, &eligible);
    }
    // The cap: each cache's slots together stay under it, least recently
    // used going first, whatever the disk looks like.
    if cap > 0 {
        let mut by_cache: BTreeMap<PathBuf, Vec<&Candidate>> = BTreeMap::new();
        for c in cands.iter().filter(|c| c.kind == Kind::CacheSlot) {
            if let Some(cache) = &c.cache
                && !run.gone.iter().any(|g| c.path.starts_with(g))
            {
                by_cache.entry(cache.clone()).or_default().push(c);
            }
        }
        for (cache, mut slots) in by_cache {
            let mut total: u64 = slots.iter().map(|c| c.size_bytes).sum();
            if total <= cap {
                continue;
            }
            slots.sort_by(|a, b| lru(a, b));
            for c in slots {
                if total <= cap || run.capped() {
                    break;
                }
                let eligible = format!(
                    "cache {} holds {} > {} cap; least recently used, idle {}",
                    cache.display(),
                    gib(total),
                    gib(cap),
                    hours(c.idle_secs)
                );
                if run.attempt(c, &eligible).is_some() {
                    total = total.saturating_sub(c.size_bytes);
                }
            }
        }
    }
    let freed = run.freed;
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
            cache: None,
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
    fn a_box_floor_makes_a_large_disk_reclaim_back_to_twice_the_floor() {
        // dev-desktop: 810 GiB at 95% leaves 40 GiB; cargo-slot refuses builds
        // under 25 GiB, so the target is 50 GiB free whatever the percentage.
        let s = ReclaimSettings {
            free_floor_gib: 25,
            critical_pct: 99,
            high_water_pct: 98,
            low_water_pct: 50,
            ..ReclaimSettings::default()
        };
        let f = Fake::new(
            810,
            770,
            vec![
                cand("/t/a", Kind::CargoTarget, 6, 50),
                cand("/t/b", Kind::CargoTarget, 6, 40),
                cand("/t/c", Kind::CargoTarget, 6, 30),
                cand("/t/d", Kind::CargoTarget, 6, 20),
            ],
        );
        let c = run(&f, &live(s.clone()));
        assert_eq!(c.mode, Mode::Pressure);
        assert_eq!((c.free_bytes_before, c.floor_bytes), (40 * GIB, 25 * GIB));
        // 10 GiB short of the 50 GiB target: the two oldest go, no more.
        let gone: Vec<String> = f
            .deleted
            .borrow()
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        assert_eq!(gone, ["/t/a", "/t/b"]);
        assert!(
            c.records[0]
                .reason
                .contains("free 40.0 GiB < 50.0 GiB target"),
            "{}",
            c.records[0].reason
        );
        // Under the floor itself it is critical (1h minimum age).
        let f = Fake::new(810, 790, vec![cand("/t/young", Kind::CargoTarget, 6, 2)]);
        let c = run(&f, &live(s.clone()));
        assert_eq!(c.mode, Mode::Critical);
        assert_eq!(f.deleted.borrow().len(), 1);
        // At the target: hygiene, the young directory stays.
        let f = Fake::new(810, 700, vec![cand("/t/young", Kind::CargoTarget, 6, 2)]);
        assert_eq!(run(&f, &live(s.clone())).mode, Mode::Hygiene);
        assert!(f.deleted.borrow().is_empty());
        // The queen's admission floor counts when it is the larger one.
        assert_eq!(s.floor_bytes(40 * GIB), 40 * GIB);
        assert_eq!(s.floor_bytes(0), 25 * GIB);
        assert_eq!(ReclaimSettings::default().floor_bytes(0), 0);
        assert!(
            ReclaimSettings {
                free_floor_gib: 2000,
                ..ReclaimSettings::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn stats_carry_the_free_space_and_floor_of_the_last_run() {
        let s = ReclaimSettings {
            free_floor_gib: 25,
            ..ReclaimSettings::default()
        };
        let f = Fake::new(100, 20, Vec::new());
        let c = run(&f, &live(s.clone()));
        let mut stats = ReclaimStats::default();
        stats.absorb(&s, 1000, &c);
        assert_eq!(
            (stats.free_bytes, stats.floor_bytes),
            (Some(80 * GIB), Some(25 * GIB))
        );
        // An old queen that never heard of the fields still reads the stats,
        // and stats from an old drone read as unknown.
        let wire = serde_json::to_value(&stats).unwrap();
        assert_eq!(wire["free_bytes"], 80 * GIB);
        let old: ReclaimStats = serde_json::from_value(serde_json::json!({
            "enabled": true, "dry_run": false, "reclaimed_bytes_total": 0,
            "last_run_at": null, "last_run_reclaimed_bytes": 0,
            "last_run_would_reclaim_bytes": 0, "last_run_errors": 0,
            "usage_pct": 50, "last_run_mode": null, "kept": {}
        }))
        .unwrap();
        assert_eq!((old.free_bytes, old.floor_bytes), (None, None));
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

    fn slot(path: &str, cache: &str, gib: u64, idle_h: u64) -> Candidate {
        Candidate {
            cache: Some(PathBuf::from(cache)),
            ..cand(path, Kind::CacheSlot, gib, idle_h)
        }
    }

    #[test]
    fn critical_evicts_build_cache_slots_least_recently_used_first_whatever_their_age() {
        // 100 GiB disk with 2 GiB free under a 10 GiB floor: critical, and
        // the target is back to 20 GiB free, so 18 GiB must go.
        let mut f = Fake::new(
            100,
            98,
            vec![
                slot("/c/ci/slots/a", "/c/ci", 8, 0),
                slot("/c/ci/slots/b", "/c/ci", 8, 2),
                slot("/c/ci/slots/held", "/c/ci", 30, 5),
                slot("/c/ci/slots/c", "/c/ci", 8, 1),
                // Not a slot: the usual critical age still applies.
                cand("/t/young", Kind::CargoTarget, 40, 0),
            ],
        );
        f.safety.insert(
            PathBuf::from("/c/ci/slots/held"),
            Safety {
                busy: true,
                ..Safety::default()
            },
        );
        let s = ReclaimSettings {
            free_floor_gib: 10,
            ..live(ReclaimSettings::default())
        };
        let c = run(&f, &s);
        assert_eq!(c.mode, Mode::Critical);
        let order: Vec<String> = f
            .deleted
            .borrow()
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        // The held slot is skipped (busy), then least recently used first;
        // the slot used this hour goes too because the floor is not met
        // without it; the young target directory never qualifies.
        assert_eq!(order, ["/c/ci/slots/b", "/c/ci/slots/c", "/c/ci/slots/a"]);
        assert_eq!(c.kept.get("busy"), Some(&1));
        assert_eq!(c.evicted.get("cache_slot"), Some(&3));
        assert!(c.reclaimed_bytes >= 18 * GIB, "{}", c.reclaimed_bytes);
    }

    #[test]
    fn pressure_takes_a_slot_only_after_the_critical_age_and_hygiene_after_a_week() {
        let items = vec![
            slot("/c/ci/slots/fresh", "/c/ci", 10, 0),
            slot("/c/ci/slots/hour", "/c/ci", 10, 1),
        ];
        // 90% of 100 GiB: pressure. Only the slot idle for the critical age
        // (1 h) is eligible; the usual 6 h minimum does not apply to slots.
        let f = Fake::new(100, 90, items.clone());
        let c = run(&f, &live(ReclaimSettings::default()));
        assert_eq!(c.mode, Mode::Pressure);
        assert_eq!(
            f.deleted.borrow().as_slice(),
            [PathBuf::from("/c/ci/slots/hour")]
        );
        // Below high water a slot is hygiene like anything else: a week.
        let f = Fake::new(100, 50, items);
        let c = run(&f, &live(ReclaimSettings::default()));
        assert_eq!(c.mode, Mode::Hygiene);
        assert!(f.deleted.borrow().is_empty());
        let f = Fake::new(100, 50, vec![slot("/c/ci/slots/old", "/c/ci", 10, 24 * 8)]);
        run(&f, &live(ReclaimSettings::default()));
        assert_eq!(
            f.deleted.borrow().as_slice(),
            [PathBuf::from("/c/ci/slots/old")]
        );
    }

    #[test]
    fn a_cache_cap_evicts_that_cache_s_least_recently_used_slots_in_any_mode() {
        // 50% used, hygiene off: an idle disk. Cache /c/ci holds 14 GiB
        // against a 10 GiB cap; /c/pub holds 4 GiB and is left alone.
        let mut f = Fake::new(
            100,
            50,
            vec![
                slot("/c/ci/slots/a", "/c/ci", 6, 1),
                slot("/c/ci/slots/b", "/c/ci", 6, 3),
                slot("/c/ci/slots/held", "/c/ci", 2, 9),
                slot("/c/pub/platform-api", "/c/pub", 4, 48),
                // A target inside a slot that goes is not tried on its own.
                Candidate {
                    within: Some(PathBuf::from("/c/ci/slots/b")),
                    ..cand("/c/ci/slots/b/cargo-target/w", Kind::CargoTarget, 5, 3)
                },
            ],
        );
        f.safety.insert(
            PathBuf::from("/c/ci/slots/held"),
            Safety {
                busy: true,
                ..Safety::default()
            },
        );
        let s = ReclaimSettings {
            idle_max_age_secs: 0,
            cache_slot_cap_gib: 10,
            ..live(ReclaimSettings::default())
        };
        let c = run(&f, &s);
        assert_eq!(c.mode, Mode::Idle);
        // The held slot is skipped and counted; the least recently used of
        // the rest goes and the cache is under its cap, so /a stays.
        assert_eq!(
            f.deleted.borrow().as_slice(),
            [PathBuf::from("/c/ci/slots/b")]
        );
        assert_eq!(c.kept.get("busy"), Some(&1));
        assert_eq!(c.evicted.get("cache_slot"), Some(&1));
        assert!(
            c.records
                .iter()
                .any(|r| r.reason.contains("holds 14.0 GiB > 10.0 GiB cap")),
            "{:?}",
            c.records
        );
        // Without a cap the idle disk reclaims nothing.
        let f = Fake::new(100, 50, vec![slot("/c/ci/slots/b", "/c/ci", 6, 3)]);
        let s = ReclaimSettings {
            idle_max_age_secs: 0,
            ..live(ReclaimSettings::default())
        };
        assert!(run(&f, &s).records.is_empty());
        assert!(
            ReclaimSettings {
                cache_slot_cap_gib: 70_000,
                ..ReclaimSettings::default()
            }
            .validate()
            .is_err()
        );
    }

    /// Old diagnostics logs go by their own age in every mode and never by
    /// the hygiene age; a temp entry waits the temp age below high water and
    /// the pressure ages above it; both are the cheapest tier.
    #[test]
    fn runner_diagnostics_and_temp_entries_go_by_their_own_ages() {
        let items = vec![
            cand("/home/r1/actions-runner/_diag", Kind::RunnerDiag, 1, 30),
            cand("/home/r2/actions-runner/_diag", Kind::RunnerDiag, 1, 12),
            cand("/tmp/go-build1", Kind::TmpEntry, 2, 30),
            cand("/tmp/go-build2", Kind::TmpEntry, 2, 12),
            cand(
                "/home/r1/actions-runner/_work/_tool",
                Kind::RunnerToolCache,
                7,
                30,
            ),
        ];
        let f = Fake::new(100, 50, items.clone());
        let c = run(&f, &live(ReclaimSettings::default()));
        assert_eq!(c.mode, Mode::Hygiene);
        assert_eq!(
            f.deleted.borrow().as_slice(),
            [
                PathBuf::from("/tmp/go-build1"),
                PathBuf::from("/home/r1/actions-runner/_diag")
            ],
            "a day old, not the 12 h ones; the tool cache waits the hygiene week"
        );
        assert_eq!(c.evicted.get("runner_diag"), Some(&1));
        // Under pressure the 12 h entries qualify by the 6 h pressure age,
        // and the tool cache by it too; the 12 h diag still waits its day.
        let f = Fake::new(100, 90, items.clone());
        let c = run(&f, &live(ReclaimSettings::default()));
        assert_eq!(c.mode, Mode::Pressure);
        let got: Vec<String> = f
            .deleted
            .borrow()
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        // Same tier, same age: the larger first.
        assert_eq!(
            got,
            [
                "/tmp/go-build1",
                "/home/r1/actions-runner/_diag",
                "/tmp/go-build2",
                "/home/r1/actions-runner/_work/_tool"
            ]
        );
        // Both ages at 0 turn them off below high water.
        let f = Fake::new(100, 50, items);
        let s = ReclaimSettings {
            runner_diag_max_age_secs: 0,
            tmp_max_age_secs: 0,
            ..live(ReclaimSettings::default())
        };
        assert!(run(&f, &s).records.is_empty());
    }

    #[test]
    fn stats_report_the_last_run_s_evictions_by_kind() {
        let f = Fake::new(
            100,
            96,
            vec![
                cand("/t/a", Kind::CargoTarget, 10, 2),
                slot("/c/ci/slots/a", "/c/ci", 10, 0),
            ],
        );
        let c = run(&f, &live(ReclaimSettings::default()));
        let mut stats = ReclaimStats::default();
        stats.absorb(&ReclaimSettings::default(), 1000, &c);
        assert_eq!(stats.evicted.get("cargo_target"), Some(&1));
        assert_eq!(stats.evicted.get("cache_slot"), Some(&1));
        let json = serde_json::to_value(&stats).unwrap();
        assert_eq!(json["evicted"]["cache_slot"], 1);
    }
}
