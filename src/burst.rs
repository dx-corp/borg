//! Burst capacity: the rule that decides when the queen should add or remove
//! short-lived cloud drones, and what they have cost.
//!
//! Nothing here touches the network or the clock. `plan` reads what the
//! queen saw this pass (`Observed`) and the remembered timers
//! ([`BurstState`]) and returns [`Action`]s. The queen carries them out
//! (dry-run: it only logs them and applies them to a simulated fleet).
//!
//! The rules, all declared in the `burst` block of the queen config:
//!
//! * **Scale up** when the weighted count of jobs borg could not place (main
//!   jobs weigh more than pull-request jobs) stays at or above
//!   `scale_up.min_weighted_unmet` for `scale_up.sustained_secs`, counting only
//!   classes in `scale_up.classes` that have at most `max_free_slots` free
//!   slots. After each create the timer restarts, so every further drone needs
//!   its own sustained window. Nothing is created while a drone is still
//!   provisioning, at `max_drones`, or without budget headroom. Deploy jobs
//!   never count: burst drones enroll with shared trust and cannot take them.
//! * **Scale down** when no demand rule holds and a drone this queen created
//!   has held no lease for `scale_down.idle_secs`. It is drained first (it
//!   takes no new lease) and deleted on a later pass, and only when it holds
//!   no lease. A drone is never deleted with a lease, whatever else is true.
//! * **Retire**: a drone within `retire_margin_secs` of its hard maximum run
//!   time is drained, so it does not take a job the cloud will cut short.
//!   Replacements follow the scale-up rule only: a drone is replaced while
//!   demand holds and not otherwise.
//! * **Budget**: drone-hours per UTC day and per UTC month. Creation needs
//!   headroom for `reserve_hours` of every drone, old and new. Once a budget
//!   is spent every drone is drained and deleted as it empties; leases already
//!   running finish (bounded by `lease_max_secs`), so overshoot is bounded by
//!   the jobs in flight, never by new work.
//!
//! Spend is counted from the queen's own records as the time from creation to
//! deletion, capped at the maximum run time (the cloud deletes the VM then),
//! which is an upper bound: a spot VM reclaimed early is still counted until
//! the queen deletes its record. Drones created by hand are counted toward
//! `max_drones` but are never drained, deleted or billed here.
//!
//! This build only simulates: `BurstSettings::validate` rejects
//! `dry_run = false`, and the queen keeps the simulated fleet in memory, so a
//! restart forgets simulated drones and their spend.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, Time};

use crate::allocator::{ClassIdle, Unmet};
use crate::classes::{Class, Scope};

const HOUR: f64 = 3600.0;
/// Decisions kept for `/status`.
const RECENT_DECISIONS: usize = 20;
/// A simulated drone counts as enrolled this long after its simulated creation.
pub(super) const SIMULATED_PROVISION_SECS: i64 = 720;

fn yes() -> bool {
    true
}
fn default_drone_prefix() -> String {
    "gcp-burst-".to_owned()
}
fn default_hourly_cost() -> f64 {
    1.20
}
fn default_step() -> u32 {
    1
}
fn default_reserve() -> f64 {
    2.0
}
fn default_retire_margin() -> i64 {
    1800
}
fn default_provision_grace() -> i64 {
    1500
}
fn default_vm_prefix() -> String {
    "borg-burst-".to_owned()
}
fn default_max_run() -> i64 {
    12 * 3600
}
fn default_true_spot() -> bool {
    true
}
fn default_disk_gb() -> u32 {
    1000
}
fn default_disk_type() -> String {
    "pd-balanced".to_owned()
}
fn default_image_family() -> String {
    "ubuntu-2204-lts".to_owned()
}
fn default_image_project() -> String {
    "ubuntu-os-cloud".to_owned()
}

/// The `burst` block of the queen config. Off unless `enabled`; dry-run unless
/// `dry_run` is set to false.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BurstSettings {
    #[serde(default)]
    pub enabled: bool,
    /// Decide and log, but create and delete nothing.
    #[serde(default = "yes")]
    pub(super) dry_run: bool,
    /// Most burst drones at once, counting ones created by hand.
    pub max_drones: u32,
    /// Name prefix of burst drones in the queen. Must be in
    /// `drone_expiry.burst_prefixes` so a dead drone's record goes away.
    #[serde(default = "default_drone_prefix")]
    pub(super) drone_prefix: String,
    /// Used only to show spend in dollars.
    #[serde(default = "default_hourly_cost")]
    hourly_cost_usd: f64,
    pub budget: BudgetSettings,
    scale_up: ScaleUpSettings,
    scale_down: ScaleDownSettings,
    pub(super) gce: GceSettings,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetSettings {
    pub drone_hours_per_day: f64,
    pub drone_hours_per_month: f64,
    /// Optional additional ceilings on estimated all-in dollar spend.
    /// These are estimates, not cloud billing quotas; both must be supplied together.
    #[serde(default)]
    max_estimated_usd_per_day: Option<f64>,
    #[serde(default)]
    max_estimated_usd_per_month: Option<f64>,
    /// Headroom required per drone, old and new, before creating one.
    #[serde(default = "default_reserve")]
    reserve_hours: f64,
}

/// Weight of one unmet job by scope. Deploy has none: burst drones enroll
/// with shared trust and never take deploy work, so it cannot justify one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeWeights {
    main: u32,
    pr: u32,
}

