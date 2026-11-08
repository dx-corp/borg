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

use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;

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
    /// What a slot-hour on this host costs the fleet (permille; 1000 = owned
    /// hardware, whose marginal cost is nothing). The queen sets it, not the
    /// drone: paid cloud drones get `placement.burst_cost_permille`, so work
    /// goes to idle owned hosts first. See [`cost_term`].
    #[serde(default = "owned_cost")]
    pub(crate) cost_permille: u32,
    /// What the drone measured about memory right now, set by the queen from
    /// fresh facts. Empty: the drone is sized by its budget alone.
    #[serde(default)]
    pub(crate) live: LiveFacts,
}

/// A drone's measured memory facts, which size it beyond its budget:
///
/// - Memory is charged at the larger of what its leases committed (their
///   class sizes) and what is in use now, so a job that outgrows its class,
///   or anything else running in the container, shrinks the room.
/// - Drones that share one kernel (several containers on one Proxmox node)
///   also draw on one pool: the node's available memory, less a reserve, less
///   the growth their leases may still make (committed minus in use). Two
///   drones cannot each admit a heavy job into the same free memory.
/// - Memory pressure feeds admission next to CPU pressure, and the state a
///   drone settled in last pass gives admission its hysteresis.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveFacts {
    /// Memory in use on the drone as it sees it (`MemTotal - MemAvailable`),
    /// MiB; `None` when not measured or stale.
    #[serde(default)]
    pub(crate) mem_used_mib: Option<u64>,
    /// The node memory this drone shares; `None` without a host view.
    #[serde(default)]
    pub(crate) host: Option<HostMemory>,
    /// `memory some avg10`, permille: the highest of what the drone sees and
    /// its host view.
    #[serde(default)]
    pub(crate) mem_psi_pm: Option<u16>,
    /// The admission state this drone settled in on the previous pass.
    #[serde(default)]
    pub(crate) admission_prev: Option<AdmissionState>,
}

/// One node's memory as a drone on it reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostMemory {
    /// Drones reporting the same key share the pool (the node's boot id).
    pub(crate) key: String,
    /// The node's `MemAvailable`, MiB.
    pub(crate) available_mib: u64,
    /// Kept free on the node for its other guests, MiB.
    pub(crate) reserve_mib: u64,
}

/// The cost of an owned host, permille.
pub(crate) const OWNED_COST_PERMILLE: u32 = 1000;

fn owned_cost() -> u32 {
    OWNED_COST_PERMILLE
}

/// The placement term for what the host costs: `-cost * (cost_permille -
/// 1000)`, so an owned host adds nothing and a paid one subtracts in
/// proportion to its price. A drone's self-reported `estimated_cost` (in its
/// bid) is a separate term; this one is the queen's.
fn cost_term(drone: &DroneState, w: &ScoreWeights) -> i64 {
    let excess = i64::from(drone.cost_permille.min(100_000)) - i64::from(OWNED_COST_PERMILLE);
    -w.cost.saturating_mul(excess)
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
    /// From this `memory some avg10` only light main and deploy grants.
    pub(crate) mem_psi_light_only_pm: u16,
    /// From this memory pressure no new grant.
    pub(crate) mem_psi_hard_pm: u16,
    /// A drone leaves a restrictive state only once its pressure is this far
    /// (permille of each threshold) below the threshold that put it there.
    pub(crate) hysteresis_pm: u16,
}

impl AdmissionThresholds {
    /// The same thresholds lowered by the hysteresis margin, never to 0.
    fn lowered(self) -> Self {
        let keep = 1000 - u32::from(self.hysteresis_pm.min(999));
        let low = |t: u16| {
            u16::try_from(u32::from(t) * keep / 1000)
                .unwrap_or(t)
                .max(1)
        };
        Self {
            psi_soft_pm: low(self.psi_soft_pm),
            psi_light_only_pm: low(self.psi_light_only_pm),
            psi_hard_pm: low(self.psi_hard_pm),
            mem_psi_light_only_pm: low(self.mem_psi_light_only_pm),
            mem_psi_hard_pm: low(self.mem_psi_hard_pm),
            hysteresis_pm: self.hysteresis_pm,
        }
    }
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

