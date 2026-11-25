//! What a heartbeat may hand its drone to start now: standby starts, placed
//! units and undelivered leases, held to the caps and the disk this heartbeat
//! reports (see `docs/operations/borg.md`).

use crate::allocator::{ActiveLease, DroneState, StandbyStarts};
use crate::classes::Class;
use crate::queen::{Queen, as_active};
use crate::store::{LeaseRow, LeaseStatus};
use crate::units::UnitState;

use super::{ApiError, HeartbeatRequest};

/// The drone as this heartbeat reports it, and what it holds.
pub(super) struct Host {
    /// Its capacity, caps and free-disk reading from this heartbeat, with the
    /// queen's disk rule.
    state: DroneState,
    /// Everything it holds against its class caps: its leases (standbys as
    /// standbys) and the units placed on it.
    held: Vec<ActiveLease>,
    /// Of `held`, what it has started or been handed: delivered leases and
    /// acknowledged units. The rest is what this heartbeat may hand over.
    started: Vec<ActiveLease>,
}

impl Host {
    pub(super) async fn read(
        queen: &Queen,
        drone: &str,
        req: &HeartbeatRequest,
    ) -> Result<Self, ApiError> {
        let store = queen.store();
        let mut state = DroneState::from_capacity(drone, &req.capacity, &req.caps);
        state.set_disk_rule(&queen.config().env_contract);
        let mut held = Vec::new();
        let mut started = Vec::new();
        for l in store.active_leases().await? {
            if l.drone != drone {
                continue;
            }
            let lease = as_active(&l);
            if !(l.state == LeaseStatus::Pending && l.jit_config.is_some()) {
                started.push(lease.clone());
            }
            held.push(lease);
        }
        if queen.config().units.enabled {
            for u in store
                .units_in(&[UnitState::Leased, UnitState::Running])
                .await?
            {
                if u.drone.as_deref() != Some(drone) {
                    continue;
                }
                let unit = ActiveLease::running(drone.to_owned(), u.class, u.scope);
                if u.state == UnitState::Running || req.active_units.contains(&u.id) {
                    started.push(unit.clone());
                }
                held.push(unit);
            }
        }
        Ok(Self {
            state,
            held,
            started,
        })
    }

    /// The starts this heartbeat hands out, each charged to one disk ledger.
    pub(super) fn handoff(&self) -> Handoff<'_> {
        let mut starts = StandbyStarts::new(&self.held);
        starts.disk_after(&self.state, &self.started);
        Handoff {
            host: &self.state,
            starts,
        }
    }
}

/// One heartbeat's starts. A start is handed over only while the drone's
/// reading, less what its started work holds (a running reserve per running
/// lease) and what earlier starts in this response took, still covers what it
/// needs; otherwise it waits for a later heartbeat. A drone without a reading
/// is not held back here.
pub(super) struct Handoff<'a> {
    host: &'a DroneState,
    starts: StandbyStarts<'a>,
}