impl ScopeWeights {
    fn of(self, scope: Scope) -> u32 {
        match scope {
            Scope::Deploy => 0,
            Scope::Main => self.main,
            Scope::Pr => self.pr,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScaleUpSettings {
    /// Classes whose unmet jobs count.
    classes: Vec<Class>,
    weights: ScopeWeights,
    min_weighted_unmet: u32,
    sustained_secs: i64,
    /// A class with more free slots than this has no unmet demand to serve.
    #[serde(default)]
    max_free_slots: u32,
    /// Most drones created in one pass.
    #[serde(default = "default_step")]
    step: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScaleDownSettings {
    idle_secs: i64,
    /// Drain a drone this long before its maximum run time.
    #[serde(default = "default_retire_margin")]
    retire_margin_secs: i64,
    /// A created drone that has not enrolled by then is deleted.
    #[serde(default = "default_provision_grace")]
    provision_grace_secs: i64,
}

/// Configuration for simulated VM placement and lifetime.
/// Provisioning-only inputs remain accepted for configuration compatibility;
/// this dry-run-only build neither provisions VMs nor reads credential files.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GceSettings {
    project: String,
    network: String,
    subnet: String,
    zones: Vec<String>,
    machine_type: String,
    /// Instance names; the queen's service account is allowed only these.
    #[serde(default = "default_vm_prefix")]
    vm_prefix: String,
    #[serde(default = "default_true_spot")]
    #[serde(rename = "spot")]
    _spot: bool,
    /// Hard lifetime; the cloud deletes the VM then.
    #[serde(default = "default_max_run")]
    pub(super) max_run_secs: i64,
    #[serde(default = "default_disk_gb")]
    #[serde(rename = "boot_disk_gb")]
    _boot_disk_gb: u32,
    #[serde(default = "default_disk_type")]
    #[serde(rename = "boot_disk_type")]
    _boot_disk_type: String,
    #[serde(default = "default_image_family")]
    #[serde(rename = "image_family")]
    _image_family: String,
    #[serde(default = "default_image_project")]
    #[serde(rename = "image_project")]
    _image_project: String,
    /// Accepted credential-path input; this build does not open the file.
    #[serde(default)]
    #[serde(rename = "credentials_path")]
    _credentials_path: Option<PathBuf>,
}

fn dns_label(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

impl BurstSettings {
    pub(super) fn validate(&self) -> Result<(), String> {
        let bad = |m: &str| Err(format!("burst: {m}"));
        let (b, up, down, gce) = (&self.budget, &self.scale_up, &self.scale_down, &self.gce);
        if !(1..=10).contains(&self.max_drones) {
            return bad("max_drones must be between 1 and 10");
        }
        if !dns_label(&self.drone_prefix) || !self.drone_prefix.ends_with('-') {
            return bad(
                "drone_prefix must be lowercase alphanumerics and dashes, ending in a dash",
            );
        }
        if !dns_label(&gce.vm_prefix) || !gce.vm_prefix.ends_with('-') {
            return bad(
                "gce.vm_prefix must be lowercase alphanumerics and dashes, ending in a dash",
            );
        }
        let positive = |v: f64| v.is_finite() && v > 0.0;
        let budget_ok = positive(b.drone_hours_per_day)
            && positive(b.drone_hours_per_month)
            && b.drone_hours_per_day <= b.drone_hours_per_month
            && b.reserve_hours.is_finite()
            && b.reserve_hours >= 0.0
            && positive(self.hourly_cost_usd);
        if !budget_ok {
            return bad(
                "budget needs 0 < drone_hours_per_day <= drone_hours_per_month and reserve_hours >= 0",
            );
        }
        match (b.max_estimated_usd_per_day, b.max_estimated_usd_per_month) {
            (None, None) => {}
            (Some(day), Some(month)) if positive(day) && positive(month) && day <= month => {}
            _ => {
                return bad(
                    "budget needs both positive estimated USD limits, day <= month, or neither",
                );
            }
        }
        if up.classes.is_empty()
            || up.min_weighted_unmet == 0
            || up.sustained_secs <= 0
            || up.step == 0
            || up.weights == (ScopeWeights { main: 0, pr: 0 })
        {
            return bad(
                "scale_up needs classes, a nonzero weight, min_weighted_unmet, sustained_secs and step",
            );
        }
        if down.idle_secs <= 0 || down.retire_margin_secs < 0 || down.provision_grace_secs <= 0 {
            return bad("scale_down needs positive idle_secs and provision_grace_secs");
        }
        if gce.zones.is_empty() || gce.zones.iter().any(|z| !dns_label(z)) {
            return bad("gce.zones must name at least one zone");
        }
        for (name, v) in [
            ("project", &gce.project),
            ("network", &gce.network),
            ("subnet", &gce.subnet),
            ("machine_type", &gce.machine_type),
        ] {
            if v.is_empty() {
                return bad(&format!("gce.{name} is required"));
            }
        }
        if gce.max_run_secs <= down.retire_margin_secs + down.idle_secs {
            return bad("gce.max_run_secs must exceed retire_margin_secs plus idle_secs");
        }
        if !self.dry_run {
            return bad(
                "dry_run = false is not supported by this build: it only simulates burst decisions",
            );
        }
        Ok(())
    }
}

/// One VM the queen created (or, in dry-run, would have).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BurstVm {
    /// The drone's name in the queen.
    pub(super) drone: String,
    vm: String,
    zone: String,
    pub(super) created_at: i64,
    draining_at: Option<i64>,
    pub(super) ended_at: Option<i64>,
}

/// A burst-prefixed drone as the queen sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedDrone {
    pub(super) name: String,
    /// Heartbeating within `drone_stale_secs`.
    pub(super) alive: bool,
    /// Pending and running leases.
    pub(super) leases: u32,
}

pub(super) struct Observed<'a> {
    pub(super) now: i64,
    pub(super) unmet: &'a [Unmet],
    pub(super) idle: &'a [ClassIdle],
    pub(super) drones: &'a [ObservedDrone],
    pub(super) vms: &'a [BurstVm],
}

/// Timers the rule remembers between passes, and what it decided.
#[derive(Debug, Default)]
pub struct BurstState {
    demand_since: Option<i64>,
    idle_since: BTreeMap<String, i64>,
    recent: VecDeque<Decision>,
    /// Passes by [`Plan::outcome`] since `counting_since`.
    outcomes: BTreeMap<&'static str, u64>,
    counting_since: Option<i64>,
}

impl BurstState {
    pub(super) fn record(&mut self, d: Decision) {
        if self.recent.len() == RECENT_DECISIONS {
            self.recent.pop_front();
        }
        self.recent.push_back(d);
    }

