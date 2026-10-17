//! Drone bids and demand pressure: the "response threshold" half of the
//! allocator.
//!
//! A drone that knows its own state (warm caches, CPU pressure, owner activity,
//! owner caps) reports a [`DroneBid`] per `(class, scope)` in its heartbeat. The
//! queen computes a demand pressure per class ([`demand_pressure`]) and
//! publishes it to drones in the heartbeat response and in `/status`. The
//! allocator ([`crate::allocator::allocate`]) then:
//!
//! 1. drops a bid when the class's pressure is below the bid's `min_pressure`
//!    (the drone's response threshold: low for a warm idle host, higher for a
//!    cold one, high for a workstation whose owner is active);
//! 2. drops a bid with no `available_slots` left;
//! 3. scores the rest with [`score`], an integer function of the bid, and picks
//!    the highest score, breaking ties by tightest memory fit and then by id.
//!
//! A drone that reports no bids at all (an older drone, or one whose bids have
//! expired) is a *legacy* drone: always eligible, neutral score. A drone that
//! reports bids but none for a given `(class, scope)` declines that key.
//!
//! Everything is integers (permille, milliseconds) so scoring is deterministic
//! and `Eq`, and the allocator stays a pure function. This module does not read
//! a clock: the queen drops expired bids (`expires_at`) before calling the
//! allocator.

use serde::{Deserialize, Serialize};

use crate::classes::{Class, Scope};

/// 1.0 in permille.
pub const PERMILLE: u16 = 1000;

/// One drone's offer for one `(class, scope)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DroneBid {
    pub class: Class,
    pub scope: Scope,
    /// How many more leases of this key the drone would take right now.
    pub available_slots: u32,
    /// Estimated time from lease grant to the runner taking a job.
    #[serde(default)]
    pub estimated_start_ms: u64,
    /// Share (permille) of the work this class typically needs that is warm in
    /// the host caches. 1000 = fully warm.
    #[serde(default)]
    pub cache_affinity: u16,
    /// Host load (permille): CPU/IO pressure, owner activity folded in.
    #[serde(default)]
    pub resource_pressure: u16,
    /// Relative cost of a slot-hour on this host; 1000 = fleet typical.
    #[serde(default = "typical_cost")]
    pub estimated_cost: u32,
    /// How much the drone trusts its own numbers (permille); the score is
    /// shrunk toward neutral as confidence falls.
    #[serde(default = "full")]
    pub confidence: u16,
    /// Unix time after which the queen ignores this bid.
    pub expires_at: i64,
    /// Response threshold: the demand pressure (permille) below which the drone
    /// does not want this work. 0 = always willing.
    #[serde(default)]
    pub min_pressure: u16,
}

fn typical_cost() -> u32 {
    1000
}
fn full() -> u16 {
    PERMILLE
}

impl DroneBid {
    /// Clamp the permille fields to range. Applied to everything a drone
    /// sends.
    #[must_use]
    pub fn clamped(mut self) -> Self {
        self.cache_affinity = self.cache_affinity.min(PERMILLE);
        self.resource_pressure = self.resource_pressure.min(PERMILLE);
        self.confidence = self.confidence.min(PERMILLE);
        self.min_pressure = self.min_pressure.min(PERMILLE);
        self.estimated_cost = self.estimated_cost.min(100_000);
        self
    }
}

/// Weights of the score terms. Integers, so the result is exact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScoreWeights {
    pub cache_affinity: i64,
    pub resource_pressure: i64,
    /// Applied to the start estimate scaled to permille of [`START_CAP_MS`].
    pub start: i64,
    /// Applied to the cost's excess over typical (permille); cheaper hosts
    /// score higher.
    pub cost: i64,
    /// Applied to a learned profile's speed excess over the fleet (permille),
    /// scaled by its confidence.
    pub profile_speed: i64,
    /// Applied to a learned profile's reliability shortfall (permille).
    pub profile_reliability: i64,
    /// Applied to the drone's current CPU pressure (permille; PSI, or the
    /// load estimate without PSI): a loaded host scores lower.
    pub(crate) host_pressure: i64,
    /// Applied to the share of the drone's vCPU budget still free (permille,
    /// divided by 1000) for medium and heavy work: an idle host scores higher
    /// than one already running builds.
    pub(crate) idle_share: i64,
}

