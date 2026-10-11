//! Homeostasis: a damped controller for the warm runner floors.
//!
//! The base `warm_floors` and the clock-based `warm_schedule` say how many idle
//! runners to keep warm. This controller adds a negative-feedback loop on
//! queue latency: when the oldest queued job of a `(class, scope)` has waited
//! longer than the target for a sustained period and the fleet has idle room, it
//! raises that key's dynamic floor one step; when demand has subsided it lowers
//! it one at a time. The effective floor is `max(schedule floor, dynamic
//! level)`, so the schedule stays a minimum.
//!
//! Damping, all of it configurable:
//! * **Sustained, not instantaneous.** The latency must stay above the target
//!   for `raise_after_secs` without a break (and below the low-water mark for
//!   `lower_after_secs` to lower). A reading in between resets both timers: that
//!   dead band is the hysteresis.
//! * **Step size and cooldown.** One `step` up or one runner down per change,
//!   and `cooldown_secs` between changes of a key.
//! * **Rate limit.** At most `max_changes_per_hour` changes per key.
//! * **Bounds.** A per-class `max_floor`, and no raise without idle room for it.
//!
//! The controller reads no clock (the caller passes `now`) and keeps its state
//! in memory: after a restart it starts from the schedule and re-learns.

use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};

use crate::classes::{Class, Scope};

/// Tunables; opt-in via `enabled`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HomeostasisSettings {
    pub enabled: bool,
    /// Queue latency (oldest queued job age) the controller steers below.
    pub target_secs: u64,
    /// Latency must exceed the target this long without a break to raise.
    pub raise_after_secs: u64,
    /// Latency must stay below `low_pct` of the target (or nothing queued)
    /// this long without a break to lower.
    pub lower_after_secs: u64,
    pub low_pct: u32,
    /// Runners added per raise.
    pub step: u32,
    /// Minimum time between two changes of the same key.
    pub cooldown_secs: u64,
    pub max_changes_per_hour: u32,
    /// Ceiling on the dynamic floor per class.
    pub max_floor: BTreeMap<Class, u32>,
}

impl Default for HomeostasisSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            target_secs: 60,
            raise_after_secs: 120,
            lower_after_secs: 900,
            low_pct: 25,
            step: 1,
            cooldown_secs: 120,
            max_changes_per_hour: 6,
            max_floor: BTreeMap::from([(Class::Light, 6), (Class::Medium, 3), (Class::Heavy, 1)]),
        }
    }
}

impl HomeostasisSettings {
    pub fn validate(&self) -> Result<(), String> {
        if self.target_secs == 0 || self.raise_after_secs == 0 || self.lower_after_secs == 0 {
            return Err(
                "homeostasis target_secs, raise_after_secs and lower_after_secs must be positive"
                    .into(),
            );
        }
        if self.step == 0 || self.max_changes_per_hour == 0 {
            return Err("homeostasis step and max_changes_per_hour must be positive".into());
        }
        if self.low_pct >= 100 {
            return Err("homeostasis low_pct must be below 100".into());
        }
        Ok(())
    }

    fn cap(&self, class: Class) -> u32 {
        self.max_floor.get(&class).copied().unwrap_or(0)
    }

    fn low_water_secs(&self) -> u64 {
        self.target_secs * u64::from(self.low_pct) / 100
    }
}

/// One key's reading for a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyInput {
    pub class: Class,
    pub scope: Scope,
    /// Age of the oldest queued job; 0 with nothing queued.
    pub latency_secs: u64,
    pub queued: u32,
    /// The floor from the base config and the clock schedule.
    pub schedule_floor: u32,
    /// Slots the fleet could still hand out for the class.
    pub idle_slots: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Latency within the dead band.
    Steady,
    /// Latency above the target, timing toward a raise.
    Over,
    /// Latency at or below the low-water mark, timing toward a lower.
    Under,
}

/// What `/status` shows per key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyReport {
    pub class: Class,
    pub scope: Scope,
    pub latency_secs: u64,
    pub schedule_floor: u32,
    /// The controller's own level (0 when idle).
    pub level: u32,
    /// What the allocator uses: `max(schedule_floor, level)`.
    pub effective_floor: u32,
    pub phase: Phase,
    /// How long the current phase has lasted without a break.
    pub phase_secs: u64,
    pub last_change_at: Option<i64>,
    pub changes_last_hour: u32,
    /// Why a due change was held back.
    pub blocked: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct KeyState {
    level: u32,
    over_since: Option<i64>,
    under_since: Option<i64>,
    last_change: Option<i64>,
    changes: VecDeque<i64>,
}

