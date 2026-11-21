//! The text of the `capacity_starved` alarm.

use std::collections::BTreeSet;

use crate::store::DroneRow;

/// A drone silent this long was removed or switched off, not lost: it is not
/// why capacity is short now, so the capacity alarm counts it without a name.
const STALE_NEWS_SECS: i64 = 86_400;

/// The stale drones a `capacity_starved` reason names: `rows` not in `fresh`;
/// see [`describe`].
pub(super) fn stale_drones(rows: &[DroneRow], fresh: &BTreeSet<String>, now: i64) -> String {
    describe(
        rows.iter()
            .filter(|d| !fresh.contains(&d.name))
            .map(|d| (d.name.as_str(), d.last_heartbeat)),
        now,
    )
}

/// The stale drones a `capacity_starved` reason names: those that
/// heartbeated within `STALE_NEWS_SECS`. A record that never heartbeated or
/// has been silent longer (2026-11-19: `borg-pr-1` and `borg-heavy-1`,
/// removed the day before, and a burst record that never enrolled, in every
/// alarm) is counted, so the list reads as what went missing recently.
fn describe<'a>(silent: impl Iterator<Item = (&'a str, Option<i64>)>, now: i64) -> String {
    let (mut recent, mut old) = (Vec::new(), 0usize);
    for (name, last_heartbeat) in silent {
        if last_heartbeat.is_some_and(|t| now - t <= STALE_NEWS_SECS) {
            recent.push(name);
        } else {
            old += 1;
        }
    }
    let mut text = format!("[{}]", recent.join(", "));
    if old > 0 {
        text.push_str(&format!(" and {old} silent over a day or never seen"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_capacity_alarm_names_recently_lost_drones_and_counts_long_gone_ones() {
        let now = 1_000_000;
        assert_eq!(describe(std::iter::empty(), now), "[]");
        let silent = [
            ("borg-main-1", Some(now - 700)),
            ("borg-pr-1", Some(now - 91_678)),
            ("at-the-edge", Some(now - STALE_NEWS_SECS)),
            ("gcp-burst-q1c80614", None),
        ];
        assert_eq!(
            describe(silent.into_iter(), now),
            "[borg-main-1, at-the-edge] and 2 silent over a day or never seen"
        );
        assert_eq!(
            describe([("borg-main-1", Some(now - 700))].into_iter(), now),
            "[borg-main-1]"
        );
    }
}
