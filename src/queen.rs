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

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

use rand::RngCore;
use rand::rngs::OsRng;
use serde::Serialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::allocator::{
    ActiveLease, AdmissionState, AllocatorInput, ClassIdle, Demand, DroneAdmission, DroneState,
    LeaseState, PlacementDecision, Promotion, QueueAge, Reservation, Unmet, WarmFloor,
    allocate_reserved, allocate_reserved_with_backlog, allocate_scope, free_room, free_slots,
    max_slots,
};
use crate::bid::{ClassPressure, ScoreWeights};
use crate::classes::{Class, Scope};
use crate::config::{QueenConfig, StandbySettings};
use crate::envcontract::{CheckKind, CheckState, Mode, Need, blocked_classes, capability_states};
use crate::explain::{MAX_HISTORY, PlacementKind, PlacementOutcome, PlacementRecord};
use crate::github::{GithubClient, GithubError, QueuedJob, Runner, WorkflowJob};
use crate::health::{DroneHealth, DroneInfo, Outcome, OutcomeKind, evaluate};
use crate::homeostasis::{Controller, KeyInput, KeyReport};
use crate::janitor::Janitor;
use crate::mirror_auth::MirrorAuth;
use crate::preempt::{PreemptInput, Running, select as select_victims};
use crate::probe::Capacity;
use crate::profile::{ClassProfile, build_profiles};
use crate::requeue::Requeuer;
use crate::rollout::{
    Check as RolloutCheck, DroneView, Observed as RolloutObserved, RolloutState, advance,
    desired_for,
};
use crate::store::{DroneRow, LeaseRow, LeaseStatus, PreemptionRow, Store, StoreError};
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
    /// CPU-pressure admission per fresh drone from the last pass.
    pub admission: Vec<DroneAdmission>,
    /// Warm-floor controller state per key (empty when off).
    pub homeostasis: Vec<KeyReport>,
    /// Learned per-drone, per-class profiles (empty when off).
    pub profiles: Vec<ClassProfile>,
    /// The gated rollout after the last pass; `None` when not gated.
    pub rollout: Option<RolloutView>,
    pub wake: WakeView,
    /// Non-borg online runners by label, from the last pass.
    pub static_runners: BTreeMap<String, StaticRunners>,
    /// Standby leases the drones are told to start now (deploy demand is queued
    /// for them); read by the heartbeat.
    pub start_standby: BTreeSet<String>,
    /// `unmet` minus what idle non-borg runners with the label can take.
    pub unmet_placeable: Vec<Unmet>,
    /// Per class, slots the fresh accepting drones could hold if empty.
    pub schedulable_slots: BTreeMap<Class, u32>,
    /// Per class, since when main demand has been unmet with schedulable
    /// slots at or under `capacity_alarm.max_slots`.
    pub starved_since: BTreeMap<Class, i64>,
    /// Classes whose `capacity_starved` event is raised and not yet restored.
    pub starved: BTreeSet<Class>,
}

/// What the fleet watch remembers between passes.
#[derive(Debug, Default)]
struct FleetWatch {
    /// Fresh drones at the previous pass; `None` before the first.
    prev_fresh: Option<BTreeSet<String>>,
    starved_since: BTreeMap<Class, i64>,
    raised: BTreeSet<Class>,
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

#[derive(Default)]
struct UnitPlacement {
    leases: Vec<ActiveLease>,
    reserved: Vec<Reservation>,
}

type UnitGroupKey<'a> = (&'a str, Option<&'a str>);

struct UnitPlacementPolicy<'a> {
    share: Option<u8>,
    reserved: &'a [Reservation],
    spent_bids: &'a [ActiveLease],
}

/// Heartbeat bids describe additional slots, so grants made earlier in the
/// same reconciliation pass spend them even when a different queue allocates.
fn spend_bids(drones: &mut [DroneState], granted: &[ActiveLease]) {
    for d in drones {
        for bid in &mut d.bids {
            let used = granted
                .iter()
                .filter(|g| g.drone_id == d.id && g.class == bid.class && g.scope == bid.scope)
                .count();
            bid.available_slots = bid
                .available_slots
                .saturating_sub(u32::try_from(used).unwrap_or(u32::MAX));
        }
    }
}

pub struct Queen {
    pub(crate) cfg: QueenConfig,
    pub(crate) store: Store,
    github: GithubClient,
    snapshot: RwLock<Snapshot>,
    janitor: Option<Janitor>,
    requeuer: Option<Requeuer>,
    tailnet: Option<TailnetEnroll>,
    mirror_auth: Option<MirrorAuth>,
    homeostasis: std::sync::Mutex<Controller>,
    profile_cache: std::sync::Mutex<Option<(i64, Vec<ClassProfile>)>>,
    wake: std::sync::Mutex<WakePlanner>,
    unit_joins: AtomicU64,
    unit_receipts: AtomicU64,
    units_superseded: AtomicU64,
    /// Transient round-robin cursor; authoritative work stays in the store.
    unit_cursor: std::sync::Mutex<BTreeMap<Scope, String>>,
    wake_sender: Arc<dyn WakeSender>,
    tick_lock: tokio::sync::Mutex<()>,
    watch: std::sync::Mutex<FleetWatch>,
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

fn as_active(l: &LeaseRow) -> ActiveLease {
    ActiveLease {
        drone_id: l.drone.clone(),
        class: l.class,
        scope: l.scope,
        state: match l.state {
            LeaseStatus::Running => LeaseState::Running,
            LeaseStatus::Standby => LeaseState::Standby,
            _ => LeaseState::Pending,
        },
    }
}

/// Where a preemptible lease's job lives: repo, run id, job id.
type JobRef<'a> = (&'a str, u64, u64);

