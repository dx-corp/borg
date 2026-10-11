//! Learned per-drone, per-class performance profiles.
//!
//! The queen learns which hosts are good at which work from the telemetry
//! drones report, with no host names in configuration. For each drone and
//! class it measures:
//!
//! * **speed**: for each *job family* the host has run enough times, its median
//!   successful duration against the fleet median for the same class and
//!   family (1000 = as fast as the fleet, 1500 = one and a half times faster),
//!   averaged over families by sample count;
//! * **reliability**: one minus the host-caused failure rate (1000 = none),
//!   shrunk toward 1000 while there are few samples. Code failures do not
//!   count against a host.
//!
//! Profiles feed bid scoring ([`crate::bid::profile_term`]): a faster, more
//! reliable host scores higher, in proportion to how much evidence there is.
//! With too little evidence a profile is neutral and has no effect.
//!
//! A *job family* is the job name with matrix suffixes removed (everything from
//! the first `(` or `[`, and trailing digits and separators), lowercased:
//! `rust-test (shard 2)` and `rust-test (shard 7)` are one family, `rust-test`.
//!
//! GitHub, not the queen, chooses which queued job a runner takes, so profiles
//! steer which *host* gets a freshly minted runner; they cannot assign a job.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::bid::ProfileScore;
use crate::classes::Class;
use crate::telemetry::{Conclusion, FailureClass, TelemetryRow};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileSettings {
    /// Learn profiles and let them influence placement.
    pub enabled: bool,
    /// Telemetry newer than this many days is used.
    pub window_days: u32,
    /// Samples a host needs per family (and in total for reliability) before
    /// the evidence counts.
    pub min_samples: u32,
    /// Recompute at most this often.
    pub refresh_secs: u64,
}

impl Default for ProfileSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            window_days: 7,
            min_samples: 5,
            refresh_secs: 300,
        }
    }
}

impl ProfileSettings {
    pub fn validate(&self) -> Result<(), String> {
        if self.window_days == 0 || self.min_samples == 0 || self.refresh_secs == 0 {
            return Err(
                "profiles window_days, min_samples and refresh_secs must be positive".into(),
            );
        }
        Ok(())
    }
}

/// Evidence for one family on one host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FamilyStat {
    pub family: String,
    pub jobs: u32,
    pub median_secs: u64,
    pub fleet_median_secs: u64,
}

/// One drone's profile for one class.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassProfile {
    pub drone: String,
    pub class: Class,
    /// Jobs in the window (successes, host failures and other failures).
    pub jobs: u32,
    /// 1000 = fleet speed; clamped to 500..=2000.
    pub speed_permille: u32,
    /// How much evidence backs `speed_permille` (0..=1000).
    pub speed_confidence_permille: u32,
    /// 1000 = no host-caused failures.
    pub reliability_permille: u32,
    pub families: Vec<FamilyStat>,
}

impl ClassProfile {
    pub fn score(&self) -> ProfileScore {
        ProfileScore {
            class: self.class,
            speed_permille: self.speed_permille,
            speed_confidence_permille: self.speed_confidence_permille,
            reliability_permille: self.reliability_permille,
        }
    }
}

/// The job family of a job name; see the module docs.
pub fn family_of(job_name: &str) -> String {
    let lower = job_name.to_lowercase();
    let cut = lower
        .find(['(', '['])
        .map_or(lower.as_str(), |i| &lower[..i]);
    let mut s = cut.trim().to_owned();
    loop {
        let t =
            s.trim_end_matches(|c: char| c.is_ascii_digit() || matches!(c, '-' | '_' | ' ' | '/'));
        if t.len() == s.len() {
            break;
        }
        s = t.to_owned();
    }
    s
}

fn median(v: &mut [u64]) -> u64 {
    v.sort_unstable();
    let n = v.len();
    if n == 0 {
        return 0;
    }
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2
    }
}

const SPEED_MIN: u64 = 500;
const SPEED_MAX: u64 = 2000;
/// Samples at which confidence is full.
const FULL_CONFIDENCE_AT: u32 = 20;