    /// Count one pass's outcome.
    pub(super) fn count(&mut self, now: i64, outcome: &'static str) {
        self.counting_since.get_or_insert(now);
        *self.outcomes.entry(outcome).or_insert(0) += 1;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Decision {
    at: i64,
    dry_run: bool,
    pub(super) action: String,
    pub(super) drone: Option<String>,
    pub(super) detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Create { zone: String },
    Drain { drone: String, why: &'static str },
    Undrain { drone: String },
    Delete { drone: String, why: &'static str },
}

impl Action {
    fn kind(&self) -> &'static str {
        match self {
            Action::Create { .. } => "create",
            Action::Drain { .. } => "drain",
            Action::Undrain { .. } => "undrain",
            Action::Delete { .. } => "delete",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Spend {
    pub(super) day_hours: f64,
    pub(super) month_hours: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DemandView {
    pub(super) weighted_unmet: u32,
    /// The scale-up condition holds right now.
    pub(super) holds: bool,
    since: Option<i64>,
}

/// Burst drones the rule counted this pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct DroneCounts {
    /// Live VMs this queen created (simulated in dry-run).
    pub(super) managed: u32,
    /// Of those, created and not yet enrolled.
    pub(super) provisioning: u32,
    /// Alive drones with the burst prefix that this queen did not create.
    pub(super) unmanaged: u32,
}

#[derive(Debug)]
pub struct Plan {
    pub(super) actions: Vec<Action>,
    demand: DemandView,
    spend: Spend,
    over_budget: bool,
    drones: DroneCounts,
    /// Stable key for why a drone is or is not being created: `no_demand`,
    /// `over_budget`, `provisioning`, `at_max_drones`, `sustaining`,
    /// `no_headroom` or `create`.
    pub(super) outcome: &'static str,
    /// The same, with numbers.
    reason: String,
}

fn live(vm: &BurstVm, max_run: i64, now: i64) -> bool {
    vm.ended_at.is_none() && now < vm.created_at + max_run
}

/// Drone-hours inside `[from, to)`.
fn hours_in(vms: &[BurstVm], max_run: i64, now: i64, from: i64, to: i64) -> f64 {
    let secs: i64 = vms
        .iter()
        .map(|v| {
            let end = v
                .ended_at
                .unwrap_or(now)
                .min(now)
                .min(v.created_at + max_run)
                .min(to);
            (end - v.created_at.max(from)).max(0)
        })
        .sum();
    #[allow(
        clippy::cast_precision_loss,
        reason = "seconds in a month are far below 2^52"
    )]
    let h = secs as f64 / HOUR;
    h
}

/// Start of the UTC day and month containing `now`.
fn windows(now: i64) -> (i64, i64) {
    let day = now - now.rem_euclid(86_400);
    let month = OffsetDateTime::from_unix_timestamp(now)
        .ok()
        .and_then(|t| t.replace_time(Time::MIDNIGHT).replace_day(1).ok())
        .map_or(day, OffsetDateTime::unix_timestamp);
    (day, month)
}

fn spend(vms: &[BurstVm], max_run: i64, now: i64) -> Spend {
    let (day, month) = windows(now);
    Spend {
        day_hours: hours_in(vms, max_run, now, day, i64::MAX),
        month_hours: hours_in(vms, max_run, now, month, i64::MAX),
    }
}

/// Weighted count of unmet jobs that justify a burst drone.
fn weighted_unmet(cfg: &ScaleUpSettings, unmet: &[Unmet], idle: &[ClassIdle]) -> u32 {
    cfg.classes
        .iter()
        .filter(|c| {
            idle.iter()
                .find(|i| i.class == **c)
                .is_none_or(|i| i.idle_slots <= cfg.max_free_slots)
        })
        .map(|c| {
            unmet
                .iter()
                .filter(|u| u.class == *c)
                .map(|u| u.count.saturating_mul(cfg.weights.of(u.scope)))
                .fold(0u32, u32::saturating_add)
        })
        .fold(0u32, u32::saturating_add)
}

/// The zone with the fewest live burst VMs; ties go to the first listed.
fn pick_zone(zones: &[String], live_vms: &[&BurstVm], planned: &[String]) -> String {
    zones
        .iter()
        .min_by_key(|z| {
            live_vms.iter().filter(|v| &v.zone == *z).count()
                + planned.iter().filter(|p| p == z).count()
        })
        .cloned()
        .unwrap_or_default()
}

/// Decide this pass. See the module documentation for the rules.
#[allow(
    clippy::too_many_lines,
    reason = "one linear rule, easier to audit whole"
)]
pub(super) fn plan(cfg: &BurstSettings, st: &mut BurstState, obs: &Observed<'_>) -> Plan {
    let now = obs.now;
    let max_run = cfg.gce.max_run_secs;
    let up = &cfg.scale_up;
    let down = &cfg.scale_down;

    let weighted = weighted_unmet(up, obs.unmet, obs.idle);
    let holds = weighted >= up.min_weighted_unmet;
    if holds {
        st.demand_since.get_or_insert(now);
    } else {
        st.demand_since = None;
    }

    let spent = spend(obs.vms, max_run, now);
    let over_budget = spent.day_hours >= cfg.budget.drone_hours_per_day
        || spent.month_hours >= cfg.budget.drone_hours_per_month
        || cfg
            .budget
            .max_estimated_usd_per_day
            .is_some_and(|cap| spent.day_hours * cfg.hourly_cost_usd >= cap)
        || cfg
            .budget
            .max_estimated_usd_per_month
            .is_some_and(|cap| spent.month_hours * cfg.hourly_cost_usd >= cap);

    let drone = |name: &str| obs.drones.iter().find(|d| d.name == name);
    let live_vms: Vec<&BurstVm> = obs.vms.iter().filter(|v| live(v, max_run, now)).collect();
    st.idle_since
        .retain(|name, _| live_vms.iter().any(|v| &v.drone == name));

    let mut actions = Vec::new();
    let mut provisioning = 0u32;
    for vm in &live_vms {
        let seen = drone(&vm.drone);
        let alive = seen.is_some_and(|d| d.alive);
        let leases = seen.map_or(0, |d| d.leases);
        let age = now - vm.created_at;
        let retiring = age >= max_run - down.retire_margin_secs;
        if !alive && age < down.provision_grace_secs && vm.draining_at.is_none() {
            provisioning += 1;
        }
        if holds || leases > 0 || !alive || vm.draining_at.is_some() {
            st.idle_since.remove(&vm.drone);
        } else {
            st.idle_since.entry(vm.drone.clone()).or_insert(now);
        }
        match vm.draining_at {
            Some(_) if holds && !over_budget && !retiring && alive => {
                actions.push(Action::Undrain {
                    drone: vm.drone.clone(),
                });
            }
            Some(at) if leases == 0 && at < now => actions.push(Action::Delete {
                drone: vm.drone.clone(),
                why: "drained and empty",
            }),
            Some(_) => {}
            None if !alive && age >= down.provision_grace_secs && leases == 0 => {
                actions.push(Action::Delete {
                    drone: vm.drone.clone(),
                    why: "never enrolled or gone silent",
                });
            }
            None if over_budget => actions.push(Action::Drain {
                drone: vm.drone.clone(),
                why: "budget spent",
            }),
            None if retiring => actions.push(Action::Drain {
                drone: vm.drone.clone(),
                why: "near its maximum run time",
            }),
            None if st
                .idle_since
                .get(&vm.drone)
                .is_some_and(|since| now - since >= down.idle_secs) =>
            {
                actions.push(Action::Drain {
                    drone: vm.drone.clone(),
                    why: "idle",
                });
            }
            None => {}
        }
    }

    let unmanaged = obs
        .drones
        .iter()
        .filter(|d| {
            d.alive
                && d.name.starts_with(&cfg.drone_prefix)
                && !obs.vms.iter().any(|v| v.drone == d.name)
        })
        .count();
    let counted = live_vms.len() + unmanaged;
    let room = usize::try_from(cfg.max_drones)
        .unwrap_or(usize::MAX)
        .saturating_sub(counted);

    let (outcome, reason) = if !holds {
        (
            "no_demand",
            format!("weighted unmet {weighted} below {}", up.min_weighted_unmet),
        )
    } else if over_budget {
        ("over_budget", "budget spent".to_owned())
    } else if provisioning > 0 {
        (
            "provisioning",
            format!("{provisioning} drone(s) still provisioning"),
        )
    } else if room == 0 {
        (
            "at_max_drones",
            format!(
                "at max_drones ({}), {unmanaged} made by hand",
                cfg.max_drones
            ),
        )
    } else if let Some(since) = st
        .demand_since
        .filter(|since| now - since < up.sustained_secs)
    {
        (
            "sustaining",
            format!(
                "demand sustained {}s of {}s",
                now - since,
                up.sustained_secs
            ),
        )
    } else {
        let mut zones: Vec<String> = Vec::new();
        let mut n = room.min(usize::try_from(up.step).unwrap_or(usize::MAX));
        // Reserve headroom for every drone that would then be live.
        let (day, month) = (
            cfg.budget.drone_hours_per_day,
            cfg.budget.drone_hours_per_month,
        );
        while n > 0 {
            #[allow(clippy::cast_precision_loss, reason = "a handful of drones")]
            let reserved = (live_vms.len() + n) as f64 * cfg.budget.reserve_hours;
            let estimated_day_usd = (spent.day_hours + reserved) * cfg.hourly_cost_usd;
            let estimated_month_usd = (spent.month_hours + reserved) * cfg.hourly_cost_usd;
            let within_usd = cfg
                .budget
                .max_estimated_usd_per_day
                .is_none_or(|cap| estimated_day_usd <= cap)
                && cfg
                    .budget
                    .max_estimated_usd_per_month
                    .is_none_or(|cap| estimated_month_usd <= cap);
            if spent.day_hours + reserved <= day
                && spent.month_hours + reserved <= month
                && within_usd
            {
                break;
            }
            n -= 1;
        }
        for _ in 0..n {
            let zone = pick_zone(&cfg.gce.zones, &live_vms, &zones);
            zones.push(zone);
        }
        if zones.is_empty() {
            (
                "no_headroom",
                "no budget headroom for another drone".to_owned(),
            )
        } else {
            let made = zones.len();
            actions.extend(zones.into_iter().map(|zone| Action::Create { zone }));
            st.demand_since = Some(now);
            ("create", format!("creating {made}"))
        }
    };

    let small = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
    Plan {
        actions,
        demand: DemandView {
            weighted_unmet: weighted,
            holds,
            since: st.demand_since,
        },
        spend: spent,
        over_budget,
        drones: DroneCounts {
            managed: small(live_vms.len()),
            provisioning,
            unmanaged: small(unmanaged),
        },
        outcome,
        reason,
    }
}

/// Forget VMs that ended (or hit their maximum run time) before the current
/// UTC month: they no longer count toward any budget window.
pub(super) fn prune(vms: &mut Vec<BurstVm>, max_run: i64, now: i64) {
    let (_, month) = windows(now);
    vms.retain(|v| v.ended_at.unwrap_or(i64::MAX).min(v.created_at + max_run) >= month);
}

/// `/status` view of the burst rule after a pass.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BurstView {
    pub(super) dry_run: bool,
    max_drones: u32,
    pub(super) budget: BudgetView,
    /// Drone-hours this UTC day and month (simulated in dry-run).
    pub(super) spend: Spend,
    spend_usd: SpendUsd,
    over_budget: bool,
    pub(super) demand: DemandView,
    pub(super) drones: DroneCounts,
    outcome: &'static str,
    reason: String,
    /// Live VMs this queen created (simulated in dry-run).
    vms: Vec<BurstVm>,
    /// Passes by outcome since `counting_since` (the queen's start).
    pub(super) outcomes: BTreeMap<&'static str, u64>,
    counting_since: Option<i64>,
    /// Newest last.
    recent: Vec<Decision>,
}

/// [`Spend`] in dollars at `hourly_cost_usd`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct SpendUsd {
    day: f64,
    month: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct BudgetView {
    pub(super) drone_hours_per_day: f64,
    pub(super) drone_hours_per_month: f64,
    reserve_hours: f64,
    hourly_cost_usd: f64,
    pub(super) max_estimated_usd_per_day: Option<f64>,
    pub(super) max_estimated_usd_per_month: Option<f64>,
}

/// The view after `p`'s actions were applied to `vms`: drone counts include
/// this pass's creates and exclude its deletes.
pub(super) fn view(
    cfg: &BurstSettings,
    st: &BurstState,
    vms: &[BurstVm],
    p: &Plan,
    now: i64,
) -> BurstView {
    let max_run = cfg.gce.max_run_secs;
    let live_vms: Vec<BurstVm> = vms
        .iter()
        .filter(|v| live(v, max_run, now))
        .cloned()
        .collect();
    let small = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
    let created = small(
        p.actions
            .iter()
            .filter(|a| matches!(a, Action::Create { .. }))
            .count(),
    );
    BurstView {
        dry_run: cfg.dry_run,
        max_drones: cfg.max_drones,
        budget: BudgetView {
            drone_hours_per_day: cfg.budget.drone_hours_per_day,
            drone_hours_per_month: cfg.budget.drone_hours_per_month,
            reserve_hours: cfg.budget.reserve_hours,
            hourly_cost_usd: cfg.hourly_cost_usd,
            max_estimated_usd_per_day: cfg.budget.max_estimated_usd_per_day,
            max_estimated_usd_per_month: cfg.budget.max_estimated_usd_per_month,
        },
        spend: p.spend,
        spend_usd: SpendUsd {
            day: p.spend.day_hours * cfg.hourly_cost_usd,
            month: p.spend.month_hours * cfg.hourly_cost_usd,
        },
        over_budget: p.over_budget,
        demand: p.demand.clone(),
        drones: DroneCounts {
            managed: small(live_vms.len()),
            provisioning: p.drones.provisioning.saturating_add(created),
            unmanaged: p.drones.unmanaged,
        },
        outcome: p.outcome,
        reason: p.reason.clone(),
        vms: live_vms,
        outcomes: st.outcomes.clone(),
        counting_since: st.counting_since,
        recent: st.recent.iter().cloned().collect(),
    }
}

/// `<prefix>q<minute in hex>[-<n>]`, with the first `n` that `taken` does not
/// already hold.
fn new_name(prefix: &str, now: i64, taken: impl Fn(&str) -> bool) -> String {
    let minute = now / 60;
    let mut name = format!("{prefix}q{minute:x}");
    let mut n = 1u32;
    while taken(&name) {
        name = format!("{prefix}q{minute:x}-{n}");
        n += 1;
    }
    name
}

/// Carry `actions` out on a simulated fleet (dry-run). Returns what was done,
/// as decisions to log.
pub(super) fn apply_simulated(
    cfg: &BurstSettings,
    vms: &mut Vec<BurstVm>,
    actions: &[Action],
    now: i64,
) -> Vec<Decision> {
    let mut out = Vec::new();
    for a in actions {
        let (drone, detail) = match a {
            Action::Create { zone } => {
                let drone = new_name(&cfg.drone_prefix, now, |n| vms.iter().any(|v| v.drone == n));
                let vm = drone.replacen(&cfg.drone_prefix, &cfg.gce.vm_prefix, 1);
                vms.push(BurstVm {
                    drone: drone.clone(),
                    vm,
                    zone: zone.clone(),
                    created_at: now,
                    draining_at: None,
                    ended_at: None,
                });
                (Some(drone), format!("zone {zone}"))
            }
            Action::Drain { drone, why } => {
                set(vms, drone, |v| v.draining_at = Some(now));
                (Some(drone.clone()), (*why).to_owned())
            }
            Action::Undrain { drone } => {
                set(vms, drone, |v| v.draining_at = None);
                (Some(drone.clone()), "demand returned".to_owned())
            }
            Action::Delete { drone, why } => {
                set(vms, drone, |v| v.ended_at = Some(now));
                (Some(drone.clone()), (*why).to_owned())
            }
        };
        out.push(Decision {
            at: now,
            dry_run: true,
            action: a.kind().to_owned(),
            drone,
            detail,
        });
    }
    out
}

fn set(vms: &mut [BurstVm], drone: &str, f: impl FnOnce(&mut BurstVm)) {
    if let Some(v) = vms.iter_mut().find(|v| v.drone == drone) {
        f(v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: &str = r#"{
      "enabled": true, "max_drones": 3,
      "budget": {"drone_hours_per_day": 24, "drone_hours_per_month": 150},
      "scale_up": {"classes": ["heavy", "medium"], "weights": {"main": 3, "pr": 1},
                   "min_weighted_unmet": 6, "sustained_secs": 600},
      "scale_down": {"idle_secs": 900},
      "gce": {"project": "p", "network": "borg-burst", "subnet": "borg-burst-usw1",
              "zones": ["us-west1-b", "us-west1-a"], "machine_type": "c2d-highcpu-56"}
    }"#;

    fn cfg() -> BurstSettings {
        serde_json::from_str(BLOCK).unwrap()
    }

    fn unmet(class: Class, scope: Scope, count: u32) -> Unmet {
        Unmet {
            class,
            scope,
            count,
        }
    }

    fn vm(drone: &str, zone: &str, created_at: i64) -> BurstVm {
        BurstVm {
            drone: drone.to_owned(),
            vm: drone.replace("gcp", "borg"),
            zone: zone.to_owned(),
            created_at,
            draining_at: None,
            ended_at: None,
        }
    }

    fn drone(name: &str, alive: bool, leases: u32) -> ObservedDrone {
        ObservedDrone {
            name: name.to_owned(),
            alive,
            leases,
        }
    }

    fn run(
        cfg: &BurstSettings,
        st: &mut BurstState,
        now: i64,
        unmet: &[Unmet],
        drones: &[ObservedDrone],
        vms: &[BurstVm],
    ) -> Plan {
        plan(
            cfg,
            st,
            &Observed {
                now,
                unmet,
                idle: &[],
                drones,
                vms,
            },
        )
    }

    const T0: i64 = 1_800_000_000; // 2027-01-15, mid-day not required

    #[test]
    fn defaults_are_off_and_dry_run_and_validation_rejects_nonsense() {
        let c = cfg();
        assert!(c.dry_run && c.enabled);
        let off: BurstSettings =
            serde_json::from_str(&BLOCK.replace("\"enabled\": true,", "")).unwrap();
        assert!(!off.enabled);
        assert!(c.validate().is_ok());
        for (from, to) in [
            ("\"max_drones\": 3", "\"max_drones\": 0"),
            (
                "\"drone_hours_per_day\": 24",
                "\"drone_hours_per_day\": 200",
            ),
            ("\"min_weighted_unmet\": 6", "\"min_weighted_unmet\": 0"),
            ("\"idle_secs\": 900", "\"idle_secs\": 0"),
            (
                "\"zones\": [\"us-west1-b\", \"us-west1-a\"]",
                "\"zones\": []",
            ),
            (
                "\"enabled\": true,",
                "\"enabled\": true, \"dry_run\": false,",
            ),
            ("\"pr\": 1", "\"pr\": 0, \"extra\": 1"),
            ("\"main\": 3", "\"deploy\": 3, \"main\": 3"),
        ] {
            let parsed = serde_json::from_str::<BurstSettings>(&BLOCK.replace(from, to));
            assert!(
                parsed.is_err() || parsed.unwrap().validate().is_err(),
                "{to}"
            );
        }
    }

    #[test]
    fn provisioning_inputs_remain_typed_accepted_configuration_only() {
        let mut block: serde_json::Value = serde_json::from_str(BLOCK).unwrap();
        let gce = block["gce"].as_object_mut().unwrap();
        gce.extend([
            ("spot".to_owned(), serde_json::json!(false)),
            ("boot_disk_gb".to_owned(), serde_json::json!(250)),
            ("boot_disk_type".to_owned(), serde_json::json!("pd-ssd")),
            (
                "image_family".to_owned(),
                serde_json::json!("custom-family"),
            ),
            (
                "image_project".to_owned(),
                serde_json::json!("custom-project"),
            ),
            (
                "credentials_path".to_owned(),
                serde_json::json!("/nonexistent/borg/credentials.json"),
            ),
        ]);
        let parsed: BurstSettings = serde_json::from_value(block.clone()).unwrap();
        parsed.validate().unwrap();
        assert!(!parsed.gce._spot);
        assert_eq!(parsed.gce._boot_disk_gb, 250);
        assert_eq!(parsed.gce._boot_disk_type, "pd-ssd");
        assert_eq!(parsed.gce._image_family, "custom-family");
        assert_eq!(parsed.gce._image_project, "custom-project");
        assert_eq!(
            parsed.gce._credentials_path,
            Some(PathBuf::from("/nonexistent/borg/credentials.json"))
        );

        let defaults = cfg();
        assert!(defaults.gce._spot);
        assert_eq!(defaults.gce._boot_disk_gb, 1000);
        assert_eq!(defaults.gce._boot_disk_type, "pd-balanced");
        assert_eq!(defaults.gce._image_family, "ubuntu-2204-lts");
        assert_eq!(defaults.gce._image_project, "ubuntu-os-cloud");
        assert_eq!(defaults.gce._credentials_path, None);

        block["gce"]["unknown_provisioning_input"] = serde_json::json!(true);
        let error = serde_json::from_value::<BurstSettings>(block.clone()).unwrap_err();
        assert!(error.to_string().contains("unknown field"), "{error}");
        block["gce"]
            .as_object_mut()
            .unwrap()
            .remove("unknown_provisioning_input");
        block["dry_run"] = serde_json::json!(false);
        let live: BurstSettings = serde_json::from_value(block).unwrap();
        assert!(live.validate().unwrap_err().contains("only simulates"));
    }

    #[test]
    fn creates_only_after_demand_is_sustained_and_restarts_the_clock() {
        let c = cfg();
        let mut st = BurstState::default();
        let heavy = [unmet(Class::Heavy, Scope::Main, 2)]; // weighted 6
        let p = run(&c, &mut st, T0, &heavy, &[], &[]);
        assert!(p.actions.is_empty() && p.demand.holds, "{}", p.reason);
        let p = run(&c, &mut st, T0 + 599, &heavy, &[], &[]);
        assert!(p.actions.is_empty(), "one second short");
        let p = run(&c, &mut st, T0 + 600, &heavy, &[], &[]);
        assert_eq!(
            p.actions,
            [Action::Create {
                zone: "us-west1-b".to_owned()
            }]
        );
        let vms = [vm("gcp-burst-qa", "us-west1-b", T0 + 600)];
        let p = run(&c, &mut st, T0 + 601, &heavy, &[], &vms);
        assert!(p.actions.is_empty(), "{}", p.reason);
    }

    #[test]
    fn pull_request_jobs_weigh_less_than_main_and_deploy_jobs_never_count() {
        let c = cfg();
        let mut st = BurstState::default();
        let five_pr = [unmet(Class::Heavy, Scope::Pr, 5)];
        assert!(!run(&c, &mut st, T0, &five_pr, &[], &[]).demand.holds);
        let six_pr = [unmet(Class::Medium, Scope::Pr, 6)];
        assert!(run(&c, &mut st, T0, &six_pr, &[], &[]).demand.holds);
        let main = [unmet(Class::Heavy, Scope::Main, 2)];
        assert!(run(&c, &mut st, T0, &main, &[], &[]).demand.holds);
        // Burst drones are shared trust: deploy work cannot run on them.
        let deploy = [unmet(Class::Heavy, Scope::Deploy, 50)];
        assert!(!run(&c, &mut st, T0, &deploy, &[], &[]).demand.holds);
        let light = [unmet(Class::Light, Scope::Main, 50)];
        assert!(
            !run(&c, &mut st, T0, &light, &[], &[]).demand.holds,
            "light is not counted"
        );
    }

    #[test]
    fn free_slots_in_a_class_mean_there_is_nothing_to_burst_for() {
        let c = cfg();
        let mut st = BurstState::default();
        let heavy = [unmet(Class::Heavy, Scope::Main, 4)];
        let idle = [ClassIdle {
            class: Class::Heavy,
            idle_slots: 1,
        }];
        let p = plan(
            &c,
            &mut st,
            &Observed {
                now: T0,
                unmet: &heavy,
                idle: &idle,
                drones: &[],
                vms: &[],
            },
        );
        assert!(!p.demand.holds);
        let idle = [ClassIdle {
            class: Class::Heavy,
            idle_slots: 0,
        }];
        let p = plan(
            &c,
            &mut st,
            &Observed {
                now: T0,
                unmet: &heavy,
                idle: &idle,
                drones: &[],
                vms: &[],
            },
        );
        assert!(p.demand.holds);
    }

    #[test]
    fn demand_that_flickers_restarts_the_sustained_window() {
        let c = cfg();
        let mut st = BurstState::default();
        let heavy = [unmet(Class::Heavy, Scope::Main, 2)];
        run(&c, &mut st, T0, &heavy, &[], &[]);
        run(&c, &mut st, T0 + 300, &[], &[], &[]);
        run(&c, &mut st, T0 + 400, &heavy, &[], &[]);
        let p = run(&c, &mut st, T0 + 700, &heavy, &[], &[]);
        assert!(p.actions.is_empty(), "only 300s since demand returned");
    }

    #[test]
    fn max_drones_counts_hand_made_drones_and_provisioning_blocks_more() {
        let c = cfg();
        let heavy = [unmet(Class::Heavy, Scope::Main, 2)];
        let by_hand = [
            drone("gcp-burst-3", true, 2),
            drone("gcp-burst-4", true, 2),
            drone("gcp-burst-5", true, 2),
        ];
        let mut st = BurstState::default();
        run(&c, &mut st, T0, &heavy, &by_hand, &[]);
        let p = run(&c, &mut st, T0 + 1000, &heavy, &by_hand, &[]);
        assert!(
            p.actions.is_empty() && p.reason.contains("max_drones"),
            "{}",
            p.reason
        );

        let mut st = BurstState::default();
        let vms = [vm("gcp-burst-qa", "us-west1-b", T0 - 60)];
        run(&c, &mut st, T0, &heavy, &[], &vms);
        let p = run(&c, &mut st, T0 + 1000, &heavy, &[], &vms);
        assert!(
            p.actions.iter().all(|a| a.kind() != "create"),
            "{}",
            p.reason
        );
    }

    #[test]
    fn zones_are_spread_and_step_limits_creates_per_pass() {
        let mut c = cfg();
        c.scale_up.step = 2;
        let heavy = [unmet(Class::Heavy, Scope::Main, 2)];
        let mut st = BurstState::default();
        run(&c, &mut st, T0, &heavy, &[], &[]);
        let p = run(&c, &mut st, T0 + 600, &heavy, &[], &[]);
        assert_eq!(
            p.actions,
            [
                Action::Create {
                    zone: "us-west1-b".to_owned()
                },
                Action::Create {
                    zone: "us-west1-a".to_owned()
                },
            ]
        );
    }

    #[test]
    fn estimated_usd_budget_is_additional_to_vm_hour_limits() {
        let mut c = cfg();
        c.hourly_cost_usd = 2.0;
        c.budget.max_estimated_usd_per_day = Some(3.0);
        c.budget.max_estimated_usd_per_month = Some(8.0);
        assert!(c.validate().is_ok());

        let heavy = [unmet(Class::Heavy, Scope::Main, 2)];
        let mut st = BurstState::default();
        run(&c, &mut st, T0, &heavy, &[], &[]);
        let p = run(&c, &mut st, T0 + 600, &heavy, &[], &[]);
        assert_eq!(p.outcome, "no_headroom");
        assert!(
            p.actions.is_empty(),
            "two reserved hours cost $4, exceeding $3/day"
        );

        // Even when the hour budget permits work, a reached dollar cap
        // drains an already-running drone without deleting its live lease.
        let running = [vm("gcp-burst-qc", "us-west1-b", T0)];
        let busy = [drone("gcp-burst-qc", true, 1)];
        let mut st = BurstState::default();
        let p = run(&c, &mut st, T0 + 5400, &heavy, &busy, &running);
        assert!(p.over_budget);
        assert_eq!(
            p.actions,
            [Action::Drain {
                drone: "gcp-burst-qc".to_owned(),
                why: "budget spent"
            }]
        );
        let v = view(&c, &st, &running, &p, T0 + 5400);
        assert_eq!(v.budget.max_estimated_usd_per_day, Some(3.0));
        assert_eq!(v.budget.max_estimated_usd_per_month, Some(8.0));
    }

    #[test]
    fn estimated_usd_budget_rejects_partial_nonfinite_or_reversed_limits() {
        let mut c = cfg();
        c.budget.max_estimated_usd_per_day = Some(5.0);
        assert!(c.validate().is_err(), "one limit is not sufficient");
        c.budget.max_estimated_usd_per_month = Some(4.0);
        assert!(c.validate().is_err(), "day cannot exceed month");
        c.budget.max_estimated_usd_per_month = Some(20.0);
        assert!(c.validate().is_ok());
        c.budget.max_estimated_usd_per_day = Some(f64::NAN);
        assert!(c.validate().is_err(), "NaN cannot bypass a limit");
    }

    #[test]
    fn budget_headroom_blocks_creation_and_a_spent_budget_drains_everything() {
        let mut c = cfg();
        c.budget.drone_hours_per_day = 3.0;
        let heavy = [unmet(Class::Heavy, Scope::Main, 2)];
        let day = T0 - T0.rem_euclid(86_400);
        // A drone ran 1.5h today and ended: 1.5 spent + 2.0 reserve > 3.
        let past = [BurstVm {
            ended_at: Some(day + 5400),
            ..vm("gcp-burst-qa", "us-west1-b", day)
        }];
        let mut st = BurstState::default();
        run(&c, &mut st, day + 7200, &heavy, &[], &past);
        let p = run(&c, &mut st, day + 7200 + 600, &heavy, &[], &past);
        assert!(
            p.actions.is_empty() && p.outcome == "no_headroom",
            "{}",
            p.reason
        );
        // Nothing spent: 2.0 reserve <= 3, one drone.
        let mut st = BurstState::default();
        run(&c, &mut st, day + 7200, &heavy, &[], &[]);
        let p = run(&c, &mut st, day + 7200 + 600, &heavy, &[], &[]);
        assert_eq!(p.actions.len(), 1);
        // A live drone needs its reserve too: 0.5 spent + 2 * 2.0 > 4.
        c.budget.drone_hours_per_day = 4.0;
        let live_vm = [vm("gcp-burst-qc", "us-west1-b", day + 7200)];
        let up = [drone("gcp-burst-qc", true, 2)];
        let mut st = BurstState::default();
        run(&c, &mut st, day + 7200 + 1200, &heavy, &up, &live_vm);
        let p = run(&c, &mut st, day + 7200 + 1800, &heavy, &up, &live_vm);
        assert!(
            p.actions.is_empty() && p.outcome == "no_headroom",
            "{}",
            p.reason
        );

        // Spent: a busy drone is drained but not deleted.
        let running = [vm("gcp-burst-qb", "us-west1-b", day)];
        let busy = [drone("gcp-burst-qb", true, 1)];
        let mut st = BurstState::default();
        let p = run(&c, &mut st, day + 5 * 3600, &heavy, &busy, &running);
        assert!(p.over_budget);
        assert_eq!(
            p.actions,
            [Action::Drain {
                drone: "gcp-burst-qb".to_owned(),
                why: "budget spent"
            }]
        );
    }

    #[test]
    fn spend_is_split_by_utc_day_and_month_and_capped_at_the_maximum_run() {
        use time::macros::datetime;
        let feb1 = datetime!(2027-02-01 00:00 UTC).unix_timestamp();
        let jan31_22 = feb1 - 2 * 3600;
        let max_run = 12 * 3600;
        // Ran 2027-01-31 22:00Z to 2027-02-01 02:00Z: 2h in February's first day.
        let v = [BurstVm {
            ended_at: Some(feb1 + 2 * 3600),
            ..vm("gcp-burst-qa", "z", jan31_22)
        }];
        assert_eq!(windows(feb1 + 3 * 3600), (feb1, feb1));
        let s = spend(&v, max_run, feb1 + 3 * 3600);
        assert!((s.day_hours - 2.0).abs() < 1e-9, "{s:?}");
        assert!(
            (s.month_hours - 2.0).abs() < 1e-9,
            "January is not billed to February: {s:?}"
        );
        // Seen from January 31st, all 4h fall in that day and month.
        let s = spend(&v, max_run, feb1 - 60);
        assert!(
            (s.day_hours - (2.0 - 1.0 / 60.0)).abs() < 1e-9,
            "billed to now: {s:?}"
        );
        let jan15 = datetime!(2027-01-15 00:00 UTC).unix_timestamp();
        let s = spend(&v, max_run, jan15);
        assert!(
            s.day_hours.abs() < 1e-9 && s.month_hours.abs() < 1e-9,
            "not started yet: {s:?}"
        );
        // A VM with no end is billed to now, never past created + max_run.
        let open = [vm("gcp-burst-qb", "z", feb1)];
        let s = spend(&open, max_run, feb1 + 20 * 3600);
        assert!((s.day_hours - 12.0).abs() < 1e-9, "{s:?}");
        assert!((s.month_hours - 12.0).abs() < 1e-9, "{s:?}");
    }

    #[test]
    fn prune_keeps_only_vms_that_count_toward_this_month() {
        use time::macros::datetime;
        let feb1 = datetime!(2027-02-01 00:00 UTC).unix_timestamp();
        let max_run = 12 * 3600;
        let mut vms = vec![
            BurstVm {
                ended_at: Some(feb1 - 3600),
                ..vm("gcp-burst-old", "z", feb1 - 7200)
            },
            vm("gcp-burst-expired", "z", feb1 - max_run - 1),
            vm("gcp-burst-straddles", "z", feb1 - 3600),
            BurstVm {
                ended_at: Some(feb1 + 60),
                ..vm("gcp-burst-ended", "z", feb1)
            },
        ];
        prune(&mut vms, max_run, feb1 + 3600);
        let names: Vec<&str> = vms.iter().map(|v| v.drone.as_str()).collect();
        assert_eq!(names, ["gcp-burst-straddles", "gcp-burst-ended"]);
    }

    #[test]
    fn new_names_never_repeat_one_already_taken() {
        let taken = ["gcp-burst-q1", "gcp-burst-q1-1"];
        assert_eq!(
            new_name("gcp-burst-", 60, |n| taken.contains(&n)),
            "gcp-burst-q1-2"
        );
        assert_eq!(
            new_name("gcp-burst-", 120, |n| taken.contains(&n)),
            "gcp-burst-q2"
        );
    }

    #[test]
    fn an_idle_drone_is_drained_then_deleted_on_a_later_pass_and_never_while_busy() {
        let c = cfg();
        let mut vms = vec![vm("gcp-burst-qa", "us-west1-b", T0 - 7200)];
        let idle = [drone("gcp-burst-qa", true, 0)];
        let mut st = BurstState::default();
        assert!(run(&c, &mut st, T0, &[], &idle, &vms).actions.is_empty());
        assert!(
            run(&c, &mut st, T0 + 899, &[], &idle, &vms)
                .actions
                .is_empty()
        );
        let p = run(&c, &mut st, T0 + 900, &[], &idle, &vms);
        assert_eq!(
            p.actions,
            [Action::Drain {
                drone: "gcp-burst-qa".to_owned(),
                why: "idle"
            }]
        );
        apply_simulated(&c, &mut vms, &p.actions, T0 + 900);
        // Same pass time: not deleted yet.
        let p = run(&c, &mut st, T0 + 900, &[], &idle, &vms);
        assert!(p.actions.is_empty());
        // A lease slipped in: it stays.
        let busy = [drone("gcp-burst-qa", true, 1)];
        let p = run(&c, &mut st, T0 + 910, &[], &busy, &vms);
        assert!(p.actions.is_empty(), "never delete a drone with a lease");
        let p = run(&c, &mut st, T0 + 920, &[], &idle, &vms);
        assert_eq!(
            p.actions,
            [Action::Delete {
                drone: "gcp-burst-qa".to_owned(),
                why: "drained and empty"
            }]
        );
    }

    #[test]
    fn a_busy_drone_never_goes_idle_and_demand_resets_the_idle_clock() {
        let c = cfg();
        let vms = [vm("gcp-burst-qa", "us-west1-b", T0 - 7200)];
        let mut st = BurstState::default();
        let busy = [drone("gcp-burst-qa", true, 1)];
        for t in [0, 500, 1000, 5000] {
            assert!(
                run(&c, &mut st, T0 + t, &[], &busy, &vms)
                    .actions
                    .is_empty()
            );
        }
        let idle = [drone("gcp-burst-qa", true, 0)];
        run(&c, &mut st, T0 + 6000, &[], &idle, &vms);
        let heavy = [unmet(Class::Heavy, Scope::Main, 2)];
        run(&c, &mut st, T0 + 6800, &heavy, &idle, &vms);
        let p = run(&c, &mut st, T0 + 7000, &[], &idle, &vms);
        assert!(
            p.actions.is_empty(),
            "idle clock restarted by demand: {:?}",
            p.actions
        );
    }

    #[test]
    fn a_drained_drone_returns_to_service_if_demand_comes_back() {
        let c = cfg();
        let heavy = [unmet(Class::Heavy, Scope::Main, 2)];
        let draining = [BurstVm {
            draining_at: Some(T0 - 30),
            ..vm("gcp-burst-qa", "us-west1-b", T0 - 7200)
        }];
        let idle = [drone("gcp-burst-qa", true, 0)];
        let mut st = BurstState::default();
        let p = run(&c, &mut st, T0, &heavy, &idle, &draining);
        assert_eq!(
            p.actions,
            [Action::Undrain {
                drone: "gcp-burst-qa".to_owned()
            }]
        );
    }

    #[test]
    fn a_drone_near_its_maximum_run_time_is_retired_not_replaced_without_demand() {
        let c = cfg();
        let old = [vm("gcp-burst-qa", "us-west1-b", T0 - (12 * 3600 - 1700))];
        let busy = [drone("gcp-burst-qa", true, 3)];
        let mut st = BurstState::default();
        let p = run(&c, &mut st, T0, &[], &busy, &old);
        assert_eq!(
            p.actions,
            [Action::Drain {
                drone: "gcp-burst-qa".to_owned(),
                why: "near its maximum run time"
            }]
        );
        assert!(p.actions.iter().all(|a| a.kind() != "create"));
    }

    #[test]
    fn a_vm_that_never_enrolled_is_deleted_after_the_grace_period() {
        let c = cfg();
        let vms = [vm("gcp-burst-qa", "us-west1-b", T0 - 1600)];
        let mut st = BurstState::default();
        let p = run(&c, &mut st, T0, &[], &[], &vms);
        assert_eq!(
            p.actions,
            [Action::Delete {
                drone: "gcp-burst-qa".to_owned(),
                why: "never enrolled or gone silent"
            }]
        );
        // The same VM with a lease on record is left for the queen to reap.
        let held = [drone("gcp-burst-qa", false, 1)];
        assert!(run(&c, &mut st, T0, &[], &held, &vms).actions.is_empty());
    }

    #[test]
    fn dry_run_simulates_the_fleet_and_stops_at_the_cap() {
        let c = cfg();
        let heavy = [unmet(Class::Heavy, Scope::Main, 2)];
        let mut st = BurstState::default();
        let mut vms: Vec<BurstVm> = Vec::new();
        let mut creates = 0;
        for step in 0..400 {
            let now = T0 + step * 60;
            let drones: Vec<ObservedDrone> = vms
                .iter()
                .map(|v| ObservedDrone {
                    name: v.drone.clone(),
                    alive: now - v.created_at >= SIMULATED_PROVISION_SECS,
                    leases: 0,
                })
                .collect();
            let p = run(&c, &mut st, now, &heavy, &drones, &vms);
            creates += p.actions.iter().filter(|a| a.kind() == "create").count();
            apply_simulated(&c, &mut vms, &p.actions, now);
        }
        assert_eq!(creates, 3, "the cap holds while demand persists");
        assert_eq!(vms.len(), 3);
        let names: std::collections::BTreeSet<_> = vms.iter().map(|v| &v.drone).collect();
        assert_eq!(names.len(), 3, "names are unique");
    }
}