    /// Order by how much is shed: open < pr_blocked < light_only < closed.
    fn rank(self) -> u8 {
        match self {
            Self::Open => 0,
            Self::PrBlocked => 1,
            Self::LightOnly => 2,
            Self::Closed => 3,
        }
    }

    fn stricter(self, other: Self) -> Self {
        if other.rank() > self.rank() {
            other
        } else {
            self
        }
    }

    fn looser(self, other: Self) -> Self {
        if other.rank() < self.rank() {
            other
        } else {
            self
        }
    }

    /// The state for CPU pressure `cpu` and memory pressure `mem` alone.
    fn raw(cpu: Option<u16>, mem: Option<u16>, t: AdmissionThresholds) -> Self {
        let by_mem = match mem {
            Some(p) if p >= t.mem_psi_hard_pm => Self::Closed,
            Some(p) if p >= t.mem_psi_light_only_pm => Self::LightOnly,
            _ => Self::Open,
        };
        Self::of(cpu, Some(t)).stricter(by_mem)
    }

    /// The state a drone settles in this pass. Rising pressure takes effect
    /// at once; a drone relaxes from `prev` only as far as thresholds lowered
    /// by the hysteresis margin allow, so pressure hovering at a threshold
    /// does not flip it every pass.
    pub(crate) fn settle(
        cpu: Option<u16>,
        mem: Option<u16>,
        prev: Option<Self>,
        t: Option<AdmissionThresholds>,
    ) -> Self {
        let Some(t) = t else {
            return Self::Open;
        };
        let raw = Self::raw(cpu, mem, t);
        match prev {
            Some(prev) if prev.rank() > raw.rank() => {
                raw.stricter(prev.looser(Self::raw(cpu, mem, t.lowered())))
            }
            _ => raw,
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

    /// This pass's admission state; see [`AdmissionState::settle`].
    pub(crate) fn admission(&self, t: Option<AdmissionThresholds>) -> AdmissionState {
        AdmissionState::settle(
            self.admission_pressure_pm(),
            self.live.mem_psi_pm,
            self.live.admission_prev,
            t,
        )
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
    /// Memory pressure, permille; see [`LiveFacts::mem_psi_pm`].
    #[serde(default)]
    pub(crate) mem_psi_pm: Option<u16>,
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
            cost_permille: OWNED_COST_PERMILLE,
            live: LiveFacts::default(),
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
    /// The drone has room but its node's shared free memory does not.
    HostMemory,
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
    /// What the host costs: 0 for owned hardware, negative for a paid cloud
    /// drone. See [`cost_term`].
    #[serde(default)]
    pub(crate) cost_score: i64,
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
    /// Why the winner beat the best other eligible drone; `None` when
    /// nothing was eligible. See [`PlacementReason`].
    #[serde(default)]
    pub(crate) reason: Option<PlacementReason>,
}

/// The score term that separated the winner from the runner-up, the best
/// other eligible drone. Each is the largest of the winner's leads in the
/// cost, profile, host and bid terms; equal scores fall to the tie-breaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PlacementReason {
    /// No other drone was eligible.
    OnlyEligible,
    /// Cheaper: an owned host over a paid burst drone.
    Cost,
    /// Faster or more reliable for this class (or the queued families).
    Profile,
    /// Less loaded or more idle right now.
    Host,
    /// A better bid (warmer caches, sooner start, lower pressure).
    Bid,
    /// Equal score; less memory left over.
    TightestFit,
    /// Equal score and fit; the lower drone name.
    Name,
}

impl PlacementReason {
    fn of(winner: &Candidate, runner_up: Option<&Candidate>) -> Self {
        let Some(other) = runner_up else {
            return Self::OnlyEligible;
        };
        if winner.score == other.score {
            return if winner.room.mem_mib == other.room.mem_mib {
                Self::Name
            } else {
                Self::TightestFit
            };
        }
        // Ties between terms go to the first listed.
        [
            (winner.cost_score - other.cost_score, Self::Cost),
            (winner.profile_score - other.profile_score, Self::Profile),
            (winner.host_score - other.host_score, Self::Host),
            (winner.bid_score - other.bid_score, Self::Bid),
        ]
        .into_iter()
        .rev()
        .max_by_key(|(lead, _)| *lead)
        .map_or(Self::Profile, |(_, reason)| reason)
    }
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

/// Free memory one node's drones share this pass; see [`LiveFacts`].
type Pool = Rc<Cell<u64>>;

/// Remaining room on a drone while allocating.
struct Free<'a> {
    drone: &'a DroneState,
    mem_mib: u64,
    /// The node pool this drone draws on too, if it shares one.
    pool: Option<Pool>,
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
    cost_score: i64,
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
        let cost_score = cost_term(self.drone, weights);
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
            cost_score,
            score: rejection_reasons.is_empty().then_some(
                bid_score
                    .saturating_add(profile_score)
                    .saturating_add(host_score)
                    .saturating_add(cost_score),
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
        } else if self.mem_room() < spec.mem_mib {
            reasons.push(Rejection::HostMemory);
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
                mem_mib: self.mem_room(),
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
            cost_score: evaluated.cost_score,
        }
    }

    /// Memory a new lease may take: the drone's room, and its node's pool.
    fn mem_room(&self) -> u64 {
        self.pool
            .as_ref()
            .map_or(self.mem_mib, |p| p.get().min(self.mem_mib))
    }

    fn fits(&self, spec: &ClassSpec) -> bool {
        self.drone.accepting
            && self.admission != AdmissionState::Closed
            && (!spec.docker || self.drone.docker)
            && self.mem_room() >= spec.mem_mib
            && self.vcpu >= spec.vcpu
            && self.leases_left != Some(0)
    }

    /// Could this drone ever hold `spec` if it were empty?
    /// Whole leases of `spec` the remaining room holds.
    fn slots(&self, spec: &ClassSpec) -> u32 {
        let by_mem = self.mem_room() / spec.mem_mib.max(1);
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
        if let Some(p) = &self.pool {
            p.set(p.get().saturating_sub(spec.mem_mib));
        }
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
            pool: None,
            vcpu: drone.vcpu,
            leases_left: drone.max_leases,
            in_flight: 0,
            admission: drone.admission(admission),
            granted: BTreeMap::new(),
        });
    }
    let mut committed: BTreeMap<&str, u64> = BTreeMap::new();
    for lease in leases {
        if let Some(f) = free.get_mut(lease.drone_id.as_str()) {
            let spec = classes.spec(lease.class);
            f.take(spec);
            f.in_flight += 1;
            *committed.entry(f.drone.id.as_str()).or_default() += spec.mem_mib;
        }
    }
    charge_live_memory(&mut free, &committed);
    free
}