/// Split the active leases into those whose in-progress job is known (the
/// job's `runner_name` is the lease's runner) and everything else.
fn split_running<'a>(
    active: &[LeaseRow],
    unit_leases: &[ActiveLease],
    observed: &[(&'a str, WorkflowJob)],
    preempted: &HashSet<&str>,
) -> (Vec<Running>, Vec<ActiveLease>, HashMap<String, JobRef<'a>>) {
    let (mut running, mut others, mut jobs) = (Vec::new(), unit_leases.to_vec(), HashMap::new());
    for l in active {
        let found = observed.iter().find_map(|(repo, j)| {
            let started = OffsetDateTime::parse(j.started_at.as_deref()?, &Rfc3339).ok()?;
            (j.status == "in_progress" && j.runner_name.as_deref() == Some(&l.runner_name))
                .then_some((*repo, j.run_id, j.id, started.unix_timestamp()))
        });
        match found {
            Some((repo, run_id, job_id, started_at)) => {
                jobs.insert(l.id.clone(), (repo, run_id, job_id));
                running.push(Running {
                    lease_id: l.id.clone(),
                    drone: l.drone.clone(),
                    class: l.class,
                    scope: l.scope,
                    started_at,
                    preempted: preempted.contains(l.id.as_str()),
                });
            }
            None => others.push(as_active(l)),
        }
    }
    (running, others, jobs)
}

/// What standby minting needs to know about the fleet.
struct StandbyView<'a> {
    active: &'a [LeaseRow],
    drones: &'a [DroneState],
    capacities: &'a HashMap<String, Capacity>,
    extra_labels: &'a HashMap<String, Vec<String>>,
    serves_deploy: &'a BTreeSet<String>,
}

