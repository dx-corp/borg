//! Host health from job telemetry: decayed evidence, quarantine
//! recommendations, bounded automatic quarantine, and a cooldown with a health
//! proof before a host returns.
//!
//! [`evaluate`] is a pure function of the clock value it is given, the drones'
//! current holds and the recent job outcomes (from the telemetry drones report
//! with each lease result). The queen applies the [`Action`]s it returns.
//!
//! Rules:
//! * Only host-caused failures (`failure_class: host`) count against a host.
//!   Code-caused failures, successes and unclassified failures never do.
//! * Evidence decays with a half-life ([`decay_permille`]); a failure from a
//!   long time ago weighs next to nothing, and a single fresh failure is below
//!   every threshold.
//! * Corroboration: automatic quarantine needs `corroboration_runs` host
//!   failures from distinct workflow runs inside the window, plus a decayed
//!   score at or above `quarantine_score_permille`. Retrying one run does not
//!   count twice.
//! * Bounds: no automatic quarantine while the evidence looks fleet-wide
//!   (many hosts failing at once points at GitHub, the network or a bad
//!   workflow, not at one host), beyond `max_auto_quarantined` held hosts, or
//!   if it would leave fewer than `min_schedulable_remaining` fresh hosts in
//!   service. In those cases the host is only *recommended* for quarantine.
//! * Return: after `cooldown_secs` (doubling per earlier strike, capped) an
//!   automatically quarantined host enters probation: at most one lease at a
//!   time. After `proof_jobs` successful jobs it is released; a host-caused
//!   failure during probation sends it back with one more strike. Operator
//!   quarantines are never lifted automatically.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// Tunables; every field has a default.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HealthSettings {
    /// Quarantine automatically when evidence is strong; otherwise only
    /// recommend.
    pub auto_quarantine: bool,
    /// Half-life of a failure's weight.
    pub half_life_secs: u64,
    /// Only failures newer than this are considered at all.
    pub window_secs: u64,
    /// Distinct runs with a host failure needed to quarantine automatically.
    pub corroboration_runs: u32,
    /// Decayed score (1000 = one fresh failure) needed to quarantine.
    pub quarantine_score_permille: u32,
    /// Decayed score at which a quarantine is recommended.
    pub recommend_score_permille: u32,
    /// Base cooldown before an auto-quarantined host enters probation.
    pub cooldown_secs: u64,
    /// Successful jobs on probation needed to return to service.
    pub proof_jobs: u32,
    /// At most this many hosts held (quarantined or on probation) by the
    /// automatic path at once.
    pub max_auto_quarantined: u32,
    /// Never auto-quarantine if fewer than this many fresh, unheld hosts
    /// would remain.
    pub min_schedulable_remaining: u32,
    /// Fleet-wide guard: with at least 3 hosts that ran jobs, if this share
    /// (percent) had host failures, nothing is auto-quarantined.
    pub systemic_share_pct: u32,
    /// Cooldown multiplier cap (cooldown doubles per strike up to this).
    pub max_cooldown_multiplier: u32,
}

impl Default for HealthSettings {
    fn default() -> Self {
        Self {
            auto_quarantine: true,
            half_life_secs: 6 * 3600,
            window_secs: 12 * 3600,
            corroboration_runs: 3,
            quarantine_score_permille: 2500,
            recommend_score_permille: 1500,
            cooldown_secs: 1800,
            proof_jobs: 2,
            max_auto_quarantined: 2,
            min_schedulable_remaining: 1,
            systemic_share_pct: 50,
            max_cooldown_multiplier: 8,
        }
    }
}