impl Handoff<'_> {
    /// Whether the queen-approved standby `lease` may start: its class cap,
    /// as this heartbeat reports it, has room beside what the drone already
    /// runs of the class, and the disk admits it.
    pub(super) fn standby(&mut self, lease: &LeaseRow) -> bool {
        self.starts.take(self.host, lease.class)
    }

    /// Whether the undelivered `lease` may be handed over; a disk label
    /// (`borg-disk-<N>g`) raises what it needs.
    pub(super) fn lease(&mut self, lease: &LeaseRow) -> bool {
        let need = as_active(lease).disk_need_on(self.host);
        self.starts.disk(self.host).take(need)
    }

    /// Whether a placed unit of `class` may be handed over.
    pub(super) fn unit(&mut self, class: Class) -> bool {
        let need = self.host.disk_need(class);
        self.starts.disk(self.host).take(need)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allocator::HostCaps;
    use crate::classes::Scope;

    const GIB: u64 = 1024;

    fn host(free_gib: u64, held: Vec<ActiveLease>, started: Vec<ActiveLease>) -> Host {
        let capacity = crate::probe::Capacity {
            hostname: "d".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            kind: crate::probe::HostKind::Vm,
            vcpu: 64,
            mem_mib: 256 * GIB,
            free_disk_mib: Some(free_gib * GIB),
            docker_usable: true,
            mise_version: None,
            git_version: None,
        };
        let mut state = DroneState::from_capacity("d", &capacity, &HostCaps::default());
        state.set_disk_rule(&crate::envcontract::ContractSettings::default());
        Host {
            state,
            held,
            started,
        }
    }

    fn row(class: Class, labels: &[&str]) -> LeaseRow {
        LeaseRow {
            id: "l".into(),
            drone: "d".into(),
            class,
            scope: Scope::Pr,
            labels: labels.iter().map(|l| (*l).to_owned()).collect(),
            runner_name: "r".into(),
            runner_id: 1,
            jit_config: None,
            state: LeaseStatus::Pending,
            created_at: 0,
            expires_at: 0,
        }
    }

    #[test]
    fn a_disk_labelled_lease_waits_when_the_reading_no_longer_holds_it() {
        // 40 GiB free: one 32g lease is handed over, the next waits, and a
        // class-floor lease (10 GiB) no longer fits the 8 left.
        let h = host(40, vec![], vec![]);
        let mut handoff = h.handoff();
        let labelled = row(Class::Medium, &["borg-ci-medium-pr", "borg-disk-32g"]);
        assert!(handoff.lease(&labelled));
        assert!(!handoff.lease(&labelled));
        assert!(!handoff.lease(&row(Class::Medium, &["borg-ci-medium-pr"])));
        // No reading: nothing is held back here.
        let mut unmeasured = host(0, vec![], vec![]);
        unmeasured.state.work_free_mib = None;
        assert!(unmeasured.handoff().lease(&labelled));
    }

    #[test]
    fn running_work_keeps_its_reserve_out_of_what_a_heartbeat_hands_over() {
        // Four running light leases keep 16 GiB of a 25 GiB reading: the 9
        // left are under a light lease's 10 GiB, for a lease and for a unit.
        let running = vec![ActiveLease::running("d".into(), Class::Light, Scope::Pr); 4];
        let h = host(25, running.clone(), running.clone());
        assert!(!h.handoff().lease(&row(Class::Light, &[])));
        assert!(!h.handoff().unit(Class::Light));
        // At 35 GiB one fits, and it is charged before the next is judged.
        let h = host(35, running.clone(), running);
        let mut handoff = h.handoff();
        assert!(handoff.unit(Class::Light));
        assert!(!handoff.lease(&row(Class::Light, &[])));
    }

    #[test]
    fn a_standby_starts_only_while_the_disk_admits_it_beside_running_work() {
        // A running medium lease keeps 4 GiB and heavy needs 32 to start.
        // The standby's own hold is not charged before it starts, so 36 GiB
        // admits it and 35 does not.
        let standby = ActiveLease {
            state: crate::allocator::LeaseState::Standby,
            ..ActiveLease::running("d".into(), Class::Heavy, Scope::Deploy)
        };
        let medium = ActiveLease::running("d".into(), Class::Medium, Scope::Deploy);
        let heavy_row = LeaseRow {
            state: LeaseStatus::Standby,
            ..row(Class::Heavy, &[])
        };
        for (free_gib, starts) in [(35, false), (36, true)] {
            let mut h = host(
                free_gib,
                vec![medium.clone(), standby.clone()],
                vec![medium.clone(), standby.clone()],
            );
            h.state.disk_floor_mib.insert(Class::Heavy, 32 * GIB);
            assert_eq!(h.handoff().standby(&heavy_row), starts, "{free_gib} GiB");
        }
    }
}