impl Default for ScoreWeights {
    fn default() -> Self {
        Self {
            cache_affinity: 3,
            resource_pressure: 3,
            start: 2,
            cost: 1,
            profile_speed: 1,
            profile_reliability: 3,
            host_pressure: 1,
            idle_share: 250,
        }
    }
}

/// Start estimates are capped here before weighing.
pub const START_CAP_MS: u64 = 60_000;

/// Deterministic score of a bid; higher wins.
///
/// `weights.cache * affinity - weights.pressure * pressure - weights.start *
/// start - weights.cost * (cost - typical)`, then scaled by confidence.
pub fn score(bid: &DroneBid, w: &ScoreWeights) -> i64 {
    let bid = bid.clamped();
    let start = i64::try_from(bid.estimated_start_ms.min(START_CAP_MS) * 1000 / START_CAP_MS)
        .unwrap_or(1000);
    let cost = i64::from(bid.estimated_cost) - 1000;
    let raw = w.cache_affinity * i64::from(bid.cache_affinity)
        - w.resource_pressure * i64::from(bid.resource_pressure)
        - w.start * start
        - w.cost * cost;
    raw * i64::from(bid.confidence) / i64::from(PERMILLE)
}

/// What the queen has learned about a drone for one class, reduced to what the
/// score needs. See [`crate::profile`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileScore {
    pub class: Class,
    /// 1000 = fleet speed.
    pub speed_permille: u32,
    pub speed_confidence_permille: u32,
    /// 1000 = no host-caused failures.
    pub reliability_permille: u32,
}

/// The score contribution of a learned profile: faster than the fleet adds (in
/// proportion to the evidence), host-caused failures subtract.
pub fn profile_term(p: &ProfileScore, w: &ScoreWeights) -> i64 {
    let speed = i64::from(p.speed_permille.clamp(500, 2000)) - 1000;
    let conf = i64::from(p.speed_confidence_permille.min(1000));
    let rel = i64::from(p.reliability_permille.min(1000)) - 1000;
    w.profile_speed * speed * conf / 1000 + w.profile_reliability * rel
}

/// Demand pressure for one class, as published to drones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassPressure {
    pub class: Class,
    /// 0 (nothing waiting for a runner) to 1000 (saturated or starving).
    pub pressure: u16,
}

