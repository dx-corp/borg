//! The lease allocator: a pure function from fleet state to lease grants.
//!
//! Inputs are queued demand per (class, scope), the drones' usable capacity,
//! leases already in flight, warm floors and per-host caps. The output says
//! which drone should run which runner, with the exact label set to mint it
//! with. Nothing here does I/O, reads a clock or draws a random number, so the
//! same input always yields the same grants.
//!
//! Rules, in priority order:
//!
//! 1. Deploy demand is satisfied before main demand (bare label), and main
//!    before pull-request demand. Within a scope larger classes are placed
//!    first so they are not starved of contiguous room by small ones.
//! 2. A main grant carries only the bare label; a pull-request grant carries
//!    both labels; a deploy grant carries only the `-deploy` label (see
//!    [`Classes::runner_labels`]). Deploy demand is placed only on
//!    [`Trust::Deploy`] drones and pull-request demand never is.
//! 3. Queued demand is first offset by leases that were granted but have not
//!    yet taken a job ([`LeaseState::Pending`]); only the shortfall is minted.
//! 4. A warm floor keeps that many pending leases of a (class, scope) alive
//!    even with nothing queued, so the first job never waits on a cold mint.
//! 5. A grant is only made if the drone still has the memory, vCPU and lease
//!    slots for it after every in-flight lease and earlier grant. Hosts are
//!    carved dynamically: the same drone can end up as one heavy, two medium
//!    or several light slots depending on what is queued.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::bid::{
    ClassPressure, DroneBid, ProfileScore, ScoreWeights, demand_pressure, profile_term, score,
};
use crate::classes::{Class, ClassSpec, Classes, Scope, Trust};
use crate::probe::Capacity;

/// Owner-set limits for one host ("this PC gives at most 8 cores / 24 GiB").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HostCaps {
    pub max_vcpu: Option<u32>,
    pub max_mem_mib: Option<u64>,
    /// Maximum concurrent leases on the host, whatever their size.
    pub max_leases: Option<u32>,
    /// Memory kept back for the operating system and the drone itself.
    pub reserve_mem_mib: u64,
}

impl Default for HostCaps {
    fn default() -> Self {
        Self {
            max_vcpu: None,
            max_mem_mib: None,
            max_leases: None,
            reserve_mem_mib: 2048,
        }
    }
}

/// One drone as the allocator sees it: budgets already net of caps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DroneState {
    pub id: String,
    pub vcpu: u32,
    pub mem_mib: u64,
    pub docker: bool,
    /// False while the drone drains (SIGTERM) or yields to its owner.
    pub accepting: bool,
    pub max_leases: Option<u32>,
    /// Live (unexpired) bids. Empty means a legacy drone: always eligible,
    /// neutral score. See [`crate::bid`].
    #[serde(default)]
    pub bids: Vec<DroneBid>,
    /// Learned per-class profiles; add to the score, see [`crate::profile`].
    #[serde(default)]
    pub profiles: Vec<ProfileScore>,
    /// Classes the build environment contract does not admit on this drone
    /// right now; see [`crate::envcontract`]. The allocator never places them.
    #[serde(default)]
    pub blocked_classes: Vec<Class>,
    /// Which scopes the host may run; see [`Trust::admits`].
    #[serde(default)]
    pub trust: Trust,
    /// Recent CPU pressure (the mean of `some avg10` readings over the
    /// admission window, permille); `None` is not measured or stale. Only
    /// read when [`AllocatorInput::admission`] is set.
    #[serde(default)]
    pub cpu_psi_pm: Option<u16>,
    /// CPU pressure estimated from the load average, for a drone without PSI
    /// (see [`load_pressure_pm`]). Admission uses it only when `cpu_psi_pm`
    /// is `None`.
    #[serde(default)]
    pub(crate) cpu_load_pm: Option<u16>,
}

/// CPU pressure (permille) estimated from the 1-minute load average per CPU
/// (`load_per_cpu_pm`, 1000 = one runnable task per CPU) for hosts without
/// PSI. With `L` runnable tasks on `N` CPUs, `(L - N) / L` of them wait, which
/// approximates PSI `cpu some`. When the root cgroup shows the CPUs under 90%
/// busy (`cpu_busy_pm`), the run queue is not CPU contention (I/O wait, or an
/// lxcfs load average) and the estimate is 0.
pub(crate) fn load_pressure_pm(
    load_per_cpu_pm: Option<u32>,
    cpu_busy_pm: Option<u16>,
) -> Option<u16> {
    let load = u64::from(load_per_cpu_pm?);
    if load <= 1000 || cpu_busy_pm.is_some_and(|b| b < 900) {
        return Some(0);
    }
    u16::try_from((load - 1000) * 1000 / load).ok()
}

/// CPU-pressure thresholds (permille) for admission; see [`AdmissionState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionThresholds {
    pub psi_soft_pm: u16,
    /// From this pressure only light main and deploy grants; at or above
    /// `psi_hard_pm` it has no effect.
    pub(crate) psi_light_only_pm: u16,
    pub psi_hard_pm: u16,
}

/// What a drone may be granted given its CPU pressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionState {
    /// Below the soft threshold, unmeasured, or admission is off.
    Open,
    /// At or above soft: pull-request grants are shed, everything else placed.
    PrBlocked,
    /// At or above `psi_light_only_pm`: pull-request grants are shed, and main
    /// work only of the light class; deploy work is still placed.
    LightOnly,
    /// At or above hard: no new grant of any scope.
    Closed,
}

impl AdmissionState {
    pub const ALL: [AdmissionState; 4] =
        [Self::Open, Self::PrBlocked, Self::LightOnly, Self::Closed];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::PrBlocked => "pr_blocked",
            Self::LightOnly => "light_only",
            Self::Closed => "closed",
        }
    }

    /// The state for a drone's pressure: its PSI reading, or the load
    /// estimate when it has no PSI.
    pub fn of(pressure: Option<u16>, t: Option<AdmissionThresholds>) -> Self {
        match (pressure, t) {
            (Some(p), Some(t)) if p >= t.psi_hard_pm => Self::Closed,
            (Some(p), Some(t)) if p >= t.psi_light_only_pm => Self::LightOnly,
            (Some(p), Some(t)) if p >= t.psi_soft_pm => Self::PrBlocked,
            _ => Self::Open,
        }
    }

    pub(crate) fn admits(self, class: Class, scope: Scope) -> bool {
        match self {
            Self::Open => true,
            Self::PrBlocked => scope != Scope::Pr,
            Self::LightOnly => {
                scope == Scope::Deploy || (scope == Scope::Main && class == Class::Light)
            }
            Self::Closed => false,
        }
    }
}

impl DroneState {
    /// The pressure admission uses: PSI when measured, else the load estimate.
    pub(crate) fn admission_pressure_pm(&self) -> Option<u16> {
        self.cpu_psi_pm.or(self.cpu_load_pm)
    }
}

/// One drone's admission state this pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DroneAdmission {
    pub drone_id: String,
    pub state: AdmissionState,
    pub cpu_psi_pm: Option<u16>,
    /// The load-average estimate; drives `state` only when `cpu_psi_pm` is
    /// `None`.
    #[serde(default)]
    pub(crate) cpu_load_pm: Option<u16>,
}

impl DroneState {
    /// Budget a probed host: apply the OS reserve and the owner's caps.
    pub fn from_capacity(id: impl Into<String>, capacity: &Capacity, caps: &HostCaps) -> Self {
        let mem = capacity.mem_mib.saturating_sub(caps.reserve_mem_mib);
        Self {
            id: id.into(),
            vcpu: caps
                .max_vcpu
                .map_or(capacity.vcpu, |m| m.min(capacity.vcpu)),
            mem_mib: caps.max_mem_mib.map_or(mem, |m| m.min(mem)),
            docker: capacity.docker_usable,
            accepting: true,
            max_leases: caps.max_leases,
            bids: Vec::new(),
            profiles: Vec::new(),
            blocked_classes: Vec::new(),
            cpu_psi_pm: None,
            cpu_load_pm: None,
            trust: Trust::Shared,
        }
    }
}

/// Whether a lease's runner has picked up a job yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LeaseState {
    /// A deploy runner held unstarted by its drone. It holds capacity like a
    /// pending deploy lease and absorbs deploy demand by promotion: the drone
    /// is told to start it instead of the queen minting another runner.
    Standby,
    /// Granted or minted, runner idle: can still absorb queued demand.
    Pending,
    /// Runner is executing a job: only consumes capacity.
    Running,
}

/// A lease already granted and not yet finished.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveLease {
    pub drone_id: String,
    pub class: Class,
    pub scope: Scope,
    pub state: LeaseState,
}

/// Queued jobs waiting for a runner of `(class, scope)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Demand {
    pub class: Class,
    pub scope: Scope,
    pub count: u32,
}

/// Keep at least `count` pending leases of `(class, scope)` alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WarmFloor {
    pub class: Class,
    pub scope: Scope,
    pub count: u32,
}

/// Default floor: one idle main-only medium runner.
pub fn default_warm_floors() -> Vec<WarmFloor> {
    vec![WarmFloor {
        class: Class::Medium,
        scope: Scope::Main,
        count: 1,
    }]
}

/// Age of the oldest queued job for `(class, scope)`, from the queen's view of
/// the jobs' `created_at`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueAge {
    pub class: Class,
    pub scope: Scope,
    pub oldest_age_secs: u64,
}

/// Everything the allocator needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllocatorInput {
    pub classes: Classes,
    pub demand: Vec<Demand>,
    pub drones: Vec<DroneState>,
    pub leases: Vec<ActiveLease>,
    pub warm_floors: Vec<WarmFloor>,
    pub queue_ages: Vec<QueueAge>,
    /// Demand that has waited at least this long is starving; `None` turns
    /// the guard off.
    pub starvation_after_secs: Option<u64>,
    pub weights: ScoreWeights,
    /// CPU-pressure admission; `None` turns it off.
    pub admission: Option<AdmissionThresholds>,
    /// Pull-request turns; the default keeps strict main-before-PR order.
    pub pr_turn: PrTurn,
}

/// Main demand is served before pull-request demand, so a main queue that
/// never empties starves pull requests forever. A class listed here gives one
/// grant to its pull-request demand, if that has waited `after_secs`, before
/// its main demand is placed. The queen lists a class once main has taken
/// enough grants in a row; see `pr_fairness` in the queen config.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PrTurn {
    /// Pull-request demand that has waited at least this long may take a
    /// turn; `None` turns turns off.
    pub(crate) after_secs: Option<u64>,
    /// Classes whose next grant goes to starving pull-request demand.
    pub(crate) classes: Vec<Class>,
}

/// An instruction to mint one runner and hand it to one drone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub drone_id: String,
    pub class: Class,
    pub scope: Scope,
    /// Exact label set to mint the runner with.
    pub labels: Vec<String>,
    pub mem_mib: u64,
    pub vcpu: u32,
}

/// Demand that could not be placed this round.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unmet {
    pub class: Class,
    pub scope: Scope,
    pub count: u32,
}

/// A drone held back for starving demand: it takes no new lease except one of
/// this `(class, scope)` until that lease fits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reservation {
    pub drone_id: String,
    pub class: Class,
    pub scope: Scope,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Allocation {
    pub grants: Vec<Grant>,
    pub unmet: Vec<Unmet>,
    pub reserved_for: Vec<Reservation>,
    /// Demand pressure per class this pass, to publish to drones.
    pub pressure: Vec<ClassPressure>,
    /// Slots each class could still be granted before this pass's grants.
    pub idle: Vec<ClassIdle>,
    /// Every drone's admission state this pass, by id.
    pub admission: Vec<DroneAdmission>,
    /// Standby deploy runners to start now, per class: queued deploy demand
    /// that pending runners do not cover, up to the standbys that exist.
    pub promote: Vec<Promotion>,
    /// Decisions captured by the same evaluation that selected each grant.
    #[serde(default)]
    pub decisions: Vec<PlacementDecision>,
    /// Extra decisions omitted to bound diagnostic memory; placement continues.
    #[serde(default)]
    pub omitted_decisions: u32,
}

