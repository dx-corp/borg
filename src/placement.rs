//! The rollout view of cost-aware placement ([`crate::config::PlacementSettings`]).
//!
//! In dry-run the queen runs the same pure allocator twice per pass: once as
//! it places today, once with paid burst drones costed above owned hosts. This
//! module compares the two grant sets and keeps running totals for `/status`
//! `placement`, so the change can be judged on real traffic before it is
//! switched on. Live, only the actual grants are counted.
//!
//! Grants are compared per `(class, scope)` as multisets of drones: the
//! allocator issues a runner per key, not per job (GitHub picks the job), so
//! two grants of one key on two drones are interchangeable. A *move* is a
//! grant the cost-aware allocator would have made on a different drone, or
//! not at all.

use std::collections::{BTreeMap, VecDeque};

use serde::Serialize;

use crate::allocator::{Grant, PlacementDecision, PlacementReason};
use crate::classes::{Class, Scope};

/// Moves kept for `/status`.
const RECENT_MOVES: usize = 20;

/// One grant the cost-aware allocator would have placed elsewhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct GrantMove {
    at: i64,
    class: Class,
    scope: Scope,
    /// Where the grant went; `None` when cost-aware placement made an extra one.
    actual: Option<String>,
    /// Where it would have gone; `None` when it would not have been made.
    would: Option<String>,
}

/// One allocator decision as `/status` shows it: where a runner went and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Placed {
    at: i64,
    class: Class,
    scope: Scope,
    /// `None`: no drone was eligible (see `/explain` for each rejection).
    drone: Option<String>,
    reason: Option<PlacementReason>,
    score: Option<i64>,
}

/// Running totals since the queen started.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PlacementView {
    /// Cost-aware placement is only simulated.
    pub(crate) dry_run: bool,
    pub(crate) burst_cost_permille: u32,
    /// First pass counted.
    since: Option<i64>,
    passes: u64,
    /// Runner grants the allocator made.
    grants: u64,
    /// Of those, on paid burst drones.
    burst_grants: u64,
    /// Dry-run only: grants cost-aware placement would have made.
    would_grants: u64,
    /// Dry-run only: of those, on paid burst drones.
    would_burst_grants: u64,
    /// Dry-run only: actual grants cost-aware placement would have made on a
    /// different drone or not at all.
    moved: u64,
    /// The newest moves.
    recent: VecDeque<GrantMove>,
    /// The newest allocator decisions, with the chosen drone and reason.
    placed: VecDeque<Placed>,
}

type Drones<'a> = BTreeMap<(Class, Scope), Vec<&'a str>>;

fn by_key(grants: &[Grant]) -> Drones<'_> {
    let mut m: Drones<'_> = BTreeMap::new();
    for g in grants {
        m.entry((g.class, g.scope))
            .or_default()
            .push(g.drone_id.as_str());
    }
    for v in m.values_mut() {
        v.sort_unstable();
    }
    m
}

/// `a` minus `b`, both sorted.
fn minus<'a>(a: &[&'a str], b: &[&str]) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut j = 0;
    for &x in a {
        while j < b.len() && b[j] < x {
            j += 1;
        }
        if j < b.len() && b[j] == x {
            j += 1;
        } else {
            out.push(x);
        }
    }
    out
}

/// Grants in `actual` that `would` places differently, paired in drone order.
fn moves(at: i64, actual: &[Grant], would: &[Grant]) -> Vec<GrantMove> {
    let a = by_key(actual);
    let w = by_key(would);
    let mut keys: Vec<(Class, Scope)> = a.keys().chain(w.keys()).copied().collect();
    keys.sort_unstable();
    keys.dedup();
    let empty = Vec::new();
    let mut out = Vec::new();
    for key in keys {
        let av = a.get(&key).unwrap_or(&empty);
        let wv = w.get(&key).unwrap_or(&empty);
        let only_a = minus(av, wv);
        let only_w = minus(wv, av);
        for i in 0..only_a.len().max(only_w.len()) {
            out.push(GrantMove {
                at,
                class: key.0,
                scope: key.1,
                actual: only_a.get(i).map(|s| (*s).to_owned()),
                would: only_w.get(i).map(|s| (*s).to_owned()),
            });
        }
    }
    out
}

