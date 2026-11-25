//! The one disk-admission rule. Jobs on a drone write to one work
//! filesystem, so every path that starts work there (placement, standby
//! promotion, heartbeat delivery, the free slots `/status` publishes) asks
//! the same [`DiskLedger`]: the drone's last free reading, less what each
//! lease on it holds, must still cover what the new start needs.

use super::{ActiveLease, DroneState, LeaseState, free_room, free_slots};
use crate::classes::{Class, Classes};
use crate::envcontract::ContractSettings;

impl DroneState {
    /// Take the disk floors and running reserves from the queen's settings.
    pub(crate) fn set_disk_rule(&mut self, settings: &ContractSettings) {
        self.disk_floor_mib = Class::ALL
            .into_iter()
            .map(|class| (class, settings.min_cache_dir_free_bytes_for(class) >> 20))
            .collect();
        self.disk_reserve_mib = settings.running_reserve_mib();
    }

    /// Free work-filesystem space `class` needs to start, MiB; 0 when unset.
    pub(crate) fn disk_floor(&self, class: Class) -> u64 {
        self.disk_floor_mib.get(&class).copied().unwrap_or(0)
    }

    /// Work-filesystem space a running lease of `class` keeps, MiB; 0 when
    /// unset.
    pub(crate) fn disk_reserve(&self, class: Class) -> u64 {
        self.disk_reserve_mib.get(&class).copied().unwrap_or(0)
    }

    /// Free work-filesystem space a lease of `class` needs to start and
    /// holds until it runs, MiB: its floor, or its running reserve when that
    /// is larger, so a start never promises less than the job will hold.
    pub(crate) fn disk_need(&self, class: Class) -> u64 {
        self.disk_floor(class).max(self.disk_reserve(class))
    }

    /// Whether the work filesystem, less what the `held` leases on this drone
    /// hold, still has what a new lease of `class` needs. True when the drone
    /// reports no reading.
    pub(crate) fn disk_admits(&self, held: &[ActiveLease], class: Class) -> bool {
        DiskLedger::new(self, held).admits(self.disk_need(class))
    }
}

impl ActiveLease {
    /// The free work disk this lease needs to start on `drone`, MiB: its
    /// class's need, or what its job asked for (`borg-disk-<N>g`) if more.
    pub(crate) fn disk_need_on(&self, drone: &DroneState) -> u64 {
        drone.disk_need(self.class).max(self.disk_mib.unwrap_or(0))
    }

    /// The free work disk this lease holds on `drone` now, MiB: what it
    /// needs to start until it runs, then its class's running reserve. A
    /// running lease's writes so far are in the free reading; the ones still
    /// to come are not.
    pub(crate) fn disk_hold_on(&self, drone: &DroneState) -> u64 {
        if self.state == LeaseState::Running {
            drone.disk_reserve(self.class)
        } else {
            self.disk_need_on(drone)
        }
    }
}

/// A drone's work filesystem as admission sees it: the last free reading less
/// every hold charged so far. A drone with no reading admits everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DiskLedger {
    left_mib: Option<u64>,
}

impl DiskLedger {
    /// `drone`'s reading less the hold of each of `held` that is on it.
    pub(crate) fn new<'a>(
        drone: &DroneState,
        held: impl IntoIterator<Item = &'a ActiveLease>,
    ) -> Self {
        let mut ledger = Self {
            left_mib: drone.work_free_mib,
        };
        for lease in held.into_iter().filter(|l| l.drone_id == drone.id) {
            ledger.hold(lease.disk_hold_on(drone));
        }
        ledger
    }

    /// The ledger a start of work `drone` already holds is judged by: the
    /// reading less the holds of everything started or delivered. Standbys
    /// are left out, because one being started must not be charged twice and
    /// one staying unstarted writes nothing; each start then [`Self::take`]s
    /// its own need.
    pub(crate) fn for_starts(drone: &DroneState, held: &[ActiveLease]) -> Self {
        Self::new(
            drone,
            held.iter().filter(|l| l.state != LeaseState::Standby),
        )
    }

    /// Charge `mib` whether or not it fits (work that already holds it).
    pub(crate) fn hold(&mut self, mib: u64) {
        self.left_mib = self.left_mib.map(|left| left.saturating_sub(mib));
    }

    /// Whether `need_mib` is still unpromised.
    pub(crate) fn admits(&self, need_mib: u64) -> bool {
        self.left_mib.is_none_or(|left| left >= need_mib)
    }

    /// Admit a start needing `need_mib` and charge it; false, with nothing
    /// charged, when it does not fit.
    pub(crate) fn take(&mut self, need_mib: u64) -> bool {
        let admitted = self.admits(need_mib);
        if admitted {
            self.hold(need_mib);
        }
        admitted
    }

    /// Whole starts needing `need_mib` each that the unpromised space holds.
    pub(crate) fn slots(&self, need_mib: u64) -> u32 {
        match self.left_mib {
            Some(left) if need_mib > 0 => u32::try_from(left / need_mib).unwrap_or(u32::MAX),
            _ => u32::MAX,
        }
    }
}

/// Whole leases of `class` that `drone` can start now beside `held`: what its
/// memory, vCPU and caps leave ([`free_slots`]), and no more than its disk
/// admits. `/status`, its metrics and standby yielding count room with this.
pub(crate) fn admissible_slots(
    drone: &DroneState,
    held: &[ActiveLease],
    class: Class,
    classes: &Classes,
) -> u32 {
    let room = free_room(drone, held, classes);
    free_slots(drone, &room, class, classes.spec(class))
        .min(DiskLedger::new(drone, held).slots(drone.disk_need(class)))
}