/// Diagnostic bounds do not limit fleet size or the number of grants.
pub const MAX_DECISIONS: usize = 128;
pub const MAX_CANDIDATES: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rejection {
    NotAccepting,
    AdmissionClosed,
    PrPressure,
    /// CPU pressure at or above `psi_light_only_pm`: no main medium or heavy.
    LightOnlyPressure,
    Trust,
    Environment,
    Docker,
    Memory,
    Vcpu,
    LeaseCap,
    Reserved,
    NoBid,
    BidExhausted,
    BidThreshold,
}

/// Facts at one placement attempt, before its winner consumes capacity. The
/// budget is already net of the owner's caps and OS reserve; room additionally
/// subtracts active leases and earlier grants. No command or credentials enter
/// this diagnostic type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub drone_id: String,
    pub budget: Room,
    pub room: Room,
    pub accepting: bool,
    pub trust: Trust,
    pub admission: AdmissionState,
    pub reserved_for: Option<(Class, Scope)>,
    pub rejection_reasons: Vec<Rejection>,
    /// The queen may attach required failing checks from the same environment
    /// evaluation that blocked this class. Raw observed values are excluded.
    #[serde(default)]
    pub environment_checks: BTreeMap<crate::envcontract::CheckKind, crate::envcontract::CheckState>,
    #[serde(default)]
    pub capability_checks: BTreeMap<crate::envcontract::Capability, crate::envcontract::CheckState>,
    pub bid: Option<DroneBid>,
    pub bid_score: i64,
    pub profile_score: i64,
    /// Current load: minus the drone's CPU pressure, plus its idle share for
    /// medium and heavy work. See [`host_term`].
    #[serde(default)]
    pub(crate) host_score: i64,
    pub score: Option<i64>,
}

/// The placement term for how loaded a drone is right now: `-host_pressure *
/// pressure` (PSI, or the load estimate without PSI; unknown counts as 0) and,
/// for medium and heavy work, `+idle_share * free vCPU share`. Light work packs
/// as before. Together with the learned speed in the profile term, main heavy
/// and medium work goes to fast, idle drones first and to slow or loaded ones
/// only when nothing better has room.
fn host_term(drone: &DroneState, free_vcpu: u32, class: Class, w: &ScoreWeights) -> i64 {
    let pressure = i64::from(drone.admission_pressure_pm().unwrap_or(0).min(1000));
    let idle = if class == Class::Light || drone.vcpu == 0 {
        0
    } else {
        i64::from(free_vcpu.min(drone.vcpu)) * 1000 / i64::from(drone.vcpu)
    };
    -w.host_pressure * pressure + w.idle_share * idle / 1000
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementDecision {
    pub class: Class,
    pub scope: Scope,
    /// Index in this allocation's grant order. Failed attempts do not advance it.
    pub grant_order: usize,
    pub requested: ClassSpec,
    pub pressure: u16,
    pub candidates: Vec<Candidate>,
    pub total_candidates: usize,
    /// A host held for this demand after an unsuccessful starving attempt.
    pub reservation: Option<String>,
    /// Retained even when the winning host is beyond the candidate limit.
    pub winner: Option<String>,
}

/// Start `count` standby deploy runners of `class`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Promotion {
    pub class: Class,
    pub count: u32,
}

/// Idle room for one class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassIdle {
    pub class: Class,
    pub idle_slots: u32,
}

/// How many leases of `spec` this drone could hold if it were empty.
pub fn max_slots(drone: &DroneState, spec: &ClassSpec) -> u32 {
    if spec.docker && !drone.docker {
        return 0;
    }
    let by_mem = drone.mem_mib / spec.mem_mib.max(1);
    let by_cpu = u64::from(drone.vcpu / spec.vcpu.max(1));
    let by_cap = drone.max_leases.map_or(u64::MAX, u64::from);
    u32::try_from(by_mem.min(by_cpu).min(by_cap)).unwrap_or(u32::MAX)
}

/// What is left of a drone's budget after the leases it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Room {
    pub mem_mib: u64,
    pub vcpu: u32,
    pub leases_left: Option<u32>,
}

/// The room left on `drone` after `held` (leases on other drones are ignored).
pub fn free_room(drone: &DroneState, held: &[ActiveLease], classes: &Classes) -> Room {
    let mut room = Room {
        mem_mib: drone.mem_mib,
        vcpu: drone.vcpu,
        leases_left: drone.max_leases,
    };
    for l in held.iter().filter(|l| l.drone_id == drone.id) {
        let spec = classes.spec(l.class);
        room.mem_mib = room.mem_mib.saturating_sub(spec.mem_mib);
        room.vcpu = room.vcpu.saturating_sub(spec.vcpu);
        room.leases_left = room.leases_left.map(|n| n.saturating_sub(1));
    }
    room
}

/// Whole leases of `spec` that fit in `room` right now; 0 if the drone takes
/// no new leases or lacks Docker for the class.
pub fn free_slots(drone: &DroneState, room: &Room, spec: &ClassSpec) -> u32 {
    if !drone.accepting || (spec.docker && !drone.docker) {
        return 0;
    }
    let by_mem = room.mem_mib / spec.mem_mib.max(1);
    let by_cpu = u64::from(room.vcpu / spec.vcpu.max(1));
    let by_cap = room.leases_left.map_or(u64::MAX, u64::from);
    u32::try_from(by_mem.min(by_cpu).min(by_cap)).unwrap_or(u32::MAX)
}

/// Remaining room on a drone while allocating.
struct Free<'a> {
    drone: &'a DroneState,
    mem_mib: u64,
    vcpu: u32,
    leases_left: Option<u32>,
    in_flight: u32,
    admission: AdmissionState,
    /// Grants made to this drone this pass, per key (bounds `available_slots`).
    granted: BTreeMap<(Class, Scope), u32>,
}

/// One evaluation shared by placement diagnostics and reservation admission.
struct BidEvaluation {
    bid: Option<DroneBid>,
    bid_score: i64,
    profile_score: i64,
    host_score: i64,
    rejection_reasons: Vec<Rejection>,
    score: Option<i64>,
}

impl Free<'_> {
    fn evaluate_bid(
        &self,
        class: Class,
        scope: Scope,
        pressure: u16,
        weights: &ScoreWeights,
    ) -> BidEvaluation {
        let profile_score = self
            .drone
            .profiles
            .iter()
            .find(|p| p.class == class)
            .map_or(0, |p| profile_term(p, weights));
        let host_score = host_term(self.drone, self.vcpu, class, weights);
        let bid = self
            .drone
            .bids
            .iter()
            .find(|b| b.class == class && b.scope == scope)
            .copied();
        let mut rejection_reasons = Vec::new();
        if !self.drone.bids.is_empty() {
            match bid {
                None => rejection_reasons.push(Rejection::NoBid),
                Some(b) => {
                    let used = self.granted.get(&(class, scope)).copied().unwrap_or(0);
                    if used >= b.available_slots {
                        rejection_reasons.push(Rejection::BidExhausted);
                    }
                    if pressure < b.min_pressure.min(1000) {
                        rejection_reasons.push(Rejection::BidThreshold);
                    }
                }
            }
        }
        let bid_score = bid.as_ref().map_or(0, |b| score(b, weights));
        BidEvaluation {
            bid,
            bid_score,
            profile_score,
            host_score,
            score: rejection_reasons.is_empty().then_some(
                bid_score
                    .saturating_add(profile_score)
                    .saturating_add(host_score),
            ),
            rejection_reasons,
        }
    }

    /// The score this drone offers for `(class, scope)` at `pressure`, or
    /// `None` if it declines.
    fn offer(
        &self,
        class: Class,
        scope: Scope,
        pressure: u16,
        weights: &ScoreWeights,
    ) -> Option<i64> {
        self.evaluate_bid(class, scope, pressure, weights).score
    }

    /// Backfill may share a reserved host only when its sole existing lease
    /// can finish and leave room for the reserved job alongside every new
    /// grant. With several existing leases, preserve the full drain instead
    /// of guessing their completion order.
    fn preserves_reservation(&self, classes: &Classes, reserved: Class, class: Class) -> bool {
        if self.in_flight != 1 {
            return false;
        }
        let reserved = classes.spec(reserved);
        let candidate = classes.spec(class);
        let mut mem = u128::from(reserved.mem_mib) + u128::from(candidate.mem_mib);
        let mut cpu = u128::from(reserved.vcpu) + u128::from(candidate.vcpu);
        let mut leases = 2_u128;
        for (&(granted_class, _), &count) in &self.granted {
            let spec = classes.spec(granted_class);
            mem += u128::from(count) * u128::from(spec.mem_mib);
            cpu += u128::from(count) * u128::from(spec.vcpu);
            leases += u128::from(count);
        }
        mem <= u128::from(self.drone.mem_mib)
            && cpu <= u128::from(self.drone.vcpu)
            && self
                .drone
                .max_leases
                .is_none_or(|cap| leases <= u128::from(cap))
    }

    /// Eligibility and scores are evaluated once, for both selection and its
    /// explanation. Keeping this next to `offer` prevents a second scheduler.
    fn candidate(
        &self,
        input: &AllocatorInput,
        class: Class,
        scope: Scope,
        pressure: u16,
        reserved_for: Option<(Class, Scope)>,
    ) -> Candidate {
        let spec = input.classes.spec(class);
        let mut reasons = Vec::new();
        if !self.drone.accepting {
            reasons.push(Rejection::NotAccepting);
        }
        match self.admission {
            AdmissionState::Closed => reasons.push(Rejection::AdmissionClosed),
            AdmissionState::PrBlocked | AdmissionState::LightOnly if scope == Scope::Pr => {
                reasons.push(Rejection::PrPressure);
            }
            AdmissionState::LightOnly if !self.admission.admits(class, scope) => {
                reasons.push(Rejection::LightOnlyPressure);
            }
            _ => {}
        }
        if !self.drone.trust.admits(scope) {
            reasons.push(Rejection::Trust);
        }
        if self.drone.blocked_classes.contains(&class) {
            reasons.push(Rejection::Environment);
        }
        if spec.docker && !self.drone.docker {
            reasons.push(Rejection::Docker);
        }
        if self.mem_mib < spec.mem_mib {
            reasons.push(Rejection::Memory);
        }
        if self.vcpu < spec.vcpu {
            reasons.push(Rejection::Vcpu);
        }
        if self.leases_left == Some(0) {
            reasons.push(Rejection::LeaseCap);
        }
        if reserved_for.is_some_and(|key| {
            key != (class, scope) && !self.preserves_reservation(&input.classes, key.0, class)
        }) {
            reasons.push(Rejection::Reserved);
        }
        let evaluated = self.evaluate_bid(class, scope, pressure, &input.weights);
        reasons.extend(evaluated.rejection_reasons);
        Candidate {
            drone_id: self.drone.id.clone(),
            budget: Room {
                mem_mib: self.drone.mem_mib,
                vcpu: self.drone.vcpu,
                leases_left: self.drone.max_leases,
            },
            room: Room {
                mem_mib: self.mem_mib,
                vcpu: self.vcpu,
                leases_left: self.leases_left,
            },
            accepting: self.drone.accepting,
            trust: self.drone.trust,
            admission: self.admission,
            reserved_for,
            score: if reasons.is_empty() {
                evaluated.score
            } else {
                None
            },
            rejection_reasons: reasons,
            environment_checks: BTreeMap::new(),
            capability_checks: BTreeMap::new(),
            bid: evaluated.bid,
            bid_score: evaluated.bid_score,
            profile_score: evaluated.profile_score,
            host_score: evaluated.host_score,
        }
    }

    fn fits(&self, spec: &ClassSpec) -> bool {
        self.drone.accepting
            && self.admission != AdmissionState::Closed
            && (!spec.docker || self.drone.docker)
            && self.mem_mib >= spec.mem_mib
            && self.vcpu >= spec.vcpu
            && self.leases_left != Some(0)
    }

    /// Could this drone ever hold `spec` if it were empty?
    /// Whole leases of `spec` the remaining room holds.
    fn slots(&self, spec: &ClassSpec) -> u32 {
        let by_mem = self.mem_mib / spec.mem_mib.max(1);
        let by_cpu = u64::from(self.vcpu / spec.vcpu.max(1));
        let by_cap = self.leases_left.map_or(u64::MAX, u64::from);
        u32::try_from(by_mem.min(by_cpu).min(by_cap)).unwrap_or(u32::MAX)
    }

    fn could_ever_fit(&self, spec: &ClassSpec) -> bool {
        self.drone.accepting
            && self.admission != AdmissionState::Closed
            && (!spec.docker || self.drone.docker)
            && self.drone.mem_mib >= spec.mem_mib
            && self.drone.vcpu >= spec.vcpu
            && self.drone.max_leases != Some(0)
    }

    fn take(&mut self, spec: &ClassSpec) {
        self.mem_mib = self.mem_mib.saturating_sub(spec.mem_mib);
        self.vcpu = self.vcpu.saturating_sub(spec.vcpu);
        self.leases_left = self.leases_left.map(|n| n.saturating_sub(1));
    }
}