impl HealthSettings {
    pub fn validate(&self) -> Result<(), String> {
        if self.half_life_secs == 0 || self.window_secs == 0 || self.cooldown_secs == 0 {
            return Err(
                "health half_life_secs, window_secs and cooldown_secs must be positive".into(),
            );
        }
        if self.corroboration_runs == 0 || self.proof_jobs == 0 || self.max_cooldown_multiplier == 0
        {
            return Err(
                "health corroboration_runs, proof_jobs and max_cooldown_multiplier must be positive"
                    .into(),
            );
        }
        if self.recommend_score_permille > self.quarantine_score_permille {
            return Err(
                "health recommend_score_permille may not exceed quarantine_score_permille".into(),
            );
        }
        if self.systemic_share_pct == 0 || self.systemic_share_pct > 100 {
            return Err("health systemic_share_pct must be 1 to 100".into());
        }
        Ok(())
    }

    /// Cooldown after `strikes` earlier failed returns.
    pub fn cooldown_for(&self, strikes: u32) -> i64 {
        let mult = 1u64
            .checked_shl(strikes.min(16))
            .unwrap_or(u64::MAX)
            .min(u64::from(self.max_cooldown_multiplier));
        i64::try_from(self.cooldown_secs.saturating_mul(mult)).unwrap_or(i64::MAX)
    }
}

/// Weight (permille) of evidence `age_secs` old: 1000 now, half after one
/// half-life, a quarter after two. Piecewise linear between half-lives so it
/// stays integer and monotone.
pub fn decay_permille(age_secs: i64, half_life_secs: u64) -> u32 {
    let Ok(age) = u64::try_from(age_secs) else {
        return 1000;
    };
    let hl = half_life_secs.max(1);
    let whole = age / hl;
    if whole >= 10 {
        return 0;
    }
    let frac = age % hl;
    let base = 1000u64 >> whole;
    let w = base - base * frac / (2 * hl);
    u32::try_from(w).unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeKind {
    /// The job succeeded.
    Ok,
    /// The job failed because the host or runner failed.
    HostFailure,
    /// Anything else: a code failure, a cancellation, an unclassified failure.
    Other,
}

/// One finished job, from telemetry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub drone: String,
    pub lease_id: String,
    pub repo: String,
    pub run_id: u64,
    pub job_name: String,
    pub finished_at: i64,
    pub kind: OutcomeKind,
}

/// Why a drone is held back from new leases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    None,
    /// Quarantined by an operator: never lifted automatically.
    Operator,
    Auto {
        since: i64,
        strikes: u32,
    },
    Probation {
        since: i64,
        strikes: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroneInfo {
    pub name: String,
    pub fresh: bool,
    pub hold: Hold,
    /// Evidence at or before this time was cleared by a return to service.
    pub cleared_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    Healthy,
    /// Evidence suggests a quarantine, but it was not applied.
    Recommended,
    Quarantined,
    Probation,
}

impl HealthState {
    pub const ALL: [Self; 4] = [
        Self::Healthy,
        Self::Recommended,
        Self::Quarantined,
        Self::Probation,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Recommended => "recommended",
            Self::Quarantined => "quarantined",
            Self::Probation => "probation",
        }
    }
}

/// One drone's health as shown in `/status` and metrics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DroneHealth {
    pub state: HealthState,
    /// `operator` or `auto` while held.
    pub source: Option<String>,
    /// Decayed host-failure evidence, 1000 = one fresh failure.
    pub score_permille: u32,
    pub host_failures: u32,
    pub corroborating_runs: u32,
    pub strikes: u32,
    /// Unix time the hold (or probation) started.
    pub since: Option<i64>,
    /// When an auto-quarantined host becomes eligible for probation.
    pub probation_at: Option<i64>,
    /// Successful jobs counted toward the health proof.
    pub proof_jobs_done: u32,
    /// Why a recommendation was not applied automatically.
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub lease_id: String,
    pub repo: String,
    pub run_id: u64,
    pub job_name: String,
    pub finished_at: i64,
    pub weight_permille: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Quarantine on strong, corroborated evidence.
    Quarantine {
        drone: String,
        reason: String,
        evidence: Vec<Evidence>,
        strikes: u32,
    },
    /// Cooldown elapsed: allow one lease at a time.
    BeginProbation { drone: String },
    /// A host failure during probation: back to quarantine.
    Requarantine {
        drone: String,
        reason: String,
        evidence: Vec<Evidence>,
        strikes: u32,
    },
    /// Health proven: return to service.
    Release { drone: String },
}

