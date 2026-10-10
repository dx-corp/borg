//! Time-of-day warm runner pools.
//!
//! The base warm floors (`warm_floors`) keep a number of idle runners alive per
//! class and scope all day. A schedule raises them during configured windows:
//! for example more warm light pull-request runners from 08:00 to 20:00 on
//! weekdays. The result is still just a list of [`WarmFloor`]s for the
//! allocator, so every allocator guarantee (never over-grant, main first)
//! holds unchanged.
//!
//! Times are wall-clock times at a fixed UTC offset
//! (`utc_offset_minutes`). The `time` crate has no zone database; the offset
//! does not follow daylight saving, so change it twice a year or write the
//! windows in UTC.

use std::collections::BTreeMap;

use serde::Deserialize;
use time::{OffsetDateTime, UtcOffset, Weekday};

use crate::allocator::WarmFloor;
use crate::classes::{Class, Scope};

const MINUTES_PER_DAY: u16 = 24 * 60;

/// Minutes since midnight, parsed from `HH:MM` (`24:00` is allowed as an end).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct TimeOfDay(u16);

impl TryFrom<String> for TimeOfDay {
    type Error = String;

    fn try_from(s: String) -> Result<Self, String> {
        let bad = || format!("{s:?} is not a time of day like 08:00");
        let (h, m) = s.split_once(':').ok_or_else(bad)?;
        let digits = |p: &str| p.len() == 2 && p.bytes().all(|b| b.is_ascii_digit());
        if !digits(h) || !digits(m) {
            return Err(bad());
        }
        let (h, m): (u16, u16) = (h.parse().map_err(|_| bad())?, m.parse().map_err(|_| bad())?);
        let total = h * 60 + m;
        if m > 59 || total > MINUTES_PER_DAY {
            return Err(bad());
        }
        Ok(Self(total))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Day {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl Day {
    fn of(w: Weekday) -> Self {
        match w {
            Weekday::Monday => Self::Mon,
            Weekday::Tuesday => Self::Tue,
            Weekday::Wednesday => Self::Wed,
            Weekday::Thursday => Self::Thu,
            Weekday::Friday => Self::Fri,
            Weekday::Saturday => Self::Sat,
            Weekday::Sunday => Self::Sun,
        }
    }
}

/// Keep `count` warm runners of `(class, scope)` between `start` and `end`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WarmWindow {
    pub class: Class,
    pub scope: Scope,
    pub count: u32,
    pub start: TimeOfDay,
    /// Exclusive. An `end` before `start` wraps past midnight.
    pub end: TimeOfDay,
    /// Days the window starts on; every day when absent.
    #[serde(default)]
    pub days: Option<Vec<Day>>,
}

impl WarmWindow {
    fn active(&self, day: Day, prev_day: Day, minute: u16) -> bool {
        let on = |d: Day| self.days.as_ref().is_none_or(|ds| ds.contains(&d));
        let (start, end) = (self.start.0, self.end.0);
        if start < end {
            on(day) && (start..end).contains(&minute)
        } else {
            // Wraps midnight: the evening part belongs to the starting day,
            // the morning part to the day before.
            (on(day) && minute >= start) || (on(prev_day) && minute < end)
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WarmSchedule {
    /// Offset of the wall clock the windows are written in, in minutes east of
    /// UTC (US Pacific standard time is -480).
    #[serde(default)]
    pub utc_offset_minutes: i32,
    #[serde(default)]
    pub windows: Vec<WarmWindow>,
}

impl WarmSchedule {
    pub fn validate(&self) -> Result<(), String> {
        if UtcOffset::from_whole_seconds(self.utc_offset_minutes.saturating_mul(60)).is_err() {
            return Err("warm_schedule.utc_offset_minutes is out of range".to_owned());
        }
        for w in &self.windows {
            if w.start == w.end {
                return Err("a warm_schedule window needs start different from end".to_owned());
            }
            if w.days.as_ref().is_some_and(Vec::is_empty) {
                return Err("a warm_schedule window's days may not be empty".to_owned());
            }
        }
        Ok(())
    }

    /// The floors in force at `now`: per `(class, scope)` the larger of the
    /// base floor and every active window. Sorted by class and scope.
    pub fn effective_floors(&self, base: &[WarmFloor], now: OffsetDateTime) -> Vec<WarmFloor> {
        let mut floors: BTreeMap<(Class, Scope), u32> = BTreeMap::new();
        for f in base {
            let e = floors.entry((f.class, f.scope)).or_default();
            *e = (*e).max(f.count);
        }
        let offset = UtcOffset::from_whole_seconds(self.utc_offset_minutes.saturating_mul(60))
            .unwrap_or(UtcOffset::UTC);
        let local = now.to_offset(offset);
        let (day, prev_day) = (
            Day::of(local.weekday()),
            Day::of(local.weekday().previous()),
        );
        let minute = u16::from(local.hour()) * 60 + u16::from(local.minute());
        for w in self
            .windows
            .iter()
            .filter(|w| w.active(day, prev_day, minute))
        {
            let e = floors.entry((w.class, w.scope)).or_default();
            *e = (*e).max(w.count);
        }
        floors
            .into_iter()
            .map(|((class, scope), count)| WarmFloor {
                class,
                scope,
                count,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;
    use crate::allocator::default_warm_floors;

    fn schedule(json: &str) -> WarmSchedule {
        let s: WarmSchedule = serde_json::from_str(json).unwrap();
        s.validate().unwrap();
        s
    }

    fn light_pr(floors: &[WarmFloor]) -> u32 {
        floors
            .iter()
            .find(|f| f.class == Class::Light && f.scope == Scope::Pr)
            .map_or(0, |f| f.count)
    }

    const DAYTIME: &str = r#"{"utc_offset_minutes": -480, "windows": [
        {"class":"light","scope":"pr","count":3,"start":"08:00","end":"20:00",
         "days":["mon","tue","wed","thu","fri"]}]}"#;

    #[test]
    fn window_applies_at_local_time_and_only_on_listed_days() {
        let s = schedule(DAYTIME);
        let base = default_warm_floors();
        // 2026-10-12 is a Monday. 15:59 UTC is 07:59 at UTC-8: not yet.
        assert_eq!(
            light_pr(&s.effective_floors(&base, datetime!(2026-10-12 15:59 UTC))),
            0
        );
        assert_eq!(
            light_pr(&s.effective_floors(&base, datetime!(2026-10-12 16:00 UTC))),
            3
        );
        // End is exclusive: 20:00 local is 04:00 UTC the next day.
        assert_eq!(
            light_pr(&s.effective_floors(&base, datetime!(2026-10-13 03:59 UTC))),
            3
        );
        assert_eq!(
            light_pr(&s.effective_floors(&base, datetime!(2026-10-13 04:00 UTC))),
            0
        );
        // Saturday noon local.
        assert_eq!(
            light_pr(&s.effective_floors(&base, datetime!(2026-10-17 20:00 UTC))),
            0
        );
    }

    #[test]
    fn windows_only_raise_the_base_floor_never_lower_it() {
        let s = schedule(
            r#"{"windows":[{"class":"medium","scope":"main","count":0,"start":"00:00","end":"24:00"}]}"#,
        );
        let floors = s.effective_floors(&default_warm_floors(), datetime!(2026-10-12 12:00 UTC));
        assert_eq!(floors, default_warm_floors());
        let s = schedule(
            r#"{"windows":[{"class":"medium","scope":"main","count":4,"start":"00:00","end":"24:00"}]}"#,
        );
        let floors = s.effective_floors(&default_warm_floors(), datetime!(2026-10-12 12:00 UTC));
        assert_eq!(floors[0].count, 4);
        assert_eq!(floors.len(), 1);
    }

    #[test]
    fn overlapping_windows_take_the_largest_count() {
        let s = schedule(
            r#"{"windows":[
              {"class":"light","scope":"pr","count":2,"start":"08:00","end":"20:00"},
              {"class":"light","scope":"pr","count":5,"start":"12:00","end":"13:00"}]}"#,
        );
        assert_eq!(
            light_pr(&s.effective_floors(&[], datetime!(2026-10-12 11:00 UTC))),
            2
        );
        assert_eq!(
            light_pr(&s.effective_floors(&[], datetime!(2026-10-12 12:30 UTC))),
            5
        );
    }

    #[test]
    fn a_window_may_wrap_midnight_and_belongs_to_its_starting_day() {
        let s = schedule(
            r#"{"windows":[{"class":"light","scope":"pr","count":2,
                "start":"22:00","end":"06:00","days":["fri"]}]}"#,
        );
        // 2026-10-16 is a Friday.
        let at = |dt| light_pr(&s.effective_floors(&[], dt));
        assert_eq!(at(datetime!(2026-10-16 21:59 UTC)), 0);
        assert_eq!(at(datetime!(2026-10-16 22:00 UTC)), 2);
        assert_eq!(at(datetime!(2026-10-17 05:59 UTC)), 2); // Saturday morning
        assert_eq!(at(datetime!(2026-10-17 06:00 UTC)), 0);
        assert_eq!(at(datetime!(2026-10-16 05:00 UTC)), 0); // Friday morning
        assert_eq!(at(datetime!(2026-10-17 22:30 UTC)), 0); // Saturday night
    }

    #[test]
    fn no_schedule_means_the_base_floors_unchanged() {
        let s = WarmSchedule::default();
        assert_eq!(
            s.effective_floors(&default_warm_floors(), datetime!(2026-10-12 12:00 UTC)),
            default_warm_floors()
        );
    }

    #[test]
    fn bad_times_and_windows_are_rejected() {
        for t in ["8:00", "08:60", "25:00", "24:01", "0800", "08:0a", ""] {
            let json = format!(
                r#"{{"windows":[{{"class":"light","scope":"pr","count":1,"start":"{t}","end":"09:00"}}]}}"#
            );
            assert!(serde_json::from_str::<WarmSchedule>(&json).is_err(), "{t}");
        }
        for json in [
            r#"{"windows":[{"class":"light","scope":"pr","count":1,"start":"08:00","end":"08:00"}]}"#,
            r#"{"windows":[{"class":"light","scope":"pr","count":1,"start":"08:00","end":"09:00","days":[]}]}"#,
            r#"{"utc_offset_minutes": 100000}"#,
        ] {
            let s: WarmSchedule = serde_json::from_str(json).unwrap();
            assert!(s.validate().is_err(), "{json}");
        }
        assert!(serde_json::from_str::<WarmSchedule>(r#"{"windows":[],"x":1}"#).is_err());
    }
}
