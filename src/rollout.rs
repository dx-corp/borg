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
//! * **update**: a told drone must report the target within `update_timeout_secs`;
//! * **heartbeat health**: drones on the target stay fresh and out of
//!   quarantine;
//! * **host failures**: the host-failure rate on the target may not exceed the
//!   control's by more than `max_host_failure_increase_pm`; with too little
//!   control data an absolute cap applies;
//! * **success rate**: the job success rate may not fall below the control's by
//!   more than `max_success_drop_pm`;
//! * **independence**: in the canary stage every canary host is also checked on
//!   its own, so one clean host cannot vouch for a failing one.
//!
//! Any failing check **halts** the rollout and **rolls back**: every drone is
//! told the previous version. A halted rollout stays halted until an operator
//! resets it or the target changes. The module is pure: the caller supplies the
//! clock, the fleet view and the job outcomes, and persists the returned state.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::health::{Outcome, OutcomeKind};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RolloutSettings {
    /// The version drones run today. Setting it (together with the top-level
    /// `rollout_target_version`) turns on the gated rollout; without it the
    /// target is only reported and every drone is told the target.
    pub previous_version: Option<String>,
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
    /// How long a told drone has to report the target (and heartbeat).
    pub update_timeout_secs: u64,
    pub max_host_failure_increase_pm: u32,
    pub max_success_drop_pm: u32,
    /// Host-failure cap (permille) when the control group is too small.
    pub absolute_host_failure_pm: u32,
    /// Control-group jobs needed to compare against.
    pub min_baseline_jobs: u32,
}

