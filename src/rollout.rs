//! Quorum rollout of a new drone version, queen side.
//!
//! The queen decides which drones are *told* to run the target version
//! (`desired_version` in the heartbeat response); drones update themselves. A
//! version number alone never promotes anything: each step is gated on what the
//! fleet observed.
//!
//! Stages (configured by [`RolloutSettings`]):
//!
//! 1. **Canary.** One fresh, schedulable drone per canary OS (default one Linux
//!    and one macOS host, chosen by name and then kept) is told the target. The
//!    stage passes when every canary reports the target and has completed
//!    `canary_jobs` successful jobs on it, with all checks agreeing.
//! 2. **Waves.** Then `waves_pct` percent of the eligible fleet at a time
//!    (default 25, 50, 100). A wave passes after `soak_secs` and `wave_jobs`
//!    jobs on the new version, with all checks agreeing. After the last wave the
//!    rollout is complete and every drone, including new ones, is told the
//!    target.
//!
//! Checks, evaluated every pass against a control group (the drones still on the
//! previous version, same period):
//! * **update**: a told drone must report the target within `update_timeout_secs`
//!   of the queen seeing its active leases finish. A told drone still holding
//!   leases is *draining*: it updates as soon as it is idle. A draining canary
//!   holds the canary stage (it must run the target to prove anything), but a
//!   draining wave member does not hold its wave: the members already on the
//!   target supply the wave's evidence. One that is still draining after
//!   `drain_timeout_secs` is *cordoned*: the queen grants it no new leases or
//!   units, so it drains within the length of its running jobs. Running jobs
//!   are never stopped;
//! * **heartbeat health**: drones on the target stay fresh and out of health
//!   quarantine. An operator's maintenance quarantine is not a verdict on the
//!   version: a canary under maintenance is replaced by another host of its OS,
//!   and a wave member under maintenance is skipped (neither waited for nor
//!   judged) until it is released;
//! * **host failures**: the host-failure rate on the target may not exceed the
//!   control's by more than `max_host_failure_increase_pm`; with too little
//!   control data an absolute cap applies;
//! * **success rate**: the job success rate may not fall below the control's by
//!   more than `max_success_drop_pm`;
//! * **independence**: in the canary stage every canary host is also checked on
//!   its own, so one clean host cannot vouch for a failing one.
//!
//! Capacity: a told drone stops taking work until it updates, so a wave tells
//! its drones a few at a time. While a member is still draining or updating, a
//! further drone is told only if the capacity then out of service (members
//! not yet on the target, and drones not accepting work) stays within
//! `max_unavailable_pct` of every class it can run. One drone may always be in
//! flight, so the rollout cannot deadlock. And while the queue of a class has
//! waited `pause_when_queued_secs`, no drone that can run that class is told,
//! for at most `max_pause_secs` of a stage. The canary stage is unchanged.
//!
//! Any failing check **halts** the rollout and **rolls back**: every drone is
//! told the previous version. A halted rollout stays halted until an operator
//! resets it or the target changes. A newer target that only follows the queen's
//! build waits for the rollout in flight, unless that rollout's stage has not
//! moved for `soak_secs + supersede_stalled_after_secs` (see
//! [`settled_target`]). The module is pure: the caller supplies the clock, the
//! fleet view and the job outcomes, and persists the returned state.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::allocator::QueueAge;
use crate::classes::Class;
use crate::health::{Outcome, OutcomeKind};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RolloutSettings {
    /// The version drones run today. Setting it (together with the top-level
    /// `rollout_target_version`) turns on the gated rollout; without it the
    /// target is only reported and every drone is told the target.
    pub previous_version: Option<String>,
    /// With no `rollout_target_version`, roll the drones out to the build the
    /// queen itself runs, so a queen self-update carries the fleet with it
    /// through the same gate. The previous version is then the one the fleet
    /// was on when that rollout started.
    pub(crate) follow_queen: bool,
    /// OSes that each need a canary host, as reported by the drone probe.
    pub canary_oses: Vec<String>,
    /// Successful jobs each canary must complete on the target.
    pub canary_jobs: u32,
    /// Cumulative share of eligible drones per wave; ascending, ends at 100.
    pub waves_pct: Vec<u32>,
    /// Jobs the new-version drones must complete in a wave.
    pub wave_jobs: u32,
    /// Minimum duration of a wave.
    pub soak_secs: u64,
    /// How long a told drone has to report the target after its active leases drain.
    pub update_timeout_secs: u64,
    /// How long a told drone may keep holding leases before the queen cordons
    /// it (grants it nothing new) so that it drains. Running jobs finish.
    pub drain_timeout_secs: u64,
    /// A stage that has not moved for `soak_secs` plus this long is stalled: a
    /// newer queen build then supersedes the rollout in flight, starting a
    /// fresh canary. 15 minutes since 2026-11-27 (was an hour): a rollout of
    /// a queen-only change sat paused behind the light backlog while the
    /// drone-side slot and git fixes waited for it and one more rollout.
    pub supersede_stalled_after_secs: u64,
    pub max_host_failure_increase_pm: u32,
    pub max_success_drop_pm: u32,
    /// Host-failure cap (permille) when the control group is too small.
    pub absolute_host_failure_pm: u32,
    /// Control-group jobs needed to compare against.
    pub min_baseline_jobs: u32,
    /// Share of each class's capacity (slots of fresh, schedulable drones)
    /// that may be out of service while a wave tells another drone.
    pub max_unavailable_pct: u32,
    /// A queue of a class that has waited this long holds back telling any
    /// drone that can run the class. `null` turns the pause off.
    pub pause_when_queued_secs: Option<u64>,
    /// How long into a stage queues may hold telling drones back.
    pub max_pause_secs: u64,
}

impl Default for RolloutSettings {
    fn default() -> Self {
        Self {
            previous_version: None,
            follow_queen: false,
            canary_oses: vec!["linux".into(), "macos".into()],
            canary_jobs: 5,
            waves_pct: vec![25, 50, 100],
            wave_jobs: 10,
            soak_secs: 1800,
            update_timeout_secs: 900,
            drain_timeout_secs: 900,
            supersede_stalled_after_secs: 900,
            max_host_failure_increase_pm: 20,
            max_success_drop_pm: 50,
            absolute_host_failure_pm: 100,
            min_baseline_jobs: 5,
            max_unavailable_pct: 25,
            pause_when_queued_secs: Some(600),
            max_pause_secs: 3600,
        }
    }
}

