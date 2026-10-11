//! The queen: one reconciliation pass ([`Queen::tick`]) turns the GitHub
//! queue plus drone heartbeats into minted leases.
//!
//! A tick:
//! 1. lists the organization's runners and reconciles every active lease with
//!    what GitHub reports (busy, gone, never came online);
//! 2. deletes stale `<prefix>-*` registrations that no lease owns;
//! 3. reads queued jobs and derives demand and queue ages per label;
//! 4. runs the pure allocator and mints a JIT runner for each grant.
//!
//! Any GitHub failure before minting aborts the tick without minting: acting
//! on a partial view of the queue would over- or under-grant.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

use rand::RngCore;
use rand::rngs::OsRng;
use serde::Serialize;
use time::OffsetDateTime;

use crate::allocator::{
    ActiveLease, AllocatorInput, ClassIdle, Demand, DroneState, Grant, LeaseState, QueueAge,
    Reservation, Unmet, WarmFloor, allocate, max_slots,
};
use crate::bid::{ClassPressure, ScoreWeights};
use crate::classes::{Class, Scope};
use crate::config::QueenConfig;
use crate::envcontract::blocked_classes;
use crate::github::{GithubClient, GithubError, QueuedJob, Runner};
use crate::health::{DroneHealth, DroneInfo, Outcome, OutcomeKind, evaluate};
use crate::homeostasis::{Controller, KeyInput, KeyReport};
use crate::janitor::Janitor;
use crate::probe::Capacity;
use crate::profile::{ClassProfile, build_profiles};
use crate::rollout::{
    Check as RolloutCheck, DroneView, Observed as RolloutObserved, RolloutState, advance,
    desired_for,
};
use crate::store::{LeaseRow, LeaseStatus, Store, StoreError};
use crate::tailnet::TailnetEnroll;
use crate::telemetry::{Conclusion, FailureClass, MAX_REPORT_ROWS, TelemetryRow};
use crate::units::{Unit, UnitState};
use crate::wake::{UdpWakeSender, WakePlanner, WakeSender, WakeView, parse_mac};

#[derive(Debug, thiserror::Error)]
pub enum QueenError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Github(#[from] GithubError),
}

/// What the last tick saw. Served by `/status` and `/metrics`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Snapshot {
    pub last_tick_at: Option<i64>,
    pub last_error: Option<String>,
    pub queue: Vec<QueueEntry>,
    pub reserved_for: Vec<Reservation>,
    pub unmet: Vec<Unmet>,
    /// The warm floors in force at the last pass (base plus schedule).
    pub warm_floors: Vec<WarmFloor>,
    /// Demand pressure per class from the last pass; published to drones.
    pub pressure: Vec<ClassPressure>,
    /// Per-drone host health from the last assessment.
    pub health: BTreeMap<String, DroneHealth>,
    /// Host failures look fleet-wide, so automatic quarantine is paused.
    pub health_systemic: bool,
    /// Slots each class could still be granted, from the last pass.
    pub idle: Vec<ClassIdle>,
    /// Warm-floor controller state per key (empty when off).
    pub homeostasis: Vec<KeyReport>,
    /// Learned per-drone, per-class profiles (empty when off).
    pub profiles: Vec<ClassProfile>,
    /// The gated rollout after the last pass; `None` when not gated.
    pub rollout: Option<RolloutView>,
    pub wake: WakeView,
    /// Non-borg online runners by label, from the last pass.
    pub static_runners: BTreeMap<String, StaticRunners>,
    /// `unmet` minus what idle non-borg runners with the label can take.
    pub unmet_placeable: Vec<Unmet>,
}

/// Runners in the organization that borg did not mint, for one label.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct StaticRunners {
    pub online: u32,
    /// Online and not running a job: able to take a queued job right now.
    pub idle: u32,
}

/// Group the organization's non-borg online runners by label.
fn static_runners(runners: &[Runner], borg_prefix: &str) -> BTreeMap<String, StaticRunners> {
    let mut out: BTreeMap<String, StaticRunners> = BTreeMap::new();
    for r in runners
        .iter()
        .filter(|r| !r.name.starts_with(borg_prefix) && r.status == "online")
    {
        for l in &r.labels {
            let e = out.entry(l.name.clone()).or_default();
            e.online += 1;
            if !r.busy {
                e.idle += 1;
            }
        }
    }
    out
}

