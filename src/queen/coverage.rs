//! Which held leases cover queued demand: undelivered leases a drone's
//! heartbeat withholds, and the standbys to start.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::as_active;
use crate::allocator::{ActiveLease, LeaseState, Promotion};
use crate::classes::{Class, Scope};
use crate::probe::Capacity;
use crate::store::{LeaseRow, LeaseStatus};

/// A lease as placement sees it. A pending lease not yet delivered to a
/// drone whose heartbeat withholds it (`withheld`) can take no job until that
/// ends: it keeps its room but covers no queued demand, so a healthy drone
/// gets the job instead.
pub(super) fn as_placed(l: &LeaseRow, withheld: &HashMap<String, Vec<Class>>) -> ActiveLease {
    let mut a = as_active(l);
    if a.state == LeaseState::Pending
        && l.jit_config.is_some()
        && withheld
            .get(&l.drone)
            .is_some_and(|classes| classes.contains(&l.class))
    {
        a.state = LeaseState::Running;
    }
    a
}

/// The standby leases to start now: per promoted class, the oldest standby
/// leases on fresh drones that accept work of that class (not one in its
/// host-lost cool-down or below the class's disk floor, which the heartbeat
/// would not hand the start to).
pub(super) fn promotions(
    promote: &[Promotion],
    active: &[LeaseRow],
    fresh: &HashMap<String, Capacity>,
    accepting: &BTreeMap<&str, &[Class]>,
) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for p in promote {
        let mut standby: Vec<&LeaseRow> = active
            .iter()
            .filter(|l| {
                l.state == LeaseStatus::Standby
                    && l.scope == Scope::Deploy
                    && l.class == p.class
                    && fresh.contains_key(&l.drone)
                    && accepting
                        .get(l.drone.as_str())
                        .is_some_and(|blocked| !blocked.contains(&l.class))
            })
            .collect();
        standby.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        out.extend(
            standby
                .into_iter()
                .take(usize::try_from(p.count).unwrap_or(usize::MAX))
                .map(|l| l.id.clone()),
        );
    }
    out
}
