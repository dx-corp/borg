//! The queued job families per class, and the per-class profiles placement
//! uses for them (see [`crate::profile::ClassProfile::queue_weighted`]).

use std::collections::BTreeMap;

use crate::classes::{Class, Scope};
use crate::profile::{ClassProfile, QueuedFamilies};

/// Queued jobs per class and family, all scopes together.
pub(super) fn families_by_class(
    by_key: &BTreeMap<(Class, Scope), QueuedFamilies>,
) -> BTreeMap<Class, QueuedFamilies> {
    let mut out: BTreeMap<Class, QueuedFamilies> = BTreeMap::new();
    for (&(class, _), families) in by_key {
        let mine = out.entry(class).or_default();
        for (family, n) in families {
            *mine.entry(family.clone()).or_default() += n;
        }
    }
    out
}

/// The per-class profiles placement uses for `drone`: weighted by what is
/// queued when `family_aware`, else the plain class profile.
pub(super) fn placement_profiles(
    profiles: &[ClassProfile],
    drone: &str,
    queued: &BTreeMap<Class, QueuedFamilies>,
    family_aware: bool,
) -> Vec<crate::bid::ProfileScore> {
    profiles
        .iter()
        .filter(|p| p.drone == drone)
        .map(|p| {
            if family_aware {
                p.queue_weighted(queued.get(&p.class))
            } else {
                p.score()
            }
        })
        .collect()
}