impl Default for RolloutSettings {
    fn default() -> Self {
        Self {
            previous_version: None,
            canary_oses: vec!["linux".into(), "macos".into()],
            canary_jobs: 5,
            waves_pct: vec![25, 50, 100],
            wave_jobs: 10,
            soak_secs: 1800,
            update_timeout_secs: 900,
            max_host_failure_increase_pm: 20,
            max_success_drop_pm: 50,
            absolute_host_failure_pm: 100,
            min_baseline_jobs: 5,
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
        if self.update_timeout_secs == 0 || self.wave_jobs == 0 {
            return Err("rollout update_timeout_secs and wave_jobs must be positive".into());
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
    pub halted_reason: Option<String>,
}

/// The fleet as the rollout sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroneView {
    pub name: String,
    pub os: String,
    pub fresh: bool,
    /// Quarantined or otherwise not schedulable.
    pub held: bool,
    pub version: Option<String>,
    /// When the queen first saw the drone report `version`.
    pub version_since: Option<i64>,
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
    if have_control && new.jobs > 0 {
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
    reason = "one linear state machine; splitting it would scatter the stage rules"
)]
pub fn advance(
    now: i64,
    s: &RolloutSettings,
    previous: &str,
    target: &str,
    state: Option<RolloutState>,
    drones: &[DroneView],
    outcomes: &[Outcome],
) -> Advance {
    let mut events = Vec::new();
    let mut st = match state {
        Some(st) if st.target == target => st,
        _ => {
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
        return done(st, events, None);
    }

    // Canary selection: one fresh, schedulable host per OS, kept once chosen.
    if st.phase == Phase::Canary && st.canaries.len() < s.canary_oses.len() {
        let chosen: BTreeSet<String> = st.canaries.iter().cloned().collect();
        let mut picked = st.canaries.clone();
        for os in s.canary_oses.iter().skip(st.canaries.len()) {
            let pick = drones
                .iter()
                .filter(|d| d.fresh && !d.held && d.os == *os && !chosen.contains(&d.name))
                .map(|d| d.name.clone())
                .min();
            match pick {
                Some(name) => {
                    events.push(format!("canary for {os}: {name}"));
                    picked.push(name);
                }
                None => {
                    st.canaries = picked;
                    st.members = st.canaries.clone();
                    let why = format!("waiting for a fresh {os} host to use as canary");
                    return done(st, events, Some(why));
                }
            }
        }
        st.canaries = picked;
        st.members = st.canaries.clone();
        st.stage_started_at = now;
    }

    let by_name = |n: &str| drones.iter().find(|d| d.name == n);
    let members: BTreeSet<&str> = st.members.iter().map(String::as_str).collect();
    let on_target = |d: &DroneView| d.version.as_deref() == Some(target);
    let since_of = |n: &str| by_name(n).and_then(|d| d.version_since).unwrap_or(i64::MAX);
    let timeout = i64::try_from(s.update_timeout_secs).unwrap_or(i64::MAX);
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

    for n in &st.members {
        let Some(d) = by_name(n) else {
            continue;
        };
        if !on_target(d) {
            if overdue {
                halt.get_or_insert(format!(
                    "{n} did not report {target} within {}s of being told",
                    s.update_timeout_secs
                ));
            } else {
                wait.get_or_insert(format!("waiting for {n} to update to {target}"));
            }
            continue;
        }
        if d.held {
            halt.get_or_insert(format!("{n} is quarantined on {target}"));
        } else if !d.fresh {
            if overdue {
                halt.get_or_insert(format!("{n} stopped heartbeating on {target}"));
            } else {
                wait.get_or_insert(format!("waiting for {n} to heartbeat on {target}"));
            }
        }
    }
    checks.push(Check {
        name: "update_and_heartbeat",
        ok: halt.is_none(),
        detail: halt
            .clone()
            .or_else(|| wait.clone())
            .unwrap_or_else(|| "all told drones run the target and heartbeat".into()),
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
            if wait.is_none() && !soaked {
                wait = Some(format!("soaking wave {} for {}s", st.wave + 1, s.soak_secs));
            } else if wait.is_none() && !enough {
                wait = Some(format!(
                    "wave {}: {} of {} jobs observed",
                    st.wave + 1,
                    stage_jobs.jobs,
                    s.wave_jobs
                ));
            }
            soaked && enough && wait.is_none()
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
            events.push(format!("rollout of {target} complete"));
        } else {
            let eligible: Vec<&DroneView> = drones.iter().filter(|d| !d.held).collect();
            let pct = usize::try_from(s.waves_pct[next]).unwrap_or(100);
            let want = (eligible.len() * pct).div_ceil(100);
            let mut extra: Vec<&str> = eligible
                .iter()
                .map(|d| d.name.as_str())
                .filter(|n| !members.contains(n))
                .collect();
            extra.sort_unstable();
            for n in extra
                .into_iter()
                .take(want.saturating_sub(st.members.len()))
            {
                st.members.push(n.to_owned());
            }
            st.phase = Phase::Wave;
            st.wave = next;
            st.stage_started_at = now;
            events.push(format!(
                "rollout of {target}: wave {} ({pct}%), {} drone(s) told",
                next + 1,
                st.members.len()
            ));
        }
        wait = None;
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

    fn drone(name: &str, os: &str, version: &str, since: i64) -> DroneView {
        DroneView {
            name: name.into(),
            os: os.into(),
            fresh: true,
            held: false,
            version: Some(version.into()),
            version_since: Some(since),
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
        advance(now, &settings(), OLD, NEW, st, fleet, jobs)
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

    #[test]
    fn a_new_target_starts_a_fresh_rollout_even_after_a_halt() {
        let f = fleet();
        let a = step(100, None, &f, &[]);
        let halted = step(500, Some(a.state), &f, &[]);
        assert_eq!(halted.state.phase, Phase::Halted);
        let b = advance(600, &settings(), OLD, "3.0.0", Some(halted.state), &f, &[]);
        assert_eq!(
            (b.state.phase, b.state.target.as_str()),
            (Phase::Canary, "3.0.0")
        );
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
        ] {
            assert!(bad.validate().is_err());
        }
    }
}
