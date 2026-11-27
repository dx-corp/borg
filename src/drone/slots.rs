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
//! reuse as many low slots as it runs heavy leases at once, as long as those
//! slots are free. No slot is held back for a class: when smaller leases have
//! filled the drone down into the low slots, a heavy lease takes the lowest
//! slot still free, which can be one no heavy lease used before. That target
//! is then an ordinary cache slot, and reclaim evicts the least recently used
//! ones to keep the drone's disk floor.
//!
//! A slot whose directory cannot be removed is fouled: no lease takes it
//! until a later cleanup removes it. Container jobs can leave root-owned
//! files in their slot that the job user cannot delete. Before 2026-11-27 the
//! next light or medium lease took that same highest slot, failed to clear it
//! and reported exit -1 within seconds, and so did every lease after it:
//! build-heavy-1, build-medium-1 and wsl-desktop lost about 1,000 leases in
//! six hours that way while the queen still counted them as free.
//!
//! [`Slots::retry_fouled`] runs after every cleanup and on every heartbeat,
//! so a drone whose slots are all fouled still recovers once the directories
//! become removable. It holds each fouled slot while it removes it: no lease
//! can take the slot meanwhile, and two retries never work on one slot.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::classes::Class;
use crate::drone::cleanup::remove_lease_dir;
use crate::drone::fsops::FsOps;

/// The holder recorded for a fouled slot while a retry removes it.
const RETRY_HOLDER: &str = "fouled-slot-retry";

pub struct Slots {
    capacity: usize,
    /// slot index -> lease id
    held: Mutex<BTreeMap<usize, String>>,
    /// Slots whose directory a cleanup could not remove.
    fouled: Mutex<BTreeSet<usize>>,
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