/// Remaining room per drone after `leases`. Duplicate drone ids collapse to
/// the first entry; leases on unknown drones are ignored.
fn free_by_drone<'a>(
    drones: &'a [DroneState],
    leases: &[ActiveLease],
    classes: &Classes,
    admission: Option<AdmissionThresholds>,
) -> BTreeMap<&'a str, Free<'a>> {
    let mut free: BTreeMap<&str, Free<'_>> = BTreeMap::new();
    for drone in drones {
        free.entry(drone.id.as_str()).or_insert(Free {
            drone,
            mem_mib: drone.mem_mib,
            vcpu: drone.vcpu,
            leases_left: drone.max_leases,
            in_flight: 0,
            admission: AdmissionState::of(drone.admission_pressure_pm(), admission),
            granted: BTreeMap::new(),
        });
    }
    for lease in leases {
        if let Some(f) = free.get_mut(lease.drone_id.as_str()) {
            f.take(classes.spec(lease.class));
            f.in_flight += 1;
        }
    }
    free
}

/// Idle slots of `class` across `free`. Spare room counts only where any
/// queued job of the class could be placed: a drone shedding a scope under
/// CPU pressure, or blocking the class by its environment, has no idle slot
/// for this purpose.
fn class_idle(free: &BTreeMap<&str, Free<'_>>, class: Class, spec: &ClassSpec) -> u32 {
    free.values()
        .filter(|f| {
            f.fits(spec)
                && !f.drone.blocked_classes.contains(&class)
                && Scope::ALL.iter().all(|s| f.admission.admits(class, *s))
        })
        .map(|f| f.slots(spec))
        .fold(0u32, u32::saturating_add)
}

/// Idle slots per class once `leases` hold their room, by the same rule the
/// allocator uses for [`Allocation::idle`]. Pass the leases held after a
/// pass's grants to learn what that pass left free.
pub(crate) fn idle_after(
    drones: &[DroneState],
    leases: &[ActiveLease],
    classes: &Classes,
    admission: Option<AdmissionThresholds>,
) -> Vec<ClassIdle> {
    let free = free_by_drone(drones, leases, classes, admission);
    Class::ALL
        .into_iter()
        .map(|class| ClassIdle {
            class,
            idle_slots: class_idle(&free, class, classes.spec(class)),
        })
        .collect()
}

type Counts = BTreeMap<(Class, Scope), u32>;

fn sum_counts<I: IntoIterator<Item = (Class, Scope, u32)>>(items: I) -> Counts {
    let mut map = BTreeMap::new();
    for (class, scope, count) in items {
        let slot: &mut u32 = map.entry((class, scope)).or_default();
        *slot = slot.saturating_add(count);
    }
    map
}

/// Place up to `wanted` runners of `(class, scope)` on the tightest-fitting
/// drones, skipping drones reserved for something else. Returns how many fit.
fn place(
    input: &AllocatorInput,
    free: &mut BTreeMap<&str, Free<'_>>,
    pressure: &BTreeMap<Class, u16>,
    reserved: &BTreeMap<String, (Class, Scope)>,
    (scope, class): (Scope, Class),
    wanted: u32,
    out: &mut Allocation,
) -> u32 {
    let level = pressure.get(&class).copied().unwrap_or(0);
    let spec = input.classes.spec(class);
    let mut placed = 0;
    for _ in 0..wanted {
        // Highest bid score; then tightest fit, which keeps hosts packed; then
        // id, which keeps the output stable.
        let mut candidates = Vec::new();
        let mut winning_candidate = None;
        let mut best: Option<(i64, u64, &str)> = None;
        for (id, f) in free.iter() {
            let candidate = f.candidate(input, class, scope, level, reserved.get(*id).copied());
            if let Some(score) = candidate.score {
                let rank = (std::cmp::Reverse(score), f.mem_mib - spec.mem_mib, *id);
                if best.is_none_or(|(s, mem, name)| rank < (std::cmp::Reverse(s), mem, name)) {
                    best = Some((score, f.mem_mib - spec.mem_mib, *id));
                    winning_candidate = Some(candidate.clone());
                }
            }
            if candidates.len() < MAX_CANDIDATES {
                candidates.push(candidate);
            }
        }
        let winner = best.map(|(_, _, id)| id);
        if let Some(candidate) = winning_candidate
            && !candidates.iter().any(|c| c.drone_id == candidate.drone_id)
        {
            candidates.pop();
            candidates.push(candidate);
        }
        if out.decisions.len() < MAX_DECISIONS {
            out.decisions.push(PlacementDecision {
                class,
                scope,
                grant_order: out.grants.len(),
                requested: *spec,
                pressure: level,
                candidates,
                total_candidates: free.len(),
                reservation: None,
                winner: winner.map(str::to_owned),
            });
        } else {
            out.omitted_decisions = out.omitted_decisions.saturating_add(1);
        }
        let Some(f) = winner.and_then(|id| free.get_mut(id)) else {
            break;
        };
        f.take(spec);
        *f.granted.entry((class, scope)).or_default() += 1;
        placed += 1;
        out.grants.push(Grant {
            drone_id: f.drone.id.clone(),
            class,
            scope,
            labels: input.classes.runner_labels(class, scope),
            mem_mib: spec.mem_mib,
            vcpu: spec.vcpu,
        });
    }
    placed
}

/// Decide which runners to mint and where. Pure and deterministic.
pub fn allocate(input: &AllocatorInput) -> Allocation {
    allocate_reserved(input, &[])
}

/// Allocate only one scope while retaining the full queue's bid pressure.
/// Coverage by pending/shared runners and standby deploy runners is calculated
/// before restricting placement, just as in a complete allocation pass.
pub fn allocate_scope(input: &AllocatorInput, scope: Scope) -> Allocation {
    allocate_inner(input, &[], None, Some(scope))
}

/// Apply reservations inherited from an earlier allocation pass. These are
/// enforced at the same eligibility boundary as new starvation reservations,
/// so their diagnostic reason is preserved instead of masquerading as drain.
pub fn allocate_reserved(input: &AllocatorInput, inherited: &[Reservation]) -> Allocation {
    allocate_inner(input, inherited, None, None)
}

/// Select only `input.demand`, but evaluate bid thresholds against the actual
/// uncovered queue. Per-client fairness selects one unit at a time; that must
/// not make a saturated queue look like one isolated request. Callers pass only
/// demand not already covered by a pending runner or leased work unit.
pub fn allocate_reserved_with_backlog(
    input: &AllocatorInput,
    inherited: &[Reservation],
    uncovered_backlog: &[Demand],
) -> Allocation {
    allocate_inner(input, inherited, Some(uncovered_backlog), None)
}