/// Pressure (permille) from queued jobs that still need a runner
/// (`shortfall`), the idle slots the fleet has for the class, and whether any
/// queued job of the class has starved. 0 with no shortfall; 1000 when
/// starving or when the shortfall is at least twice the idle slots.
pub fn demand_pressure(shortfall: u32, idle_slots: u32, starving: bool) -> u16 {
    if shortfall == 0 {
        return 0;
    }
    if starving {
        return PERMILLE;
    }
    let p = u64::from(shortfall) * 500 / u64::from(idle_slots.max(1));
    u16::try_from(p.min(u64::from(PERMILLE))).unwrap_or(PERMILLE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bid() -> DroneBid {
        DroneBid {
            class: Class::Light,
            scope: Scope::Pr,
            available_slots: 4,
            estimated_start_ms: 0,
            cache_affinity: 0,
            resource_pressure: 0,
            estimated_cost: 1000,
            confidence: PERMILLE,
            expires_at: 0,
            min_pressure: 0,
        }
    }

    #[test]
    fn warmer_idler_cheaper_faster_hosts_score_higher() {
        let w = ScoreWeights::default();
        let base = score(&bid(), &w);
        assert_eq!(base, 0);
        assert!(
            score(
                &DroneBid {
                    cache_affinity: 800,
                    ..bid()
                },
                &w
            ) > base
        );
        assert!(
            score(
                &DroneBid {
                    resource_pressure: 500,
                    ..bid()
                },
                &w
            ) < base
        );
        assert!(
            score(
                &DroneBid {
                    estimated_start_ms: 30_000,
                    ..bid()
                },
                &w
            ) < base
        );
        assert!(
            score(
                &DroneBid {
                    estimated_cost: 500,
                    ..bid()
                },
                &w
            ) > base
        );
        assert!(
            score(
                &DroneBid {
                    estimated_cost: 2000,
                    ..bid()
                },
                &w
            ) < base
        );
    }

    #[test]
    fn exact_values_and_confidence_shrinkage() {
        let w = ScoreWeights::default();
        let b = DroneBid {
            cache_affinity: 1000,
            resource_pressure: 200,
            estimated_start_ms: 30_000,
            estimated_cost: 1100,
            ..bid()
        };
        // 3*1000 - 3*200 - 2*500 - 1*100 = 1300
        assert_eq!(score(&b, &w), 1300);
        assert_eq!(
            score(
                &DroneBid {
                    confidence: 500,
                    ..b
                },
                &w
            ),
            650
        );
        assert_eq!(score(&DroneBid { confidence: 0, ..b }, &w), 0);
    }

    #[test]
    fn hostile_numbers_are_clamped_not_overflowed() {
        let w = ScoreWeights::default();
        let b = DroneBid {
            cache_affinity: u16::MAX,
            resource_pressure: u16::MAX,
            estimated_start_ms: u64::MAX,
            estimated_cost: u32::MAX,
            confidence: u16::MAX,
            min_pressure: u16::MAX,
            ..bid()
        };
        let c = b.clamped();
        assert_eq!(
            (
                c.cache_affinity,
                c.resource_pressure,
                c.confidence,
                c.min_pressure
            ),
            (1000, 1000, 1000, 1000)
        );
        let _ = score(&b, &w);
    }

    #[test]
    fn pressure_rises_with_shortfall_and_falls_with_idle_capacity() {
        assert_eq!(demand_pressure(0, 0, true), 0);
        assert_eq!(demand_pressure(1, 8, false), 62);
        assert_eq!(demand_pressure(4, 8, false), 250);
        assert_eq!(demand_pressure(1, 0, false), 500);
        assert_eq!(demand_pressure(2, 0, false), 1000);
        assert_eq!(demand_pressure(100, 1, false), 1000);
        assert_eq!(demand_pressure(1, 100, true), 1000);
    }

    #[test]
    fn profiles_add_for_speed_in_proportion_to_evidence_and_subtract_for_unreliability() {
        let w = ScoreWeights::default();
        let p = |speed, conf, rel| ProfileScore {
            class: Class::Light,
            speed_permille: speed,
            speed_confidence_permille: conf,
            reliability_permille: rel,
        };
        assert_eq!(profile_term(&p(1000, 1000, 1000), &w), 0);
        assert_eq!(profile_term(&p(1500, 1000, 1000), &w), 500);
        assert_eq!(profile_term(&p(1500, 500, 1000), &w), 250);
        assert_eq!(profile_term(&p(1500, 0, 1000), &w), 0);
        assert_eq!(profile_term(&p(1000, 1000, 800), &w), -600);
        assert_eq!(profile_term(&p(u32::MAX, 5000, u32::MAX), &w), 1000);
    }

    #[test]
    fn wire_format_defaults() {
        let b: DroneBid = serde_json::from_str(
            r#"{"class":"light","scope":"pr","available_slots":2,"expires_at":99}"#,
        )
        .unwrap();
        assert_eq!(
            (b.confidence, b.estimated_cost, b.min_pressure),
            (1000, 1000, 0)
        );
    }
}