/// Apply [`LiveFacts`] to rooms already net of their leases' class sizes:
/// charge each drone the larger of committed and used memory, and join drones
/// on one node to a pool of its free memory less its reserve and the growth
/// their leases may still make.
fn charge_live_memory(free: &mut BTreeMap<&str, Free<'_>>, committed: &BTreeMap<&str, u64>) {
    let mut pools: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for f in free.values_mut() {
        let held = committed.get(f.drone.id.as_str()).copied().unwrap_or(0);
        let used = f.drone.live.mem_used_mib;
        if let Some(used) = used {
            f.mem_mib = f
                .mem_mib
                .min(f.drone.mem_mib.saturating_sub(used.max(held)));
        }
        if let Some(h) = &f.drone.live.host {
            // A drone without a reading owes its whole commitment.
            let growth = held.saturating_sub(used.unwrap_or(0));
            let free_now = h.available_mib.saturating_sub(h.reserve_mib);
            let entry = pools.entry(h.key.clone()).or_insert((free_now, 0));
            // Readings arrive at different times; the lowest is the safe one.
            entry.0 = entry.0.min(free_now);
            entry.1 = entry.1.saturating_add(growth);
        }
    }
    let shared: BTreeMap<String, Pool> = pools
        .into_iter()
        .map(|(key, (free_now, growth))| (key, Rc::new(Cell::new(free_now.saturating_sub(growth)))))
        .collect();
    for f in free.values_mut() {
        f.pool = f
            .drone
            .live
            .host
            .as_ref()
            .and_then(|h| shared.get(&h.key))
            .cloned();
    }
}