impl PlacementView {
    /// Keep the newest of this pass's decisions.
    pub(crate) fn record_decisions(&mut self, at: i64, decisions: &[PlacementDecision]) {
        for d in decisions {
            if self.placed.len() == RECENT_MOVES {
                self.placed.pop_front();
            }
            let score = d.winner.as_deref().and_then(|w| {
                d.candidates
                    .iter()
                    .find(|c| c.drone_id == w)
                    .and_then(|c| c.score)
            });
            self.placed.push_back(Placed {
                at,
                class: d.class,
                scope: d.scope,
                drone: d.winner.clone(),
                reason: d.reason,
                score,
            });
        }
    }

    /// Count one pass. `would` is the cost-aware grant set in dry-run, `None`
    /// live.
    pub(crate) fn record(
        &mut self,
        at: i64,
        actual: &[Grant],
        would: Option<&[Grant]>,
        is_burst: impl Fn(&str) -> bool,
    ) {
        let n = |v: usize| u64::try_from(v).unwrap_or(u64::MAX);
        let burst = |g: &[Grant]| n(g.iter().filter(|g| is_burst(&g.drone_id)).count());
        self.since.get_or_insert(at);
        self.passes += 1;
        self.grants += n(actual.len());
        self.burst_grants += burst(actual);
        let Some(would) = would else {
            return;
        };
        self.would_grants += n(would.len());
        self.would_burst_grants += burst(would);
        let moved = moves(at, actual, would);
        self.moved += n(moved.iter().filter(|m| m.actual.is_some()).count());
        for m in moved {
            if self.recent.len() == RECENT_MOVES {
                self.recent.pop_front();
            }
            self.recent.push_back(m);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(drone: &str, class: Class, scope: Scope) -> Grant {
        Grant {
            drone_id: drone.into(),
            class,
            scope,
            labels: vec![],
            mem_mib: 0,
            vcpu: 0,
        }
    }

    #[test]
    fn identical_placements_move_nothing() {
        let a = [
            grant("wsl", Class::Light, Scope::Pr),
            grant("gcp-burst-a", Class::Heavy, Scope::Main),
        ];
        let b = [a[1].clone(), a[0].clone()];
        assert!(moves(0, &a, &b).is_empty());
    }

    #[test]
    fn grants_are_compared_per_key_as_multisets() {
        let actual = [
            grant("gcp-burst-a", Class::Heavy, Scope::Main),
            grant("gcp-burst-a", Class::Heavy, Scope::Main),
            grant("wsl", Class::Light, Scope::Pr),
        ];
        let would = [
            grant("gcp-burst-a", Class::Heavy, Scope::Main),
            grant("build-heavy-1", Class::Heavy, Scope::Main),
            grant("wsl", Class::Light, Scope::Pr),
            grant("wsl", Class::Light, Scope::Pr),
        ];
        let m = moves(7, &actual, &would);
        assert_eq!(
            m,
            vec![
                GrantMove {
                    at: 7,
                    class: Class::Light,
                    scope: Scope::Pr,
                    actual: None,
                    would: Some("wsl".into()),
                },
                GrantMove {
                    at: 7,
                    class: Class::Heavy,
                    scope: Scope::Main,
                    actual: Some("gcp-burst-a".into()),
                    would: Some("build-heavy-1".into()),
                },
            ]
        );
    }

    #[test]
    fn view_counts_burst_and_moves_and_bounds_recent() {
        let burst = |d: &str| d.starts_with("gcp-burst-");
        let mut v = PlacementView::default();
        let actual = [grant("gcp-burst-a", Class::Medium, Scope::Main)];
        let would = [grant("wsl", Class::Medium, Scope::Main)];
        for t in 0..30 {
            v.record(t, &actual, Some(&would), burst);
        }
        assert_eq!(
            (
                v.since,
                v.passes,
                v.grants,
                v.burst_grants,
                v.would_grants,
                v.would_burst_grants,
                v.moved
            ),
            (Some(0), 30, 30, 30, 30, 0, 30)
        );
        assert_eq!(v.recent.len(), RECENT_MOVES);
        assert_eq!(v.recent.front().map(|m| m.at), Some(10));
        // Live: only actual grants count.
        v.record(31, &would, None, burst);
        assert_eq!((v.grants, v.burst_grants, v.moved), (31, 30, 30));
    }
}