impl RolloutSettings {
    pub fn validate(&self) -> Result<(), String> {
        if self.canary_oses.is_empty() || self.canary_jobs == 0 {
            return Err(
                "rollout canary_oses and canary_jobs must be non-empty and positive".into(),
            );
        }
        let ascending = self.waves_pct.windows(2).all(|w| w[0] < w[1]);
        if self.waves_pct.is_empty()
            || !ascending
            || self.waves_pct.last() != Some(&100)
            || self.waves_pct.contains(&0)
        {
            return Err("rollout waves_pct must be ascending percentages ending at 100".into());
        }
        if self.max_unavailable_pct > 100 {
            return Err("rollout max_unavailable_pct must be at most 100".into());
        }
        if self.update_timeout_secs == 0 || self.wave_jobs == 0 || self.drain_timeout_secs == 0 {
            return Err(
                "rollout update_timeout_secs, drain_timeout_secs and wave_jobs must be positive"
                    .into(),
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Canary,
    Wave,
    Complete,
    Halted,
}

impl Phase {
    pub const ALL: [Self; 4] = [Self::Canary, Self::Wave, Self::Complete, Self::Halted];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Canary => "canary",
            Self::Wave => "wave",
            Self::Complete => "complete",
            Self::Halted => "halted",
        }
    }
}

/// Whether a drone reporting `reported` runs `target`. Drones report SemVer
/// build metadata (`0.1.0+<binary hash>`); a target without it names a
/// release and matches every build of it, a target with it names one build.
pub(crate) fn same_release(reported: &str, target: &str) -> bool {
    if target.contains('+') {
        return reported == target;
    }
    reported.split('+').next() == Some(target)
}

/// The version a rollout to `target` rolls back to when nothing configures
/// one. An unchanged target keeps the previous it started with; a new target
/// starts from where the last rollout left the fleet (its target if it
/// completed, its previous otherwise); with no history, the version most
/// fresh drones run, other than `target`. `None` when nothing is known.
pub(crate) fn previous_for(
    target: &str,
    stored: Option<&RolloutState>,
    drones: &[DroneView],
) -> Option<String> {
    match stored {
        Some(s) if s.target == target => return Some(s.previous.clone()),
        Some(s) if s.phase == Phase::Complete => return Some(s.target.clone()),
        Some(s) => return Some(s.previous.clone()),
        None => {}
    }
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for d in drones.iter().filter(|d| d.fresh) {
        if let Some(v) = d.version.as_deref().filter(|v| !same_release(v, target)) {
            *counts.entry(v).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(v, _)| v.to_owned())
}

/// The target a pass works toward when the target follows the queen's own
/// build. Every borg merge restarts the queen on a new build, and starting a
/// fresh rollout on each one restarted the canary stage before it could
/// finish. So a rollout in progress keeps its target until it completes or
/// halts, including every wave's observation period. The next pass then
/// adopts the current queen build.
///
/// A rollout whose current stage has not moved for `soak_secs` plus
/// `supersede_stalled_after_secs` is stalled, and the newer build supersedes
/// it: holding every later fix behind a stage that cannot finish helps no one.
/// The new rollout starts from the stalled one's previous version, the last
/// one the whole fleet ran, with a fresh canary.
pub(crate) fn settled_target(
    target: String,
    stored: Option<&RolloutState>,
    now: i64,
    s: &RolloutSettings,
) -> String {
    match stored {
        Some(st) if matches!(st.phase, Phase::Canary | Phase::Wave) && !stalled(st, now, s) => {
            st.target.clone()
        }
        _ => target,
    }
}

fn secs(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// Whether the rollout's current stage has gone without moving for longer
/// than its soak plus `supersede_stalled_after_secs`.
fn stalled(st: &RolloutState, now: i64, s: &RolloutSettings) -> bool {
    now.saturating_sub(st.stage_started_at)
        > secs(s.soak_secs).saturating_add(secs(s.supersede_stalled_after_secs))
}

/// Whether the queen must grant `name` nothing new: a told member of a rollout
/// in flight that kept its leases past `drain_timeout_secs`.
pub fn cordoned(state: &RolloutState, name: &str) -> bool {
    matches!(state.phase, Phase::Canary | Phase::Wave) && state.cordoned.iter().any(|c| c == name)
}

/// The persisted state of one rollout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutState {
    pub target: String,
    pub previous: String,
    pub phase: Phase,
    /// Canary hosts chosen so far, in `canary_oses` order.
    pub canaries: Vec<String>,
    /// Drones told to run the target (grows monotonically).
    pub members: Vec<String>,
    /// Index into `waves_pct` while in `Wave`.
    pub wave: usize,
    pub started_at: i64,
    pub stage_started_at: i64,
    /// When the queen first saw each updating drone without active leases.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub update_started_at: BTreeMap<String, i64>,
    /// When the queen first saw each told member, not yet on the target,
    /// holding active leases.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub drain_started_at: BTreeMap<String, i64>,
    /// Told members still draining after `drain_timeout_secs`: granted no new
    /// leases or units until they drain. Running jobs are never stopped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cordoned: Vec<String>,
    pub halted_reason: Option<String>,
}

/// The fleet as the rollout sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroneView {
    pub name: String,
    pub os: String,
    pub fresh: bool,
    /// The queen could not have heard this drone's heartbeats lately: it has
    /// not listened long enough for a drone retrying a down queen to reach it,
    /// or it hears no drone at all. A silence then says nothing about the
    /// version, so it never halts a rollout.
    pub(crate) queen_blind: bool,
    /// Quarantined or otherwise not schedulable.
    pub held: bool,
    /// Held by an operator (maintenance), not by health evidence. Such a hold
    /// says nothing about the version the drone runs.
    pub(crate) maintenance: bool,
    /// Work already granted to this drone, excluding unused standby runners.
    pub(crate) has_active_leases: bool,
    pub version: Option<String>,
    /// When the queen first saw the drone report `version`.
    pub version_since: Option<i64>,
    /// Runs shared CI work ([`crate::classes::Trust::Shared`]). Canaries are
    /// chosen among these first: a deploy-only drone runs few jobs, so it
    /// proves little and stalls the canary stage.
    pub(crate) shared: bool,
    /// Takes leases now. A canary must run jobs to prove anything, so a
    /// reclaim-only drone (dev-desktop) or one that stopped accepting is
    /// never picked; it still updates with the waves.
    pub(crate) runs_jobs: bool,
    /// Placement admits work on it now: it runs jobs and is not in a
    /// suspect cool-down or otherwise refused by the allocator. Capacity of a
    /// drone out of service is already lost before the rollout tells it.
    pub(crate) in_service: bool,
    /// Leases of each class the drone holds when empty; capacity a told drone
    /// takes out of service until it updates.
    pub(crate) slots: BTreeMap<Class, u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct Observed {
    pub jobs: u32,
    pub ok: u32,
    pub host_failures: u32,
}

impl Observed {
    fn rate(part: u32, whole: u32) -> u32 {
        if whole == 0 {
            0
        } else {
            u32::try_from(u64::from(part) * 1000 / u64::from(whole)).unwrap_or(1000)
        }
    }

    fn host_pm(&self) -> u32 {
        Self::rate(self.host_failures, self.jobs)
    }

    fn success_pm(&self) -> u32 {
        Self::rate(self.ok, self.jobs)
    }
}

/// The result of one pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advance {
    pub state: RolloutState,
    /// Audit-trail lines for transitions made in this pass.
    pub events: Vec<String>,
    pub checks: Vec<Check>,
    /// Why the rollout is not moving yet.
    pub waiting: Option<String>,
    pub new_group: Observed,
    pub control_group: Observed,
}

/// The version `name` should run under `state`.
pub fn desired_for<'a>(state: &'a RolloutState, name: &str) -> &'a str {
    match state.phase {
        Phase::Complete => &state.target,
        Phase::Halted => &state.previous,
        Phase::Canary | Phase::Wave => {
            if state.members.iter().any(|m| m == name) {
                &state.target
            } else {
                &state.previous
            }
        }
    }
}

/// Rebuild the drain clocks and cordons of `st.members`: a told member that is
/// fresh, not in maintenance, not yet on `target` and holding leases is
/// draining since the queen first saw it so; past `drain_timeout` it is
/// cordoned. A member that drained, updated or went silent drops out.
fn track_drains(
    st: &mut RolloutState,
    drones: &[DroneView],
    target: &str,
    now: i64,
    drain_timeout: i64,
) {
    let before = std::mem::take(&mut st.drain_started_at);
    st.cordoned.clear();
    for n in &st.members {
        let draining = drones.iter().find(|d| d.name == *n).is_some_and(|d| {
            d.fresh
                && !d.maintenance
                && d.has_active_leases
                && !d
                    .version
                    .as_deref()
                    .is_some_and(|v| same_release(v, target))
        });
        if !draining {
            continue;
        }
        let since = before.get(n).copied().unwrap_or(now);
        st.drain_started_at.insert(n.clone(), since);
        if now.saturating_sub(since) > drain_timeout {
            st.cordoned.push(n.clone());
        }
    }
}

/// Per class: the slots of fresh, schedulable drones, and of those the ones
/// out of service (members not yet on `target`, or not accepting work); and
/// whether any member is still on its way to the target.
fn out_of_service(
    st: &RolloutState,
    drones: &[DroneView],
    target: &str,
) -> (BTreeMap<Class, u64>, BTreeMap<Class, u64>, bool) {
    let (mut total, mut lost) = (BTreeMap::new(), BTreeMap::new());
    let mut in_flight = false;
    for d in drones.iter().filter(|d| d.fresh && !d.held) {
        let updating = st.members.contains(&d.name)
            && !d.maintenance
            && !d
                .version
                .as_deref()
                .is_some_and(|v| same_release(v, target));
        in_flight |= updating;
        for (&class, &n) in &d.slots {
            *total.entry(class).or_default() += u64::from(n);
            if updating || !d.in_service {
                *lost.entry(class).or_default() += u64::from(n);
            }
        }
    }
    (total, lost, in_flight)
}

/// Tell drones the target, by name, until the wave has `want` members, as far
/// as capacity allows (see the module docs). Returns why it stopped short.
fn tell_within_capacity(
    st: &mut RolloutState,
    s: &RolloutSettings,
    drones: &[DroneView],
    target: &str,
    now: i64,
    queued: &[QueueAge],
    want: usize,
) -> Option<String> {
    let paused: BTreeSet<Class> = match s.pause_when_queued_secs {
        Some(after) if now.saturating_sub(st.stage_started_at) < secs(s.max_pause_secs) => queued
            .iter()
            .filter(|q| q.oldest_age_secs >= after)
            .map(|q| q.class)
            .collect(),
        _ => BTreeSet::new(),
    };
    let mut candidates: Vec<&DroneView> = drones
        .iter()
        .filter(|d| d.fresh && !d.held && !st.members.contains(&d.name))
        .collect();
    candidates.sort_by(|a, b| a.name.cmp(&b.name));
    let mut why = None;
    for d in candidates {
        if st.members.len() >= want {
            break;
        }
        let on_target = d
            .version
            .as_deref()
            .is_some_and(|v| same_release(v, target));
        // A drone already on the target takes nothing out of service.
        if !on_target {
            let runs = |c: &Class| d.slots.get(c).is_some_and(|&n| n > 0);
            if let Some(c) = paused.iter().find(|c| runs(c)) {
                why.get_or_insert(format!(
                    "{} waits: {} jobs have queued for {}s or more",
                    d.name,
                    c.as_str(),
                    s.pause_when_queued_secs.unwrap_or_default()
                ));
                continue;
            }
            let (total, lost, in_flight) = out_of_service(st, drones, target);
            let over = in_flight
                .then(|| {
                    // A drone already out of service is already in `lost`;
                    // telling it takes nothing more out.
                    let adds = |n: u32| if d.in_service { u64::from(n) } else { 0 };
                    d.slots.iter().find(|&(c, &n)| {
                        let all = total.get(c).copied().unwrap_or(0);
                        let out = lost.get(c).copied().unwrap_or(0) + adds(n);
                        adds(n) > 0 && out * 100 > all * u64::from(s.max_unavailable_pct)
                    })
                })
                .flatten();
            if let Some((c, _)) = over {
                why.get_or_insert(format!(
                    "{} waits: telling it now would take more than {}% of {} capacity out of service",
                    d.name,
                    s.max_unavailable_pct,
                    c.as_str()
                ));
                continue;
            }
        }
        st.members.push(d.name.clone());
    }
    why
}