/// Demand borg itself has to place: each unmet count less the idle non-borg
/// runners that carry the job's label (an estimate: GitHub picks the runner).
fn placeable_unmet(
    unmet: &[Unmet],
    statics: &BTreeMap<String, StaticRunners>,
    classes: &crate::classes::Classes,
) -> Vec<Unmet> {
    unmet
        .iter()
        .filter_map(|u| {
            let idle = statics
                .get(&classes.job_label(u.class, u.scope))
                .map_or(0, |s| s.idle);
            let count = u.count.saturating_sub(idle);
            (count > 0).then_some(Unmet { count, ..*u })
        })
        .collect()
}

/// What `/status` shows of a gated rollout.
#[derive(Debug, Clone, Serialize)]
pub struct RolloutView {
    pub state: RolloutState,
    pub checks: Vec<RolloutCheck>,
    pub waiting: Option<String>,
    /// Jobs on drones told the target, since they got it.
    pub new_group: RolloutObserved,
    /// Jobs on the other drones in the same period.
    pub control_group: RolloutObserved,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueueEntry {
    pub label: String,
    pub class: Class,
    pub scope: Scope,
    pub queued: u32,
    pub oldest_age_secs: u64,
}

pub struct Queen {
    pub(crate) cfg: QueenConfig,
    pub(crate) store: Store,
    github: GithubClient,
    snapshot: RwLock<Snapshot>,
    janitor: Option<Janitor>,
    tailnet: Option<TailnetEnroll>,
    homeostasis: std::sync::Mutex<Controller>,
    profile_cache: std::sync::Mutex<Option<(i64, Vec<ClassProfile>)>>,
    wake: std::sync::Mutex<WakePlanner>,
    unit_joins: AtomicU64,
    unit_receipts: AtomicU64,
    wake_sender: Arc<dyn WakeSender>,
    tick_lock: tokio::sync::Mutex<()>,
}

/// The full label set a JIT runner is registered with: the labels GitHub
/// assigns to every self-hosted runner, then the grant's class labels. They
/// are sent explicitly because the API documents only "custom" labels.
pub fn jit_labels(os: &str, arch: &str, extra: &[String], class_labels: &[String]) -> Vec<String> {
    let mut labels = vec!["self-hosted".to_owned()];
    match os {
        "linux" => labels.push("Linux".to_owned()),
        "macos" => labels.push("macOS".to_owned()),
        "windows" => labels.push("Windows".to_owned()),
        _ => {}
    }
    match arch {
        "x86_64" | "x86" => labels.push("X64".to_owned()),
        "aarch64" | "arm64" => labels.push("ARM64".to_owned()),
        "arm" => labels.push("ARM".to_owned()),
        _ => {}
    }
    labels.extend(extra.iter().cloned());
    labels.extend(class_labels.iter().cloned());
    labels
}

/// Reclaim audit entries are kept this long (90 days).
const RECLAIM_LOG_KEEP_SECS: i64 = 90 * 86_400;

/// How a finished job counts for host health. Skipped jobs say nothing.
fn outcome_of(row: &TelemetryRow) -> Option<Outcome> {
    let t = &row.telemetry;
    let kind = match (t.conclusion, t.failure_class) {
        (Conclusion::Skipped, _) => return None,
        (Conclusion::Success, _) => OutcomeKind::Ok,
        (_, FailureClass::Host) => OutcomeKind::HostFailure,
        _ => OutcomeKind::Other,
    };
    Some(Outcome {
        drone: row.drone.clone(),
        lease_id: row.lease_id.clone(),
        repo: t.repo.clone(),
        run_id: t.run_id,
        job_name: t.job_name.clone(),
        finished_at: row.finished_at,
        kind,
    })
}

/// A fresh work unit id.
pub fn new_unit_id() -> String {
    let mut b = [0u8; 8];
    OsRng.fill_bytes(&mut b);
    format!("u-{}", hex::encode(b))
}

/// `<os>-<arch>` as units name platforms (`linux-x86_64`, `macos-aarch64`).
pub fn platform_of(cap: &Capacity) -> String {
    let arch = match cap.arch.as_str() {
        "arm64" => "aarch64",
        "amd64" => "x86_64",
        other => other,
    };
    format!("{}-{arch}", cap.os)
}

fn new_lease_id() -> String {
    let mut b = [0u8; 6];
    OsRng.fill_bytes(&mut b);
    hex::encode(b)
}

fn age_secs(now: OffsetDateTime, then: OffsetDateTime) -> u64 {
    u64::try_from((now - then).whole_seconds()).unwrap_or(0)
}

impl Queen {
    pub fn new(cfg: QueenConfig, store: Store, github: GithubClient) -> Self {
        Self {
            cfg,
            store,
            github,
            snapshot: RwLock::new(Snapshot::default()),
            janitor: None,
            tailnet: None,
            homeostasis: std::sync::Mutex::new(Controller::new()),
            profile_cache: std::sync::Mutex::new(None),
            wake: std::sync::Mutex::new(WakePlanner::new()),
            unit_joins: AtomicU64::new(0),
            unit_receipts: AtomicU64::new(0),
            wake_sender: Arc::new(UdpWakeSender),
            tick_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Attach the opt-in supersede janitor; `api::serve` runs it.
    #[must_use]
    pub fn with_janitor(mut self, janitor: Janitor) -> Self {
        self.janitor = Some(janitor);
        self
    }

    /// Enable `POST /v1/enroll` with `verifier` deciding who the caller is.
    #[must_use]
    pub fn with_tailnet_enroll(mut self, enroll: TailnetEnroll) -> Self {
        self.tailnet = Some(enroll);
        self
    }

    /// Replace how magic packets are sent (tests use a recording fake).
    #[must_use]
    pub fn with_wake_sender(mut self, sender: Arc<dyn WakeSender>) -> Self {
        self.wake_sender = sender;
        self
    }

    pub fn count_unit_join(&self) {
        self.unit_joins.fetch_add(1, Ordering::Relaxed);
    }

    pub fn count_unit_receipt(&self) {
        self.unit_receipts.fetch_add(1, Ordering::Relaxed);
    }

    /// Submissions that joined a running unit / reused a receipt since start.
    pub fn unit_reuse_counts(&self) -> (u64, u64) {
        (
            self.unit_joins.load(Ordering::Relaxed),
            self.unit_receipts.load(Ordering::Relaxed),
        )
    }

    pub fn tailnet_enroll(&self) -> Option<&TailnetEnroll> {
        self.tailnet.as_ref()
    }

    pub fn janitor(&self) -> Option<&Janitor> {
        self.janitor.as_ref()
    }

    pub fn config(&self) -> &QueenConfig {
        &self.cfg
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.read().map(|s| s.clone()).unwrap_or_default()
    }

    fn update_snapshot(&self, f: impl FnOnce(&mut Snapshot)) {
        if let Ok(mut s) = self.snapshot.write() {
            f(&mut s);
        }
    }

    pub fn record_tick_error(&self, now: OffsetDateTime, err: &QueenError) {
        self.update_snapshot(|s| {
            s.last_tick_at = Some(now.unix_timestamp());
            s.last_error = Some(err.to_string());
        });
    }

    fn runner_prefix(&self) -> String {
        format!("{}-", self.cfg.runner_prefix)
    }

    /// One reconciliation pass.
    pub async fn tick(&self, now: OffsetDateTime) -> Result<(), QueenError> {
        let _serial = self.tick_lock.lock().await;
        let now_ts = now.unix_timestamp();

        let runners = self.github.list_runners().await?;
        let prefix = self.runner_prefix();
        let ours: HashMap<&str, &Runner> = runners
            .iter()
            .filter(|r| r.name.starts_with(&prefix))
            .map(|r| (r.name.as_str(), r))
            .collect();

        let statics = static_runners(&runners, &prefix);
        let active = self.reconcile(&ours, now_ts).await?;
        self.reap(&ours, &active).await;

        let mut jobs = Vec::new();
        for repo in &self.cfg.repos {
            jobs.extend(self.github.queued_jobs(repo).await?);
        }
        let (queue, demand, queue_ages) = self.summarize(&jobs, now);

        if let Err(e) = self.assess_health(now_ts).await {
            tracing::warn!("health assessment failed: {e}");
        }
        if let Err(e) = self.advance_rollout(now_ts).await {
            tracing::warn!("rollout pass failed: {e}");
        }
        let profiles = self.profiles(now_ts).await;
        let drone_rows = self.store.drones().await?;
        let mut fresh_names: BTreeSet<String> = BTreeSet::new();
        let mut capacities: HashMap<String, Capacity> = HashMap::new();
        let mut extra_labels: HashMap<String, Vec<String>> = HashMap::new();
        let mut drones = Vec::new();
        for d in &drone_rows {
            let fresh = d
                .last_heartbeat
                .is_some_and(|t| now_ts - t <= self.cfg.drone_stale_secs);
            if fresh {
                fresh_names.insert(d.name.clone());
            }
            if let (true, Some(cap)) = (fresh, &d.capacity) {
                let mut state = d.allocator_state(cap, now_ts);
                state.blocked_classes = blocked_classes(
                    &self.cfg.env_contract,
                    d.env_facts.as_ref(),
                    &self.cfg.classes,
                    None,
                    now_ts,
                );
                state.profiles = profiles
                    .iter()
                    .filter(|p| p.drone == d.name)
                    .map(ClassProfile::score)
                    .collect();
                drones.push(state);
                capacities.insert(d.name.clone(), cap.clone());
                extra_labels.insert(
                    d.name.clone(),
                    self.cfg.classes.sanitize_extra_labels(&d.extra_labels),
                );
            }
        }
        if let Err(e) = self.advance_units(now_ts, &fresh_names).await {
            tracing::warn!("unit pass failed: {e}");
        }
        let unit_leases = self.unit_leases().await;
        let leases = active
            .iter()
            .map(|l| ActiveLease {
                drone_id: l.drone.clone(),
                class: l.class,
                scope: l.scope,
                state: if l.state == LeaseStatus::Running {
                    LeaseState::Running
                } else {
                    LeaseState::Pending
                },
            })
            .chain(unit_leases)
            .collect::<Vec<ActiveLease>>();
        let schedule_floors = self
            .cfg
            .warm_schedule
            .effective_floors(&self.cfg.warm_floors, now);
        let (warm_floors, homeostasis) = self.regulate(now_ts, &schedule_floors, &queue);
        let placement_drones = drones.clone();
        let placement_leases = leases.clone();
        let allocation = allocate(&AllocatorInput {
            classes: self.cfg.classes.clone(),
            demand,
            drones,
            leases,
            warm_floors: warm_floors.clone(),
            queue_ages,
            starvation_after_secs: self.cfg.starvation_after_secs,
            weights: ScoreWeights::default(),
        });

        if let Err(e) = self
            .place_units(
                now_ts,
                &placement_drones,
                &capacities,
                placement_leases,
                &allocation.grants,
            )
            .await
        {
            tracing::warn!("unit placement failed: {e}");
        }
        // Demand that idle non-borg runners will take is not borg's to wake
        // hosts for.
        let unmet_placeable = placeable_unmet(&allocation.unmet, &statics, &self.cfg.classes);
        let wake = self.wake_pass(now_ts, &unmet_placeable, &fresh_names).await;
        let mut mint_error = None;
        for grant in &allocation.grants {
            let Some(cap) = capacities.get(&grant.drone_id) else {
                continue;
            };
            let lease_id = new_lease_id();
            let runner_name = format!("{prefix}{}-{lease_id}", grant.drone_id);
            let extra = extra_labels
                .get(&grant.drone_id)
                .map_or(&[][..], Vec::as_slice);
            let labels = jit_labels(&cap.os, &cap.arch, extra, &grant.labels);
            let minted = match self
                .github
                .mint_jit_runner(
                    &runner_name,
                    self.cfg.github.runner_group_id,
                    &labels,
                    &self.cfg.work_folder,
                )
                .await
            {
                Ok(m) => m,
                Err(e) => {
                    // Likely rate limiting or permissions: stop minting this tick.
                    mint_error = Some(e);
                    break;
                }
            };
            let row = LeaseRow {
                id: lease_id,
                drone: grant.drone_id.clone(),
                class: grant.class,
                scope: grant.scope,
                labels,
                runner_name,
                runner_id: minted.runner_id,
                jit_config: Some(minted.jit_config),
                state: LeaseStatus::Pending,
                created_at: now_ts,
                expires_at: now_ts + self.cfg.lease_ttl_secs,
            };
            if let Err(e) = self.store.insert_lease(&row).await {
                // Do not leave an unowned registration behind.
                let _ = self.github.delete_runner(minted.runner_id).await;
                return Err(e.into());
            }
        }

        self.update_snapshot(|s| {
            s.last_tick_at = Some(now_ts);
            s.last_error = mint_error.as_ref().map(|e| format!("mint failed: {e}"));
            s.queue = queue;
            s.reserved_for = allocation.reserved_for.clone();
            s.unmet = allocation.unmet.clone();
            s.warm_floors = warm_floors;
            s.pressure = allocation.pressure.clone();
            s.idle = allocation.idle.clone();
            s.homeostasis = homeostasis;
            s.profiles = profiles;
            s.wake = wake;
            s.static_runners = statics;
            s.unmet_placeable = unmet_placeable;
        });
        if let Err(e) = self
            .store
            .prune_reclaim_log(now_ts - RECLAIM_LOG_KEEP_SECS)
            .await
        {
            tracing::warn!("prune reclaim log failed: {e}");
        }
        let keep = i64::from(self.cfg.telemetry_retention_days) * 86_400;
        if let Err(e) = self.store.prune_telemetry(now_ts - keep).await {
            tracing::warn!("prune telemetry failed: {e}");
        }
        Ok(())
    }

    /// The version `drone` should run: from the gated rollout when it is on,
    /// otherwise the configured target (report-only mode), otherwise no
    /// instruction.
    pub fn desired_version(&self, drone: &str) -> Option<String> {
        let target = self.cfg.rollout_target_version.as_ref()?;
        if self.cfg.rollout.previous_version.is_none() {
            return Some(target.clone());
        }
        self.snapshot()
            .rollout
            .map(|r| desired_for(&r.state, drone).to_owned())
    }

    /// One pass of the quorum rollout gate (only when both the target and
    /// `rollout.previous_version` are configured).
    async fn advance_rollout(&self, now: i64) -> Result<(), QueenError> {
        let (Some(target), Some(previous)) = (
            self.cfg.rollout_target_version.as_deref(),
            self.cfg.rollout.previous_version.as_deref(),
        ) else {
            return Ok(());
        };
        let stored = self.store.rollout_state().await?;
        let rows = self.store.drones().await?;
        let since = stored
            .as_ref()
            .filter(|s| s.target == target)
            .map_or(now.saturating_sub(86_400), |s| s.started_at);
        let outcomes: Vec<Outcome> = self
            .store
            .telemetry_since(since, MAX_REPORT_ROWS)
            .await?
            .iter()
            .filter_map(outcome_of)
            .collect();
        let views: Vec<DroneView> = rows
            .iter()
            .map(|d| DroneView {
                name: d.name.clone(),
                os: d
                    .capacity
                    .as_ref()
                    .map(|c| c.os.clone())
                    .unwrap_or_default(),
                fresh: d
                    .last_heartbeat
                    .is_some_and(|t| now - t <= self.cfg.drone_stale_secs),
                held: d.held(),
                version: d.version.clone(),
                version_since: d.version_since,
            })
            .collect();
        let adv = advance(
            now,
            &self.cfg.rollout,
            previous,
            target,
            stored.clone(),
            &views,
            &outcomes,
        );
        if stored.as_ref() != Some(&adv.state) {
            self.store.save_rollout_state(&adv.state, now).await?;
        }
        for e in &adv.events {
            tracing::warn!("rollout: {e}");
            self.store.log_rollout(now, e).await?;
        }
        self.update_snapshot(|s| {
            s.rollout = Some(RolloutView {
                state: adv.state,
                checks: adv.checks,
                waiting: adv.waiting,
                new_group: adv.new_group,
                control_group: adv.control_group,
            });
        });
        Ok(())
    }

    /// Units already placed hold capacity like running leases.
    async fn unit_leases(&self) -> Vec<ActiveLease> {
        if !self.cfg.units.enabled {
            return Vec::new();
        }
        match self
            .store
            .units_in(&[UnitState::Leased, UnitState::Running])
            .await
        {
            Ok(units) => units
                .into_iter()
                .filter_map(|u| {
                    Some(ActiveLease {
                        drone_id: u.drone?,
                        class: u.class,
                        scope: Scope::Pr,
                        state: LeaseState::Running,
                    })
                })
                .collect(),
            Err(e) => {
                tracing::warn!("reading units failed: {e}");
                Vec::new()
            }
        }
    }

    /// Time out, re-queue or fail units whose drone went quiet, and prune old
    /// finished ones.
    async fn advance_units(&self, now: i64, fresh: &BTreeSet<String>) -> Result<(), QueenError> {
        let cfg = &self.cfg.units;
        if !cfg.enabled {
            return Ok(());
        }
        for u in self
            .store
            .units_in(&[UnitState::Leased, UnitState::Running])
            .await?
        {
            let silent = u.drone.as_ref().is_none_or(|d| !fresh.contains(d));
            let unacked = u.state == UnitState::Leased
                && u.leased_at.is_some_and(|t| now - t > cfg.lease_ttl_secs);
            let overdue = u.state == UnitState::Running
                && u.started_at
                    .is_some_and(|t| now - t > i64::try_from(u.timeout_secs).unwrap_or(i64::MAX));
            if overdue {
                self.store
                    .fail_unit(&u.id, &format!("timed out after {}s", u.timeout_secs), now)
                    .await?;
            } else if silent || unacked {
                let why = if silent {
                    "drone went silent"
                } else {
                    "drone never started it"
                };
                self.store
                    .retry_or_fail(&u.id, u.attempt, cfg.max_attempts, why, now)
                    .await?;
            }
        }
        for u in self.store.units_in(&[UnitState::Queued]).await? {
            if u.awaiting_bundle && now - u.created_at > cfg.bundle_wait_secs {
                self.store
                    .fail_unit(&u.id, "the client's bundle never arrived", now)
                    .await?;
            }
        }
        self.store.prune_units(now - cfg.keep_secs).await?;
        Ok(())
    }

    /// Place queued units with the runner allocator, on drones of each unit's
    /// platform, in the capacity left after CI runner grants.
    async fn place_units(
        &self,
        now: i64,
        drones: &[DroneState],
        capacities: &HashMap<String, Capacity>,
        leases: Vec<ActiveLease>,
        runner_grants: &[Grant],
    ) -> Result<(), QueenError> {
        if !self.cfg.units.enabled {
            return Ok(());
        }
        let queued: Vec<Unit> = self
            .store
            .units_in(&[UnitState::Queued])
            .await?
            .into_iter()
            .filter(|u| !u.awaiting_bundle)
            .collect();
        if queued.is_empty() {
            return Ok(());
        }
        let mut base = leases;
        base.extend(runner_grants.iter().map(|g| ActiveLease {
            drone_id: g.drone_id.clone(),
            class: g.class,
            scope: g.scope,
            state: LeaseState::Pending,
        }));
        // Units of one platform and Rust toolchain share a candidate set: the
        // drones of that platform whose build environment contract admits the
        // class for that toolchain.
        let mut by_platform: BTreeMap<(&str, Option<&str>), Vec<&Unit>> = BTreeMap::new();
        for u in &queued {
            by_platform
                .entry((u.platform.as_str(), u.rust_toolchain.as_deref()))
                .or_default()
                .push(u);
        }
        let rows = self.store.drones().await?;
        for ((platform, toolchain), units) in by_platform {
            let group: Vec<DroneState> = drones
                .iter()
                .filter(|d| {
                    capacities
                        .get(&d.id)
                        .is_some_and(|c| platform_of(c) == platform)
                })
                .map(|d| {
                    let mut d = d.clone();
                    d.blocked_classes = blocked_classes(
                        &self.cfg.env_contract,
                        rows.iter()
                            .find(|r| r.name == d.id)
                            .and_then(|r| r.env_facts.as_ref()),
                        &self.cfg.classes,
                        toolchain,
                        now,
                    );
                    d
                })
                .collect();
            if group.is_empty() {
                continue;
            }
            let mut demand: BTreeMap<Class, u32> = BTreeMap::new();
            for u in &units {
                *demand.entry(u.class).or_default() += 1;
            }
            let allocation = allocate(&AllocatorInput {
                classes: self.cfg.classes.clone(),
                demand: demand
                    .into_iter()
                    .map(|(class, count)| Demand {
                        class,
                        scope: Scope::Pr,
                        count,
                    })
                    .collect(),
                drones: group,
                leases: base.clone(),
                warm_floors: Vec::new(),
                queue_ages: Vec::new(),
                starvation_after_secs: None,
                weights: ScoreWeights::default(),
            });
            // Grants of a class are interchangeable: hand them to the units
            // of that class in submission order.
            let mut grants: Vec<&Grant> = allocation.grants.iter().collect();
            for u in units {
                if let Some(i) = grants.iter().position(|g| g.class == u.class) {
                    let g = grants.remove(i);
                    if self.store.lease_unit(&u.id, &g.drone_id, now).await? {
                        base.push(ActiveLease {
                            drone_id: g.drone_id.clone(),
                            class: g.class,
                            scope: Scope::Pr,
                            state: LeaseState::Running,
                        });
                        tracing::info!("unit {} placed on {}", u.id, g.drone_id);
                    }
                }
            }
        }
        Ok(())
    }

    /// Send wake-on-LAN packets for demand that has stayed unmet.
    async fn wake_pass(&self, now: i64, unmet: &[Unmet], fresh: &BTreeSet<String>) -> WakeView {
        let settings = &self.cfg.wake;
        let classes: BTreeSet<Class> = unmet
            .iter()
            .filter(|u| u.count > 0)
            .map(|u| u.class)
            .collect();
        let awake: BTreeSet<&str> = fresh.iter().map(String::as_str).collect();
        let decisions = self
            .wake
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .step(now, settings, &classes, &awake);
        for (i, event) in decisions {
            let host = &settings.hosts[i];
            let Some(mac) = parse_mac(&host.mac) else {
                continue;
            };
            match self.wake_sender.send(mac, &settings.destinations()).await {
                Ok(sent) => tracing::warn!(
                    "wake: sent a magic packet for {} ({}) to {} destination(s)",
                    host.name,
                    event.reason,
                    sent.len()
                ),
                Err(e) => tracing::warn!("wake: sending to {} failed: {e}", host.name),
            }
        }
        self.wake
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .view(now, settings.enabled)
    }

    /// Learned drone profiles, recomputed from telemetry at most every
    /// `profiles.refresh_secs`. Empty when profiles are off.
    async fn profiles(&self, now: i64) -> Vec<ClassProfile> {
        let settings = &self.cfg.profiles;
        if !settings.enabled {
            return Vec::new();
        }
        let refresh = i64::try_from(settings.refresh_secs).unwrap_or(i64::MAX);
        let cached = self
            .profile_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some((at, profiles)) = &cached
            && (0..refresh).contains(&(now - at))
        {
            return profiles.clone();
        }
        let since = now.saturating_sub(i64::from(settings.window_days) * 86_400);
        match self.store.telemetry_since(since, MAX_REPORT_ROWS).await {
            Ok(rows) => {
                let profiles = build_profiles(&rows, settings);
                *self
                    .profile_cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some((now, profiles.clone()));
                profiles
            }
            Err(e) => {
                tracing::warn!("profile refresh failed: {e}");
                cached.map(|(_, p)| p).unwrap_or_default()
            }
        }
    }

    /// Run the warm-floor controller: the schedule's floors plus the dynamic
    /// level per key. With homeostasis off this is the schedule unchanged.
    fn regulate(
        &self,
        now: i64,
        schedule: &[WarmFloor],
        queue: &[QueueEntry],
    ) -> (Vec<WarmFloor>, Vec<KeyReport>) {
        let settings = &self.cfg.homeostasis;
        if !settings.enabled {
            return (schedule.to_vec(), Vec::new());
        }
        let idle = self.snapshot().idle;
        let mut inputs = Vec::new();
        for class in Class::ALL {
            for scope in Scope::ALL {
                let entry = queue.iter().find(|q| q.class == class && q.scope == scope);
                inputs.push(KeyInput {
                    class,
                    scope,
                    latency_secs: entry.map_or(0, |q| q.oldest_age_secs),
                    queued: entry.map_or(0, |q| q.queued),
                    schedule_floor: schedule
                        .iter()
                        .filter(|f| f.class == class && f.scope == scope)
                        .map(|f| f.count)
                        .max()
                        .unwrap_or(0),
                    idle_slots: idle
                        .iter()
                        .find(|i| i.class == class)
                        .map_or(0, |i| i.idle_slots),
                });
            }
        }
        let reports = self
            .homeostasis
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .step(now, settings, &inputs);
        for r in reports.iter().filter(|r| r.last_change_at == Some(now)) {
            tracing::info!(
                "homeostasis: {} {} warm floor now {} (latency {}s, target {}s)",
                r.class,
                format!("{:?}", r.scope).to_lowercase(),
                r.effective_floor,
                r.latency_secs,
                settings.target_secs
            );
        }
        let floors = reports
            .iter()
            .filter(|r| r.effective_floor > 0)
            .map(|r| WarmFloor {
                class: r.class,
                scope: r.scope,
                count: r.effective_floor,
            })
            .collect();
        (floors, reports)
    }

    /// Evaluate host health from recent telemetry and apply the automatic
    /// quarantine, probation and release actions it decides.
    async fn assess_health(&self, now: i64) -> Result<(), QueenError> {
        let settings = &self.cfg.health;
        let rows = self.store.drones().await?;
        let window = i64::try_from(settings.window_secs).unwrap_or(i64::MAX);
        // Probation proof needs outcomes since the probation began, which can
        // predate the window if no work came.
        let since = rows
            .iter()
            .filter_map(|d| d.probation_since)
            .fold(now.saturating_sub(window), i64::min);
        let outcomes: Vec<Outcome> = self
            .store
            .telemetry_since(since, MAX_REPORT_ROWS)
            .await?
            .iter()
            .filter_map(outcome_of)
            .collect();
        let infos: Vec<DroneInfo> = rows
            .iter()
            .map(|d| DroneInfo {
                name: d.name.clone(),
                fresh: d
                    .last_heartbeat
                    .is_some_and(|t| now - t <= self.cfg.drone_stale_secs),
                hold: d.hold(),
                cleared_at: d.health_cleared_at,
            })
            .collect();
        let eval = evaluate(now, settings, &infos, &outcomes);
        for action in &eval.actions {
            self.store.apply_health_action(action, now).await?;
            tracing::warn!("health: {} {}", action.event(), action.drone());
        }
        self.update_snapshot(|s| {
            s.health = eval.drones;
            s.health_systemic = eval.systemic;
        });
        Ok(())
    }

    /// Bring lease state in line with GitHub; returns the leases still active.
    async fn reconcile(
        &self,
        ours: &HashMap<&str, &Runner>,
        now: i64,
    ) -> Result<Vec<LeaseRow>, QueenError> {
        let mut still_active = Vec::new();
        for lease in self.store.active_leases().await? {
            let runner = ours.get(lease.runner_name.as_str());
            let overdue = now - lease.created_at > self.cfg.lease_max_secs;
            let never_came_online =
                now > lease.expires_at && runner.is_none_or(|r| r.status != "online");
            match runner {
                _ if overdue => {
                    self.end(&lease, runner.copied(), LeaseStatus::Expired, now)
                        .await?
                }
                Some(r) if r.busy => {
                    if lease.state == LeaseStatus::Pending {
                        self.store
                            .set_state(&lease.id, LeaseStatus::Running, now)
                            .await?;
                    }
                    still_active.push(lease);
                }
                // Ephemeral runners deregister after their one job.
                None if lease.state == LeaseStatus::Running => {
                    self.store
                        .set_state(&lease.id, LeaseStatus::Done, now)
                        .await?;
                }
                _ if never_came_online => {
                    self.end(&lease, runner.copied(), LeaseStatus::Expired, now)
                        .await?
                }
                _ => still_active.push(lease),
            }
        }
        Ok(still_active)
    }

    async fn end(
        &self,
        lease: &LeaseRow,
        runner: Option<&Runner>,
        to: LeaseStatus,
        now: i64,
    ) -> Result<(), QueenError> {
        self.store.set_state(&lease.id, to, now).await?;
        if let Some(r) = runner {
            // Best effort: the reaper retries next tick.
            if let Err(e) = self.github.delete_runner(r.id).await {
                tracing::warn!("delete runner {} failed: {e}", r.name);
            }
        }
        Ok(())
    }

    /// Delete offline `<prefix>-*` registrations that no active lease owns.
    async fn reap(&self, ours: &HashMap<&str, &Runner>, active: &[LeaseRow]) {
        let owned: HashSet<&str> = active.iter().map(|l| l.runner_name.as_str()).collect();
        for (name, r) in ours {
            if !owned.contains(name)
                && r.status == "offline"
                && !r.busy
                && let Err(e) = self.github.delete_runner(r.id).await
            {
                tracing::warn!("reap runner {name} failed: {e}");
            }
        }
    }

    fn summarize(
        &self,
        jobs: &[QueuedJob],
        now: OffsetDateTime,
    ) -> (Vec<QueueEntry>, Vec<Demand>, Vec<QueueAge>) {
        let mut by: BTreeMap<(Class, Scope), (u32, u64)> = BTreeMap::new();
        for job in jobs {
            let Some((class, scope)) = job
                .labels
                .iter()
                .find_map(|l| self.cfg.classes.parse_label(l))
            else {
                continue;
            };
            let e = by.entry((class, scope)).or_default();
            e.0 += 1;
            e.1 = e.1.max(age_secs(now, job.created_at));
        }
        let queue = by
            .iter()
            .map(|(&(class, scope), &(queued, oldest_age_secs))| QueueEntry {
                label: self.cfg.classes.job_label(class, scope),
                class,
                scope,
                queued,
                oldest_age_secs,
            })
            .collect();
        let demand = by
            .iter()
            .map(|(&(class, scope), &(count, _))| Demand {
                class,
                scope,
                count,
            })
            .collect();
        let ages = by
            .iter()
            .map(|(&(class, scope), &(_, oldest_age_secs))| QueueAge {
                class,
                scope,
                oldest_age_secs,
            })
            .collect();
        (queue, demand, ages)
    }

    /// Per-class totals for status and metrics: slots the fleet could hold.
    pub(crate) fn class_slots(&self, drones: &[DroneState]) -> BTreeMap<Class, u32> {
        Class::ALL
            .into_iter()
            .map(|c| {
                let spec = self.cfg.classes.spec(c);
                (
                    c,
                    drones
                        .iter()
                        .filter(|d| d.accepting)
                        .map(|d| max_slots(d, spec))
                        .sum(),
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jit_labels_list_platform_labels_first_then_class_labels() {
        let class = vec!["borg-ci-medium".to_owned(), "borg-ci-medium-pr".to_owned()];
        assert_eq!(
            jit_labels("linux", "x86_64", &[], &class),
            [
                "self-hosted",
                "Linux",
                "X64",
                "borg-ci-medium",
                "borg-ci-medium-pr"
            ]
        );
        assert_eq!(
            jit_labels("macos", "aarch64", &[], &class[..1]),
            ["self-hosted", "macOS", "ARM64", "borg-ci-medium"]
        );
        assert_eq!(jit_labels("plan9", "mips", &[], &[]), ["self-hosted"]);
        assert_eq!(
            jit_labels("linux", "x86_64", &["on-prem".to_owned()], &class[..1]),
            ["self-hosted", "Linux", "X64", "on-prem", "borg-ci-medium"],
            "extra labels sit between the platform and class labels"
        );
    }
}
