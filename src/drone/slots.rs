//! Slot directories. A lease runs in `<work_root>/slot-<N>`, so concurrent
//! leases always get distinct slots, a slot is reused once its lease has been
//! cleaned up, and the path never contains the lease id. Identical paths are
//! what let compile caches hit when a slot is reused.
//!
//! A heavy lease takes the lowest free slot and every other lease the highest.
//! Jobs keep a cargo target directory and a tool home per slot
//! (`<cache_root>/<name>-ci/slots/<key>`), and the heavy ones are tens of
//! gigabytes each. With one lowest-first order for every class, a heavy lease
//! landed wherever the light and medium leases left a gap: borg-main-1 hosts
//! five heavy leases at once and ran `merge-gate` in seven different slots in
//! eight hours on 2026-11-20, each with its own retained target, and its
//! 400 GB disk fell under the heavy disk floor. Now a drone's heavy leases
//! reuse as many low slots as it runs heavy leases at once.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::classes::Class;

pub struct Slots {
    capacity: usize,
    /// slot index -> lease id
    held: Mutex<BTreeMap<usize, String>>,
}

/// A slot held by a lease; released on drop.
pub struct SlotGuard {
    slots: Arc<Slots>,
    index: usize,
}

impl SlotGuard {
    pub fn index(&self) -> usize {
        self.index
    }
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        if let Ok(mut held) = self.slots.held.lock() {
            held.remove(&self.index);
        }
    }
}

impl Slots {
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            capacity,
            held: Mutex::new(BTreeMap::new()),
        })
    }

    /// A free slot for a lease of `class`, or `None` when all are taken: the
    /// lowest for a heavy lease, the highest for any other.
    pub fn acquire(self: &Arc<Self>, lease_id: &str, class: Class) -> Option<SlotGuard> {
        let mut held = self.held.lock().ok()?;
        let mut free = (0..self.capacity).filter(|i| !held.contains_key(i));
        let index = match class {
            Class::Heavy => free.next(),
            Class::Light | Class::Medium => free.next_back(),
        }?;
        held.insert(index, lease_id.to_owned());
        tracing::info!("lease {lease_id} -> slot {index}");
        Some(SlotGuard {
            slots: Arc::clone(self),
            index,
        })
    }

    /// `(slot, lease id)` for every held slot, or `None` when ownership is unavailable.
    pub(crate) fn try_assignments(&self) -> Option<Vec<(usize, String)>> {
        self.held
            .lock()
            .ok()
            .map(|h| h.iter().map(|(i, l)| (*i, l.clone())).collect())
    }

    /// `(slot, lease id)` for every held slot.
    pub fn assignments(&self) -> Vec<(usize, String)> {
        self.try_assignments().unwrap_or_default()
    }
}

/// `<work_root>/slot-<index>`
pub fn slot_dir(work_root: &Path, index: usize) -> PathBuf {
    work_root.join(slot_name(index))
}

pub fn slot_name(index: usize) -> String {
    format!("slot-{index}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_heavy_leases_get_distinct_lowest_free_slots() {
        let slots = Slots::new(3);
        let a = slots.acquire("a", Class::Heavy).unwrap();
        let b = slots.acquire("b", Class::Heavy).unwrap();
        let c = slots.acquire("c", Class::Heavy).unwrap();
        assert_eq!((a.index(), b.index(), c.index()), (0, 1, 2));
        assert!(
            slots.acquire("d", Class::Heavy).is_none(),
            "no slot beyond capacity"
        );
        assert_eq!(
            slots.assignments(),
            [(0, "a".into()), (1, "b".into()), (2, "c".into())]
        );
    }

    #[test]
    fn a_released_slot_is_reused_lowest_first() {
        let slots = Slots::new(3);
        let a = slots.acquire("a", Class::Heavy).unwrap();
        let _b = slots.acquire("b", Class::Heavy).unwrap();
        drop(a);
        let c = slots.acquire("c", Class::Heavy).unwrap();
        assert_eq!(c.index(), 0, "the freed low slot, not slot 2");
        assert_eq!(slots.assignments().len(), 2);
    }

    #[test]
    fn smaller_leases_fill_from_the_top_and_leave_the_low_slots_to_heavy() {
        // borg-main-1 on 2026-11-20: light and medium leases held the low
        // slots, so its heavy leases ran in slots 0-4, 8 and 9 and kept a
        // cargo target in each.
        let slots = Slots::new(8);
        let light = slots.acquire("light", Class::Light).unwrap();
        let medium = slots.acquire("medium", Class::Medium).unwrap();
        assert_eq!((light.index(), medium.index()), (7, 6));
        let first = slots.acquire("heavy-1", Class::Heavy).unwrap();
        let second = slots.acquire("heavy-2", Class::Heavy).unwrap();
        assert_eq!((first.index(), second.index()), (0, 1));
        drop(first);
        drop(light);
        let again = slots.acquire("heavy-3", Class::Heavy).unwrap();
        assert_eq!(again.index(), 0, "the next heavy lease reuses slot 0");
        let small = slots.acquire("light-2", Class::Light).unwrap();
        assert_eq!(small.index(), 7, "and the next light lease slot 7");
    }

    #[test]
    fn a_full_drone_still_gives_each_class_the_last_free_slot() {
        let slots = Slots::new(2);
        let _light = slots.acquire("light", Class::Light).unwrap();
        let heavy = slots.acquire("heavy", Class::Heavy).unwrap();
        assert_eq!(heavy.index(), 0);
        drop(heavy);
        let light = slots.acquire("light-2", Class::Light).unwrap();
        assert_eq!(light.index(), 0, "the only free slot, whatever the class");
        assert!(slots.acquire("none", Class::Medium).is_none());
    }

    #[test]
    fn unavailable_ownership_is_not_an_empty_assignment_list() {
        let slots = Slots::new(1);
        let _a = slots.acquire("a", Class::Heavy).unwrap();
        let poison = Arc::clone(&slots);
        assert!(
            std::thread::spawn(move || {
                let _held = poison.held.lock().unwrap();
                panic!("poison the ownership table");
            })
            .join()
            .is_err()
        );
        assert!(slots.try_assignments().is_none());
    }

    #[test]
    fn paths_never_contain_the_lease_id() {
        let slots = Slots::new(2);
        let g = slots.acquire("0123456789ab", Class::Heavy).unwrap();
        let dir = slot_dir(Path::new("/var/lib/borg/work"), g.index());
        assert_eq!(dir, Path::new("/var/lib/borg/work/slot-0"));
        assert!(!dir.to_string_lossy().contains("0123456789ab"));
    }
}
