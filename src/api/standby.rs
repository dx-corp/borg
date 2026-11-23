//! The standby starts a heartbeat hands out, held to the caps it reports.

use crate::allocator::{ActiveLease, DroneState, LeaseState, StandbyStarts};
use crate::queen::Queen;
use crate::store::{LeaseRow, LeaseStatus};
use crate::units::UnitState;

use super::{ApiError, HeartbeatRequest};

/// The `approved` starts the drone may take now. The queen approved them under
/// the caps it last saw; a class cap this heartbeat lowers applies at once,
/// counting what the drone already runs of the class (leases and units).
pub(super) async fn within_caps<'a>(
    queen: &Queen,
    drone: &str,
    req: &HeartbeatRequest,
    approved: impl Iterator<Item = &'a LeaseRow>,
) -> Result<Vec<String>, ApiError> {
    let approved: Vec<&LeaseRow> = approved.collect();
    if approved.is_empty() || req.caps.max_class_leases.is_empty() {
        return Ok(approved.into_iter().map(|l| l.id.clone()).collect());
    }
    let held = held_on(queen, drone).await?;
    let capped = DroneState::from_capacity(drone, &req.capacity, &req.caps);
    let mut starts = StandbyStarts::new(&held);
    Ok(approved
        .into_iter()
        .filter(|l| starts.take(&capped, l.class))
        .map(|l| l.id.clone())
        .collect())
}

/// What `drone` holds against its class caps: its leases (standbys as
/// standbys) and the units placed on it.
async fn held_on(queen: &Queen, drone: &str) -> Result<Vec<ActiveLease>, ApiError> {
    let store = queen.store();
    let mut held: Vec<ActiveLease> = store
        .active_leases()
        .await?
        .into_iter()
        .filter(|l| l.drone == drone)
        .map(|l| ActiveLease {
            drone_id: l.drone,
            class: l.class,
            scope: l.scope,
            state: if l.state == LeaseStatus::Standby {
                LeaseState::Standby
            } else {
                LeaseState::Running
            },
            disk_mib: None,
        })
        .collect();
    if queen.config().units.enabled {
        held.extend(
            store
                .units_in(&[UnitState::Leased, UnitState::Running])
                .await?
                .into_iter()
                .filter(|u| u.drone.as_deref() == Some(drone))
                .map(|u| ActiveLease {
                    drone_id: drone.to_owned(),
                    class: u.class,
                    scope: u.scope,
                    state: LeaseState::Running,
                    disk_mib: None,
                }),
        );
    }
    Ok(held)
}