/// Build every profile from `rows`. Deterministic; sorted by drone and class.
pub fn build_profiles(rows: &[TelemetryRow], s: &ProfileSettings) -> Vec<ClassProfile> {
    // Successful durations per (class, family, drone).
    let mut durations: BTreeMap<(Class, String, &str), Vec<u64>> = BTreeMap::new();
    // Job counts per (drone, class): total and host failures.
    let mut counts: BTreeMap<(&str, Class), (u32, u32)> = BTreeMap::new();
    for r in rows {
        let t = &r.telemetry;
        if t.conclusion == Conclusion::Skipped {
            continue;
        }
        let c = counts.entry((r.drone.as_str(), r.class)).or_default();
        c.0 += 1;
        if t.conclusion != Conclusion::Success && t.failure_class == FailureClass::Host {
            c.1 += 1;
        }
        if t.conclusion == Conclusion::Success {
            durations
                .entry((r.class, family_of(&t.job_name), r.drone.as_str()))
                .or_default()
                .push(r.duration_secs);
        }
    }

    // Fleet medians per (class, family), only where at least two hosts ran it.
    let mut fleet: BTreeMap<(Class, &str), (u64, usize)> = BTreeMap::new();
    let mut families: BTreeSet<(Class, &str)> = BTreeSet::new();
    for (class, family, _) in durations.keys() {
        families.insert((*class, family.as_str()));
    }
    for (class, family) in families {
        let mut all: Vec<u64> = Vec::new();
        let mut drones = 0usize;
        for ((c, f, _), v) in &durations {
            if *c == class && f == family {
                all.extend(v);
                drones += 1;
            }
        }
        if drones >= 2 {
            fleet.insert((class, family), (median(&mut all), drones));
        }
    }

    let min = usize::try_from(s.min_samples).unwrap_or(usize::MAX);
    let mut out = Vec::new();
    for (&(drone, class), &(total, host)) in &counts {
        let mut fams = Vec::new();
        let mut weighted = 0u64;
        let mut samples = 0u32;
        for ((c, family, d), v) in &durations {
            if *c != class || *d != drone || v.len() < min {
                continue;
            }
            let Some(&(fleet_med, _)) = fleet.get(&(class, family.as_str())) else {
                continue;
            };
            let mut mine = v.clone();
            let med = median(&mut mine);
            let n = u32::try_from(v.len()).unwrap_or(u32::MAX);
            let ratio = (fleet_med.saturating_mul(1000) / med.max(1)).clamp(SPEED_MIN, SPEED_MAX);
            weighted += ratio * u64::from(n);
            samples = samples.saturating_add(n);
            fams.push(FamilyStat {
                family: family.clone(),
                jobs: n,
                median_secs: med,
                fleet_median_secs: fleet_med,
            });
        }
        let speed = if samples == 0 {
            1000
        } else {
            u32::try_from(weighted / u64::from(samples)).unwrap_or(1000)
        };
        let reliability = if total >= s.min_samples {
            let failure_permille = u64::from(host) * 1000 / u64::from(total);
            let weight = u64::from(total.min(FULL_CONFIDENCE_AT));
            let shrunk = failure_permille * weight / u64::from(FULL_CONFIDENCE_AT);
            u32::try_from(1000u64.saturating_sub(shrunk)).unwrap_or(1000)
        } else {
            1000
        };
        out.push(ClassProfile {
            drone: drone.to_owned(),
            class,
            jobs: total,
            speed_permille: speed,
            speed_confidence_permille: samples.min(FULL_CONFIDENCE_AT) * 1000 / FULL_CONFIDENCE_AT,
            reliability_permille: reliability,
            families: fams,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classes::Scope;
    use crate::telemetry::JobTelemetry;

    fn row(
        drone: &str,
        class: Class,
        job: &str,
        secs: u64,
        c: Conclusion,
        f: FailureClass,
    ) -> TelemetryRow {
        TelemetryRow {
            lease_id: format!("{drone}-{job}-{secs}"),
            drone: drone.into(),
            class,
            scope: Scope::Pr,
            duration_secs: secs,
            finished_at: 0,
            telemetry: JobTelemetry {
                job_name: job.into(),
                repo: "o/r".into(),
                run_id: 1,
                conclusion: c,
                failure_class: f,
                queue_wait_secs: None,
                steps: vec![],
            },
        }
    }

    fn ok(drone: &str, class: Class, job: &str, secs: u64) -> TelemetryRow {
        row(
            drone,
            class,
            job,
            secs,
            Conclusion::Success,
            FailureClass::Unknown,
        )
    }

    fn profile<'a>(p: &'a [ClassProfile], drone: &str, class: Class) -> &'a ClassProfile {
        p.iter()
            .find(|x| x.drone == drone && x.class == class)
            .unwrap()
    }

    #[test]
    fn families_drop_matrix_suffixes() {
        assert_eq!(family_of("rust-test (shard 2)"), "rust-test");
        assert_eq!(family_of("Rust-Test (shard 7)"), "rust-test");
        assert_eq!(family_of("build [linux, x64]"), "build");
        assert_eq!(family_of("test-shard-12"), "test-shard");
        assert_eq!(
            family_of("rust-lint-and-release-security"),
            "rust-lint-and-release-security"
        );
        assert_eq!(family_of("  lint  "), "lint");
        assert_eq!(family_of("42"), "");
    }

    #[test]
    fn a_faster_host_scores_above_one_and_a_slower_below() {
        let s = ProfileSettings::default();
        let mut rows = Vec::new();
        for i in 0..6 {
            rows.push(ok("fast", Class::Medium, "rust-test (shard 1)", 100 + i));
            rows.push(ok("slow", Class::Medium, "rust-test (shard 2)", 200 + i));
        }
        let p = build_profiles(&rows, &s);
        let fast = profile(&p, "fast", Class::Medium);
        let slow = profile(&p, "slow", Class::Medium);
        assert!(fast.speed_permille > 1200, "{fast:?}");
        assert!(slow.speed_permille < 900, "{slow:?}");
        assert_eq!(fast.families[0].family, "rust-test");
        assert_eq!(fast.speed_confidence_permille, 300); // 6 of 20 samples
        assert_eq!(fast.reliability_permille, 1000);
    }

    #[test]
    fn too_little_evidence_or_a_lone_host_is_neutral() {
        let s = ProfileSettings::default();
        // Four samples each: below min_samples.
        let mut rows = Vec::new();
        for _ in 0..4 {
            rows.push(ok("a", Class::Light, "lint", 10));
            rows.push(ok("b", Class::Light, "lint", 100));
        }
        for p in build_profiles(&rows, &s) {
            assert_eq!(
                (p.speed_permille, p.speed_confidence_permille),
                (1000, 0),
                "{p:?}"
            );
        }
        // One host only: nothing to compare with.
        let rows: Vec<_> = (0..10).map(|_| ok("a", Class::Light, "lint", 10)).collect();
        let p = build_profiles(&rows, &s);
        assert_eq!(profile(&p, "a", Class::Light).speed_confidence_permille, 0);
    }

    #[test]
    fn host_failures_lower_reliability_and_code_failures_do_not() {
        let s = ProfileSettings::default();
        let mut rows = Vec::new();
        for _ in 0..10 {
            rows.push(row(
                "code",
                Class::Light,
                "lint",
                5,
                Conclusion::Failure,
                FailureClass::Code,
            ));
        }
        for _ in 0..5 {
            rows.push(ok("flaky", Class::Light, "lint", 5));
        }
        for _ in 0..5 {
            rows.push(row(
                "flaky",
                Class::Light,
                "lint",
                5,
                Conclusion::Failure,
                FailureClass::Host,
            ));
        }
        let p = build_profiles(&rows, &s);
        assert_eq!(profile(&p, "code", Class::Light).reliability_permille, 1000);
        // 50% host failures over 10 jobs: shrunk by 10/20 to 25% -> 750.
        assert_eq!(profile(&p, "flaky", Class::Light).reliability_permille, 750);
    }

    #[test]
    fn classes_are_profiled_separately_and_skipped_jobs_are_ignored() {
        let s = ProfileSettings::default();
        let mut rows = Vec::new();
        for _ in 0..6 {
            rows.push(ok("a", Class::Light, "lint", 10));
            rows.push(ok("b", Class::Light, "lint", 20));
            rows.push(ok("a", Class::Heavy, "build", 500));
            rows.push(row(
                "a",
                Class::Light,
                "lint",
                1,
                Conclusion::Skipped,
                FailureClass::Unknown,
            ));
        }
        let p = build_profiles(&rows, &s);
        assert_eq!(profile(&p, "a", Class::Light).jobs, 6);
        assert_eq!(profile(&p, "a", Class::Heavy).speed_confidence_permille, 0);
    }

    #[test]
    fn extreme_durations_are_clamped_and_the_result_is_deterministic() {
        let s = ProfileSettings::default();
        let mut rows = Vec::new();
        for _ in 0..6 {
            rows.push(ok("a", Class::Light, "lint", 1));
            rows.push(ok("b", Class::Light, "lint", u64::MAX / 4000));
        }
        let p = build_profiles(&rows, &s);
        assert_eq!(profile(&p, "a", Class::Light).speed_permille, 2000);
        assert!(profile(&p, "b", Class::Light).speed_permille >= 500);
        assert_eq!(p, build_profiles(&rows, &s));
    }

    #[test]
    fn settings_validate() {
        assert!(ProfileSettings::default().validate().is_ok());
        assert!(
            ProfileSettings {
                min_samples: 0,
                ..ProfileSettings::default()
            }
            .validate()
            .is_err()
        );
    }
}