/// Drones a wave of `pct` percent tells: that share of the drones that can
/// update now (fresh and schedulable), rounded up.
fn wave_want(drones: &[DroneView], pct: u32) -> usize {
    let eligible = drones.iter().filter(|d| d.fresh && !d.held).count();
    (eligible * usize::try_from(pct).unwrap_or(100)).div_ceil(100)
}

fn observe<'a>(outcomes: &'a [Outcome], pick: impl Fn(&Outcome) -> bool + 'a) -> Observed {
    let mut o = Observed::default();
    for x in outcomes.iter().filter(|x| pick(x)) {
        match x.kind {
            OutcomeKind::Ok => {
                o.jobs += 1;
                o.ok += 1;
            }
            OutcomeKind::HostFailure => {
                o.jobs += 1;
                o.host_failures += 1;
            }
            OutcomeKind::Other => o.jobs += 1,
        }
    }
    o
}

fn compare(s: &RolloutSettings, label: &str, new: &Observed, control: &Observed) -> Vec<Check> {
    let have_control = control.jobs >= s.min_baseline_jobs;
    let host_limit = if have_control {
        control.host_pm() + s.max_host_failure_increase_pm
    } else {
        s.absolute_host_failure_pm
    };
    let mut checks = vec![Check {
        name: "host_failures",
        ok: new.host_pm() <= host_limit,
        detail: format!(
            "{label}: {} host failures in {} jobs ({} permille, limit {host_limit}{})",
            new.host_failures,
            new.jobs,
            new.host_pm(),
            if have_control {
                ", vs control"
            } else {
                ", absolute"
            }
        ),
    }];
    // One job's own test failure is not a success rate: the new version is
    // judged on as many jobs as the control needs. Host failures above still
    // count from the first job.
    if have_control && new.jobs >= s.min_baseline_jobs {
        let floor = control.success_pm().saturating_sub(s.max_success_drop_pm);
        checks.push(Check {
            name: "success_rate",
            ok: new.success_pm() >= floor,
            detail: format!(
                "{label}: success {} permille, floor {floor} (control {})",
                new.success_pm(),
                control.success_pm()
            ),
        });
    }
    checks
}