#[derive(Debug, Default)]
pub struct Controller {
    state: BTreeMap<(Class, Scope), KeyState>,
}

impl Controller {
    pub fn new() -> Self {
        Self::default()
    }

    /// One control step for every input key. Returns the report per key.
    pub fn step(
        &mut self,
        now: i64,
        s: &HomeostasisSettings,
        inputs: &[KeyInput],
    ) -> Vec<KeyReport> {
        inputs.iter().map(|i| self.step_key(now, s, i)).collect()
    }

    fn step_key(&mut self, now: i64, s: &HomeostasisSettings, i: &KeyInput) -> KeyReport {
        let st = self.state.entry((i.class, i.scope)).or_default();
        while st.changes.front().is_some_and(|&t| now - t >= 3600) {
            st.changes.pop_front();
        }
        let effective = i.schedule_floor.max(st.level);
        let over = i.latency_secs > s.target_secs;
        let under = i.queued == 0 || i.latency_secs <= s.low_water_secs();

        // Phase timers: a reading outside a phase breaks it.
        if over {
            st.over_since.get_or_insert(now);
            st.under_since = None;
        } else {
            st.over_since = None;
            if under {
                st.under_since.get_or_insert(now);
            } else {
                st.under_since = None;
            }
        }
        let (phase, since) = match (st.over_since, st.under_since) {
            (Some(t), _) => (Phase::Over, Some(t)),
            (_, Some(t)) => (Phase::Under, Some(t)),
            _ => (Phase::Steady, None),
        };

        let cooled = st
            .last_change
            .is_none_or(|t| now - t >= i64::try_from(s.cooldown_secs).unwrap_or(i64::MAX));
        let limited = u32::try_from(st.changes.len()).unwrap_or(u32::MAX) >= s.max_changes_per_hour;
        let held = |why: &str| Some(why.to_owned());
        let mut blocked = None;

        match phase {
            Phase::Over
                if since.is_some_and(|t| {
                    now - t >= i64::try_from(s.raise_after_secs).unwrap_or(i64::MAX)
                }) =>
            {
                let want = (effective + s.step).min(s.cap(i.class));
                if want <= effective {
                    blocked = held("at the floor ceiling for the class");
                } else if i.idle_slots < want - effective {
                    blocked = held("no idle capacity for another warm runner");
                } else if !cooled {
                    blocked = held("cooling down");
                } else if limited {
                    blocked = held("hourly change limit reached");
                } else {
                    st.level = want;
                    st.last_change = Some(now);
                    st.changes.push_back(now);
                    // Another step needs another sustained period.
                    st.over_since = Some(now);
                }
            }
            Phase::Under
                if since.is_some_and(|t| {
                    now - t >= i64::try_from(s.lower_after_secs).unwrap_or(i64::MAX)
                }) && st.level > i.schedule_floor =>
            {
                if !cooled {
                    blocked = held("cooling down");
                } else if limited {
                    blocked = held("hourly change limit reached");
                } else {
                    st.level -= 1;
                    st.last_change = Some(now);
                    st.changes.push_back(now);
                    st.under_since = Some(now);
                }
            }
            _ => {}
        }
        // A level at or below the schedule is inert; drop it so it cannot
        // resurface when the schedule window closes.
        if st.level <= i.schedule_floor {
            st.level = 0;
        }

        let level = st.level;
        KeyReport {
            class: i.class,
            scope: i.scope,
            latency_secs: i.latency_secs,
            schedule_floor: i.schedule_floor,
            level,
            effective_floor: i.schedule_floor.max(level),
            phase,
            phase_secs: since.map_or(0, |t| u64::try_from(now - t).unwrap_or(0)),
            last_change_at: st.last_change,
            changes_last_hour: u32::try_from(st.changes.len()).unwrap_or(u32::MAX),
            blocked,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> HomeostasisSettings {
        HomeostasisSettings {
            enabled: true,
            ..HomeostasisSettings::default()
        }
    }

    fn key(latency: u64, queued: u32, schedule: u32, idle: u32) -> KeyInput {
        KeyInput {
            class: Class::Light,
            scope: Scope::Pr,
            latency_secs: latency,
            queued,
            schedule_floor: schedule,
            idle_slots: idle,
        }
    }

    fn one(c: &mut Controller, now: i64, s: &HomeostasisSettings, k: KeyInput) -> KeyReport {
        c.step(now, s, &[k]).remove(0)
    }

    #[test]
    fn a_sustained_overshoot_raises_one_step_then_waits_for_another_sustained_period() {
        let s = settings();
        let mut c = Controller::new();
        let hot = key(200, 3, 0, 8);
        assert_eq!(one(&mut c, 0, &s, hot).effective_floor, 0);
        assert_eq!(one(&mut c, 60, &s, hot).effective_floor, 0);
        // 120s of continuous overshoot: raise by one step.
        let r = one(&mut c, 120, &s, hot);
        assert_eq!((r.level, r.effective_floor, r.phase), (1, 1, Phase::Over));
        // Still hot, but a further step needs another full sustained period.
        assert_eq!(one(&mut c, 180, &s, hot).level, 1);
        assert_eq!(one(&mut c, 240, &s, hot).level, 2);
        assert_eq!(one(&mut c, 360, &s, hot).level, 3);
    }

    #[test]
    fn a_blip_never_raises_and_flapping_readings_reset_the_timer() {
        let s = settings();
        let mut c = Controller::new();
        for t in (0..3600).step_by(10) {
            // Alternates above and below the target every tick.
            let latency = if (t / 10) % 2 == 0 { 300 } else { 40 };
            let r = one(&mut c, t, &s, key(latency, 1, 0, 8));
            assert_eq!(r.level, 0, "t={t}");
        }
    }

    #[test]
    fn the_ceiling_and_idle_capacity_bound_the_raise() {
        let s = settings();
        let mut c = Controller::new();
        // No idle room: held back, with the reason.
        let r = (0..=12)
            .map(|n| one(&mut c, n * 60, &s, key(500, 5, 0, 0)))
            .last()
            .unwrap();
        assert_eq!(r.level, 0);
        assert!(r.blocked.as_deref().unwrap().contains("idle capacity"));
        // Heavy has a ceiling of 1.
        let mut c = Controller::new();
        let heavy = KeyInput {
            class: Class::Heavy,
            ..key(500, 5, 0, 8)
        };
        let r = (0..=20)
            .map(|n| one(&mut c, n * 130, &s, heavy))
            .last()
            .unwrap();
        assert_eq!(r.level, 1);
        assert!(r.blocked.as_deref().unwrap().contains("ceiling"));
    }

    #[test]
    fn demand_subsiding_lowers_one_runner_at_a_time_after_a_quiet_period() {
        // Room in the hourly limit, so the lowering is not rate limited.
        let s = HomeostasisSettings {
            max_changes_per_hour: 20,
            ..settings()
        };
        let mut c = Controller::new();
        for n in 0..=6 {
            one(&mut c, n * 130, &s, key(500, 5, 0, 8));
        }
        let peak = one(&mut c, 800, &s, key(500, 5, 0, 8)).level;
        assert!(peak >= 3, "{peak}");
        // Quiet: nothing for a while, then one step down per quiet period.
        let t0 = 1000;
        assert_eq!(one(&mut c, t0, &s, key(0, 0, 0, 8)).level, peak);
        assert_eq!(one(&mut c, t0 + 899, &s, key(0, 0, 0, 8)).level, peak);
        assert_eq!(one(&mut c, t0 + 900, &s, key(0, 0, 0, 8)).level, peak - 1);
        assert_eq!(one(&mut c, t0 + 1799, &s, key(0, 0, 0, 8)).level, peak - 1);
        assert_eq!(one(&mut c, t0 + 1800, &s, key(0, 0, 0, 8)).level, peak - 2);
    }

    #[test]
    fn the_dead_band_between_low_water_and_target_changes_nothing() {
        let s = settings();
        let mut c = Controller::new();
        for n in 0..=3 {
            one(&mut c, n * 130, &s, key(500, 5, 0, 8));
        }
        let level = one(&mut c, 400, &s, key(500, 5, 0, 8)).level;
        // 40s is below the 60s target and above the 15s low-water mark.
        for t in (500..10_000).step_by(30) {
            let r = one(&mut c, t, &s, key(40, 2, 0, 8));
            assert_eq!((r.level, r.phase), (level, Phase::Steady), "t={t}");
        }
    }

    #[test]
    fn the_schedule_is_a_minimum_and_an_inert_level_does_not_resurface() {
        let s = settings();
        let mut c = Controller::new();
        // Schedule floor 3 covers the controller's level of 1: effective 3.
        for n in 0..=2 {
            one(&mut c, n * 130, &s, key(500, 5, 3, 8));
        }
        let r = one(&mut c, 300, &s, key(500, 5, 3, 8));
        assert_eq!(r.effective_floor, 4.max(r.level));
        // The window closes: the dynamic level, not the schedule, now decides.
        let mut c = Controller::new();
        let r = one(&mut c, 0, &s, key(0, 0, 3, 8));
        assert_eq!((r.level, r.effective_floor), (0, 3));
        let r = one(&mut c, 10, &s, key(0, 0, 0, 8));
        assert_eq!(r.effective_floor, 0);
    }

    #[test]
    fn cooldown_and_the_hourly_limit_cap_the_change_rate() {
        let s = HomeostasisSettings {
            raise_after_secs: 10,
            cooldown_secs: 300,
            max_changes_per_hour: 3,
            max_floor: BTreeMap::from([(Class::Light, 50)]),
            ..settings()
        };
        let mut c = Controller::new();
        let mut changes = Vec::new();
        let mut last = 0;
        for t in (0..3600).step_by(10) {
            let r = one(&mut c, t, &s, key(500, 5, 0, 100));
            if r.level != last {
                changes.push(t);
                last = r.level;
            }
        }
        assert_eq!(changes.len(), 3, "{changes:?}");
        assert!(
            changes.windows(2).all(|w| w[1] - w[0] >= 300),
            "{changes:?}"
        );
        // An hour after the first change the limit window slides.
        let r = one(&mut c, 3700, &s, key(500, 5, 0, 100));
        assert!(r.changes_last_hour <= 3);
    }

    #[test]
    fn a_square_wave_load_does_not_thrash() {
        let s = settings();
        let mut c = Controller::new();
        let mut events: Vec<(i64, i64)> = Vec::new(); // (time, +1/-1)
        let mut last = 0u32;
        // Eight hours: 10 minutes of heavy queueing, 10 minutes quiet, repeat.
        for t in (0..8 * 3600).step_by(10) {
            let busy = (t / 600) % 2 == 0;
            let k = if busy {
                key(300, 6, 0, 20)
            } else {
                key(0, 0, 0, 20)
            };
            let r = one(&mut c, t, &s, k);
            if r.level != last {
                events.push((t, if r.level > last { 1 } else { -1 }));
                last = r.level;
            }
        }
        // Bounded by the rate limit, and no two changes inside the cooldown.
        assert!(events.len() <= 8 * 6, "{events:?}");
        assert!(
            events.windows(2).all(|w| w[1].0 - w[0].0 >= 120),
            "{events:?}"
        );
        // Opposite-direction changes never happen back to back within the
        // lower-after period: a raise is never undone by a quick lower.
        assert!(
            events
                .windows(2)
                .all(|w| w[0].1 == w[1].1 || w[1].0 - w[0].0 >= 900),
            "{events:?}"
        );
    }

    #[test]
    fn keys_are_independent_and_the_controller_is_deterministic() {
        let s = settings();
        let run = || {
            let mut c = Controller::new();
            let mut out = Vec::new();
            for t in (0..1200).step_by(30) {
                let inputs = [
                    key(300, 4, 0, 8),
                    KeyInput {
                        scope: Scope::Main,
                        ..key(0, 0, 1, 8)
                    },
                ];
                out.push(c.step(t, &s, &inputs));
            }
            out
        };
        let a = run();
        assert_eq!(a, run());
        let last = a.last().unwrap();
        assert!(last[0].level >= 1);
        assert_eq!((last[1].level, last[1].effective_floor), (0, 1));
    }

    #[test]
    fn settings_validate() {
        assert!(HomeostasisSettings::default().validate().is_ok());
        for bad in [
            HomeostasisSettings {
                target_secs: 0,
                ..HomeostasisSettings::default()
            },
            HomeostasisSettings {
                step: 0,
                ..HomeostasisSettings::default()
            },
            HomeostasisSettings {
                low_pct: 100,
                ..HomeostasisSettings::default()
            },
        ] {
            assert!(bad.validate().is_err());
        }
        assert!(!HomeostasisSettings::default().enabled);
    }
}