impl Action {
    pub fn drone(&self) -> &str {
        match self {
            Self::Quarantine { drone, .. }
            | Self::BeginProbation { drone }
            | Self::Requarantine { drone, .. }
            | Self::Release { drone } => drone,
        }
    }

    pub fn event(&self) -> &'static str {
        match self {
            Self::Quarantine { .. } => "auto_quarantine",
            Self::BeginProbation { .. } => "probation_start",
            Self::Requarantine { .. } => "requarantine",
            Self::Release { .. } => "release",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Evaluation {
    pub drones: BTreeMap<String, DroneHealth>,
    pub actions: Vec<Action>,
    /// Host failures look fleet-wide; automatic quarantine is paused.
    pub systemic: bool,
}

fn run_key(o: &Outcome) -> String {
    if o.run_id == 0 {
        format!("lease:{}", o.lease_id)
    } else {
        format!("{}#{}", o.repo, o.run_id)
    }
}

fn score_text(permille: u32) -> String {
    format!("{}.{}", permille / 1000, permille % 1000 / 100)
}

struct Failures {
    score: u32,
    count: u32,
    runs: u32,
    evidence: Vec<Evidence>,
}

fn failures_of(
    now: i64,
    s: &HealthSettings,
    drone: &str,
    since: Option<i64>,
    outcomes: &[Outcome],
) -> Failures {
    let mut f = Failures {
        score: 0,
        count: 0,
        runs: 0,
        evidence: Vec::new(),
    };
    let mut runs = BTreeSet::new();
    let window = i64::try_from(s.window_secs).unwrap_or(i64::MAX);
    for o in outcomes.iter().filter(|o| {
        o.drone == drone
            && o.kind == OutcomeKind::HostFailure
            && now.saturating_sub(o.finished_at) <= window
            && since.is_none_or(|t| o.finished_at > t)
    }) {
        let weight = decay_permille(now - o.finished_at, s.half_life_secs);
        f.score = f.score.saturating_add(weight);
        f.count += 1;
        runs.insert(run_key(o));
        f.evidence.push(Evidence {
            lease_id: o.lease_id.clone(),
            repo: o.repo.clone(),
            run_id: o.run_id,
            job_name: o.job_name.clone(),
            finished_at: o.finished_at,
            weight_permille: weight,
        });
    }
    f.runs = u32::try_from(runs.len()).unwrap_or(u32::MAX);
    f.evidence.sort_by_key(|e| std::cmp::Reverse(e.finished_at));
    f.evidence.truncate(20);
    f
}

/// Evaluate every drone. Pure: the same inputs give the same actions.
pub fn evaluate(
    now: i64,
    s: &HealthSettings,
    drones: &[DroneInfo],
    outcomes: &[Outcome],
) -> Evaluation {
    let window = i64::try_from(s.window_secs).unwrap_or(i64::MAX);
    let recent = |o: &&Outcome| now.saturating_sub(o.finished_at) <= window;

    // Fleet-wide guard.
    let ran: BTreeSet<&str> = outcomes
        .iter()
        .filter(recent)
        .map(|o| o.drone.as_str())
        .collect();
    let hit: BTreeSet<&str> = outcomes
        .iter()
        .filter(recent)
        .filter(|o| o.kind == OutcomeKind::HostFailure)
        .map(|o| o.drone.as_str())
        .collect();
    let systemic = ran.len() >= 3
        && u64::try_from(hit.len()).unwrap_or(0) * 100
            >= u64::try_from(ran.len()).unwrap_or(0) * u64::from(s.systemic_share_pct);

    let mut out = Evaluation {
        systemic,
        ..Evaluation::default()
    };
    let mut auto_held = u32::try_from(
        drones
            .iter()
            .filter(|d| matches!(d.hold, Hold::Auto { .. } | Hold::Probation { .. }))
            .count(),
    )
    .unwrap_or(u32::MAX);
    let mut in_service = u32::try_from(
        drones
            .iter()
            .filter(|d| d.fresh && d.hold == Hold::None)
            .count(),
    )
    .unwrap_or(u32::MAX);

    let health = |state, source: Option<&str>, f: &Failures, strikes, since| DroneHealth {
        state,
        source: source.map(str::to_owned),
        score_permille: f.score,
        host_failures: f.count,
        corroborating_runs: f.runs,
        strikes,
        since,
        probation_at: None,
        proof_jobs_done: 0,
        note: None,
    };

    let mut candidates: Vec<(u32, &DroneInfo, Failures)> = Vec::new();
    for d in drones {
        let f = failures_of(now, s, &d.name, d.cleared_at, outcomes);
        match d.hold {
            Hold::None => {
                if f.runs >= s.corroboration_runs && f.score >= s.quarantine_score_permille {
                    candidates.push((f.score, d, f));
                } else {
                    let state = if f.score >= s.recommend_score_permille {
                        HealthState::Recommended
                    } else {
                        HealthState::Healthy
                    };
                    out.drones
                        .insert(d.name.clone(), health(state, None, &f, 0, None));
                }
            }
            Hold::Operator => {
                out.drones.insert(
                    d.name.clone(),
                    health(HealthState::Quarantined, Some("operator"), &f, 0, None),
                );
            }
            Hold::Auto { since, strikes } => {
                let ready = since + s.cooldown_for(strikes);
                let mut h = health(
                    HealthState::Quarantined,
                    Some("auto"),
                    &f,
                    strikes,
                    Some(since),
                );
                h.probation_at = Some(ready);
                if now >= ready {
                    out.actions.push(Action::BeginProbation {
                        drone: d.name.clone(),
                    });
                }
                out.drones.insert(d.name.clone(), h);
            }
            Hold::Probation { since, strikes } => {
                let mine = |o: &&Outcome| o.drone == d.name && o.finished_at >= since;
                let failed: Vec<&Outcome> = outcomes
                    .iter()
                    .filter(mine)
                    .filter(|o| o.kind == OutcomeKind::HostFailure)
                    .collect();
                let ok = u32::try_from(
                    outcomes
                        .iter()
                        .filter(mine)
                        .filter(|o| o.kind == OutcomeKind::Ok)
                        .count(),
                )
                .unwrap_or(u32::MAX);
                let mut h = health(
                    HealthState::Probation,
                    Some("auto"),
                    &f,
                    strikes,
                    Some(since),
                );
                h.proof_jobs_done = ok;
                if !failed.is_empty() {
                    let evidence = failures_of(now, s, &d.name, Some(since - 1), outcomes).evidence;
                    out.actions.push(Action::Requarantine {
                        drone: d.name.clone(),
                        reason: format!(
                            "auto: host-caused failure during probation ({} job(s))",
                            failed.len()
                        ),
                        evidence,
                        strikes: strikes + 1,
                    });
                } else if ok >= s.proof_jobs {
                    out.actions.push(Action::Release {
                        drone: d.name.clone(),
                    });
                }
                out.drones.insert(d.name.clone(), h);
            }
        }
    }

    candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    for (_, d, f) in candidates {
        let after = in_service.saturating_sub(u32::from(d.fresh));
        let blocked = if !s.auto_quarantine {
            Some("automatic quarantine is off")
        } else if systemic {
            Some("failures look fleet-wide")
        } else if auto_held >= s.max_auto_quarantined {
            Some("automatic quarantine limit reached")
        } else if after < s.min_schedulable_remaining {
            Some("would leave too few hosts in service")
        } else {
            None
        };
        match blocked {
            None => {
                auto_held += 1;
                in_service = after;
                let reason = format!(
                    "auto: {} host-caused failures in {} distinct runs, decayed score {}",
                    f.count,
                    f.runs,
                    score_text(f.score)
                );
                let mut h = health(HealthState::Quarantined, Some("auto"), &f, 0, Some(now));
                h.probation_at = Some(now + s.cooldown_for(0));
                out.drones.insert(d.name.clone(), h);
                out.actions.push(Action::Quarantine {
                    drone: d.name.clone(),
                    reason,
                    evidence: f.evidence,
                    strikes: 0,
                });
            }
            Some(why) => {
                let mut h = health(HealthState::Recommended, None, &f, 0, None);
                h.note = Some(why.to_owned());
                out.drones.insert(d.name.clone(), h);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const HL: u64 = 3600;

    fn settings() -> HealthSettings {
        HealthSettings {
            half_life_secs: HL,
            window_secs: 12 * 3600,
            ..HealthSettings::default()
        }
    }

    fn drone(name: &str) -> DroneInfo {
        DroneInfo {
            name: name.into(),
            fresh: true,
            hold: Hold::None,
            cleared_at: None,
        }
    }

    fn out(drone: &str, run: u64, at: i64, kind: OutcomeKind) -> Outcome {
        Outcome {
            drone: drone.into(),
            lease_id: format!("{drone}-{run}-{at}"),
            repo: "o/r".into(),
            run_id: run,
            job_name: "job".into(),
            finished_at: at,
            kind,
        }
    }

    fn fails(drone: &str, runs: &[u64], at: i64) -> Vec<Outcome> {
        runs.iter()
            .map(|&r| out(drone, r, at, OutcomeKind::HostFailure))
            .collect()
    }

    #[test]
    fn decay_halves_per_half_life_and_is_monotone() {
        assert_eq!(decay_permille(0, HL), 1000);
        assert_eq!(decay_permille(-50, HL), 1000);
        assert_eq!(decay_permille(3600, HL), 500);
        assert_eq!(decay_permille(7200, HL), 250);
        assert_eq!(decay_permille(1800, HL), 750);
        assert_eq!(decay_permille(36_000, HL), 0);
        let mut last = 1000;
        for t in (0..40_000).step_by(300) {
            let w = decay_permille(t, HL);
            assert!(w <= last, "t={t}");
            last = w;
        }
    }

    #[test]
    fn one_failure_never_triggers_anything_and_two_only_recommend() {
        let s = settings();
        let d = [drone("a"), drone("b")];
        let e = evaluate(1000, &s, &d, &fails("a", &[1], 1000));
        assert!(e.actions.is_empty());
        assert_eq!(e.drones["a"].state, HealthState::Healthy);
        let e = evaluate(1000, &s, &d, &fails("a", &[1, 2], 1000));
        assert!(e.actions.is_empty());
        assert_eq!(e.drones["a"].state, HealthState::Recommended);
    }

    #[test]
    fn three_corroborated_fresh_failures_quarantine_with_evidence() {
        let s = settings();
        let d = [drone("a"), drone("b")];
        let e = evaluate(1000, &s, &d, &fails("a", &[1, 2, 3], 990));
        assert_eq!(e.drones["a"].state, HealthState::Quarantined);
        let Action::Quarantine {
            drone,
            reason,
            evidence,
            strikes,
        } = &e.actions[0]
        else {
            panic!("{:?}", e.actions);
        };
        assert_eq!((drone.as_str(), *strikes, evidence.len()), ("a", 0, 3));
        assert!(
            reason.contains("3 host-caused failures in 3 distinct runs"),
            "{reason}"
        );
        assert_eq!(e.drones["b"].state, HealthState::Healthy);
    }

    #[test]
    fn retries_of_one_run_do_not_corroborate() {
        let s = settings();
        let d = [drone("a"), drone("b")];
        let e = evaluate(1000, &s, &d, &fails("a", &[7, 7, 7, 7], 990));
        assert!(e.actions.is_empty());
        assert_eq!(e.drones["a"].state, HealthState::Recommended);
    }

    #[test]
    fn code_failures_and_cancellations_never_count_against_a_host() {
        let s = settings();
        let d = [drone("a"), drone("b")];
        let outcomes: Vec<Outcome> = (1..=10)
            .map(|r| out("a", r, 990, OutcomeKind::Other))
            .collect();
        let e = evaluate(1000, &s, &d, &outcomes);
        assert!(e.actions.is_empty());
        assert_eq!(e.drones["a"].score_permille, 0);
    }

    #[test]
    fn old_evidence_decays_below_the_thresholds() {
        let s = settings();
        let d = [drone("a"), drone("b")];
        // Three failures, five half-lives ago: weight 31 each.
        let e = evaluate(5 * 3600 + 1000, &s, &d, &fails("a", &[1, 2, 3], 1000));
        assert!(e.actions.is_empty());
        assert_eq!(e.drones["a"].state, HealthState::Healthy);
        // Outside the window they are ignored entirely.
        let e = evaluate(13 * 3600, &s, &d, &fails("a", &[1, 2, 3], 0));
        assert_eq!(e.drones["a"].host_failures, 0);
    }

    #[test]
    fn fleet_wide_failures_only_recommend() {
        let s = settings();
        let d = [drone("a"), drone("b"), drone("c")];
        let mut o = fails("a", &[1, 2, 3], 990);
        o.extend(fails("b", &[4], 990));
        o.push(out("c", 5, 990, OutcomeKind::Ok));
        let e = evaluate(1000, &s, &d, &o);
        assert!(e.systemic);
        assert!(e.actions.is_empty());
        assert_eq!(e.drones["a"].state, HealthState::Recommended);
        assert!(
            e.drones["a"]
                .note
                .as_deref()
                .unwrap()
                .contains("fleet-wide")
        );
    }

    #[test]
    fn bounds_keep_the_last_hosts_and_the_auto_limit() {
        let s = settings();
        // The only fresh host is never auto-quarantined.
        let e = evaluate(1000, &s, &[drone("a")], &fails("a", &[1, 2, 3], 990));
        assert!(e.actions.is_empty());
        assert_eq!(e.drones["a"].state, HealthState::Recommended);
        // Five hosts, two bad (40%, below the fleet-wide share): both can go
        // (limit 2); the worse goes first.
        let d = [drone("a"), drone("b"), drone("c"), drone("d"), drone("e")];
        let mut o = fails("a", &[1, 2, 3], 990);
        o.extend(fails("b", &[4, 5, 6, 7], 990));
        o.extend((10..13).map(|r| out("c", r, 990, OutcomeKind::Ok)));
        o.extend((20..23).map(|r| out("d", r, 990, OutcomeKind::Ok)));
        o.extend((30..33).map(|r| out("e", r, 990, OutcomeKind::Ok)));
        let e = evaluate(1000, &s, &d, &o);
        let names: Vec<&str> = e.actions.iter().map(Action::drone).collect();
        assert_eq!(names, ["b", "a"]);
        // Limit of one: only the worst.
        let one = HealthSettings {
            max_auto_quarantined: 1,
            ..s.clone()
        };
        let e = evaluate(1000, &one, &d, &o);
        assert_eq!(e.actions.len(), 1);
        assert_eq!(e.drones["a"].state, HealthState::Recommended);
        // Auto off: recommendations only.
        let off = HealthSettings {
            auto_quarantine: false,
            ..s
        };
        let e = evaluate(1000, &off, &d, &o);
        assert!(e.actions.is_empty());
    }

    #[test]
    fn cooldown_then_probation_then_proof_releases_and_failure_requarantines() {
        let s = settings();
        let mut a = drone("a");
        let d_b = drone("b");
        a.hold = Hold::Auto {
            since: 1000,
            strikes: 0,
        };
        // Before the cooldown ends: nothing.
        let e = evaluate(1000 + 1799, &s, &[a.clone(), d_b.clone()], &[]);
        assert!(e.actions.is_empty());
        assert_eq!(e.drones["a"].probation_at, Some(2800));
        // After: probation.
        let e = evaluate(2800, &s, &[a.clone(), d_b.clone()], &[]);
        assert_eq!(e.actions, [Action::BeginProbation { drone: "a".into() }]);

        a.hold = Hold::Probation {
            since: 2800,
            strikes: 0,
        };
        // One success is not enough; old successes before probation do not count.
        let o = vec![
            out("a", 1, 2000, OutcomeKind::Ok),
            out("a", 2, 2900, OutcomeKind::Ok),
        ];
        let e = evaluate(3000, &s, &[a.clone(), d_b.clone()], &o);
        assert!(e.actions.is_empty());
        assert_eq!(e.drones["a"].proof_jobs_done, 1);
        // Two successes: released.
        let o = vec![
            out("a", 2, 2900, OutcomeKind::Ok),
            out("a", 3, 2950, OutcomeKind::Ok),
        ];
        let e = evaluate(3000, &s, &[a.clone(), d_b.clone()], &o);
        assert_eq!(e.actions, [Action::Release { drone: "a".into() }]);
        // A host failure on probation wins over successes.
        let mut o = o;
        o.push(out("a", 4, 2960, OutcomeKind::HostFailure));
        let e = evaluate(3000, &s, &[a, d_b], &o);
        let Action::Requarantine { strikes, .. } = &e.actions[0] else {
            panic!("{:?}", e.actions);
        };
        assert_eq!(*strikes, 1);
    }

    #[test]
    fn cooldown_doubles_per_strike_up_to_the_cap() {
        let s = settings();
        assert_eq!(s.cooldown_for(0), 1800);
        assert_eq!(s.cooldown_for(1), 3600);
        assert_eq!(s.cooldown_for(2), 7200);
        assert_eq!(s.cooldown_for(3), 14_400);
        assert_eq!(s.cooldown_for(10), 14_400);
        assert_eq!(s.cooldown_for(u32::MAX), 14_400);
    }

    #[test]
    fn evidence_before_a_return_to_service_is_forgotten() {
        let s = settings();
        let mut a = drone("a");
        a.cleared_at = Some(995);
        let d = [a, drone("b")];
        let e = evaluate(1000, &s, &d, &fails("a", &[1, 2, 3], 990));
        assert!(e.actions.is_empty());
        assert_eq!(e.drones["a"].host_failures, 0);
    }

    #[test]
    fn operator_quarantines_are_never_lifted_automatically() {
        let s = settings();
        let mut a = drone("a");
        a.hold = Hold::Operator;
        let e = evaluate(10_000_000, &s, &[a], &[]);
        assert!(e.actions.is_empty());
        assert_eq!(e.drones["a"].source.as_deref(), Some("operator"));
    }

    #[test]
    fn evaluation_is_deterministic() {
        let s = settings();
        let d = [drone("a"), drone("b"), drone("c"), drone("d")];
        let mut o = fails("a", &[1, 2, 3], 990);
        o.extend(fails("b", &[4, 5, 6], 990));
        assert_eq!(evaluate(1000, &s, &d, &o), evaluate(1000, &s, &d, &o));
    }

    #[test]
    fn settings_validate() {
        assert!(HealthSettings::default().validate().is_ok());
        for bad in [
            HealthSettings {
                half_life_secs: 0,
                ..HealthSettings::default()
            },
            HealthSettings {
                corroboration_runs: 0,
                ..HealthSettings::default()
            },
            HealthSettings {
                recommend_score_permille: 9000,
                ..HealthSettings::default()
            },
            HealthSettings {
                systemic_share_pct: 0,
                ..HealthSettings::default()
            },
        ] {
            assert!(bad.validate().is_err());
        }
    }
}