/// Advance the rollout by one pass. `state` is the persisted state (or `None`
/// before the first pass for this target).
#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    reason = "one linear state machine over the fleet, its jobs and its queues; splitting it would scatter the stage rules"
)]
pub fn advance(
    now: i64,
    s: &RolloutSettings,
    previous: &str,
    target: &str,
    state: Option<RolloutState>,
    drones: &[DroneView],
    outcomes: &[Outcome],
    queued: &[QueueAge],
) -> Advance {
    let mut events = Vec::new();
    let mut st = match state {
        Some(st) if st.target == target => st,
        old => {
            if let Some(o) = old
                .as_ref()
                .filter(|o| matches!(o.phase, Phase::Canary | Phase::Wave))
            {
                events.push(format!(
                    "rollout of {} superseded by {target} in its {} stage (unchanged for {}s)",
                    o.target,
                    o.phase.as_str(),
                    now.saturating_sub(o.stage_started_at)
                ));
            }
            events.push(format!("rollout of {target} started (from {previous})"));
            RolloutState {
                target: target.to_owned(),
                previous: previous.to_owned(),
                phase: Phase::Canary,
                canaries: Vec::new(),
                members: Vec::new(),
                wave: 0,
                started_at: now,
                stage_started_at: now,
                update_started_at: BTreeMap::new(),
                drain_started_at: BTreeMap::new(),
                cordoned: Vec::new(),
                halted_reason: None,
            }
        }
    };
    let done = |st: RolloutState, events: Vec<String>, waiting: Option<String>| Advance {
        state: st,
        events,
        checks: Vec::new(),
        waiting,
        new_group: Observed::default(),
        control_group: Observed::default(),
    };
    if matches!(st.phase, Phase::Complete | Phase::Halted) {
        st.drain_started_at.clear();
        st.cordoned.clear();
        return done(st, events, None);
    }

    // A canary an operator took out for maintenance proves nothing either way:
    // replace it with another fresh, schedulable host of its OS, in place, so
    // the canaries stay in `canary_oses` order. With none free it is kept and
    // the stage waits (see the member checks).
    if st.phase == Phase::Canary {
        for i in 0..st.canaries.len() {
            let name = st.canaries[i].clone();
            let Some(old) = drones.iter().find(|d| d.name == name && d.maintenance) else {
                continue;
            };
            let replacement = drones
                .iter()
                .filter(|d| {
                    d.fresh
                        && !d.held
                        && d.runs_jobs
                        && d.os == old.os
                        && !st.canaries.contains(&d.name)
                })
                .min_by_key(|d| (!d.shared, d.name.as_str()))
                .map(|d| d.name.clone());
            if let Some(new) = replacement {
                events.push(format!(
                    "canary {name} is in maintenance; canary for {}: {new}",
                    old.os
                ));
                st.update_started_at.remove(&name);
                st.canaries[i] = new;
                st.members = st.canaries.clone();
                st.stage_started_at = now;
            }
        }
    }

    // Canary selection: one fresh, schedulable host per OS, kept once chosen.
    if st.phase == Phase::Canary && st.canaries.len() < s.canary_oses.len() {
        let chosen: BTreeSet<String> = st.canaries.iter().cloned().collect();
        let mut picked = st.canaries.clone();
        for os in s.canary_oses.iter().skip(st.canaries.len()) {
            let pick = drones
                .iter()
                .filter(|d| {
                    d.fresh && !d.held && d.runs_jobs && d.os == *os && !chosen.contains(&d.name)
                })
                .min_by_key(|d| (!d.shared, d.name.as_str()))
                .map(|d| d.name.clone());
            match pick {
                Some(name) => {
                    events.push(format!("canary for {os}: {name}"));
                    picked.push(name);
                }
                None => {
                    st.canaries = picked;
                    st.members = st.canaries.clone();
                    // The canaries already chosen keep their drain clock and
                    // are cordoned on time, so they are ready when the
                    // missing host appears.
                    track_drains(&mut st, drones, target, now, secs(s.drain_timeout_secs));
                    let why = format!("waiting for a fresh {os} host to use as canary");
                    return done(st, events, Some(why));
                }
            }
        }
        st.canaries = picked;
        st.members = st.canaries.clone();
        st.stage_started_at = now;
    }

    track_drains(&mut st, drones, target, now, secs(s.drain_timeout_secs));
    let by_name = |n: &str| drones.iter().find(|d| d.name == n);
    let members: BTreeSet<&str> = st.members.iter().map(String::as_str).collect();
    let on_target = |d: &DroneView| {
        d.version
            .as_deref()
            .is_some_and(|v| same_release(v, target))
    };
    // Since when `n` has run the target. A member that was told but still
    // runs another version has run it never: its jobs on the old build are
    // not evidence about the new one (a deploy drone told mid-drain once
    // halted a rollout with failures from hours before it was told).
    let since_of = |n: &str| {
        by_name(n)
            .filter(|&d| on_target(d))
            .and_then(|d| d.version_since)
            .unwrap_or(i64::MAX)
    };
    let timeout = secs(s.update_timeout_secs);
    let overdue = now.saturating_sub(st.stage_started_at) > timeout;

    // Observations: members on the target, since they got it (and since the
    // stage began for wave gating); control = everyone not told the target.
    let stage_start = st.stage_started_at;
    let new_group = observe(outcomes, |o| {
        members.contains(o.drone.as_str()) && o.finished_at >= since_of(&o.drone)
    });
    let control_group = observe(outcomes, |o| {
        !members.contains(o.drone.as_str()) && o.finished_at >= stage_start
    });
    let stage_jobs = observe(outcomes, |o| {
        members.contains(o.drone.as_str()) && o.finished_at >= since_of(&o.drone).max(stage_start)
    });

    let mut checks = Vec::new();
    let mut halt: Option<String> = None;
    let mut wait: Option<String> = None;
    // Rebuilt every pass, so a member that drained, updated or went silent
    // drops out of both.
    let mut draining = Vec::new();

    for n in &st.members {
        let Some(d) = by_name(n) else {
            continue;
        };
        if d.maintenance {
            // Not evidence about the version. A wave member is skipped until
            // released; a canary that could not be replaced holds the stage.
            if st.phase == Phase::Canary {
                wait.get_or_insert(format!(
                    "canary {n} is in maintenance and no other {} host is free to replace it",
                    d.os
                ));
            }
            continue;
        }
        if !on_target(d) {
            // A drone that went silent before reporting the target never ran
            // it: a burst drone retired mid-rollout is not evidence against
            // the new version.
            if !d.fresh {
                continue;
            }
            if d.has_active_leases {
                st.update_started_at.remove(n);
                let cordon = st.cordoned.contains(n);
                let label = if cordon {
                    format!("{n} (cordoned)")
                } else {
                    n.clone()
                };
                if st.phase == Phase::Canary {
                    // A canary proves nothing until it runs the target.
                    wait.get_or_insert(format!("waiting for {label} to drain its active leases"));
                } else {
                    // The members already on the target supply the wave's
                    // evidence; this one updates once it is idle.
                    draining.push(label);
                }
                continue;
            }
            let started_at = *st.update_started_at.entry(n.clone()).or_insert(now);
            if now.saturating_sub(started_at) > timeout {
                halt.get_or_insert(format!(
                    "{n} did not report {target} within {}s after draining its active leases",
                    s.update_timeout_secs
                ));
            } else {
                wait.get_or_insert(format!("waiting for {n} to update to {target}"));
            }
            continue;
        }
        // Maintenance holds were skipped above: this is a health quarantine.
        if d.held {
            halt.get_or_insert(format!("{n} is quarantined on {target}"));
        } else if !d.fresh {
            if overdue && !d.queen_blind {
                halt.get_or_insert(format!("{n} stopped heartbeating on {target}"));
            } else {
                wait.get_or_insert(format!("waiting for {n} to heartbeat on {target}"));
            }
        }
    }
    checks.push(Check {
        name: "update_and_heartbeat",
        ok: halt.is_none(),
        detail: halt.clone().or_else(|| wait.clone()).unwrap_or_else(|| {
            if draining.is_empty() {
                "all told drones run the target and heartbeat".into()
            } else {
                format!(
                    "told drones on the target heartbeat; still draining, updating when idle: {}",
                    draining.join(", ")
                )
            }
        }),
    });

    checks.extend(compare(s, "new version", &new_group, &control_group));
    if st.phase == Phase::Canary {
        // Independence: each canary host must pass on its own.
        for n in &st.canaries {
            let own = observe(outcomes, |o| o.drone == *n && o.finished_at >= since_of(n));
            let own_checks = compare(s, n, &own, &control_group);
            for c in own_checks {
                if !c.ok && own.jobs >= 3 {
                    let others_ok = st.canaries.len() > 1;
                    halt.get_or_insert(format!(
                        "canary {n} failed {}{}: {}",
                        c.name,
                        if others_ok {
                            " while another canary did not"
                        } else {
                            ""
                        },
                        c.detail
                    ));
                }
                checks.push(c);
            }
        }
    }
    if let Some(bad) = checks.iter().find(|c| !c.ok) {
        halt.get_or_insert(format!("{}: {}", bad.name, bad.detail));
    }

    if let Some(reason) = halt {
        st.phase = Phase::Halted;
        st.halted_reason = Some(reason.clone());
        st.drain_started_at.clear();
        st.cordoned.clear();
        events.push(format!(
            "rollout of {target} HALTED, rolled back to {}: {reason}",
            st.previous
        ));
        return Advance {
            state: st,
            events,
            checks,
            waiting: None,
            new_group,
            control_group,
        };
    }

    // Promotion.
    let promote = match st.phase {
        Phase::Canary => {
            let enough = st.canaries.iter().all(|n| {
                observe(outcomes, |o| o.drone == *n && o.finished_at >= since_of(n)).ok
                    >= s.canary_jobs
            });
            let all_on = wait.is_none();
            if !enough {
                wait.get_or_insert(format!(
                    "observing canaries: need {} successful jobs on each",
                    s.canary_jobs
                ));
            }
            enough && all_on
        }
        Phase::Wave => {
            let soaked = now.saturating_sub(st.stage_started_at)
                >= i64::try_from(s.soak_secs).unwrap_or(i64::MAX);
            let enough = stage_jobs.jobs >= s.wave_jobs;
            // The wave tells its drones as capacity allows; it is not done
            // until all of them are told.
            let want = wave_want(drones, s.waves_pct[st.wave]);
            let before = st.members.len();
            let short = tell_within_capacity(&mut st, s, drones, target, now, queued, want);
            // This pass's update and heartbeat checks ran before these drones
            // were told, so the wave waits a pass to check them too.
            let added = st.members.len() > before;
            let told = st.members.len() >= want;
            if wait.is_none() && !told {
                wait = Some(short.unwrap_or_else(|| {
                    format!(
                        "wave {}: {} of {want} drones told",
                        st.wave + 1,
                        st.members.len()
                    )
                }));
            } else if wait.is_none() && added {
                wait = Some(format!(
                    "wave {}: {} drone(s) just told; checking them first",
                    st.wave + 1,
                    st.members.len() - before
                ));
            } else if wait.is_none() && !soaked {
                wait = Some(format!("soaking wave {} for {}s", st.wave + 1, s.soak_secs));
            } else if wait.is_none() && !enough {
                wait = Some(format!(
                    "wave {}: {} of {} jobs observed",
                    st.wave + 1,
                    stage_jobs.jobs,
                    s.wave_jobs
                ));
            }
            told && !added && soaked && enough && wait.is_none()
        }
        _ => false,
    };
    if promote {
        let next = if st.phase == Phase::Canary {
            0
        } else {
            st.wave + 1
        };
        if next >= s.waves_pct.len() {
            st.phase = Phase::Complete;
            st.drain_started_at.clear();
            st.cordoned.clear();
            events.push(format!("rollout of {target} complete"));
        } else {
            // Only drones that can update now: a silent one would only stall
            // the wave until it timed out. The rest of the wave is told on
            // later passes as capacity allows.
            let pct = s.waves_pct[next];
            st.phase = Phase::Wave;
            st.wave = next;
            st.stage_started_at = now;
            wait = tell_within_capacity(
                &mut st,
                s,
                drones,
                target,
                now,
                queued,
                wave_want(drones, pct),
            );
            events.push(format!(
                "rollout of {target}: wave {} ({pct}%), {} drone(s) told",
                next + 1,
                st.members.len()
            ));
        }
        if st.phase == Phase::Complete {
            wait = None;
        }
    }
    Advance {
        state: st,
        events,
        checks,
        waiting: wait,
        new_group,
        control_group,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn build_metadata_is_ignored_unless_the_target_names_a_build() {
        use super::same_release;
        assert!(same_release("0.1.0+abcdef123456", "0.1.0"));
        assert!(same_release("0.1.0", "0.1.0"));
        assert!(!same_release("0.2.0+abcdef123456", "0.1.0"));
        assert!(same_release("0.1.0+abcdef123456", "0.1.0+abcdef123456"));
        assert!(!same_release("0.1.0+000000000000", "0.1.0+abcdef123456"));
        assert!(!same_release("0.1.0", "0.1.0+abcdef123456"));
    }

    use super::*;

    const NEW: &str = "2.0.0";
    const OLD: &str = "1.0.0";

    fn settings() -> RolloutSettings {
        RolloutSettings {
            previous_version: Some(OLD.into()),
            canary_jobs: 3,
            wave_jobs: 4,
            soak_secs: 100,
            update_timeout_secs: 60,
            min_baseline_jobs: 4,
            ..RolloutSettings::default()
        }
    }

    #[test]
    fn a_drone_that_takes_no_leases_is_never_the_canary() {
        let s = settings();
        let fleet = [
            DroneView {
                runs_jobs: false,
                ..drone("a-dev-desktop", "linux", "0.1.0+old", 0)
            },
            drone("b", "linux", "0.1.0+old", 0),
        ];
        let a = advance(10, &s, "0.1.0+old", "0.1.0+new", None, &fleet, &[], &[]);
        assert_eq!(a.state.canaries, ["b"]);
        // With only a reclaim-only drone there is no canary yet.
        let a = advance(
            10,
            &s,
            "0.1.0+old",
            "0.1.0+new",
            None,
            &fleet[..1],
            &[],
            &[],
        );
        assert!(a.state.canaries.is_empty());
        assert!(a.waiting.is_some());
    }

    fn drone(name: &str, os: &str, version: &str, since: i64) -> DroneView {
        DroneView {
            shared: true,
            runs_jobs: true,
            name: name.into(),
            os: os.into(),
            fresh: true,
            queen_blind: false,
            held: false,
            maintenance: false,
            has_active_leases: false,
            version: Some(version.into()),
            version_since: Some(since),
            slots: BTreeMap::new(),
            in_service: true,
        }
    }

    fn job(drone: &str, at: i64, kind: OutcomeKind) -> Outcome {
        Outcome {
            drone: drone.into(),
            lease_id: format!("{drone}-{at}"),
            repo: "o/r".into(),
            run_id: u64::try_from(at).unwrap_or(0),
            job_name: "j".into(),
            finished_at: at,
            kind,
        }
    }

    /// Fleet: linux l1..l3, macos m1, m2, all on OLD.
    fn fleet() -> Vec<DroneView> {
        vec![
            drone("l1", "linux", OLD, 0),
            drone("l2", "linux", OLD, 0),
            drone("l3", "linux", OLD, 0),
            drone("m1", "macos", OLD, 0),
            drone("m2", "macos", OLD, 0),
        ]
    }

    fn step(now: i64, st: Option<RolloutState>, fleet: &[DroneView], jobs: &[Outcome]) -> Advance {
        advance(now, &settings(), OLD, NEW, st, fleet, jobs, &[])
    }

    fn set_version(fleet: &mut [DroneView], names: &[&str], version: &str, since: i64) {
        for d in fleet
            .iter_mut()
            .filter(|d| names.contains(&d.name.as_str()))
        {
            d.version = Some(version.into());
            d.version_since = Some(since);
        }
    }

    #[test]
    fn the_first_pass_picks_one_canary_per_os_and_tells_only_them() {
        let a = step(100, None, &fleet(), &[]);
        assert_eq!(a.state.canaries, ["l1", "m1"]);
        assert_eq!(a.state.phase, Phase::Canary);
        assert_eq!(desired_for(&a.state, "l1"), NEW);
        assert_eq!(desired_for(&a.state, "m1"), NEW);
        assert_eq!(desired_for(&a.state, "l2"), OLD);
        assert!(a.events[0].contains("started"));
        // Waiting for the canaries to update.
        assert!(a.waiting.as_deref().unwrap().contains("update"));
    }

    #[test]
    fn without_a_macos_host_the_canary_stage_waits_and_keeps_the_linux_pick() {
        let f: Vec<DroneView> = fleet().into_iter().filter(|d| d.os == "linux").collect();
        let a = step(100, None, &f, &[]);
        assert!(a.waiting.as_deref().unwrap().contains("macos"));
        assert_eq!(a.state.canaries, ["l1"]);
        // Later a macOS host appears: the stage proceeds with the same Linux pick.
        let b = step(110, Some(a.state), &fleet(), &[]);
        assert_eq!(b.state.canaries, ["l1", "m1"]);
    }

    #[test]
    fn canaries_promote_to_the_first_wave_only_after_enough_successful_jobs_on_each() {
        let mut f = fleet();
        let a = step(100, None, &f, &[]);
        set_version(&mut f, &["l1", "m1"], NEW, 105);
        // Two successes on l1, none on m1: not enough.
        let mut jobs: Vec<Outcome> = (0..3)
            .map(|i| job("l1", 110 + i, OutcomeKind::Ok))
            .collect();
        jobs.push(job("m1", 120, OutcomeKind::Ok));
        let b = step(130, Some(a.state), &f, &jobs);
        assert_eq!(b.state.phase, Phase::Canary);
        assert!(b.waiting.as_deref().unwrap().contains("observing canaries"));
        // Jobs from before the drone got the new version do not count.
        jobs.push(job("m1", 50, OutcomeKind::Ok));
        jobs.push(job("m1", 51, OutcomeKind::Ok));
        let c = step(131, Some(b.state.clone()), &f, &jobs);
        assert_eq!(c.state.phase, Phase::Canary);
        jobs.push(job("m1", 121, OutcomeKind::Ok));
        jobs.push(job("m1", 122, OutcomeKind::Ok));
        let d = step(140, Some(b.state), &f, &jobs);
        assert_eq!(d.state.phase, Phase::Wave);
        assert_eq!(d.state.wave, 0);
        // 25% of 5 eligible drones, rounded up: 2 drones, the two canaries.
        assert_eq!(d.state.members, ["l1", "m1"]);
    }

    fn promoted_to_wave() -> (Vec<DroneView>, RolloutState) {
        let mut f = fleet();
        let a = step(100, None, &f, &[]);
        set_version(&mut f, &["l1", "m1"], NEW, 105);
        let jobs: Vec<Outcome> = ["l1", "m1"]
            .iter()
            .flat_map(|d| (0..3).map(move |i| job(d, 110 + i, OutcomeKind::Ok)))
            .collect();
        let b = step(140, Some(a.state), &f, &jobs);
        assert_eq!(b.state.phase, Phase::Wave);
        (f, b.state)
    }

    #[test]
    fn waves_need_a_soak_and_jobs_then_grow_the_set_and_finally_complete() {
        let (mut f, st) = promoted_to_wave();
        let jobs: Vec<Outcome> = (0..6)
            .map(|i| job("l1", 150 + i, OutcomeKind::Ok))
            .collect();
        // Soak (100s from 140) not over yet.
        let a = step(200, Some(st.clone()), &f, &jobs);
        assert_eq!(a.state.wave, 0);
        assert!(a.waiting.as_deref().unwrap().contains("soaking"));
        // Soaked and enough jobs: wave 2 (50% of 5 = 3 drones).
        let b = step(241, Some(st), &f, &jobs);
        assert_eq!((b.state.phase, b.state.wave), (Phase::Wave, 1));
        assert_eq!(b.state.members, ["l1", "m1", "l2"]);
        assert_eq!(desired_for(&b.state, "l2"), NEW);
        assert_eq!(desired_for(&b.state, "l3"), OLD);
        // Wave 3 is 100%: everyone is told.
        set_version(&mut f, &["l2"], NEW, 250);
        let jobs2: Vec<Outcome> = (0..6)
            .map(|i| job("l2", 260 + i, OutcomeKind::Ok))
            .collect();
        let c = step(400, Some(b.state), &f, &jobs2);
        assert_eq!((c.state.phase, c.state.wave), (Phase::Wave, 2));
        assert_eq!(c.state.members.len(), 5);
        // After the last wave soaks and passes: complete, new drones included.
        set_version(&mut f, &["l3", "m2"], NEW, 410);
        let jobs3: Vec<Outcome> = (0..6)
            .map(|i| job("l3", 420 + i, OutcomeKind::Ok))
            .collect();
        let d = step(520, Some(c.state), &f, &jobs3);
        assert_eq!(d.state.phase, Phase::Complete);
        assert_eq!(desired_for(&d.state, "brand-new"), NEW);
    }

    #[test]
    fn a_host_failure_spike_on_the_new_version_halts_and_rolls_everyone_back() {
        let (f, st) = promoted_to_wave();
        let mut jobs: Vec<Outcome> = (0..4)
            .map(|i| job("l1", 150 + i, OutcomeKind::Ok))
            .collect();
        jobs.extend((0..3).map(|i| job("m1", 150 + i, OutcomeKind::HostFailure)));
        let a = step(160, Some(st), &f, &jobs);
        assert_eq!(a.state.phase, Phase::Halted);
        assert!(
            a.state
                .halted_reason
                .as_deref()
                .unwrap()
                .contains("host_failures")
        );
        assert_eq!(desired_for(&a.state, "l1"), OLD);
        assert_eq!(desired_for(&a.state, "l3"), OLD);
        assert!(a.events.last().unwrap().contains("HALTED"));
        // A halted rollout stays halted, whatever happens next.
        let b = step(10_000, Some(a.state.clone()), &f, &[]);
        assert_eq!(b.state, a.state);
    }

    #[test]
    fn a_told_member_still_on_the_old_version_is_not_judged_as_the_new_one() {
        let (mut f, st) = promoted_to_wave();
        let ok: Vec<Outcome> = (0..6)
            .map(|i| job("l1", 150 + i, OutcomeKind::Ok))
            .collect();
        let b = step(241, Some(st), &f, &ok);
        assert_eq!(b.state.members, ["l1", "m1", "l2"]);
        // l2 is told but still drains on OLD, which it has run since 0, and
        // its jobs on OLD fail (its own workload, a bad day): no halt.
        let mut jobs = ok.clone();
        jobs.extend((0..8).map(|i| job("l2", 100 + i * 10, OutcomeKind::Other)));
        jobs.extend((0..4).map(|i| job("l3", 245 + i, OutcomeKind::Ok)));
        f.iter_mut()
            .find(|d| d.name == "l2")
            .unwrap()
            .has_active_leases = true;
        let c = step(250, Some(b.state.clone()), &f, &jobs);
        assert_eq!(c.state.phase, Phase::Wave, "{:?}", c.state.halted_reason);
        assert_eq!(c.new_group.jobs, 6, "only l1's jobs on NEW count");
        // Once l2 runs NEW, its failures on NEW do count.
        f.iter_mut()
            .find(|d| d.name == "l2")
            .unwrap()
            .has_active_leases = false;
        set_version(&mut f, &["l2"], NEW, 255);
        jobs.extend((0..8).map(|i| job("l2", 260 + i, OutcomeKind::Other)));
        let d = step(270, Some(b.state), &f, &jobs);
        assert_eq!(d.state.phase, Phase::Halted);
        assert!(
            d.state
                .halted_reason
                .as_deref()
                .unwrap()
                .contains("success_rate")
        );
    }

    #[test]
    fn the_control_group_makes_the_comparison_fair() {
        // The whole fleet is failing equally (a bad day, not a bad version):
        // the new version is no worse than the control, so it is not halted.
        let (f, st) = promoted_to_wave();
        let mut jobs = Vec::new();
        for d in ["l1", "m1", "l2", "l3", "m2"] {
            jobs.push(job(d, 150, OutcomeKind::Ok));
            jobs.push(job(d, 151, OutcomeKind::Ok));
            jobs.push(job(d, 152, OutcomeKind::Other));
            jobs.push(job(d, 153, OutcomeKind::Other));
        }
        let a = step(160, Some(st.clone()), &f, &jobs);
        assert_eq!(a.state.phase, Phase::Wave);
        // The same failures only on the new version do halt it (success drop).
        let mut jobs: Vec<Outcome> = Vec::new();
        for d in ["l1", "m1"] {
            jobs.extend((0..4).map(|i| job(d, 150 + i, OutcomeKind::Other)));
        }
        for d in ["l2", "l3", "m2"] {
            jobs.extend((0..4).map(|i| job(d, 150 + i, OutcomeKind::Ok)));
        }
        let b = step(160, Some(st), &f, &jobs);
        assert_eq!(b.state.phase, Phase::Halted);
        assert!(
            b.state
                .halted_reason
                .as_deref()
                .unwrap()
                .contains("success_rate")
        );
    }

    #[test]
    fn one_clean_canary_cannot_vouch_for_a_failing_one() {
        let mut f = fleet();
        let a = step(100, None, &f, &[]);
        set_version(&mut f, &["l1", "m1"], NEW, 105);
        let mut jobs: Vec<Outcome> = (0..8)
            .map(|i| job("l1", 110 + i, OutcomeKind::Ok))
            .collect();
        // m1: 3 host failures out of 3 jobs; aggregate 3/11 = 272 permille.
        jobs.extend((0..3).map(|i| job("m1", 110 + i, OutcomeKind::HostFailure)));
        let b = step(130, Some(a.state), &f, &jobs);
        assert_eq!(b.state.phase, Phase::Halted);
        assert!(
            b.state.halted_reason.as_deref().unwrap().contains("m1"),
            "{:?}",
            b.state.halted_reason
        );
    }

    #[test]
    fn a_canary_that_never_updates_or_goes_silent_halts_after_the_timeout() {
        let f = fleet();
        let a = step(100, None, &f, &[]);
        // Within the timeout (60s): wait.
        let b = step(150, Some(a.state.clone()), &f, &[]);
        assert_eq!(b.state.phase, Phase::Canary);
        assert!(b.waiting.as_deref().unwrap().contains("update"));
        // Past it: halt.
        let c = step(170, Some(a.state.clone()), &f, &[]);
        assert_eq!(c.state.phase, Phase::Halted);
        assert!(
            c.state
                .halted_reason
                .as_deref()
                .unwrap()
                .contains("did not report")
        );
        // A quarantined canary halts immediately.
        let mut g = fleet();
        set_version(&mut g, &["l1", "m1"], NEW, 105);
        g[0].held = true;
        let d = step(110, Some(a.state), &g, &[]);
        assert_eq!(d.state.phase, Phase::Halted);
        assert!(
            d.state
                .halted_reason
                .as_deref()
                .unwrap()
                .contains("quarantined")
        );
    }

    fn set_maintenance(fleet: &mut [DroneView], name: &str) {
        for d in fleet.iter_mut().filter(|d| d.name == name) {
            d.held = true;
            d.maintenance = true;
        }
    }

    #[test]
    fn a_canary_taken_out_for_maintenance_is_replaced_not_judged() {
        let mut f = fleet();
        let a = step(100, None, &f, &[]);
        assert_eq!(a.state.canaries, ["l1", "m1"]);
        set_version(&mut f, &["l1", "m1"], NEW, 105);
        set_maintenance(&mut f, "l1");
        let b = step(110, Some(a.state), &f, &[]);
        assert_eq!(b.state.phase, Phase::Canary, "{:?}", b.state.halted_reason);
        // Replaced in place by the next Linux host; the macOS canary stays.
        assert_eq!(b.state.canaries, ["l2", "m1"]);
        assert_eq!(b.state.members, ["l2", "m1"]);
        assert!(
            b.events.iter().any(|e| e.contains("l1 is in maintenance")),
            "{:?}",
            b.events
        );
        assert_eq!(desired_for(&b.state, "l1"), OLD);
        assert_eq!(desired_for(&b.state, "l2"), NEW);
        // A health quarantine on the target still halts.
        let mut g = f.clone();
        set_version(&mut g, &["l2"], NEW, 115);
        g.iter_mut()
            .filter(|d| d.name == "l2")
            .for_each(|d| d.held = true);
        let c = step(120, Some(b.state), &g, &[]);
        assert_eq!(c.state.phase, Phase::Halted);
        assert!(
            c.state
                .halted_reason
                .as_deref()
                .unwrap()
                .contains("l2 is quarantined")
        );
    }

    #[test]
    fn a_canary_in_maintenance_with_no_replacement_holds_the_stage() {
        let mut f: Vec<DroneView> = fleet()
            .into_iter()
            .filter(|d| d.name == "l1" || d.os == "macos")
            .collect();
        let a = step(100, None, &f, &[]);
        set_version(&mut f, &["l1", "m1"], NEW, 105);
        set_maintenance(&mut f, "l1");
        // Long past the update timeout: still waiting, never halted.
        let b = step(100 + 10_000, Some(a.state), &f, &[]);
        assert_eq!(b.state.phase, Phase::Canary, "{:?}", b.state.halted_reason);
        assert_eq!(b.state.canaries, ["l1", "m1"]);
        assert!(
            b.waiting
                .as_deref()
                .unwrap()
                .contains("l1 is in maintenance")
        );
    }

    #[test]
    fn a_wave_member_in_maintenance_is_skipped_until_released() {
        let (mut f, st) = promoted_to_wave();
        let jobs: Vec<Outcome> = (0..6)
            .map(|i| job("l1", 150 + i, OutcomeKind::Ok))
            .collect();
        let b = step(241, Some(st), &f, &jobs);
        assert_eq!(b.state.members, ["l1", "m1", "l2"]);
        // l2 never updates (it is in maintenance), far past the update timeout.
        set_maintenance(&mut f, "l2");
        let c = step(241 + 10_000, Some(b.state.clone()), &f, &jobs);
        assert_ne!(c.state.phase, Phase::Halted, "{:?}", c.state.halted_reason);
        assert!(
            c.state.members.contains(&"l2".to_owned()),
            "still told the target"
        );
    }

    #[test]
    fn one_failed_job_does_not_halt_a_canary_but_a_failing_sample_does() {
        let mut f = fleet();
        let a = step(100, None, &f, &[]);
        set_version(&mut f, &["l1", "m1"], NEW, 105);
        // Control: other drones mostly succeed since the stage began.
        let mut jobs: Vec<Outcome> = (0..8)
            .map(|i| job("l2", 106 + i, OutcomeKind::Ok))
            .collect();
        jobs.push(job("l1", 110, OutcomeKind::Other));
        let b = step(120, Some(a.state.clone()), &f, &jobs);
        assert_ne!(b.state.phase, Phase::Halted, "{:?}", b.state.halted_reason);
        // Four failures on l1 (the minimum sample) still halt.
        jobs.extend((0..3).map(|i| job("l1", 111 + i, OutcomeKind::Other)));
        let c = step(130, Some(a.state), &f, &jobs);
        assert_eq!(c.state.phase, Phase::Halted, "{:?}", c.checks);
    }

    #[test]
    fn silent_drones_are_neither_told_nor_waited_for() {
        let (mut f, st) = promoted_to_wave();
        // "a-gone" sorts first, so a wave would pick it before l2.
        let mut gone = drone("a-gone", "linux", OLD, 0);
        gone.fresh = false;
        f.push(gone);
        let jobs: Vec<Outcome> = (0..6)
            .map(|i| job("l1", 150 + i, OutcomeKind::Ok))
            .collect();
        // Wave 2 is 50% of the 5 fresh drones: 3, and never the silent one.
        let b = step(241, Some(st), &f, &jobs);
        assert_eq!((b.state.phase, b.state.wave), (Phase::Wave, 1));
        assert_eq!(b.state.members, ["l1", "m1", "l2"]);
        // A member told, then silent without ever reporting the target, is
        // skipped rather than halting the rollout past the update timeout.
        let mut told = b.state.clone();
        told.members.push("a-gone".into());
        let mut updated = f.clone();
        set_version(&mut updated, &["l2"], NEW, 250);
        let c = step(241 + 1000, Some(told), &updated, &jobs);
        assert_ne!(c.state.phase, Phase::Halted, "{:?}", c.state.halted_reason);
        // One that reported the target and then went silent still halts.
        let mut quiet = f.clone();
        set_version(&mut quiet, &["l2"], NEW, 250);
        quiet
            .iter_mut()
            .filter(|d| d.name == "l2")
            .for_each(|d| d.fresh = false);
        let d = step(241 + 1000, Some(b.state), &quiet, &jobs);
        assert_eq!(d.state.phase, Phase::Halted);
    }

    #[test]
    fn canaries_come_from_shared_drones_before_deploy_only_ones() {
        let mut fleet = fleet();
        let mut deploy = drone("a-deploy", "linux", OLD, 0);
        deploy.shared = false;
        fleet.push(deploy.clone());
        let a = step(1000, None, &fleet, &[]);
        assert!(
            a.state.canaries.iter().all(|c| c != "a-deploy"),
            "{:?}",
            a.state.canaries
        );
        // With nothing shared left, a deploy-only drone still serves.
        let a = step(1000, None, &[deploy], &[]);
        assert_eq!(a.state.canaries, vec!["a-deploy".to_owned()]);
    }

    #[test]
    fn a_rollout_in_flight_keeps_its_target_until_done() {
        let fleet = fleet();
        let canary = step(1000, None, &fleet, &[]).state;
        assert_eq!(canary.phase, Phase::Canary);
        let s = settings();
        let hold =
            |stored: &RolloutState| settled_target("0.1.0+newer".into(), Some(stored), 1000, &s);
        for phase in [Phase::Canary, Phase::Wave] {
            let active = RolloutState {
                phase,
                ..canary.clone()
            };
            assert_eq!(hold(&active), active.target);
        }
        for phase in [Phase::Complete, Phase::Halted] {
            let done = RolloutState {
                phase,
                ..canary.clone()
            };
            assert_eq!(hold(&done), "0.1.0+newer");
        }
        assert_eq!(settled_target("x".into(), None, 1000, &s), "x");
    }

    #[test]
    fn a_new_target_starts_a_fresh_rollout_even_after_a_halt() {
        let f = fleet();
        let a = step(100, None, &f, &[]);
        let halted = step(500, Some(a.state), &f, &[]);
        assert_eq!(halted.state.phase, Phase::Halted);
        let b = advance(
            600,
            &settings(),
            OLD,
            "3.0.0",
            Some(halted.state),
            &f,
            &[],
            &[],
        );
        assert_eq!(
            (b.state.phase, b.state.target.as_str()),
            (Phase::Canary, "3.0.0")
        );
    }

    fn set_busy(fleet: &mut [DroneView], name: &str, busy: bool) {
        for d in fleet.iter_mut().filter(|d| d.name == name) {
            d.has_active_leases = busy;
        }
    }

    #[test]
    fn a_draining_wave_member_does_not_hold_the_wave_and_is_cordoned_after_the_drain_timeout() {
        let (mut f, st) = promoted_to_wave();
        let ok: Vec<Outcome> = (0..6)
            .map(|i| job("l1", 150 + i, OutcomeKind::Ok))
            .collect();
        let b = step(241, Some(st), &f, &ok);
        assert_eq!(b.state.members, ["l1", "m1", "l2"]);
        // l2 is told but always busy on OLD (a deploy drone).
        set_busy(&mut f, "l2", true);
        let c = step(250, Some(b.state), &f, &ok);
        assert_eq!(c.state.drain_started_at.get("l2"), Some(&250));
        assert!(c.state.cordoned.is_empty(), "inside the drain timeout");
        assert!(!cordoned(&c.state, "l2"));
        // Past the drain timeout (900s) it is cordoned, and never halted,
        // while l1 on NEW keeps running jobs.
        let ok: Vec<Outcome> = ok
            .iter()
            .cloned()
            .chain((0..6).map(|i| job("l1", 300 + i, OutcomeKind::Ok)))
            .collect();
        let d = step(250 + 901, Some(c.state), &f, &ok);
        assert_ne!(d.state.phase, Phase::Halted, "{:?}", d.state.halted_reason);
        assert_eq!(d.state.cordoned, ["l2"]);
        assert!(cordoned(&d.state, "l2"));
        assert!(!cordoned(&d.state, "l1"));
        // The members on NEW satisfied the wave meanwhile: it promoted to 100%.
        assert_eq!((d.state.phase, d.state.wave), (Phase::Wave, 2));
        let detail = &d.checks[0].detail;
        assert!(detail.contains("l2 (cordoned)"), "{detail}");
        // It drains and updates: no longer draining or cordoned.
        set_busy(&mut f, "l2", false);
        set_version(&mut f, &["l2"], NEW, 1200);
        let e = step(1210, Some(d.state), &f, &ok);
        assert!(e.state.drain_started_at.is_empty());
        assert!(e.state.cordoned.is_empty());
    }

    #[test]
    fn a_soaked_wave_promotes_past_a_member_that_never_goes_idle() {
        // The stall seen live: the wave soaked, the updated members ran
        // enough jobs, and one member was always busy. It used to wait for
        // that member forever; it now promotes.
        let (mut f, st) = promoted_to_wave();
        let ok: Vec<Outcome> = (0..6)
            .map(|i| job("l1", 150 + i, OutcomeKind::Ok))
            .collect();
        let b = step(241, Some(st), &f, &ok);
        set_busy(&mut f, "l2", true);
        let more: Vec<Outcome> = ok
            .iter()
            .cloned()
            .chain((0..6).map(|i| job("m1", 250 + i, OutcomeKind::Ok)))
            .collect();
        let c = step(241 + 101, Some(b.state), &f, &more);
        assert_eq!((c.state.phase, c.state.wave), (Phase::Wave, 2));
        assert_eq!(c.waiting, None);
    }

    #[test]
    fn a_draining_canary_still_holds_the_canary_stage_and_is_cordoned_after_the_timeout() {
        let mut f = fleet();
        let a = step(100, None, &f, &[]);
        set_busy(&mut f, "l1", true);
        set_version(&mut f, &["m1"], NEW, 105);
        let jobs: Vec<Outcome> = (0..3)
            .map(|i| job("m1", 110 + i, OutcomeKind::Ok))
            .collect();
        let b = step(200, Some(a.state), &f, &jobs);
        assert_eq!(b.state.phase, Phase::Canary);
        assert!(b.waiting.as_deref().unwrap().contains("l1 to drain"));
        assert!(b.state.cordoned.is_empty());
        let c = step(200 + 901, Some(b.state), &f, &jobs);
        assert_eq!(c.state.phase, Phase::Canary, "{:?}", c.state.halted_reason);
        assert_eq!(c.state.cordoned, ["l1"]);
        assert!(c.waiting.as_deref().unwrap().contains("l1 (cordoned)"));
        // A halt or completion lifts every cordon.
        let halted = RolloutState {
            phase: Phase::Halted,
            ..c.state.clone()
        };
        assert!(!cordoned(&halted, "l1"));
        let d = step(2000, Some(halted), &f, &jobs);
        assert!(d.state.cordoned.is_empty());
    }

    #[test]
    fn a_chosen_canary_is_cordoned_on_time_while_another_os_has_no_host() {
        // Linux canary chosen and always busy; no macOS host at all.
        let mut f: Vec<DroneView> = fleet().into_iter().filter(|d| d.os == "linux").collect();
        set_busy(&mut f, "l1", true);
        let a = step(100, None, &f, &[]);
        assert_eq!(a.state.canaries, ["l1"]);
        assert!(a.waiting.as_deref().unwrap().contains("macos"));
        assert_eq!(a.state.drain_started_at.get("l1"), Some(&100));
        assert!(a.state.cordoned.is_empty());
        let b = step(100 + 901, Some(a.state), &f, &[]);
        assert_eq!(b.state.phase, Phase::Canary);
        assert!(b.waiting.as_deref().unwrap().contains("macos"));
        assert_eq!(b.state.cordoned, ["l1"]);
        assert!(cordoned(&b.state, "l1"));
        // Once it drains the cordon lifts, and the stage still waits.
        set_busy(&mut f, "l1", false);
        let c = step(1100, Some(b.state), &f, &[]);
        assert!(c.state.cordoned.is_empty() && c.state.drain_started_at.is_empty());
        assert_eq!(c.state.phase, Phase::Canary);
    }

    #[test]
    fn a_newer_build_supersedes_only_a_stalled_rollout() {
        let s = settings();
        let (_, st) = promoted_to_wave();
        // The wave started at 140; soak 100 plus the supersede delay.
        assert_eq!(
            s.supersede_stalled_after_secs, 900,
            "15 minutes past the soak"
        );
        let limit = 140 + 100 + 900;
        assert_eq!(settled_target("3.0.0".into(), Some(&st), limit, &s), NEW);
        assert_eq!(
            settled_target("3.0.0".into(), Some(&st), limit + 1, &s),
            "3.0.0"
        );
        // The superseding pass starts a fresh canary from the stalled
        // rollout's previous version and says what it replaced.
        let prev = previous_for("3.0.0", Some(&st), &fleet()).unwrap();
        assert_eq!(prev, OLD);
        let a = advance(limit + 1, &s, &prev, "3.0.0", Some(st), &fleet(), &[], &[]);
        assert_eq!(a.state.phase, Phase::Canary);
        assert!(
            a.events[0].contains("2.0.0 superseded by 3.0.0 in its wave stage"),
            "{:?}",
            a.events
        );
        assert!(a.state.cordoned.is_empty() && a.state.drain_started_at.is_empty());
    }

    #[test]
    fn the_pass_is_deterministic_and_settings_validate() {
        let f = fleet();
        assert_eq!(step(100, None, &f, &[]), step(100, None, &f, &[]));
        assert!(RolloutSettings::default().validate().is_ok());
        for bad in [
            RolloutSettings {
                waves_pct: vec![],
                ..RolloutSettings::default()
            },
            RolloutSettings {
                waves_pct: vec![50, 25, 100],
                ..RolloutSettings::default()
            },
            RolloutSettings {
                waves_pct: vec![25, 50],
                ..RolloutSettings::default()
            },
            RolloutSettings {
                canary_oses: vec![],
                ..RolloutSettings::default()
            },
            RolloutSettings {
                canary_jobs: 0,
                ..RolloutSettings::default()
            },
            RolloutSettings {
                drain_timeout_secs: 0,
                ..RolloutSettings::default()
            },
        ] {
            assert!(bad.validate().is_err());
        }
    }

    #[test]
    fn previous_comes_from_the_rollout_history_then_the_fleet() {
        let fleet = [
            drone("a", "linux", "0.1.0+old", 0),
            drone("b", "linux", "0.1.0+old", 0),
            drone("c", "linux", "0.1.0+other", 0),
            drone("d", "linux", "0.1.0+new", 0),
            DroneView {
                fresh: false,
                ..drone("e", "linux", "0.1.0+stale", 0)
            },
        ];
        // No history: what most fresh drones run, never the target itself.
        assert_eq!(
            previous_for("0.1.0+new", None, &fleet).as_deref(),
            Some("0.1.0+old")
        );
        assert_eq!(previous_for("0.1.0+new", None, &fleet[3..]), None);
        let state = |target: &str, previous: &str, phase| RolloutState {
            target: target.into(),
            previous: previous.into(),
            phase,
            canaries: vec![],
            members: vec![],
            wave: 0,
            started_at: 0,
            stage_started_at: 0,
            update_started_at: BTreeMap::new(),
            drain_started_at: BTreeMap::new(),
            cordoned: Vec::new(),
            halted_reason: None,
        };
        // The same rollout keeps the previous it started with, whatever the
        // fleet runs now.
        let running = state("0.1.0+new", "0.1.0+first", Phase::Wave);
        assert_eq!(
            previous_for("0.1.0+new", Some(&running), &fleet).as_deref(),
            Some("0.1.0+first")
        );
        // A new target starts from a completed rollout's target, or from a
        // halted one's previous (where it rolled the fleet back to).
        let done = state("0.1.0+mid", "0.1.0+first", Phase::Complete);
        assert_eq!(
            previous_for("0.1.0+new", Some(&done), &fleet).as_deref(),
            Some("0.1.0+mid")
        );
        let halted = state("0.1.0+mid", "0.1.0+first", Phase::Halted);
        assert_eq!(
            previous_for("0.1.0+new", Some(&halted), &fleet).as_deref(),
            Some("0.1.0+first")
        );
    }

    #[test]
    fn a_drone_told_in_the_last_wave_is_checked_before_the_rollout_completes() {
        let (mut f, st) = promoted_to_wave();
        let jobs: Vec<Outcome> = (0..6)
            .map(|i| job("l1", 150 + i, OutcomeKind::Ok))
            .collect();
        let b = step(241, Some(st), &f, &jobs);
        set_version(&mut f, &["l2"], NEW, 250);
        let jobs2: Vec<Outcome> = (0..6)
            .map(|i| job("l2", 260 + i, OutcomeKind::Ok))
            .collect();
        let c = step(400, Some(b.state), &f, &jobs2);
        assert_eq!((c.state.phase, c.state.wave), (Phase::Wave, 2));
        set_version(&mut f, &["l3", "m2"], NEW, 410);
        // A drone joins on the old version as the last wave would complete:
        // it is told, and the wave waits to check it before completing.
        f.push(drone("l4", "linux", OLD, 0));
        let jobs3: Vec<Outcome> = (0..6)
            .map(|i| job("l3", 420 + i, OutcomeKind::Ok))
            .collect();
        let d = step(520, Some(c.state), &f, &jobs3);
        assert_eq!(d.state.phase, Phase::Wave);
        assert!(d.state.members.iter().any(|m| m == "l4"));
        assert!(
            d.waiting.as_deref().unwrap().contains("just told"),
            "{:?}",
            d.waiting
        );
        // Once it runs the target, the rollout completes.
        set_version(&mut f, &["l4"], NEW, 525);
        let e = step(530, Some(d.state), &f, &jobs3);
        assert_eq!(e.state.phase, Phase::Complete);
    }

    #[test]
    fn telling_a_drone_already_out_of_service_takes_nothing_more_out() {
        // pve-a is still updating, so the capacity rule applies. borg-heavy-1
        // is not accepting work: its heavy slots are already out of service,
        // so telling it adds none and it is told; borg-heavy-2 then waits.
        let f = morning_fleet();
        let s = RolloutSettings {
            canary_oses: vec!["linux".into()],
            ..settings()
        };
        let a = advance(1000, &s, OLD, NEW, Some(last_wave(1000)), &f, &[], &[]);
        assert_eq!(told_heavy(&a.state, &f), ["borg-heavy-1"]);
    }

    /// The shared fleet on 2026-11-20 as the rollout saw it, with the slots
    /// each drone holds when empty: borg-main-1 holds half the heavy slots and
    /// borg-heavy-1 is not accepting work.
    fn morning_fleet() -> Vec<DroneView> {
        let with = |name: &str, heavy: u32, medium: u32, light: u32| {
            let mut d = drone(name, "linux", OLD, 0);
            d.slots = [
                (Class::Heavy, heavy),
                (Class::Medium, medium),
                (Class::Light, light),
            ]
            .into_iter()
            .filter(|&(_, n)| n > 0)
            .collect();
            d
        };
        let mut idle = with("borg-heavy-1", 2, 4, 8);
        idle.runs_jobs = false;
        idle.in_service = false;
        vec![
            idle,
            with("borg-heavy-2", 2, 4, 8),
            with("borg-main-1", 5, 14, 32),
            with("gha-heavy", 1, 3, 8),
            with("pve-a", 0, 0, 3),
            with("pve-b", 0, 0, 3),
            with("pve-c", 0, 0, 3),
        ]
    }

    /// The last wave (100%) just started, with only `pve-a` told so far.
    fn last_wave(at: i64) -> RolloutState {
        RolloutState {
            target: NEW.into(),
            previous: OLD.into(),
            phase: Phase::Wave,
            canaries: vec!["pve-a".into()],
            members: vec!["pve-a".into()],
            wave: 2,
            started_at: 0,
            stage_started_at: at,
            update_started_at: BTreeMap::new(),
            drain_started_at: BTreeMap::new(),
            cordoned: Vec::new(),
            halted_reason: None,
        }
    }

    fn told_heavy(st: &RolloutState, fleet: &[DroneView]) -> Vec<String> {
        fleet
            .iter()
            .filter(|d| st.members.contains(&d.name) && d.slots.contains_key(&Class::Heavy))
            .filter(|d| d.version.as_deref() != Some(NEW))
            .map(|d| d.name.clone())
            .collect()
    }

    #[test]
    fn a_wave_tells_heavy_capable_drones_one_at_a_time() {
        let mut f = morning_fleet();
        set_version(&mut f, &["pve-a"], NEW, 0);
        let s = RolloutSettings {
            canary_oses: vec!["linux".into()],
            ..settings()
        };
        let a = advance(1000, &s, OLD, NEW, Some(last_wave(1000)), &f, &[], &[]);
        // One drone is always allowed; the small ones fit the light budget;
        // no second heavy-capable drone goes out while borg-heavy-1 updates.
        assert_eq!(a.state.members, ["pve-a", "borg-heavy-1", "pve-b", "pve-c"]);
        assert_eq!(told_heavy(&a.state, &f), ["borg-heavy-1"]);
        let why = a.waiting.as_deref().unwrap();
        assert!(why.contains("capacity out of service"), "{why}");

        // Once they run the target, the next heavy-capable drone is told,
        // and only it.
        set_version(&mut f, &["borg-heavy-1", "pve-b", "pve-c"], NEW, 1010);
        let b = advance(1020, &s, OLD, NEW, Some(a.state), &f, &[], &[]);
        assert_eq!(told_heavy(&b.state, &f), ["borg-heavy-2"]);
        // The wave cannot complete until every drone is told.
        assert_eq!(b.state.phase, Phase::Wave);
    }

    #[test]
    fn aged_queues_hold_back_drones_that_run_their_class_for_a_bounded_time() {
        let mut f = morning_fleet();
        set_version(&mut f, &["pve-a"], NEW, 0);
        let s = RolloutSettings {
            canary_oses: vec!["linux".into()],
            ..settings()
        };
        let heavy_queue = [QueueAge {
            class: Class::Heavy,
            scope: crate::classes::Scope::Main,
            oldest_age_secs: 4877,
        }];
        let a = advance(
            1000,
            &s,
            OLD,
            NEW,
            Some(last_wave(1000)),
            &f,
            &[],
            &heavy_queue,
        );
        // Only drones that cannot run heavy work are told.
        assert_eq!(a.state.members, ["pve-a", "pve-b", "pve-c"]);
        assert!(
            a.waiting
                .as_deref()
                .unwrap()
                .contains("heavy jobs have queued")
        );
        // The pause lasts at most max_pause_secs into the stage.
        let late = 1000 + secs(s.max_pause_secs) + 1;
        let b = advance(
            late,
            &s,
            OLD,
            NEW,
            Some(last_wave(1000)),
            &f,
            &[],
            &heavy_queue,
        );
        assert_eq!(told_heavy(&b.state, &f), ["borg-heavy-1"]);
    }

    #[test]
    fn canaries_are_chosen_as_before_whatever_the_queues() {
        let f = morning_fleet();
        let s = RolloutSettings {
            canary_oses: vec!["linux".into()],
            ..settings()
        };
        let heavy_queue = [QueueAge {
            class: Class::Heavy,
            scope: crate::classes::Scope::Main,
            oldest_age_secs: 4877,
        }];
        let a = advance(10, &s, OLD, NEW, None, &f, &[], &heavy_queue);
        assert_eq!(a.state.canaries, ["borg-heavy-2"]);
    }
}