/// The standby leases to start now: per promoted class, the oldest standby
/// leases on fresh drones.
fn promotions(
    promote: &[Promotion],
    active: &[LeaseRow],
    fresh: &HashMap<String, Capacity>,
) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for p in promote {
        let mut standby: Vec<&LeaseRow> = active
            .iter()
            .filter(|l| {
                l.state == LeaseStatus::Standby
                    && l.scope == Scope::Deploy
                    && l.class == p.class
                    && fresh.contains_key(&l.drone)
            })
            .collect();
        standby.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        out.extend(
            standby
                .into_iter()
                .take(usize::try_from(p.count).unwrap_or(usize::MAX))
                .map(|l| l.id.clone()),
        );
    }
    out
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
            requeuer: None,
            tailnet: None,
            mirror_auth: None,
            homeostasis: std::sync::Mutex::new(Controller::new()),
            profile_cache: std::sync::Mutex::new(None),
            wake: std::sync::Mutex::new(WakePlanner::new()),
            unit_joins: AtomicU64::new(0),
            unit_receipts: AtomicU64::new(0),
            units_superseded: AtomicU64::new(0),
            unit_cursor: std::sync::Mutex::new(BTreeMap::new()),
            wake_sender: Arc::new(UdpWakeSender),
            tick_lock: tokio::sync::Mutex::new(()),
            watch: std::sync::Mutex::new(FleetWatch::default()),
        }
    }

    /// Attach the preemption requeuer; `api::serve` runs it after each
    /// janitor pass.
    #[must_use]
    pub fn with_requeuer(mut self, requeuer: Requeuer) -> Self {
        self.requeuer = Some(requeuer);
        self
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

    /// Hand drones short-lived read-only tokens for their git mirrors.
    #[must_use]
    pub fn with_mirror_auth(mut self, mirror_auth: MirrorAuth) -> Self {
        self.mirror_auth = Some(mirror_auth);
        self
    }

    pub fn mirror_auth(&self) -> Option<&MirrorAuth> {
        self.mirror_auth.as_ref()
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

    pub fn count_units_superseded(&self, n: usize) {
        self.units_superseded
            .fetch_add(u64::try_from(n).unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    /// Units cancelled by a newer submission with the same supersede key since
    /// start.
    pub fn units_superseded(&self) -> u64 {
        self.units_superseded.load(Ordering::Relaxed)
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

    pub fn requeuer(&self) -> Option<&Requeuer> {
        self.requeuer.as_ref()
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
        let mut observed = Vec::new();
        for repo in &self.cfg.repos {
            for job in self.github.workflow_jobs(repo).await? {
                if let Some(queued) = job.queued(repo)? {
                    jobs.push(queued);
                }
                observed.push((repo.as_str(), job));
            }
        }
        let (queue, demand, queue_ages) = self.summarize(&jobs, now);

        if let Err(e) = self.assess_health(now_ts).await {
            tracing::warn!("health assessment failed: {e}");
        }
        if let Err(e) = self.advance_rollout(now_ts).await {
            tracing::warn!("rollout pass failed: {e}");
        }
        let profiles = self.profiles(now_ts).await;
        if let Err(e) = self.expire_drones(now_ts).await {
            tracing::warn!("drone expiry failed: {e}");
        }
        let drone_rows = self.store.drones().await?;
        let mut fresh_names: BTreeSet<String> = BTreeSet::new();
        let mut capacities: HashMap<String, Capacity> = HashMap::new();
        let mut extra_labels: HashMap<String, Vec<String>> = HashMap::new();
        let mut serves_deploy: BTreeSet<String> = BTreeSet::new();
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
                state.accepting &= !crate::envcontract::disk_admission(
                    self.cfg.env_contract.min_cache_dir_free_bytes,
                    cap.free_disk_mib,
                    d.env_facts
                        .as_ref()
                        .filter(|facts| {
                            (0..=self.cfg.env_contract.facts_max_age_secs)
                                .contains(&(now_ts - facts.observed_at))
                        })
                        .and_then(|facts| facts.cache_dir_free_bytes),
                )
                .blocked();
                state.blocked_classes = blocked_classes(
                    &self.cfg.env_contract,
                    d.env_facts.as_ref(),
                    &self.cfg.classes,
                    None,
                    now_ts,
                );
                if let Some(a) = &self.cfg.admission {
                    state.cpu_psi_pm = d
                        .env_facts
                        .as_ref()
                        .filter(|f| now_ts - f.observed_at <= a.facts_max_age_secs)
                        .and_then(|f| f.cpu_psi_some_avg10_pm);
                }
                state.profiles = profiles
                    .iter()
                    .filter(|p| p.drone == d.name)
                    .map(ClassProfile::score)
                    .collect();
                drones.push(state);
                capacities.insert(d.name.clone(), cap.clone());
                if d.trust.serves_deploy() {
                    serves_deploy.insert(d.name.clone());
                }
                extra_labels.insert(
                    d.name.clone(),
                    self.cfg.classes.sanitize_extra_labels(&d.extra_labels),
                );
            }
        }
        let schedulable_slots = self.class_slots(&drones);
        if let Err(e) = self.advance_units(now_ts, &fresh_names).await {
            tracing::warn!("unit pass failed: {e}");
        }
        let unit_leases = self.unit_leases().await;
        let leases = active
            .iter()
            .map(as_active)
            .chain(unit_leases.iter().cloned())
            .collect::<Vec<ActiveLease>>();
        let schedule_floors = self
            .cfg
            .warm_schedule
            .effective_floors(&self.cfg.warm_floors, now);
        let (warm_floors, homeostasis) = self.regulate(now_ts, &schedule_floors, &queue);
        let placement_drones = drones.clone();
        let mut placement_leases = leases.clone();
        let preempt_ages = queue_ages.clone();
        let mut runner_input = AllocatorInput {
            classes: self.cfg.classes.clone(),
            demand,
            drones,
            leases,
            warm_floors: warm_floors.clone(),
            queue_ages,
            starvation_after_secs: self.cfg.starvation_after_secs,
            weights: ScoreWeights::default(),
            admission: self.cfg.admission.as_ref().map(|a| a.thresholds()),
        };
        let mut unit_reserved = Vec::new();
        let mut deploy_grants = Vec::new();
        let mut deploy_decisions = Vec::new();
        let mut spent_bids = Vec::new();
        if self.cfg.units.enabled && self.cfg.units.capacity_share_percent > 0 {
            // Deploy CI retains first claim. Hold these grants while admitting
            // the unit share, then let the ordinary allocator fill the rest.
            // Pressure still comes from the full runner queue after the usual
            // pending/standby coverage, including shared PR runners for Main.
            let deploy = allocate_scope(&runner_input, Scope::Deploy);
            deploy_grants = deploy.grants;
            deploy_decisions = deploy.decisions;
            let mut held = placement_leases.clone();
            spent_bids.extend(deploy_grants.iter().map(|g| ActiveLease {
                drone_id: g.drone_id.clone(),
                class: g.class,
                scope: g.scope,
                state: LeaseState::Pending,
            }));
            held.extend(spent_bids.iter().cloned());
            let units = self
                .place_units(
                    now_ts,
                    &placement_drones,
                    &capacities,
                    held,
                    UnitPlacementPolicy {
                        share: Some(self.cfg.units.capacity_share_percent),
                        reserved: &deploy.reserved_for,
                        spent_bids: &spent_bids,
                    },
                )
                .await?;
            unit_reserved = units.reserved;
            spent_bids.extend(units.leases);
            // Re-read persisted placements; units hold capacity without
            // offsetting runner demand.
            placement_leases = active
                .iter()
                .map(as_active)
                .chain(self.unit_leases().await)
                .collect();
            runner_input.leases = placement_leases.clone();
            runner_input
                .leases
                .extend(deploy_grants.iter().map(|g| ActiveLease {
                    drone_id: g.drone_id.clone(),
                    class: g.class,
                    scope: g.scope,
                    state: LeaseState::Pending,
                }));
            // A starving unit needs contiguous room as existing work drains.
            // Do not refill its eligible reserved host with new CI this tick.
            spend_bids(&mut runner_input.drones, &spent_bids);
        }
        let mut allocation = allocate_reserved(&runner_input, &unit_reserved);
        deploy_grants.append(&mut allocation.grants);
        allocation.grants = deploy_grants;
        deploy_decisions.append(&mut allocation.decisions);
        allocation.decisions = deploy_decisions;
        allocation.reserved_for.extend(unit_reserved);
        let mut held = placement_leases.clone();
        let runner_spent: Vec<ActiveLease> = allocation
            .grants
            .iter()
            .map(|g| ActiveLease {
                drone_id: g.drone_id.clone(),
                class: g.class,
                scope: g.scope,
                state: LeaseState::Pending,
            })
            .collect();
        held.extend(runner_spent.iter().cloned());
        // Deploy grants already occur in spent_bids; add only later CI grants.
        spent_bids.extend(
            runner_spent
                .into_iter()
                .filter(|g| self.cfg.units.capacity_share_percent == 0 || g.scope != Scope::Deploy),
        );
        if let Err(e) = self
            .place_units(
                now_ts,
                &placement_drones,
                &capacities,
                held,
                UnitPlacementPolicy {
                    share: None,
                    reserved: &allocation.reserved_for,
                    spent_bids: &spent_bids,
                },
            )
            .await
        {
            tracing::warn!("unit placement failed: {e}");
        }
        let unit_leases = self.unit_leases().await;
        placement_leases = active
            .iter()
            .map(as_active)
            .chain(unit_leases.iter().cloned())
            .collect();
        // Demand that idle non-borg runners will take is not borg's to wake
        // hosts for.
        let unmet_placeable = placeable_unmet(&allocation.unmet, &statics, &self.cfg.classes);
        let wake = self.wake_pass(now_ts, &unmet_placeable, &fresh_names).await;
        let (starved_since, starved) = self
            .watch_fleet(
                now_ts,
                &drone_rows,
                &fresh_names,
                &schedulable_slots,
                &unmet_placeable,
            )
            .await;
        let mut mint_error = None;
        let mut lease_error = None;
        let mut persisted = BTreeSet::new();
        // Deploy decisions came from the preliminary pass; align their order
        // with the combined grants actually minted below.
        let mut assigned = BTreeSet::new();
        for decision in allocation
            .decisions
            .iter_mut()
            .filter(|d| d.winner.is_some())
        {
            if let Some((order, _)) = allocation.grants.iter().enumerate().find(|(i, g)| {
                !assigned.contains(i)
                    && Some(g.drone_id.as_str()) == decision.winner.as_deref()
                    && g.class == decision.class
                    && g.scope == decision.scope
            }) {
                decision.grant_order = order;
                assigned.insert(order);
            }
        }
        let rows_by_id = drone_rows.iter().map(|d| (d.name.as_str(), d)).collect();
        self.enrich_decisions(&mut allocation.decisions, &rows_by_id, None, now_ts);
        for (order, grant) in allocation.grants.iter().enumerate() {
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
                lease_error = Some(e);
                break;
            }
            persisted.insert(order);
        }
        for decision in &allocation.decisions {
            let key = self.cfg.classes.job_label(decision.class, decision.scope);
            let outcome = if decision.winner.is_none() {
                PlacementOutcome::Unplaced
            } else if persisted.contains(&decision.grant_order) {
                PlacementOutcome::Granted
            } else {
                PlacementOutcome::Failed
            };
            self.record_placement(PlacementRecord {
                at: now_ts,
                kind: PlacementKind::Runner,
                key: &key,
                client: None,
                outcome,
                decision,
            })
            .await;
        }
        if let Some(e) = lease_error {
            return Err(e.into());
        }

        let start_standby = promotions(&allocation.promote, &active, &capacities);
        if mint_error.is_none()
            && let Some(st) = &self.cfg.standby
        {
            let mut held = placement_leases.clone();
            held.extend(allocation.grants.iter().map(|g| ActiveLease {
                drone_id: g.drone_id.clone(),
                class: g.class,
                scope: g.scope,
                state: LeaseState::Pending,
            }));
            mint_error = self
                .top_up_standby(
                    st,
                    &StandbyView {
                        active: &active,
                        drones: &placement_drones,
                        capacities: &capacities,
                        extra_labels: &extra_labels,
                        serves_deploy: &serves_deploy,
                    },
                    held,
                    &prefix,
                    now_ts,
                )
                .await?;
        }

        if let Some(p) = &self.cfg.preemption {
            let preempted = self.store.preempted_active().await?;
            let (running, others, jobs) = split_running(
                &active,
                &unit_leases,
                &observed,
                &preempted.iter().map(|(id, _)| id.as_str()).collect(),
            );
            let victims = select_victims(&PreemptInput {
                classes: &self.cfg.classes,
                settings: p,
                drones: &placement_drones,
                others: &others,
                running: &running,
                unmet: &unmet_placeable,
                queue_ages: &preempt_ages,
                now: now_ts,
            });
            for v in victims {
                let Some((repo, run_id, job_id)) = jobs.get(&v.lease_id) else {
                    continue;
                };
                let row = PreemptionRow {
                    lease_id: v.lease_id,
                    drone: v.drone,
                    class: v.class,
                    scope: v.scope,
                    for_scope: v.for_scope,
                    repo: (*repo).to_owned(),
                    run_id: *run_id,
                    job_id: *job_id,
                    at: now_ts,
                    requeued_at: None,
                };
                if self.store.record_preemption(&row, now_ts).await? {
                    tracing::warn!(
                        "preempting {} lease {} on {} (job {} of {}) for starving {} {} demand",
                        row.scope.as_str(),
                        row.lease_id,
                        row.drone,
                        row.job_id,
                        row.repo,
                        v.for_class,
                        row.for_scope.as_str(),
                    );
                }
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
            s.admission = allocation.admission.clone();
            s.homeostasis = homeostasis;
            s.profiles = profiles;
            s.wake = wake;
            s.static_runners = statics;
            s.start_standby = start_standby;
            s.unmet_placeable = unmet_placeable;
            s.schedulable_slots = schedulable_slots;
            s.starved_since = starved_since;
            s.starved = starved;
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
        // Grants and the snapshot are published before optional feedback. Bound
        // this pass so unavailable job history cannot stall subsequent ticks.
        match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.collect_job_telemetry(&observed, now_ts),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!("GitHub job telemetry collection failed: {e}"),
            Err(_) => tracing::warn!("GitHub job telemetry collection exceeded its deadline"),
        }
        Ok(())
    }

    /// Mint standby deploy runners until every drone that serves deploy work
    /// holds `per_drone` of each configured class. Returns a mint error to
    /// surface in the snapshot; store failures abort the tick like grants do.
    async fn top_up_standby(
        &self,
        st: &StandbySettings,
        view: &StandbyView<'_>,
        mut held: Vec<ActiveLease>,
        prefix: &str,
        now_ts: i64,
    ) -> Result<Option<GithubError>, QueenError> {
        for drone in view.drones {
            let (Some(cap), true) = (
                view.capacities.get(&drone.id),
                view.serves_deploy.contains(&drone.id),
            ) else {
                continue;
            };
            for &class in &st.classes {
                if drone.blocked_classes.contains(&class) {
                    continue;
                }
                let have = view
                    .active
                    .iter()
                    .filter(|l| {
                        l.state == LeaseStatus::Standby && l.drone == drone.id && l.class == class
                    })
                    .count();
                let spec = self.cfg.classes.spec(class);
                for _ in have..usize::try_from(st.per_drone).unwrap_or(usize::MAX) {
                    let room = free_room(drone, &held, &self.cfg.classes);
                    if free_slots(drone, &room, spec) == 0 {
                        break;
                    }
                    let lease_id = new_lease_id();
                    let runner_name = format!("{prefix}{}-{lease_id}", drone.id);
                    let extra = view
                        .extra_labels
                        .get(&drone.id)
                        .map_or(&[][..], Vec::as_slice);
                    let labels = jit_labels(
                        &cap.os,
                        &cap.arch,
                        extra,
                        &self.cfg.classes.runner_labels(class, Scope::Deploy),
                    );
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
                        Err(e) => return Ok(Some(e)),
                    };
                    let row = LeaseRow {
                        id: lease_id,
                        drone: drone.id.clone(),
                        class,
                        scope: Scope::Deploy,
                        labels,
                        runner_name,
                        runner_id: minted.runner_id,
                        jit_config: Some(minted.jit_config),
                        state: LeaseStatus::Standby,
                        created_at: now_ts,
                        expires_at: i64::MAX,
                    };
                    if let Err(e) = self.store.insert_lease(&row).await {
                        let _ = self.github.delete_runner(minted.runner_id).await;
                        return Err(e.into());
                    }
                    held.push(ActiveLease {
                        drone_id: drone.id.clone(),
                        class,
                        scope: Scope::Deploy,
                        state: LeaseState::Standby,
                    });
                }
            }
        }
        Ok(None)
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
                        scope: u.scope,
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

    /// Diagnostic writes never change admission or lease persistence.
    async fn record_placement(&self, record: PlacementRecord<'_>) {
        if let Err(e) = self.store.record_placement(record).await {
            tracing::warn!("placement history write failed: {e}");
        }
    }

    async fn record_unit_placements(
        &self,
        now: i64,
        history: &VecDeque<(String, String, PlacementOutcome, PlacementDecision)>,
    ) {
        let records: Vec<_> = history
            .iter()
            .map(|(key, client, outcome, decision)| PlacementRecord {
                at: now,
                kind: PlacementKind::Unit,
                key,
                client: Some(client),
                outcome: *outcome,
                decision,
            })
            .collect();
        if let Err(e) = self.store.record_placements(&records).await {
            tracing::warn!("placement history write failed: {e}");
        }
    }

    /// Copy only typed effective blockers, never raw environment values or
    /// free-form reasons, from the same snapshot used for admission.
    fn enrich_decisions(
        &self,
        decisions: &mut [PlacementDecision],
        rows: &HashMap<&str, &DroneRow>,
        toolchain: Option<&str>,
        now: i64,
    ) {
        for decision in decisions {
            let need = Need::for_class(
                &self.cfg.env_contract,
                decision.class,
                self.cfg.classes.spec(decision.class),
                toolchain,
            );
            for candidate in &mut decision.candidates {
                let facts = rows
                    .get(candidate.drone_id.as_str())
                    .and_then(|d| d.env_facts.as_ref());
                let verdict =
                    crate::envcontract::evaluate(&self.cfg.env_contract, facts, &need, now);
                if verdict.admit {
                    continue;
                }
                candidate.environment_checks = verdict
                    .checks
                    .iter()
                    .filter(|c| c.mode == Mode::Required && c.state != CheckState::Pass)
                    .map(|c| (c.kind, c.state))
                    .collect();
                if candidate
                    .environment_checks
                    .contains_key(&CheckKind::Capabilities)
                {
                    candidate.capability_checks =
                        capability_states(&self.cfg.env_contract, facts, decision.class, now);
                }
            }
        }
    }

    /// Place units fairly across clients, platforms and toolchains. A share
    /// pass precedes non-deploy CI; a borrowing pass consumes only leftover room.
    async fn place_units(
        &self,
        now: i64,
        drones: &[DroneState],
        capacities: &HashMap<String, Capacity>,
        mut base: Vec<ActiveLease>,
        policy: UnitPlacementPolicy<'_>,
    ) -> Result<UnitPlacement, QueenError> {
        if !self.cfg.units.enabled {
            return Ok(UnitPlacement::default());
        }
        let queued: Vec<Unit> = self
            .store
            .units_in(&[UnitState::Queued])
            .await?
            .into_iter()
            .filter(|u| !u.awaiting_bundle)
            .collect();
        if queued.is_empty() {
            return Ok(UnitPlacement::default());
        }
        let inflight = self
            .store
            .units_in(&[UnitState::Leased, UnitState::Running])
            .await?;
        let rows = self.store.drones().await?;
        let rows_by_id: HashMap<&str, &DroneRow> =
            rows.iter().map(|d| (d.name.as_str(), d)).collect();
        let admission = self.cfg.admission.as_ref().map(|a| a.thresholds());
        // Platform and toolchain determine the contract snapshot, independently
        // of unit class/scope. Share it across queued units while keeping blocked
        // hosts visible to diagnostics and scope eligibility per request.
        let mut groups: BTreeMap<UnitGroupKey<'_>, Vec<DroneState>> = BTreeMap::new();
        let mut backlogs: BTreeMap<UnitGroupKey<'_>, BTreeMap<(Class, Scope), u32>> =
            BTreeMap::new();
        for u in &queued {
            let key = (u.platform.as_str(), u.rust_toolchain.as_deref());
            groups.entry(key).or_insert_with(|| {
                let mut group: Vec<DroneState> = drones
                    .iter()
                    .filter(|d| {
                        capacities
                            .get(&d.id)
                            .is_some_and(|c| platform_of(c) == u.platform)
                    })
                    .map(|d| {
                        let mut d = d.clone();
                        d.blocked_classes = blocked_classes(
                            &self.cfg.env_contract,
                            rows_by_id
                                .get(d.id.as_str())
                                .and_then(|r| r.env_facts.as_ref()),
                            &self.cfg.classes,
                            u.rust_toolchain.as_deref(),
                            now,
                        );
                        d
                    })
                    .collect();
                spend_bids(&mut group, policy.spent_bids);
                group
            });
            let count = backlogs
                .entry(key)
                .or_default()
                .entry((u.class, u.scope))
                .or_default();
            *count = count.saturating_add(1);
        }
        let eligible: BTreeSet<&str> = queued
            .iter()
            .flat_map(|u| {
                groups[&(u.platform.as_str(), u.rust_toolchain.as_deref())]
                    .iter()
                    .filter(|d| {
                        let state = AdmissionState::of(d.cpu_psi_pm, admission);
                        d.accepting
                            && d.trust.admits(u.scope)
                            && state != AdmissionState::Closed
                            && (state != AdmissionState::PrBlocked || u.scope != Scope::Pr)
                            && !d.blocked_classes.contains(&u.class)
                            && max_slots(d, self.cfg.classes.spec(u.class)) > 0
                            && (d.bids.is_empty()
                                || d.bids.iter().any(|b| {
                                    b.class == u.class
                                        && b.scope == u.scope
                                        && b.available_slots > 0
                                }))
                    })
                    .map(|d| d.id.as_str())
            })
            .collect();
        let (vcpu, mem, slots) = drones
            .iter()
            .filter(|d| eligible.contains(d.id.as_str()))
            .fold((0u64, 0u64, 0u64), |(c, m, slots), d| {
                let capacity = Class::ALL
                    .into_iter()
                    .map(|class| max_slots(d, self.cfg.classes.spec(class)))
                    .max()
                    .unwrap_or(0);
                (
                    c.saturating_add(u64::from(d.vcpu)),
                    m.saturating_add(d.mem_mib),
                    slots.saturating_add(u64::from(capacity)),
                )
            });
        let (mut used_cpu, mut used_mem, mut used_slots) = inflight
            .iter()
            .filter(|u| u.drone.as_deref().is_some_and(|d| eligible.contains(d)))
            .fold((0u64, 0u64, 0u64), |(c, m, slots), u| {
                let spec = self.cfg.classes.spec(u.class);
                (
                    c.saturating_add(u64::from(spec.vcpu)),
                    m.saturating_add(spec.mem_mib),
                    slots.saturating_add(1),
                )
            });
        let budget = policy.share.map(|percent| {
            let portion = |n: u64| n.saturating_mul(u64::from(percent)).div_ceil(100);
            (portion(vcpu), portion(mem), portion(slots))
        });
        let mut loads: BTreeMap<&str, usize> = BTreeMap::new();
        for u in &inflight {
            *loads.entry(&u.client).or_default() += 1;
        }
        let mut cursors = self
            .unit_cursor
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut remaining: Vec<usize> = (0..queued.len()).collect();
        let mut reservations = policy.reserved.to_vec();
        let mut placed = UnitPlacement::default();
        let mut history = VecDeque::new();
        while !remaining.is_empty() {
            // The share is a soft boundary: allow one whole unit to cross it,
            // so even a one-slot fleet and heavy units can make progress.
            if budget
                .is_some_and(|(c, m, slots)| used_cpu >= c || used_mem >= m || used_slots >= slots)
            {
                break;
            }
            let next = remaining
                .iter()
                .enumerate()
                .min_by_key(|(_, i)| {
                    let u = &queued[**i];
                    let age = u64::try_from(now.saturating_sub(u.created_at)).unwrap_or(0);
                    let starving = self
                        .cfg
                        .starvation_after_secs
                        .is_some_and(|after| age >= after);
                    let after_cursor = cursors.get(&u.scope).is_none_or(|last| &u.client > last);
                    (
                        u.scope,
                        std::cmp::Reverse(starving),
                        starving.then_some(std::cmp::Reverse(u.class)),
                        loads.get(u.client.as_str()).copied().unwrap_or(0),
                        std::cmp::Reverse(after_cursor),
                        &u.client,
                        u.created_at,
                        &u.id,
                    )
                })
                .map(|(i, _)| i)
                .expect("remaining is nonempty");
            let i = remaining.remove(next);
            let u = &queued[i];
            let key = (u.platform.as_str(), u.rust_toolchain.as_deref());
            let mut group = groups[&key].clone();
            spend_bids(&mut group, &placed.leases);
            let backlog: Vec<Demand> = backlogs[&key]
                .iter()
                .filter(|(_, count)| **count > 0)
                .map(|(&(class, scope), &count)| Demand {
                    class,
                    scope,
                    count,
                })
                .collect();
            let mut allocation = allocate_reserved_with_backlog(
                &AllocatorInput {
                    classes: self.cfg.classes.clone(),
                    demand: vec![Demand {
                        class: u.class,
                        scope: u.scope,
                        count: 1,
                    }],
                    drones: group,
                    // Pending runner leases must consume room without offsetting
                    // unit demand: they execute a different queue's work.
                    leases: base
                        .iter()
                        .cloned()
                        .map(|mut l| {
                            l.state = LeaseState::Running;
                            l
                        })
                        .collect(),
                    warm_floors: Vec::new(),
                    queue_ages: vec![QueueAge {
                        class: u.class,
                        scope: u.scope,
                        oldest_age_secs: u64::try_from(now.saturating_sub(u.created_at))
                            .unwrap_or(0),
                    }],
                    starvation_after_secs: self.cfg.starvation_after_secs,
                    weights: ScoreWeights::default(),
                    admission,
                },
                &reservations,
                &backlog,
            );
            for r in allocation.reserved_for {
                if !reservations.iter().any(|old| old.drone_id == r.drone_id) {
                    reservations.push(r.clone());
                    placed.reserved.push(r);
                }
            }
            self.enrich_decisions(
                &mut allocation.decisions,
                &rows_by_id,
                u.rust_toolchain.as_deref(),
                now,
            );
            let leased = match allocation.grants.first() {
                Some(g) => self.store.lease_unit(&u.id, &g.drone_id, now).await,
                None => Ok(false),
            };
            let outcome = if allocation.grants.is_empty() {
                PlacementOutcome::Unplaced
            } else if matches!(leased, Ok(true)) {
                PlacementOutcome::Granted
            } else {
                PlacementOutcome::Failed
            };
            for decision in allocation.decisions {
                // Only the newest retained records can survive store pruning.
                if history.len() == MAX_HISTORY as usize {
                    history.pop_front();
                }
                let outcome = if decision.winner.is_none() {
                    PlacementOutcome::Unplaced
                } else {
                    outcome
                };
                history.push_back((u.id.clone(), u.client.clone(), outcome, decision));
            }
            let leased = match leased {
                Ok(leased) => leased,
                Err(e) => {
                    self.record_unit_placements(now, &history).await;
                    return Err(e.into());
                }
            };
            if leased && let Some(g) = allocation.grants.first() {
                // Only persisted leases cover backlog; failed and attempted
                // unplaced work continues to contribute to demand pressure.
                let count = backlogs
                    .get_mut(&key)
                    .and_then(|counts| counts.get_mut(&(u.class, u.scope)))
                    .expect("queued unit has a backlog count");
                *count = count.saturating_sub(1);
                let lease = ActiveLease {
                    drone_id: g.drone_id.clone(),
                    class: g.class,
                    scope: g.scope,
                    state: LeaseState::Running,
                };
                base.push(lease.clone());
                placed.leases.push(lease);
                let spec = self.cfg.classes.spec(u.class);
                used_cpu = used_cpu.saturating_add(u64::from(spec.vcpu));
                used_mem = used_mem.saturating_add(spec.mem_mib);
                used_slots = used_slots.saturating_add(1);
                *loads.entry(&u.client).or_default() += 1;
                cursors.insert(u.scope, u.client.clone());
                self.unit_cursor
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(u.scope, u.client.clone());
                tracing::info!("unit {} placed on {}", u.id, g.drone_id);
            }
        }
        self.record_unit_placements(now, &history).await;
        Ok(placed)
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

    /// Fill the existing telemetry owner from GitHub's job records. The durable
    /// binding is needed because a workflow can finish between queue polls; a
    /// transient cache would lose that final outcome on a queen restart.
    async fn collect_job_telemetry(
        &self,
        observed: &[(&str, WorkflowJob)],
        now: i64,
    ) -> Result<(), QueenError> {
        let prefix = self.runner_prefix();
        let by_job: HashMap<_, _> = observed
            .iter()
            .map(|(repo, job)| ((*repo, job.id), job))
            .collect();
        for (repo, job) in observed {
            if (job.status == "completed" && job.telemetry(repo).is_none())
                || !job
                    .runner_name
                    .as_deref()
                    .is_some_and(|name| name.starts_with(&prefix))
            {
                continue;
            }
            if let (Some(id), Some(name)) = (job.runner_id, job.runner_name.as_deref())
                && let Some(lease) = self.store.lease_for_runner(id, name).await?
            {
                self.store.remember_job(&lease, repo, job.id, now).await?;
            }
        }
        let mut pending = self.store.pending_jobs().await?;
        if !pending.is_empty() {
            // Rotate the starting point so a slow old job cannot starve later ones.
            let tick =
                now.div_euclid(i64::try_from(self.cfg.poll_interval_secs).unwrap_or(i64::MAX));
            let offset = tick.rem_euclid(pending.len() as i64) as usize;
            pending.rotate_left(offset);
        }
        let mut lookups = 0;
        for (lease, repo, id) in pending {
            let job = if let Some(job) = by_job.get(&(repo.as_str(), id)) {
                Cow::Borrowed(*job)
            } else {
                if lookups >= 16 {
                    continue;
                }
                lookups += 1;
                match tokio::time::timeout(
                    std::time::Duration::from_secs(1),
                    self.github.job(&repo, id),
                )
                .await
                {
                    Ok(Ok(job)) => Cow::Owned(job),
                    Ok(Err(GithubError::Status { status: 404, .. })) => {
                        self.store.forget_job(&lease.id).await?;
                        continue;
                    }
                    Ok(Err(e)) => {
                        tracing::warn!("job {repo}/{id} telemetry unavailable: {e}");
                        continue;
                    }
                    Err(_) => {
                        tracing::warn!("job {repo}/{id} telemetry lookup timed out");
                        continue;
                    }
                }
            };
            if job.id != id
                || job.runner_id != Some(lease.runner_id)
                || job.runner_name.as_deref() != Some(lease.runner_name.as_str())
            {
                continue;
            }
            if let Some((telemetry, duration, finished)) = job.telemetry(&repo) {
                self.store
                    .record_github_telemetry(&lease, &telemetry, duration, finished)
                    .await?;
            }
            if job.status == "completed" {
                // A terminal response with unusable timing cannot yield a sample.
                self.store.forget_job(&lease.id).await?;
            }
        }
        Ok(())
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
            let standby = lease.state == LeaseStatus::Standby;
            // A standby waits for its drone indefinitely: only `lease_max_secs`
            // rotates it.
            let never_came_online =
                !standby && now > lease.expires_at && runner.is_none_or(|r| r.status != "online");
            match runner {
                _ if overdue => {
                    self.end(&lease, runner.copied(), LeaseStatus::Expired, now)
                        .await?
                }
                Some(r) if r.busy => {
                    if matches!(lease.state, LeaseStatus::Pending | LeaseStatus::Standby) {
                        self.store
                            .set_state(&lease.id, LeaseStatus::Running, now)
                            .await?;
                    }
                    still_active.push(lease);
                }
                // Ephemeral runners deregister after their one job.
                // A standby whose registration is gone was started and has
                // finished (or GitHub dropped it): the next tick mints another.
                None if matches!(lease.state, LeaseStatus::Running | LeaseStatus::Standby) => {
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
    /// Remove drones silent past their `drone_expiry` window (never one named
    /// in `wake.hosts`, never one that has not heartbeated yet) and log each
    /// removal as a `drone_expired` health event.
    async fn expire_drones(&self, now: i64) -> Result<(), StoreError> {
        for d in self.store.drones().await? {
            let Some(last) = d.last_heartbeat else {
                continue;
            };
            let Some(after) = self.cfg.drone_expiry.expiry_for(&d.name) else {
                continue;
            };
            let silent = now - last;
            if silent <= after.max(self.cfg.drone_stale_secs)
                || self.cfg.wake.hosts.iter().any(|h| h.name == d.name)
            {
                continue;
            }
            if let Some(dropped) = self.store.remove_drone(&d.name).await? {
                let reason = format!(
                    "no heartbeat for {silent}s (expiry {after}s); {dropped} unfinished leases dropped"
                );
                tracing::warn!("expired drone {}: {reason}", d.name);
                self.store
                    .record_health_event(now, &d.name, "drone_expired", &reason)
                    .await?;
            }
        }
        Ok(())
    }

    /// Log drones that went stale since the last pass, and raise or clear the
    /// capacity alarm per class. Returns the alarm state for the snapshot.
    async fn watch_fleet(
        &self,
        now: i64,
        rows: &[DroneRow],
        fresh: &BTreeSet<String>,
        slots: &BTreeMap<Class, u32>,
        unmet: &[Unmet],
    ) -> (BTreeMap<Class, i64>, BTreeSet<Class>) {
        let alarm = &self.cfg.capacity_alarm;
        let stale: Vec<&str> = rows
            .iter()
            .filter(|d| !fresh.contains(&d.name))
            .map(|d| d.name.as_str())
            .collect();
        let mut events: Vec<(String, &'static str, String)> = Vec::new();
        let state = {
            let mut w = self.watch.lock().expect("fleet watch lock");
            if let Some(prev) = &w.prev_fresh {
                for d in rows
                    .iter()
                    .filter(|d| prev.contains(&d.name) && !fresh.contains(&d.name))
                {
                    let age = d.last_heartbeat.map_or(-1, |t| now - t);
                    events.push((
                        d.name.clone(),
                        "drone_stale",
                        format!("no heartbeat for {age}s"),
                    ));
                }
            }
            w.prev_fresh = Some(fresh.clone());
            for class in Class::ALL {
                let main: u32 = unmet
                    .iter()
                    .filter(|u| u.class == class && u.scope == Scope::Main)
                    .map(|u| u.count)
                    .sum();
                let have = slots.get(&class).copied().unwrap_or(0);
                if alarm.enabled
                    && alarm.classes.contains(&class)
                    && main > 0
                    && have <= alarm.max_slots
                {
                    let since = *w.starved_since.entry(class).or_insert(now);
                    if now - since >= alarm.after_secs && w.raised.insert(class) {
                        events.push((
                            "fleet".to_owned(),
                            "capacity_starved",
                            format!(
                                "{class}: {main} main jobs unmet for {}s with {have} schedulable slots; stale drones: [{}]",
                                now - since,
                                stale.join(", ")
                            ),
                        ));
                    }
                } else {
                    w.starved_since.remove(&class);
                    if w.raised.remove(&class) {
                        events.push((
                            "fleet".to_owned(),
                            "capacity_restored",
                            format!("{class}: {have} schedulable slots, {main} main jobs unmet"),
                        ));
                    }
                }
            }
            (w.starved_since.clone(), w.raised.clone())
        };
        for (drone, event, reason) in events {
            tracing::warn!("{event} {drone}: {reason}");
            if let Err(e) = self
                .store
                .record_health_event(now, &drone, event, &reason)
                .await
            {
                tracing::warn!("record {event} failed: {e}");
            }
        }
        state
    }

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

    fn lease_row(id: &str, runner: &str) -> LeaseRow {
        LeaseRow {
            id: id.to_owned(),
            drone: "host-a".to_owned(),
            class: Class::Medium,
            scope: Scope::Pr,
            labels: Vec::new(),
            runner_name: runner.to_owned(),
            runner_id: 1,
            jit_config: None,
            state: LeaseStatus::Running,
            created_at: 0,
            expires_at: 0,
        }
    }

    fn job(id: u64, status: &str, runner: &str) -> WorkflowJob {
        serde_json::from_value(serde_json::json!({
            "id": id, "run_id": 77, "status": status, "created_at": "2026-01-01T00:00:00Z",
            "started_at": "2026-01-01T00:10:00Z", "runner_name": runner,
        }))
        .unwrap()
    }

    #[test]
    fn only_leases_with_an_in_progress_job_on_their_runner_are_preemptible() {
        let active = [
            lease_row("l1", "borg-host-a-l1"),
            lease_row("l2", "borg-host-a-l2"),
            lease_row("l3", "borg-host-a-l3"),
        ];
        let observed = [
            ("example-org/app", job(5, "in_progress", "borg-host-a-l1")),
            ("example-org/app", job(6, "queued", "borg-host-a-l2")),
        ];
        let preempted = HashSet::from(["l1"]);
        let (running, others, jobs) = split_running(&active, &[], &observed, &preempted);
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].lease_id, "l1");
        assert_eq!(running[0].started_at, 1_767_226_200);
        assert!(running[0].preempted);
        assert_eq!(others.len(), 2);
        assert_eq!(jobs["l1"], ("example-org/app", 77, 5));
    }
}
