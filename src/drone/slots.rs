//! Slot directories. A lease runs in `<work_root>/slot-<N>` and takes the
//! lowest free slot, so concurrent leases always get distinct slots, a slot is
//! reused once its lease has been cleaned up, and the path never contains the
//! lease id. Identical paths are what let compile caches hit when a slot is
//! reused.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

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

    /// The lowest free slot, or `None` when all are taken.
    pub fn acquire(self: &Arc<Self>, lease_id: &str) -> Option<SlotGuard> {
        let mut held = self.held.lock().ok()?;
        let index = (0..self.capacity).find(|i| !held.contains_key(i))?;
        held.insert(index, lease_id.to_owned());
        tracing::info!("lease {lease_id} -> slot {index}");
        Some(SlotGuard {
            slots: Arc::clone(self),
            index,
        })
    }

    /// `(slot, lease id)` for every held slot.
    pub fn assignments(&self) -> Vec<(usize, String)> {
        self.held
            .lock()
            .map(|h| h.iter().map(|(i, l)| (*i, l.clone())).collect())
            .unwrap_or_default()
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
    fn concurrent_leases_get_distinct_lowest_free_slots() {
        let slots = Slots::new(3);
        let a = slots.acquire("a").unwrap();
        let b = slots.acquire("b").unwrap();
        let c = slots.acquire("c").unwrap();
        assert_eq!((a.index(), b.index(), c.index()), (0, 1, 2));
        assert!(slots.acquire("d").is_none(), "no slot beyond capacity");
        assert_eq!(
            slots.assignments(),
            [(0, "a".into()), (1, "b".into()), (2, "c".into())]
        );
    }

    #[test]
    fn a_released_slot_is_reused_lowest_first() {
        let slots = Slots::new(3);
        let a = slots.acquire("a").unwrap();
        let _b = slots.acquire("b").unwrap();
        drop(a);
        let c = slots.acquire("c").unwrap();
        assert_eq!(c.index(), 0, "the freed low slot, not slot 2");
        assert_eq!(slots.assignments().len(), 2);
    }

    #[test]
    fn paths_never_contain_the_lease_id() {
        let slots = Slots::new(2);
        let g = slots.acquire("0123456789ab").unwrap();
        let dir = slot_dir(Path::new("/var/lib/borg/work"), g.index());
        assert_eq!(dir, Path::new("/var/lib/borg/work/slot-0"));
        assert!(!dir.to_string_lossy().contains("0123456789ab"));
    }
}