fn allocate_inner(
    input: &AllocatorInput,
    inherited: &[Reservation],
    backlog: Option<&[Demand]>,
    scope: Option<Scope>,
) -> Allocation {
    let mut free = free_by_drone(
        &input.drones,
        &input.leases,
        &input.classes,
        input.admission,
    );

    let queued = sum_counts(input.demand.iter().map(|d| (d.class, d.scope, d.count)));
    let floors = sum_counts(
        input
            .warm_floors
            .iter()
            .map(|w| (w.class, w.scope, w.count)),
    );
    let pending = sum_counts(
        input
            .leases
            .iter()
            .filter(|l| l.state == LeaseState::Pending && free.contains_key(l.drone_id.as_str()))
            .map(|l| (l.class, l.scope, 1)),
    );
    let standby = sum_counts(
        input
            .leases
            .iter()
            .filter(|l| l.state == LeaseState::Standby && free.contains_key(l.drone_id.as_str()))
            .map(|l| (l.class, l.scope, 1)),
    );
    let get = |m: &Counts, c, s| m.get(&(c, s)).copied().unwrap_or(0);
    let mut promote: Vec<Promotion> = Vec::new();

    // How many runners each (scope, class) still has to mint.
    let mut need: BTreeMap<(Scope, Class), u32> = BTreeMap::new();
    for class in Class::ALL {
        // Deploy demand is absorbed only by pending deploy runners, which in
        // turn absorb nothing else.
        let deploy_total =
            get(&queued, class, Scope::Deploy).max(get(&floors, class, Scope::Deploy));
        let pending_deploy = get(&pending, class, Scope::Deploy);
        // Queued deploy jobs that pending runners do not cover start standby
        // runners before the queen mints anything.
        let standby_deploy = get(&standby, class, Scope::Deploy);
        let promoted = get(&queued, class, Scope::Deploy)
            .saturating_sub(pending_deploy)
            .min(standby_deploy);
        if promoted > 0 {
            promote.push(Promotion {
                class,
                count: promoted,
            });
        }
        let deploy_need = deploy_total.saturating_sub(pending_deploy + standby_deploy);

        let pending_main = get(&pending, class, Scope::Main);
        let pending_pr = get(&pending, class, Scope::Pr);

        // Main demand uses main-only pending runners first, then spare
        // pull-request runners (they carry the bare label too).
        let main_total = get(&queued, class, Scope::Main).max(get(&floors, class, Scope::Main));
        let after_main_pending = main_total.saturating_sub(pending_main);
        let from_pr_pending = after_main_pending.min(pending_pr);
        let main_need = after_main_pending - from_pr_pending;
        let pending_pr_left = pending_pr - from_pr_pending;

        let pr_total = get(&queued, class, Scope::Pr).max(get(&floors, class, Scope::Pr));
        let pr_need = pr_total.saturating_sub(pending_pr_left);

        need.insert((Scope::Deploy, class), deploy_need);
        need.insert((Scope::Main, class), main_need);
        need.insert((Scope::Pr, class), pr_need);
    }

    let mut initial_need = need.clone();
    let pressure_backlog =
        backlog.map(|demand| sum_counts(demand.iter().map(|d| (d.class, d.scope, d.count))));
    let waited = |class, scope, after: Option<u64>| {
        after.is_some_and(|after| {
            input
                .queue_ages
                .iter()
                .any(|a| a.class == class && a.scope == scope && a.oldest_age_secs >= after)
        })
    };
    let aged = |class, scope| waited(class, scope, input.starvation_after_secs);
    let mut pressure: BTreeMap<Class, u16> = BTreeMap::new();
    let mut idle_by_class: Vec<ClassIdle> = Vec::new();
    for class in Class::ALL {
        let spec = input.classes.spec(class);
        let mut shortfall = 0u32;
        let mut starving = false;
        for scope in Scope::ALL {
            let queued_here = get(pressure_backlog.as_ref().unwrap_or(&queued), class, scope);
            let uncovered = if pressure_backlog.is_some() {
                queued_here
            } else {
                queued_here.min(initial_need.get(&(scope, class)).copied().unwrap_or(0))
            };
            shortfall = shortfall.saturating_add(uncovered);
            starving |= queued_here > 0 && aged(class, scope);
        }
        let idle = class_idle(&free, class, spec);
        pressure.insert(class, demand_pressure(shortfall, idle, starving));
        idle_by_class.push(ClassIdle {
            class,
            idle_slots: idle,
        });
    }
    if let Some(scope) = scope {
        need.retain(|&(s, _), _| s == scope);
        initial_need.retain(|&(s, _), _| s == scope);
        if scope != Scope::Deploy {
            promote.clear();
        }
    }
    let mut out = Allocation {
        pressure: pressure
            .iter()
            .map(|(&class, &pressure)| ClassPressure { class, pressure })
            .collect(),
        idle: idle_by_class,
        admission: free
            .iter()
            .map(|(id, f)| DroneAdmission {
                drone_id: (*id).to_owned(),
                state: f.admission,
                cpu_psi_pm: f.drone.cpu_psi_pm,
                cpu_load_pm: f.drone.cpu_load_pm,
            })
            .collect(),
        promote,
        ..Allocation::default()
    };
    let mut placed: BTreeMap<(Scope, Class), u32> = BTreeMap::new();
    let mut reserved: BTreeMap<String, (Class, Scope)> = inherited
        .iter()
        .map(|r| (r.drone_id.clone(), (r.class, r.scope)))
        .collect();

    // Pull-request turn: one grant per listed class to pull-request demand
    // that has waited long enough, ahead of main's demand for that class.
    // Deploy demand is unaffected: it runs only on deploy-trust drones, which
    // never take pull-request work.
    for class in Class::ALL.into_iter().rev() {
        let key = (Scope::Pr, class);
        let wanted = need.get(&key).copied().unwrap_or(0);
        if wanted == 0
            || !input.pr_turn.classes.contains(&class)
            || get(&queued, class, Scope::Pr) == 0
            || !waited(class, Scope::Pr, input.pr_turn.after_secs)
        {
            continue;
        }
        let n = place(input, &mut free, &pressure, &reserved, key, 1, &mut out);
        *placed.entry(key).or_default() += n;
        need.insert(key, wanted - n);
    }

    // Starvation guard. Demand that has waited too long is placed first; if it
    // still cannot fit, hold back the drone that will free up soonest so the
    // stream of smaller jobs cannot keep it full.
    if input.starvation_after_secs.is_some() {
        for scope in Scope::ALL {
            for class in Class::ALL.into_iter().rev() {
                let wanted = need.get(&(scope, class)).copied().unwrap_or(0);
                if wanted == 0 || get(&queued, class, scope) == 0 || !aged(class, scope) {
                    continue;
                }
                let n = place(
                    input,
                    &mut free,
                    &pressure,
                    &reserved,
                    (scope, class),
                    wanted,
                    &mut out,
                );
                *placed.entry((scope, class)).or_default() += n;
                need.insert((scope, class), 0);
                if n < wanted {
                    let spec = input.classes.spec(class);
                    let target = free
                        .iter()
                        .filter(|(id, f)| {
                            !reserved.contains_key(**id)
                                && f.drone.trust.admits(scope)
                                && f.could_ever_fit(spec)
                                && f.admission.admits(class, scope)
                                && !f.drone.blocked_classes.contains(&class)
                                && f.offer(
                                    class,
                                    scope,
                                    pressure.get(&class).copied().unwrap_or(0),
                                    &input.weights,
                                )
                                .is_some()
                        })
                        .min_by_key(|(id, f)| (f.in_flight, **id))
                        .map(|(id, _)| (*id).to_owned());
                    if let Some(id) = target {
                        if let Some(decision) = out.decisions.last_mut()
                            && decision.class == class
                            && decision.scope == scope
                            && decision.winner.is_none()
                        {
                            decision.reservation = Some(id.clone());
                        }
                        reserved.insert(id.clone(), (class, scope));
                        out.reserved_for.push(Reservation {
                            drone_id: id,
                            class,
                            scope,
                        });
                    }
                }
            }
        }
    }

    for scope in Scope::ALL {
        for class in Class::ALL.into_iter().rev() {
            let wanted = need.get(&(scope, class)).copied().unwrap_or(0);
            let n = place(
                input,
                &mut free,
                &pressure,
                &reserved,
                (scope, class),
                wanted,
                &mut out,
            );
            *placed.entry((scope, class)).or_default() += n;
        }
    }

    // Only real queued demand is reported as unmet, not unfilled floors.
    for scope in Scope::ALL {
        for class in Class::ALL.into_iter().rev() {
            let got = placed.get(&(scope, class)).copied().unwrap_or(0);
            let wanted = initial_need.get(&(scope, class)).copied().unwrap_or(0);
            let unmet = get(&queued, class, scope).min(wanted).saturating_sub(got);
            if unmet > 0 {
                out.unmet.push(Unmet {
                    class,
                    scope,
                    count: unmet,
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 28 GiB host with 16 vCPU and Docker, already net of the OS reserve.
    fn host(id: &str) -> DroneState {
        DroneState {
            id: id.into(),
            vcpu: 16,
            mem_mib: 28 * 1024,
            docker: true,
            accepting: true,
            max_leases: None,
            bids: vec![],
            profiles: vec![],
            blocked_classes: vec![],
            cpu_psi_pm: None,
            cpu_load_pm: None,
            trust: Trust::Shared,
        }
    }

    fn input(demand: &[(Class, Scope, u32)], drones: Vec<DroneState>) -> AllocatorInput {
        AllocatorInput {
            classes: Classes::default(),
            demand: demand
                .iter()
                .map(|&(class, scope, count)| Demand {
                    class,
                    scope,
                    count,
                })
                .collect(),
            drones,
            leases: vec![],
            warm_floors: vec![],
            queue_ages: vec![],
            starvation_after_secs: None,
            weights: ScoreWeights::default(),
            admission: None,
            pr_turn: PrTurn::default(),
        }
    }

    fn count(a: &Allocation, class: Class, scope: Scope) -> usize {
        a.grants
            .iter()
            .filter(|g| g.class == class && g.scope == scope)
            .count()
    }

    fn lease(drone: &str, class: Class, scope: Scope, state: LeaseState) -> ActiveLease {
        ActiveLease {
            drone_id: drone.into(),
            class,
            scope,
            state,
        }
    }

    fn assert_within_budget(i: &AllocatorInput, a: &Allocation) {
        for d in &i.drones {
            let spec = |c| i.classes.spec(c);
            let mut mem = 0;
            let mut vcpu = 0;
            let mut n = 0u32;
            for l in i.leases.iter().filter(|l| l.drone_id == d.id) {
                mem += spec(l.class).mem_mib;
                vcpu += spec(l.class).vcpu;
                n += 1;
            }
            for g in a.grants.iter().filter(|g| g.drone_id == d.id) {
                mem += g.mem_mib;
                vcpu += g.vcpu;
                n += 1;
                assert!(d.accepting, "grant to non-accepting drone");
                assert!(
                    d.docker || !spec(g.class).docker,
                    "docker-less drone got docker class"
                );
            }
            if a.grants.iter().any(|g| g.drone_id == d.id) {
                assert!(
                    mem <= d.mem_mib,
                    "{} over-granted memory: {mem} > {}",
                    d.id,
                    d.mem_mib
                );
                assert!(
                    vcpu <= d.vcpu,
                    "{} over-granted vcpu: {vcpu} > {}",
                    d.id,
                    d.vcpu
                );
                if let Some(cap) = d.max_leases {
                    assert!(n <= cap, "{} over lease cap", d.id);
                }
            }
        }
    }

    fn profile(class: Class, speed: u32) -> ProfileScore {
        ProfileScore {
            class,
            speed_permille: speed,
            speed_confidence_permille: 1000,
            reliability_permille: 1000,
        }
    }

    #[test]
    fn main_heavy_prefers_fast_idle_drones_and_slow_or_loaded_ones_take_overflow() {
        let fast = DroneState {
            profiles: vec![profile(Class::Heavy, 1100)],
            ..host("fast")
        };
        // Ids sort before "fast", so ties would go to them.
        // As fast, but its host is under CPU pressure right now.
        let loaded = DroneState {
            cpu_psi_pm: Some(450),
            profiles: vec![profile(Class::Heavy, 1100)],
            ..host("a-loaded")
        };
        // Idle, but historically half as fast.
        let slow = DroneState {
            profiles: vec![profile(Class::Heavy, 550)],
            ..host("0-slow")
        };
        let drones = vec![loaded.clone(), slow.clone(), fast.clone()];
        let first = |n| {
            let a = allocate(&input(&[(Class::Heavy, Scope::Main, n)], drones.clone()));
            a.grants
                .iter()
                .map(|g| g.drone_id.clone())
                .collect::<Vec<_>>()
        };
        // One default heavy fits per 28 GiB host. Overflow goes to the less
        // loaded of the rest; the historically slow host comes last.
        assert_eq!(first(1), ["fast"]);
        assert_eq!(first(2), ["fast", "a-loaded"]);
        assert_eq!(first(3), ["fast", "a-loaded", "0-slow"]);

        // Same speed: the idle host wins medium/heavy, light still packs.
        let busy = host("busy");
        let idle = host("idle");
        let mut i = input(
            &[(Class::Medium, Scope::Main, 1)],
            vec![busy.clone(), idle.clone()],
        );
        i.leases = vec![lease(
            "busy",
            Class::Medium,
            Scope::Main,
            LeaseState::Running,
        )];
        assert_eq!(allocate(&i).grants[0].drone_id, "idle");
        i.demand = vec![Demand {
            class: Class::Light,
            scope: Scope::Main,
            count: 1,
        }];
        assert_eq!(allocate(&i).grants[0].drone_id, "busy");

        let w = ScoreWeights::default();
        assert_eq!(host_term(&loaded, 16, Class::Heavy, &w), -450 + 250);
        assert_eq!(host_term(&slow, 8, Class::Heavy, &w), 125);
        assert_eq!(host_term(&loaded, 16, Class::Light, &w), -450);
    }

    #[test]
    fn load_estimate_approximates_waiting_share_and_ignores_idle_cpus() {
        assert_eq!(load_pressure_pm(None, Some(1000)), None);
        assert_eq!(load_pressure_pm(Some(800), None), Some(0));
        assert_eq!(load_pressure_pm(Some(1000), Some(1000)), Some(0));
        // Load 93 on 16 CPUs, CPUs saturated: ~83% of runnable tasks wait.
        assert_eq!(load_pressure_pm(Some(5838), Some(990)), Some(828));
        assert_eq!(load_pressure_pm(Some(2000), None), Some(500));
        // Same run queue with idle CPU time is not CPU contention.
        assert_eq!(load_pressure_pm(Some(5838), Some(550)), Some(0));
    }

    #[test]
    fn light_only_sheds_pr_and_main_medium_heavy_but_keeps_light_main_and_deploy() {
        let t = AdmissionThresholds {
            psi_soft_pm: 300,
            psi_light_only_pm: 500,
            psi_hard_pm: 600,
        };
        let of = |p| AdmissionState::of(Some(p), Some(t));
        assert_eq!(of(299), AdmissionState::Open);
        assert_eq!(of(300), AdmissionState::PrBlocked);
        assert_eq!(of(540), AdmissionState::LightOnly);
        assert_eq!(of(600), AdmissionState::Closed);
        let s = AdmissionState::LightOnly;
        assert!(s.admits(Class::Light, Scope::Main));
        assert!(s.admits(Class::Heavy, Scope::Deploy));
        assert!(!s.admits(Class::Medium, Scope::Main));
        assert!(!s.admits(Class::Heavy, Scope::Main));
        assert!(!s.admits(Class::Light, Scope::Pr));
        // Unset light-only tier (== hard): PR-blocked straight to closed.
        let old = AdmissionThresholds {
            psi_light_only_pm: 600,
            ..t
        };
        assert_eq!(
            AdmissionState::of(Some(540), Some(old)),
            AdmissionState::PrBlocked
        );

        // A loaded drone without PSI: its load estimate drives admission.
        let loaded = DroneState {
            cpu_load_pm: Some(828),
            ..host("loaded")
        };
        let mut i = input(
            &[
                (Class::Heavy, Scope::Main, 1),
                (Class::Medium, Scope::Main, 1),
                (Class::Light, Scope::Main, 1),
            ],
            vec![loaded.clone()],
        );
        i.admission = Some(t);
        let a = allocate(&i);
        assert_eq!(a.admission[0].state, AdmissionState::Closed);
        assert!(a.grants.is_empty());

        let busy = DroneState {
            cpu_psi_pm: Some(540),
            ..loaded
        };
        i.drones = vec![busy];
        let a = allocate(&i);
        assert_eq!(
            a.admission[0].state,
            AdmissionState::LightOnly,
            "PSI wins over the load estimate"
        );
        assert_eq!(count(&a, Class::Light, Scope::Main), 1);
        assert_eq!(count(&a, Class::Medium, Scope::Main), 0);
        assert_eq!(count(&a, Class::Heavy, Scope::Main), 0);
        let rejected: Vec<_> = a
            .decisions
            .iter()
            .filter(|d| d.class == Class::Heavy)
            .flat_map(|d| {
                d.candidates
                    .iter()
                    .flat_map(|c| c.rejection_reasons.clone())
            })
            .collect();
        assert!(
            rejected.contains(&Rejection::LightOnlyPressure),
            "{rejected:?}"
        );
    }

    #[test]
    fn free_room_and_slots_subtract_what_the_drone_already_holds() {
        let classes = Classes::default();
        let d = host("a"); // 28 GiB, 16 vCPU, docker
        let held = vec![
            lease("a", Class::Heavy, Scope::Pr, LeaseState::Running),
            lease("b", Class::Heavy, Scope::Pr, LeaseState::Running),
        ];
        let room = free_room(&d, &held, &classes);
        let heavy = classes.spec(Class::Heavy);
        assert_eq!(room.mem_mib, d.mem_mib - heavy.mem_mib);
        assert_eq!(room.vcpu, d.vcpu - heavy.vcpu);
        // Theoretical: one heavy fits an empty host; with one held, none do.
        let empty = free_room(&d, &[], &classes);
        assert_eq!(free_slots(&d, &empty, heavy), max_slots(&d, heavy));
        assert_eq!(free_slots(&d, &room, heavy), 0);
        assert!(free_slots(&d, &room, classes.spec(Class::Light)) > 0);
        // A draining drone and a docker-less one have no free slots.
        let draining = DroneState {
            accepting: false,
            ..host("c")
        };
        assert_eq!(free_slots(&draining, &empty, heavy), 0);
        let nodocker = DroneState {
            docker: false,
            ..host("e")
        };
        assert_eq!(
            free_slots(&nodocker, &empty, classes.spec(Class::Medium)),
            0
        );
        assert!(free_slots(&nodocker, &empty, classes.spec(Class::Light)) > 0);
        // The lease cap counts too.
        let capped = DroneState {
            max_leases: Some(1),
            ..host("f")
        };
        let one = vec![lease("f", Class::Light, Scope::Pr, LeaseState::Running)];
        assert_eq!(
            free_slots(
                &capped,
                &free_room(&capped, &one, &classes),
                classes.spec(Class::Light)
            ),
            0
        );
    }

    #[test]
    fn no_demand_no_floor_no_grants() {
        let a = allocate(&input(&[], vec![host("a")]));
        assert!(a.grants.is_empty() && a.unmet.is_empty() && a.reserved_for.is_empty());
        assert!(a.pressure.iter().all(|p| p.pressure == 0));
    }

    #[test]
    fn warm_floor_alone_grants_exactly_the_floor() {
        let mut i = input(&[], vec![host("a")]);
        i.warm_floors = default_warm_floors();
        let a = allocate(&i);
        assert_eq!(a.grants.len(), 1);
        assert_eq!(
            (a.grants[0].class, a.grants[0].scope),
            (Class::Medium, Scope::Main)
        );
        assert_eq!(a.grants[0].labels, ["borg-ci-medium"]);
        assert!(a.unmet.is_empty(), "an unfilled floor is not unmet demand");
    }

    #[test]
    fn warm_floor_is_satisfied_by_an_existing_pending_lease() {
        let mut i = input(&[], vec![host("a")]);
        i.warm_floors = default_warm_floors();
        i.leases = vec![lease("a", Class::Medium, Scope::Main, LeaseState::Pending)];
        assert!(allocate(&i).grants.is_empty());
    }

    #[test]
    fn running_lease_does_not_satisfy_the_floor() {
        let mut i = input(&[], vec![host("a")]);
        i.warm_floors = default_warm_floors();
        i.leases = vec![lease("a", Class::Medium, Scope::Main, LeaseState::Running)];
        assert_eq!(allocate(&i).grants.len(), 1);
    }

    #[test]
    fn floor_and_queued_demand_do_not_double_count() {
        let mut i = input(&[(Class::Medium, Scope::Main, 1)], vec![host("a")]);
        i.warm_floors = default_warm_floors();
        assert_eq!(allocate(&i).grants.len(), 1);
    }

    #[test]
    fn main_demand_is_placed_before_pr_demand_when_room_is_short() {
        // Room for exactly one heavy; main must win over pr.
        let i = input(
            &[(Class::Heavy, Scope::Pr, 1), (Class::Heavy, Scope::Main, 1)],
            vec![host("a")],
        );
        let a = allocate(&i);
        assert_eq!(a.grants.len(), 1);
        assert_eq!(a.grants[0].scope, Scope::Main);
        assert_eq!(
            a.unmet,
            [Unmet {
                class: Class::Heavy,
                scope: Scope::Pr,
                count: 1
            }]
        );
    }

    #[test]
    fn small_main_beats_large_pr() {
        // 24 GiB budget: main medium x2 fills it, leaving no room for pr heavy.
        let mut h = host("a");
        h.mem_mib = 24 * 1024;
        let i = input(
            &[
                (Class::Heavy, Scope::Pr, 1),
                (Class::Medium, Scope::Main, 2),
            ],
            vec![h],
        );
        let a = allocate(&i);
        assert_eq!(count(&a, Class::Medium, Scope::Main), 2);
        assert_eq!(count(&a, Class::Heavy, Scope::Pr), 0);
    }

    #[test]
    fn main_grants_carry_only_the_bare_label() {
        let a = allocate(&input(&[(Class::Light, Scope::Main, 1)], vec![host("a")]));
        assert_eq!(a.grants[0].labels, ["borg-ci-light"]);
    }

    #[test]
    fn pr_grants_carry_both_labels() {
        let a = allocate(&input(&[(Class::Light, Scope::Pr, 1)], vec![host("a")]));
        assert_eq!(a.grants[0].labels, ["borg-ci-light", "borg-ci-light-pr"]);
    }

    #[test]
    fn labels_follow_the_configured_prefix() {
        let mut i = input(&[(Class::Light, Scope::Pr, 1)], vec![host("a")]);
        i.classes.label_prefix = "example-ci-".into();
        assert_eq!(
            allocate(&i).grants[0].labels,
            ["example-ci-light", "example-ci-light-pr"]
        );
    }

    #[test]
    fn carves_one_28gib_host_into_one_heavy() {
        let i = input(&[(Class::Heavy, Scope::Main, 3)], vec![host("a")]);
        let a = allocate(&i);
        assert_eq!(count(&a, Class::Heavy, Scope::Main), 1);
        assert_eq!(
            a.unmet,
            [Unmet {
                class: Class::Heavy,
                scope: Scope::Main,
                count: 2
            }]
        );
        assert_within_budget(&i, &a);
    }

    #[test]
    fn carves_one_28gib_host_into_two_medium() {
        let i = input(&[(Class::Medium, Scope::Pr, 5)], vec![host("a")]);
        let a = allocate(&i);
        assert_eq!(count(&a, Class::Medium, Scope::Pr), 2);
        assert_within_budget(&i, &a);
    }

    #[test]
    fn carves_one_28gib_host_into_eight_light() {
        // Memory alone would allow 11; the 16 vCPU budget caps it at 8.
        let i = input(&[(Class::Light, Scope::Pr, 20)], vec![host("a")]);
        let a = allocate(&i);
        assert_eq!(count(&a, Class::Light, Scope::Pr), 8);
        assert_within_budget(&i, &a);
    }

    #[test]
    fn mixed_demand_fills_remaining_room_after_the_big_class() {
        let i = input(
            &[
                (Class::Light, Scope::Main, 4),
                (Class::Heavy, Scope::Main, 1),
            ],
            vec![host("a")],
        );
        let a = allocate(&i);
        assert_eq!(count(&a, Class::Heavy, Scope::Main), 1);
        // 28 - 24 = 4 GiB left: one light (2.5 GiB) fits.
        assert_eq!(count(&a, Class::Light, Scope::Main), 1);
        assert_within_budget(&i, &a);
    }

    #[test]
    fn in_flight_leases_reduce_available_room() {
        let mut i = input(&[(Class::Medium, Scope::Main, 2)], vec![host("a")]);
        i.leases = vec![lease("a", Class::Medium, Scope::Pr, LeaseState::Running)];
        let a = allocate(&i);
        assert_eq!(a.grants.len(), 1);
        assert_within_budget(&i, &a);
    }

    #[test]
    fn pending_leases_absorb_queued_demand_of_the_same_label() {
        let mut i = input(&[(Class::Light, Scope::Main, 2)], vec![host("a")]);
        i.leases = vec![lease("a", Class::Light, Scope::Main, LeaseState::Pending)];
        assert_eq!(allocate(&i).grants.len(), 1);
    }

    #[test]
    fn pending_main_lease_cannot_serve_pr_demand() {
        let mut i = input(&[(Class::Light, Scope::Pr, 1)], vec![host("a")]);
        i.leases = vec![lease("a", Class::Light, Scope::Main, LeaseState::Pending)];
        assert_eq!(allocate(&i).grants.len(), 1);
    }

    #[test]
    fn pending_pr_lease_can_serve_main_demand() {
        let mut i = input(&[(Class::Light, Scope::Main, 1)], vec![host("a")]);
        i.leases = vec![lease("a", Class::Light, Scope::Pr, LeaseState::Pending)];
        assert!(allocate(&i).grants.is_empty());
    }

    #[test]
    fn a_pending_pr_lease_is_not_counted_twice() {
        let mut i = input(
            &[(Class::Light, Scope::Main, 1), (Class::Light, Scope::Pr, 1)],
            vec![host("a")],
        );
        i.leases = vec![lease("a", Class::Light, Scope::Pr, LeaseState::Pending)];
        let a = allocate(&i);
        assert_eq!(a.grants.len(), 1);
        assert_eq!(a.grants[0].scope, Scope::Pr);
    }

    #[test]
    fn host_lease_cap_limits_grants() {
        let mut h = host("a");
        h.max_leases = Some(3);
        let i = input(&[(Class::Light, Scope::Main, 8)], vec![h]);
        let a = allocate(&i);
        assert_eq!(a.grants.len(), 3);
        assert_within_budget(&i, &a);
    }

    #[test]
    fn host_lease_cap_counts_in_flight_leases() {
        let mut h = host("a");
        h.max_leases = Some(2);
        let mut i = input(&[(Class::Light, Scope::Main, 8)], vec![h]);
        i.leases = vec![lease("a", Class::Light, Scope::Pr, LeaseState::Running)];
        assert_eq!(allocate(&i).grants.len(), 1);
    }

    #[test]
    fn owner_caps_shrink_the_budget() {
        let capacity = Capacity {
            hostname: "h".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            kind: crate::probe::HostKind::Wsl,
            vcpu: 20,
            mem_mib: 64 * 1024,
            free_disk_mib: None,
            docker_usable: true,
            mise_version: None,
            git_version: None,
        };
        let caps = HostCaps {
            max_vcpu: Some(8),
            max_mem_mib: Some(24 * 1024),
            ..HostCaps::default()
        };
        let d = DroneState::from_capacity("pc", &capacity, &caps);
        assert_eq!((d.vcpu, d.mem_mib), (8, 24 * 1024));

        let i = input(&[(Class::Light, Scope::Main, 20)], vec![d]);
        let a = allocate(&i);
        assert_eq!(a.grants.len(), 4); // 8 vCPU / 2
        assert_within_budget(&i, &a);
    }

    #[test]
    fn reserve_is_subtracted_from_total_memory() {
        let capacity = Capacity {
            hostname: "h".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            kind: crate::probe::HostKind::Bare,
            vcpu: 16,
            mem_mib: 28 * 1024,
            free_disk_mib: None,
            docker_usable: true,
            mise_version: None,
            git_version: None,
        };
        let d = DroneState::from_capacity("a", &capacity, &HostCaps::default());
        assert_eq!(d.mem_mib, 26 * 1024);
        // Still fits one heavy.
        let a = allocate(&input(&[(Class::Heavy, Scope::Main, 1)], vec![d]));
        assert_eq!(a.grants.len(), 1);
    }

    #[test]
    fn docker_classes_skip_hosts_without_docker() {
        let mut nodocker = host("a");
        nodocker.docker = false;
        let i = input(
            &[
                (Class::Medium, Scope::Main, 1),
                (Class::Light, Scope::Main, 1),
            ],
            vec![nodocker, host("b")],
        );
        let a = allocate(&i);
        let medium = a.grants.iter().find(|g| g.class == Class::Medium).unwrap();
        assert_eq!(medium.drone_id, "b");
        assert_within_budget(&i, &a);
    }

    #[test]
    fn draining_drones_get_nothing() {
        let mut h = host("a");
        h.accepting = false;
        let a = allocate(&input(&[(Class::Light, Scope::Main, 2)], vec![h]));
        assert!(a.grants.is_empty());
        assert_eq!(
            a.unmet,
            [Unmet {
                class: Class::Light,
                scope: Scope::Main,
                count: 2
            }]
        );
    }

    #[test]
    fn spreads_over_hosts_when_one_is_full_and_prefers_the_tightest_fit() {
        let mut small = host("small");
        small.mem_mib = 13 * 1024;
        let i = input(&[(Class::Medium, Scope::Main, 3)], vec![host("big"), small]);
        let a = allocate(&i);
        assert_eq!(a.grants.len(), 3);
        // The first medium goes to the tighter 13 GiB host.
        assert_eq!(a.grants[0].drone_id, "small");
        assert_within_budget(&i, &a);
    }

    #[test]
    fn leases_on_unknown_drones_are_ignored() {
        let mut i = input(&[(Class::Light, Scope::Main, 1)], vec![host("a")]);
        i.leases = vec![lease(
            "gone",
            Class::Heavy,
            Scope::Main,
            LeaseState::Pending,
        )];
        assert_eq!(allocate(&i).grants.len(), 1);
    }

    #[test]
    fn duplicate_demand_entries_are_summed() {
        let i = input(
            &[
                (Class::Light, Scope::Main, 1),
                (Class::Light, Scope::Main, 2),
            ],
            vec![host("a")],
        );
        assert_eq!(allocate(&i).grants.len(), 3);
    }

    #[test]
    fn allocation_is_deterministic() {
        let i = input(
            &[
                (Class::Light, Scope::Pr, 5),
                (Class::Medium, Scope::Main, 1),
            ],
            vec![host("b"), host("a")],
        );
        assert_eq!(allocate(&i), allocate(&i));
    }

    fn aged(i: &mut AllocatorInput, class: Class, scope: Scope, secs: u64) {
        i.queue_ages.push(QueueAge {
            class,
            scope,
            oldest_age_secs: secs,
        });
        i.starvation_after_secs = Some(120);
    }

    /// Two 28 GiB drones, each too full for a heavy lease; `a` carries more
    /// leases than `b`.
    fn crowded() -> AllocatorInput {
        let mut i = input(
            &[
                (Class::Heavy, Scope::Pr, 1),
                (Class::Light, Scope::Main, 10),
            ],
            vec![host("a"), host("b")],
        );
        i.leases = vec![
            lease("a", Class::Medium, Scope::Pr, LeaseState::Running),
            lease("a", Class::Light, Scope::Pr, LeaseState::Running),
            lease("b", Class::Medium, Scope::Pr, LeaseState::Running),
        ];
        i
    }

    #[test]
    fn starving_heavy_reserves_the_drone_with_fewest_leases() {
        let mut i = crowded();
        aged(&mut i, Class::Heavy, Scope::Pr, 300);
        let a = allocate(&i);
        assert_eq!(
            a.reserved_for,
            [Reservation {
                drone_id: "b".into(),
                class: Class::Heavy,
                scope: Scope::Pr
            }]
        );
        assert_eq!(a.grants.iter().filter(|g| g.drone_id == "b").count(), 1);
        let mut next = i.clone();
        next.leases.retain(|l| l.drone_id != "b");
        next.leases.extend(
            a.grants
                .iter()
                .map(|g| lease(&g.drone_id, g.class, g.scope, LeaseState::Running)),
        );
        next.demand.retain(|d| d.class == Class::Heavy);
        assert_eq!(count(&allocate(&next), Class::Heavy, Scope::Pr), 1);
        assert!(
            count(&a, Class::Light, Scope::Main) > 0,
            "other drone keeps working"
        );
        assert!(a.unmet.contains(&Unmet {
            class: Class::Heavy,
            scope: Scope::Pr,
            count: 1
        }));
        assert_within_budget(&i, &a);
    }

    /// A running heavy lease leaves 12 GiB idle on a 26 GiB host. Six
    /// 2 GiB backfills can finish independently: the next 14 GiB heavy
    /// still fits as soon as the original lease ends.
    fn backfill_input() -> AllocatorInput {
        let mut i = input(
            &[
                (Class::Heavy, Scope::Main, 1),
                (Class::Light, Scope::Pr, 10),
            ],
            vec![DroneState {
                mem_mib: 26 * 1024,
                vcpu: 11,
                ..host("a")
            }],
        );
        i.classes.light = ClassSpec {
            mem_mib: 2048,
            vcpu: 1,
            docker: false,
        };
        i.classes.medium = ClassSpec {
            mem_mib: 8192,
            vcpu: 2,
            docker: true,
        };
        i.classes.heavy = ClassSpec {
            mem_mib: 14336,
            vcpu: 4,
            docker: true,
        };
        i.leases = vec![lease("a", Class::Heavy, Scope::Main, LeaseState::Running)];
        aged(&mut i, Class::Heavy, Scope::Main, 300);
        i
    }

    #[test]
    fn reservation_backfills_without_delaying_the_reserved_job() {
        let mut i = backfill_input();
        let a = allocate(&i);
        assert_eq!(a.reserved_for.len(), 1);
        assert_eq!(count(&a, Class::Light, Scope::Pr), 6);
        assert_within_budget(&i, &a);
        // The original heavy finishes while every backfill keeps running.
        i.leases = a
            .grants
            .iter()
            .map(|g| lease(&g.drone_id, g.class, g.scope, LeaseState::Running))
            .collect();
        i.demand.retain(|d| d.class == Class::Heavy);
        let next = allocate(&i);
        assert_eq!(count(&next, Class::Heavy, Scope::Main), 1);
        assert_within_budget(&i, &next);
    }

    #[test]
    fn reservation_backfill_preserves_cpu_and_lease_limits() {
        for (vcpu, cap, expected) in [
            (11, None, 6),
            (5, None, 1),
            (11, Some(2), 1),
            (11, Some(1), 0),
        ] {
            let mut i = backfill_input();
            i.drones[0].vcpu = vcpu;
            i.drones[0].max_leases = cap;
            let a = allocate(&i);
            assert_eq!(
                count(&a, Class::Light, Scope::Pr),
                expected,
                "vcpu={vcpu} cap={cap:?}"
            );
            assert_within_budget(&i, &a);
        }
    }

    #[test]
    fn reservation_with_multiple_existing_leases_does_not_backfill() {
        let mut i = backfill_input();
        i.leases
            .push(lease("a", Class::Light, Scope::Main, LeaseState::Running));
        let a = allocate(&i);
        assert_eq!(a.reserved_for.len(), 1);
        assert!(a.grants.is_empty());
    }

    #[test]
    fn reservation_ties_break_by_drone_id() {
        let mut i = crowded();
        i.leases = vec![
            lease("b", Class::Medium, Scope::Pr, LeaseState::Running),
            lease("a", Class::Medium, Scope::Pr, LeaseState::Running),
        ];
        aged(&mut i, Class::Heavy, Scope::Pr, 300);
        assert_eq!(allocate(&i).reserved_for[0].drone_id, "a");
    }

    #[test]
    fn young_demand_is_not_starving() {
        let mut i = crowded();
        aged(&mut i, Class::Heavy, Scope::Pr, 119);
        let a = allocate(&i);
        assert!(a.reserved_for.is_empty());
        assert!(a.grants.iter().any(|g| g.drone_id == "b"));
    }

    #[test]
    fn guard_can_be_disabled() {
        let mut i = crowded();
        aged(&mut i, Class::Heavy, Scope::Pr, 9999);
        i.starvation_after_secs = None;
        assert!(allocate(&i).reserved_for.is_empty());
    }

    #[test]
    fn starving_demand_that_fits_is_granted_first_without_reservation() {
        let mut i = input(
            &[
                (Class::Light, Scope::Main, 20),
                (Class::Heavy, Scope::Pr, 1),
            ],
            vec![host("a")],
        );
        aged(&mut i, Class::Heavy, Scope::Pr, 500);
        let a = allocate(&i);
        assert_eq!(count(&a, Class::Heavy, Scope::Pr), 1);
        assert!(a.reserved_for.is_empty());
        assert_within_budget(&i, &a);
    }

    #[test]
    fn drones_that_could_never_fit_are_not_reserved() {
        let mut small = host("small");
        small.mem_mib = 8 * 1024;
        let mut nodocker = host("nodocker");
        nodocker.docker = false;
        let mut i = input(&[(Class::Heavy, Scope::Main, 1)], vec![small, nodocker]);
        aged(&mut i, Class::Heavy, Scope::Main, 500);
        let a = allocate(&i);
        assert!(a.reserved_for.is_empty());
        assert!(a.grants.is_empty());
    }

    #[test]
    fn starving_medium_also_reserves() {
        let mut h = host("a");
        h.mem_mib = 14 * 1024;
        let mut i = input(
            &[
                (Class::Medium, Scope::Main, 1),
                (Class::Light, Scope::Pr, 5),
            ],
            vec![h],
        );
        i.leases = vec![lease("a", Class::Light, Scope::Pr, LeaseState::Running)];
        aged(&mut i, Class::Medium, Scope::Main, 200);
        let a = allocate(&i);
        assert_eq!(a.reserved_for.len(), 1);
        assert!(
            a.grants.is_empty(),
            "nothing else may land on the reserved drone"
        );
    }

    #[test]
    fn never_over_grants_across_a_demand_sweep() {
        let drones = vec![
            host("a"),
            DroneState {
                id: "b".into(),
                vcpu: 4,
                mem_mib: 8 * 1024,
                docker: false,
                accepting: true,
                max_leases: Some(2),
                bids: vec![],
                profiles: vec![],
                blocked_classes: vec![],
                cpu_psi_pm: None,
                cpu_load_pm: None,
                trust: Trust::Shared,
            },
            DroneState {
                id: "c".into(),
                vcpu: 8,
                mem_mib: 14 * 1024,
                docker: true,
                accepting: true,
                max_leases: None,
                bids: vec![],
                profiles: vec![],
                blocked_classes: vec![],
                cpu_psi_pm: None,
                cpu_load_pm: None,
                trust: Trust::Shared,
            },
        ];
        for lm in 0..3 {
            for mm in 0..3 {
                for hm in 0..3 {
                    for lp in [0, 4] {
                        for hp in [0, 2] {
                            let mut i = input(
                                &[
                                    (Class::Light, Scope::Main, lm),
                                    (Class::Medium, Scope::Main, mm),
                                    (Class::Heavy, Scope::Main, hm),
                                    (Class::Light, Scope::Pr, lp),
                                    (Class::Heavy, Scope::Pr, hp),
                                ],
                                drones.clone(),
                            );
                            i.warm_floors = default_warm_floors();
                            i.leases =
                                vec![lease("c", Class::Light, Scope::Pr, LeaseState::Running)];
                            assert_within_budget(&i, &allocate(&i));
                        }
                    }
                }
            }
        }
    }

    // ---- bids -----------------------------------------------------------

    use crate::bid::DroneBid;

    fn bid(class: Class, scope: Scope) -> DroneBid {
        DroneBid {
            class,
            scope,
            available_slots: 8,
            estimated_start_ms: 0,
            cache_affinity: 0,
            resource_pressure: 0,
            estimated_cost: 1000,
            confidence: 1000,
            expires_at: i64::MAX,
            min_pressure: 0,
        }
    }

    fn bidder(id: &str, b: DroneBid) -> DroneState {
        DroneState {
            bids: vec![b],
            ..host(id)
        }
    }

    fn only_drone(a: &Allocation) -> &str {
        assert_eq!(a.grants.len(), 1, "{a:?}");
        &a.grants[0].drone_id
    }

    #[test]
    fn the_warmest_bid_wins_over_the_tightest_fit() {
        let mut cold = bid(Class::Light, Scope::Pr);
        cold.cache_affinity = 100;
        let mut warm = bid(Class::Light, Scope::Pr);
        warm.cache_affinity = 900;
        // "a" is the emptier host and would lose a pure tightest-fit contest
        // only if it were fuller; give "b" a lease so tightest-fit prefers b.
        let mut i = input(
            &[(Class::Light, Scope::Pr, 1)],
            vec![bidder("a", warm), bidder("b", cold)],
        );
        i.leases = vec![lease("b", Class::Medium, Scope::Pr, LeaseState::Running)];
        let a = allocate(&i);
        assert_eq!(only_drone(&a), "a");
    }

    #[test]
    fn loaded_slow_expensive_hosts_lose_and_ties_fall_back_to_fit_then_id() {
        let mut loaded = bid(Class::Light, Scope::Pr);
        loaded.resource_pressure = 800;
        let i = input(
            &[(Class::Light, Scope::Pr, 1)],
            vec![
                bidder("a", loaded),
                bidder("b", bid(Class::Light, Scope::Pr)),
            ],
        );
        assert_eq!(only_drone(&allocate(&i)), "b");

        // Equal bids: tightest fit (b has a lease), then id.
        let mut i = input(
            &[(Class::Light, Scope::Pr, 1)],
            vec![
                bidder("a", bid(Class::Light, Scope::Pr)),
                bidder("b", bid(Class::Light, Scope::Pr)),
            ],
        );
        i.leases = vec![lease("b", Class::Light, Scope::Pr, LeaseState::Running)];
        assert_eq!(only_drone(&allocate(&i)), "b");
        i.leases.clear();
        assert_eq!(only_drone(&allocate(&i)), "a");
    }

    #[test]
    fn a_bidding_drone_declines_keys_it_did_not_bid_on_and_exhausted_bids() {
        let i = input(
            &[(Class::Medium, Scope::Pr, 1)],
            vec![bidder("a", bid(Class::Light, Scope::Pr))],
        );
        assert!(allocate(&i).grants.is_empty());

        let mut one = bid(Class::Light, Scope::Pr);
        one.available_slots = 2;
        let i = input(&[(Class::Light, Scope::Pr, 5)], vec![bidder("a", one)]);
        let a = allocate(&i);
        assert_eq!(count(&a, Class::Light, Scope::Pr), 2);
        assert_eq!(a.unmet[0].count, 3);
    }

    #[test]
    fn legacy_drones_without_bids_stay_eligible_with_a_neutral_score() {
        let mut bad = bid(Class::Light, Scope::Pr);
        bad.resource_pressure = 900;
        // The legacy drone ("a", no bids) beats a heavily loaded bidder.
        let i = input(
            &[(Class::Light, Scope::Pr, 1)],
            vec![host("a"), bidder("b", bad)],
        );
        assert_eq!(only_drone(&allocate(&i)), "a");
    }

    #[test]
    fn response_thresholds_gate_participation_on_demand_pressure() {
        let mut shy = bid(Class::Light, Scope::Pr);
        shy.min_pressure = 600;
        // One queued job against eight idle slots: low pressure, shy host out.
        let i = input(&[(Class::Light, Scope::Pr, 1)], vec![bidder("a", shy)]);
        let a = allocate(&i);
        assert!(a.grants.is_empty());
        assert!(
            a.pressure
                .iter()
                .any(|p| p.class == Class::Light && p.pressure < 600)
        );

        // Many queued jobs against the same room: pressure crosses the
        // threshold and the shy host joins.
        let i = input(&[(Class::Light, Scope::Pr, 30)], vec![bidder("a", shy)]);
        let a = allocate(&i);
        assert!(!a.grants.is_empty());
        assert_eq!(
            a.pressure
                .iter()
                .find(|p| p.class == Class::Light)
                .map(|p| p.pressure),
            Some(1000)
        );
    }

    #[test]
    fn starving_demand_overrides_every_threshold() {
        let mut shy = bid(Class::Light, Scope::Pr);
        shy.min_pressure = 1000;
        let mut i = input(&[(Class::Light, Scope::Pr, 1)], vec![bidder("a", shy)]);
        assert!(allocate(&i).grants.is_empty());
        aged(&mut i, Class::Light, Scope::Pr, 500);
        assert_eq!(only_drone(&allocate(&i)), "a");
    }

    #[test]
    fn a_warm_floor_is_placed_only_on_always_willing_hosts() {
        let mut shy = bid(Class::Medium, Scope::Main);
        shy.min_pressure = 200;
        let mut i = input(&[], vec![bidder("a", shy)]);
        i.warm_floors = default_warm_floors();
        assert!(allocate(&i).grants.is_empty());
        let eager = bid(Class::Medium, Scope::Main);
        i.drones = vec![bidder("b", eager)];
        assert_eq!(only_drone(&allocate(&i)), "b");
    }

    #[test]
    fn bid_scoring_is_deterministic_and_never_over_grants() {
        let mut drones = Vec::new();
        for (n, id) in ["a", "b", "c", "d"].into_iter().enumerate() {
            let mut b = bid(Class::Light, Scope::Pr);
            b.cache_affinity = u16::try_from(n * 250).unwrap();
            b.available_slots = 3;
            drones.push(bidder(id, b));
        }
        let i = input(&[(Class::Light, Scope::Pr, 9)], drones);
        let first = allocate(&i);
        assert_eq!(first, allocate(&i));
        assert_within_budget(&i, &first);
        assert_eq!(count(&first, Class::Light, Scope::Pr), 9);
        // The warmest host fills its three slots first.
        assert_eq!(first.grants.iter().filter(|g| g.drone_id == "d").count(), 3);
    }

    // ---- learned profiles -------------------------------------------------

    fn profiled(id: &str, class: Class, speed: u32, conf: u32, rel: u32) -> DroneState {
        DroneState {
            profiles: vec![ProfileScore {
                class,
                speed_permille: speed,
                speed_confidence_permille: conf,
                reliability_permille: rel,
            }],
            ..host(id)
        }
    }

    #[test]
    fn a_proven_faster_host_wins_ties_that_would_go_to_the_lower_id() {
        let i = input(
            &[(Class::Light, Scope::Pr, 1)],
            vec![
                profiled("a", Class::Light, 1000, 0, 1000),
                profiled("b", Class::Light, 1600, 1000, 1000),
            ],
        );
        assert_eq!(only_drone(&allocate(&i)), "b");
    }

    #[test]
    fn unreliable_hosts_lose_and_unproven_profiles_change_nothing() {
        let i = input(
            &[(Class::Light, Scope::Pr, 1)],
            vec![
                profiled("a", Class::Light, 1000, 1000, 700),
                profiled("b", Class::Light, 1000, 1000, 1000),
            ],
        );
        assert_eq!(only_drone(&allocate(&i)), "b");
        // Speed with no confidence is ignored: the id tie-break decides.
        let i = input(
            &[(Class::Light, Scope::Pr, 1)],
            vec![
                profiled("a", Class::Light, 1000, 0, 1000),
                profiled("b", Class::Light, 2000, 0, 1000),
            ],
        );
        assert_eq!(only_drone(&allocate(&i)), "a");
    }

    #[test]
    fn profiles_are_per_class_and_stack_with_bids() {
        // The profile for medium does not help a light placement.
        let i = input(
            &[(Class::Light, Scope::Pr, 1)],
            vec![
                profiled("a", Class::Light, 1000, 0, 1000),
                profiled("b", Class::Medium, 2000, 1000, 1000),
            ],
        );
        assert_eq!(only_drone(&allocate(&i)), "a");
        // A bidding drone's score includes its profile.
        let mut warm = bid(Class::Light, Scope::Pr);
        warm.cache_affinity = 100;
        let fast = DroneState {
            bids: vec![bid(Class::Light, Scope::Pr)],
            ..profiled("b", Class::Light, 2000, 1000, 1000)
        };
        let i = input(
            &[(Class::Light, Scope::Pr, 1)],
            vec![bidder("a", warm), fast],
        );
        // a: 3*100 = 300; b: 1000 from the profile.
        assert_eq!(only_drone(&allocate(&i)), "b");
        assert_eq!(allocate(&i), allocate(&i));
    }

    fn deploy_host(id: &str) -> DroneState {
        DroneState {
            trust: Trust::Deploy,
            ..host(id)
        }
    }

    #[test]
    fn deploy_demand_lands_only_on_deploy_drones_with_only_the_deploy_label() {
        let a = allocate(&input(
            &[(Class::Medium, Scope::Deploy, 2)],
            vec![host("shared"), deploy_host("dep")],
        ));
        assert_eq!(count(&a, Class::Medium, Scope::Deploy), 2);
        assert!(a.grants.iter().all(|g| g.drone_id == "dep"));
        assert!(
            a.grants
                .iter()
                .all(|g| g.labels == ["borg-ci-medium-deploy"])
        );

        let a = allocate(&input(
            &[(Class::Medium, Scope::Deploy, 1)],
            vec![host("shared")],
        ));
        assert!(a.grants.is_empty());
        assert_eq!(a.unmet.len(), 1);
        assert_eq!(a.unmet[0].scope, Scope::Deploy);
    }

    #[test]
    fn pr_demand_never_lands_on_deploy_drones_but_main_may() {
        let a = allocate(&input(
            &[(Class::Light, Scope::Pr, 3), (Class::Light, Scope::Main, 1)],
            vec![deploy_host("dep")],
        ));
        assert_eq!(count(&a, Class::Light, Scope::Pr), 0);
        assert_eq!(count(&a, Class::Light, Scope::Main), 1);
        assert_eq!(a.unmet[0].scope, Scope::Pr);
    }

    #[test]
    fn deploy_is_placed_before_main_and_pr_under_contention() {
        let mut d = deploy_host("dep");
        d.max_leases = Some(1);
        let a = allocate(&input(
            &[
                (Class::Light, Scope::Pr, 1),
                (Class::Light, Scope::Main, 1),
                (Class::Light, Scope::Deploy, 1),
            ],
            vec![d],
        ));
        assert_eq!(a.grants.len(), 1);
        assert_eq!(a.grants[0].scope, Scope::Deploy);
    }

    #[test]
    fn starved_deploy_demand_reserves_only_a_deploy_drone() {
        let mut i = input(
            &[(Class::Heavy, Scope::Deploy, 1)],
            vec![host("shared"), deploy_host("dep")],
        );
        i.starvation_after_secs = Some(10);
        i.queue_ages = vec![QueueAge {
            class: Class::Heavy,
            scope: Scope::Deploy,
            oldest_age_secs: 60,
        }];
        i.leases = vec![
            lease("dep", Class::Heavy, Scope::Main, LeaseState::Running),
            lease("dep", Class::Light, Scope::Main, LeaseState::Running),
        ];
        let a = allocate(&i);
        assert!(a.grants.is_empty());
        assert_eq!(a.reserved_for.len(), 1);
        assert_eq!(a.reserved_for[0].drone_id, "dep");
    }

    #[test]
    fn pending_absorption_respects_trust_scopes() {
        // A pending PR runner (bare label) absorbs main demand, never deploy.
        let mut i = input(&[(Class::Light, Scope::Main, 1)], vec![host("a")]);
        i.leases = vec![lease("a", Class::Light, Scope::Pr, LeaseState::Pending)];
        assert!(allocate(&i).grants.is_empty());

        let mut i = input(
            &[(Class::Light, Scope::Deploy, 1)],
            vec![host("a"), deploy_host("dep")],
        );
        i.leases = vec![lease("a", Class::Light, Scope::Pr, LeaseState::Pending)];
        assert_eq!(count(&allocate(&i), Class::Light, Scope::Deploy), 1);
        i.leases = vec![lease(
            "dep",
            Class::Light,
            Scope::Deploy,
            LeaseState::Pending,
        )];
        assert!(allocate(&i).grants.is_empty());

        // A pending deploy runner carries only the deploy label: main demand
        // still needs its own runner.
        let mut i = input(&[(Class::Light, Scope::Main, 1)], vec![deploy_host("dep")]);
        i.leases = vec![lease(
            "dep",
            Class::Light,
            Scope::Deploy,
            LeaseState::Pending,
        )];
        assert_eq!(count(&allocate(&i), Class::Light, Scope::Main), 1);
    }

    #[test]
    fn warm_floors_may_name_the_deploy_scope() {
        let mut i = input(&[], vec![deploy_host("dep")]);
        i.warm_floors = vec![WarmFloor {
            class: Class::Light,
            scope: Scope::Deploy,
            count: 2,
        }];
        assert_eq!(count(&allocate(&i), Class::Light, Scope::Deploy), 2);
    }

    fn deploy_only_host(id: &str) -> DroneState {
        DroneState {
            trust: Trust::DeployOnly,
            ..host(id)
        }
    }

    #[test]
    fn deploy_only_drones_take_deploy_demand_and_nothing_else() {
        let a = allocate(&input(
            &[
                (Class::Medium, Scope::Main, 1),
                (Class::Medium, Scope::Pr, 1),
            ],
            vec![deploy_only_host("only")],
        ));
        assert!(a.grants.is_empty(), "{:?}", a.grants);
        assert_eq!(a.unmet.len(), 2);
        let a = allocate(&input(
            &[(Class::Medium, Scope::Deploy, 1)],
            vec![deploy_only_host("only")],
        ));
        assert_eq!(count(&a, Class::Medium, Scope::Deploy), 1);
        assert_eq!(a.grants[0].drone_id, "only");
    }

    #[test]
    fn a_standby_holds_capacity_and_counts_as_pending_deploy_capacity() {
        // No queued deploy demand: the standby keeps the deploy floor met and
        // nothing is promoted or minted.
        let mut i = input(&[], vec![deploy_only_host("dep")]);
        i.warm_floors = vec![WarmFloor {
            class: Class::Medium,
            scope: Scope::Deploy,
            count: 1,
        }];
        i.leases = vec![lease(
            "dep",
            Class::Medium,
            Scope::Deploy,
            LeaseState::Standby,
        )];
        let a = allocate(&i);
        assert!(a.grants.is_empty() && a.promote.is_empty());
        // It also takes room: a 1-lease cap is full.
        let mut capped = deploy_only_host("dep");
        capped.max_leases = Some(1);
        let mut i = input(&[(Class::Light, Scope::Deploy, 1)], vec![capped]);
        i.leases = vec![lease(
            "dep",
            Class::Medium,
            Scope::Deploy,
            LeaseState::Standby,
        )];
        assert!(allocate(&i).grants.is_empty());
    }

    #[test]
    fn deploy_demand_with_a_standby_promotes_it_instead_of_minting() {
        let mut i = input(
            &[(Class::Medium, Scope::Deploy, 1)],
            vec![deploy_only_host("dep")],
        );
        i.leases = vec![lease(
            "dep",
            Class::Medium,
            Scope::Deploy,
            LeaseState::Standby,
        )];
        let a = allocate(&i);
        assert!(a.grants.is_empty(), "{:?}", a.grants);
        assert!(a.unmet.is_empty());
        assert_eq!(
            a.promote,
            vec![Promotion {
                class: Class::Medium,
                count: 1
            }]
        );
        // Two queued jobs, one standby: one promotion, one mint.
        let mut i = input(
            &[(Class::Medium, Scope::Deploy, 2)],
            vec![deploy_only_host("dep")],
        );
        i.leases = vec![lease(
            "dep",
            Class::Medium,
            Scope::Deploy,
            LeaseState::Standby,
        )];
        let a = allocate(&i);
        assert_eq!(a.promote.len(), 1);
        assert_eq!(count(&a, Class::Medium, Scope::Deploy), 1);
        // A pending deploy runner already covers the job: no promotion.
        let mut i = input(
            &[(Class::Medium, Scope::Deploy, 1)],
            vec![deploy_only_host("dep")],
        );
        i.leases = vec![
            lease("dep", Class::Medium, Scope::Deploy, LeaseState::Standby),
            lease("dep", Class::Medium, Scope::Deploy, LeaseState::Pending),
        ];
        let a = allocate(&i);
        assert!(a.grants.is_empty() && a.promote.is_empty());
    }

    fn pr_turn(i: &mut AllocatorInput, class: Class) {
        i.queue_ages.retain(|q| q.scope != Scope::Pr);
        i.queue_ages.push(QueueAge {
            class,
            scope: Scope::Pr,
            oldest_age_secs: 1000,
        });
        i.pr_turn = PrTurn {
            after_secs: Some(900),
            classes: vec![class],
        };
    }

    #[test]
    fn pr_turn_gives_starving_pr_demand_one_grant_ahead_of_main() {
        let mut i = input(
            &[(Class::Heavy, Scope::Main, 3), (Class::Heavy, Scope::Pr, 3)],
            vec![host("a")],
        );
        pr_turn(&mut i, Class::Heavy);
        let a = allocate(&i);
        assert_eq!(count(&a, Class::Heavy, Scope::Pr), 1);
        assert_eq!(count(&a, Class::Heavy, Scope::Main), 0);
        assert_within_budget(&i, &a);
        // Room for two: the turn is one grant, main takes the rest.
        i.drones.push(host("b"));
        let a = allocate(&i);
        assert_eq!(count(&a, Class::Heavy, Scope::Pr), 1);
        assert_eq!(count(&a, Class::Heavy, Scope::Main), 1);
        assert_within_budget(&i, &a);
    }

    #[test]
    fn pr_turn_needs_the_class_listed_and_its_demand_aged() {
        let mut i = input(
            &[(Class::Heavy, Scope::Main, 3), (Class::Heavy, Scope::Pr, 3)],
            vec![host("a")],
        );
        pr_turn(&mut i, Class::Medium);
        assert_eq!(count(&allocate(&i), Class::Heavy, Scope::Main), 1);
        pr_turn(&mut i, Class::Heavy);
        i.queue_ages
            .iter_mut()
            .for_each(|q| q.oldest_age_secs = 100);
        assert_eq!(count(&allocate(&i), Class::Heavy, Scope::Main), 1);
        i.queue_ages
            .iter_mut()
            .for_each(|q| q.oldest_age_secs = 5000);
        i.pr_turn.after_secs = None;
        assert_eq!(count(&allocate(&i), Class::Heavy, Scope::Main), 1);
    }

    #[test]
    fn pr_turn_obeys_admission_and_the_scope_filter() {
        let mut i = input(
            &[(Class::Heavy, Scope::Main, 1), (Class::Heavy, Scope::Pr, 1)],
            vec![host("a")],
        );
        pr_turn(&mut i, Class::Heavy);
        i.drones[0].cpu_psi_pm = Some(400);
        i.admission = Some(AdmissionThresholds {
            psi_soft_pm: 300,
            psi_light_only_pm: 500,
            psi_hard_pm: 600,
        });
        let a = allocate(&i);
        assert_eq!(count(&a, Class::Heavy, Scope::Pr), 0);
        assert_eq!(count(&a, Class::Heavy, Scope::Main), 1);
        i.admission = None;
        i.drones[0].cpu_psi_pm = None;
        let a = allocate_scope(&i, Scope::Main);
        assert_eq!(count(&a, Class::Heavy, Scope::Pr), 0);
        assert_eq!(count(&a, Class::Heavy, Scope::Main), 1);
    }
}