    /// The slots this guard's slot belongs to.
    pub fn slots(&self) -> &Arc<Slots> {
        &self.slots
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
            fouled: Mutex::new(BTreeSet::new()),
        })
    }

    /// Keep every later lease out of slot `index` until [`Slots::clear`].
    pub fn foul(&self, index: usize) {
        if let Ok(mut fouled) = self.fouled.lock() {
            fouled.insert(index);
        }
    }

    /// Let leases take slot `index` again.
    pub fn clear(&self, index: usize) {
        if let Ok(mut fouled) = self.fouled.lock() {
            fouled.remove(&index);
        }
    }

    /// How many slots the drone has.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Whether any slot is fouled.
    pub fn any_fouled(&self) -> bool {
        self.fouled.lock().is_ok_and(|f| !f.is_empty())
    }

    /// Try to remove every fouled slot no lease holds, holding each while it
    /// is removed, and let leases use the ones that are gone. Blocking.
    /// Returns the slots made usable again.
    pub fn retry_fouled(self: &Arc<Self>, fs: &dyn FsOps, work_root: &Path) -> Vec<usize> {
        let claimed: Vec<SlotGuard> = {
            let (Ok(mut held), Ok(fouled)) = (self.held.lock(), self.fouled.lock()) else {
                return Vec::new();
            };
            fouled
                .iter()
                .copied()
                .filter(|i| !held.contains_key(i))
                .collect::<Vec<_>>()
                .into_iter()
                .map(|index| {
                    held.insert(index, RETRY_HOLDER.to_owned());
                    SlotGuard {
                        slots: Arc::clone(self),
                        index,
                    }
                })
                .collect()
        };
        let mut cleared = Vec::new();
        for guard in claimed {
            if remove_lease_dir(fs, work_root, &slot_name(guard.index)).is_ok() {
                // Cleared while still held: the slot is offered only once its
                // directory is gone.
                self.clear(guard.index);
                cleared.push(guard.index);
            }
        }
        cleared
    }

    /// Fouled slots that no lease holds, lowest first.
    pub fn fouled_idle(&self) -> Vec<usize> {
        let held = self
            .held
            .lock()
            .map(|h| h.keys().copied().collect::<BTreeSet<_>>());
        let fouled = self.fouled.lock().map(|f| f.clone());
        match (held, fouled) {
            (Ok(held), Ok(fouled)) => fouled.difference(&held).copied().collect(),
            _ => Vec::new(),
        }
    }

    /// A free slot for a lease of `class`, or `None` when all are taken: the
    /// lowest for a heavy lease, the highest for any other.
    pub fn acquire(self: &Arc<Self>, lease_id: &str, class: Class) -> Option<SlotGuard> {
        let mut held = self.held.lock().ok()?;
        let fouled = self.fouled.lock().ok()?.clone();
        let mut free = (0..self.capacity).filter(|i| !held.contains_key(i) && !fouled.contains(i));
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
    fn a_fouled_slot_is_skipped_until_cleared() {
        // build-medium-1 on 2026-11-27: slot 15 held root-owned files and
        // every light lease went to slot 15.
        let slots = Slots::new(3);
        let a = slots.acquire("a", Class::Light).unwrap();
        assert_eq!(a.index(), 2);
        slots.foul(a.index());
        drop(a);
        assert_eq!(slots.fouled_idle(), [2]);
        let b = slots.acquire("b", Class::Light).unwrap();
        assert_eq!(b.index(), 1, "the next highest clean slot");
        let c = slots.acquire("c", Class::Heavy).unwrap();
        assert_eq!(c.index(), 0);
        assert!(
            slots.acquire("d", Class::Medium).is_none(),
            "the fouled slot is not free"
        );
        slots.clear(2);
        assert!(slots.fouled_idle().is_empty());
        assert_eq!(slots.acquire("e", Class::Light).unwrap().index(), 2);
    }

    #[test]
    fn a_fouled_slot_held_by_a_lease_is_not_idle() {
        let slots = Slots::new(2);
        let a = slots.acquire("a", Class::Light).unwrap();
        slots.foul(a.index());
        assert!(slots.fouled_idle().is_empty());
        drop(a);
        assert_eq!(slots.fouled_idle(), [1]);
    }

    /// Removal fails while `stuck` is set, as `rm -rf` does on root-owned
    /// files the job user cannot delete.
    struct StuckFs {
        stuck: std::sync::atomic::AtomicBool,
    }

    impl FsOps for StuckFs {
        fn mkdir_p(&self, _: &Path) -> std::io::Result<()> {
            Ok(())
        }
        fn write_file(&self, _: &Path, _: &str) -> std::io::Result<()> {
            Ok(())
        }
        fn remove_all(&self, path: &Path) -> std::io::Result<()> {
            if self.stuck.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(std::io::Error::other("rm -rf failed"));
            }
            std::fs::remove_dir_all(path)
        }
        fn clone_tree(&self, _: &Path, _: &Path) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_failed_cleanup_fouls_the_slot_until_a_retry_removes_it() {
        let work = tempfile::tempdir().unwrap();
        let fs = StuckFs {
            stuck: std::sync::atomic::AtomicBool::new(true),
        };
        let slots = Slots::new(1);
        let a = slots.acquire("a", Class::Light).unwrap();
        std::fs::create_dir_all(work.path().join("slot-0/_work/_tool")).unwrap();
        // The cleanup path: removal fails, the slot is fouled.
        if remove_lease_dir(&fs, work.path(), &slot_name(a.index())).is_err() {
            slots.foul(a.index());
        }
        drop(a);
        assert!(
            slots.acquire("b", Class::Light).is_none(),
            "every slot fouled"
        );
        assert!(
            slots.retry_fouled(&fs, work.path()).is_empty(),
            "still stuck"
        );
        assert!(slots.acquire("b", Class::Light).is_none());
        // An operator fixes ownership; the next heartbeat's retry frees it.
        fs.stuck.store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(slots.retry_fouled(&fs, work.path()), [0]);
        assert!(!work.path().join("slot-0").exists());
        assert_eq!(slots.acquire("b", Class::Light).unwrap().index(), 0);
    }

    #[test]
    fn a_retry_holds_the_slot_so_no_lease_takes_it_meanwhile() {
        let slots = Slots::new(2);
        slots.foul(1);
        let claimed = {
            let mut held = slots.held.lock().unwrap();
            held.insert(1, RETRY_HOLDER.to_owned());
            held.len()
        };
        assert_eq!(claimed, 1);
        assert!(slots.fouled_idle().is_empty(), "a second retry skips it");
        assert_eq!(slots.acquire("x", Class::Light).unwrap().index(), 0);
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