/// Idle slots of `class` across `free`. Spare room counts only where any
/// queued job of the class could be placed: a drone shedding a scope under
/// CPU pressure, or blocking the class by its environment, has no idle slot
/// for this purpose.
fn class_idle(free: &BTreeMap<&str, Free<'_>>, class: Class, spec: &ClassSpec) -> u32 {
    // A node pool is drawn down as its drones are counted, so its memory is
    // not offered once per drone.
    let mut pools: BTreeMap<*const Cell<u64>, u64> = BTreeMap::new();
    let mut idle = 0u32;
    for f in free.values().filter(|f| {
        f.fits(spec)
            && !f.drone.blocked_classes.contains(&class)
            && Scope::ALL.iter().all(|s| f.admission.admits(class, *s))
    }) {
        let mut slots = f.slots(spec);
        if let Some(p) = &f.pool {
            let left = pools.entry(Rc::as_ptr(p)).or_insert_with(|| p.get());
            slots = slots.min(u32::try_from(*left / spec.mem_mib.max(1)).unwrap_or(u32::MAX));
            *left = left.saturating_sub(u64::from(slots) * spec.mem_mib);
        }
        idle = idle.saturating_add(slots);
    }
    idle
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
        let mut runner_up: Option<Candidate> = None;
        let mut best: Option<(i64, u64, &str)> = None;
        let mut second: Option<(i64, u64, &str)> = None;
        for (id, f) in free.iter() {
            let candidate = f.candidate(input, class, scope, level, reserved.get(*id).copied());
            if let Some(score) = candidate.score {
                let rank = (std::cmp::Reverse(score), f.mem_room() - spec.mem_mib, *id);
                let beats = |held: Option<(i64, u64, &str)>| {
                    held.is_none_or(|(s, mem, name)| rank < (std::cmp::Reverse(s), mem, name))
                };
                if beats(best) {
                    second = best;
                    runner_up = winning_candidate.take();
                    best = Some((score, f.mem_room() - spec.mem_mib, *id));
                    winning_candidate = Some(candidate.clone());
                } else if beats(second) {
                    second = Some((score, f.mem_room() - spec.mem_mib, *id));
                    runner_up = Some(candidate.clone());
                }
            }
            if candidates.len() < MAX_CANDIDATES {
                candidates.push(candidate);
            }
        }
        let winner = best.map(|(_, _, id)| id);
        let reason = winning_candidate
            .as_ref()
            .map(|w| PlacementReason::of(w, runner_up.as_ref()));
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
                reason,
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
                mem_psi_pm: f.drone.live.mem_psi_pm,
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

    // Deploy keeps first claim. On shared hosts, reserve larger aged work
    // across Main and PR before a smaller grant or PR turn can refill its
    // draining host. Main stays first within a class, apart from its PR turn.
    let placement_order = Class::ALL
        .into_iter()
        .rev()
        .map(|class| (Scope::Deploy, class))
        .chain(
            Class::ALL
                .into_iter()
                .rev()
                .flat_map(|class| [(Scope::Main, class), (Scope::Pr, class)]),
        );
    for (scope, class) in placement_order {
        if scope == Scope::Main {
            // One grant per listed class to aged PR demand, ahead of Main's
            // demand for that class, while respecting larger reservations.
            let key = (Scope::Pr, class);
            let wanted = need.get(&key).copied().unwrap_or(0);
            if wanted > 0
                && input.pr_turn.classes.contains(&class)
                && get(&queued, class, Scope::Pr) > 0
                && waited(class, Scope::Pr, input.pr_turn.after_secs)
            {
                let n = place(input, &mut free, &pressure, &reserved, key, 1, &mut out);
                *placed.entry(key).or_default() += n;
                need.insert(key, wanted - n);
            }
        }

        // Aged work that still cannot fit holds the host that can free up
        // soonest. Smaller grants may only use the existing safe backfill.
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
mod tests;
