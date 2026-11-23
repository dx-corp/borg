//! The queen's side of [`crate::hostlost`]: classifying leases it ends
//! without a drone result, and removing expired drones.

use super::Queen;
use crate::hostlost::{EndClass, HostLostRerunner, LeaseEnd, classify_end};
use crate::store::{DroneRow, LeaseRow, LeaseStatus, StoreError};

impl Queen {
    /// Attach the host-lost rerunner; `api::serve` runs it.
    #[must_use]
    pub fn with_host_lost(mut self, rerunner: HostLostRerunner) -> Self {
        self.host_lost = Some(rerunner);
        self
    }

    pub fn host_lost(&self) -> Option<&HostLostRerunner> {
        self.host_lost.as_ref()
    }

    /// A lease the queen is ending without a result from its drone: record
    /// it as host-lost when the drone was not heard (see [`crate::hostlost`]).
    /// Never fails the pass. `drones` caches the drone rows for one pass.
    pub(super) async fn note_unreported_end(
        &self,
        lease: &LeaseRow,
        now: i64,
        drones: &mut Option<Vec<DroneRow>>,
    ) {
        if lease.state != LeaseStatus::Running {
            return;
        }
        if drones.is_none() {
            match self.store.drones().await {
                Ok(rows) => *drones = Some(rows),
                Err(e) => {
                    tracing::warn!("lease {}: host-loss check skipped: {e}", lease.id);
                    return;
                }
            }
        }
        let rows = drones.as_deref().unwrap_or_default();
        if let Err(e) = self.classify_lost(lease, now, rows, false).await {
            tracing::warn!("lease {}: host-loss check failed: {e}", lease.id);
        }
    }

    /// Classify a running lease that is ending with no drone result, and on
    /// host loss record it, mark the drone suspect and log a health event.
    async fn classify_lost(
        &self,
        lease: &LeaseRow,
        now: i64,
        drones: &[DroneRow],
        drone_removed: bool,
    ) -> Result<(), StoreError> {
        let own = drones.iter().find(|d| d.name == lease.drone);
        let end = LeaseEnd {
            running: lease.state == LeaseStatus::Running,
            // Only unfinished leases reach here; a result finishes the lease.
            reported: false,
            preempted: self.store.lease_preempted(&lease.id).await?,
            created_at: lease.created_at,
            ended_at: now,
            drone_removed,
            last_heartbeat: own.and_then(|d| d.last_heartbeat),
            silence: own.and_then(|d| d.silence),
            others_heard: drones
                .iter()
                .filter(|d| d.name != lease.drone)
                .filter_map(|d| d.last_heartbeat)
                .max(),
            queen_started_at: self.started_at(),
            stale_secs: self.cfg.drone_stale_secs,
        };
        let EndClass::HostLost { silent_secs } = classify_end(&end) else {
            return Ok(());
        };
        if !self.store.record_host_lost(lease, now, silent_secs).await? {
            return Ok(());
        }
        let cooldown = i64::try_from(self.cfg.host_lost.suspect_cooldown_secs).unwrap_or(i64::MAX);
        self.store
            .suspect_drone(&lease.drone, now.saturating_add(cooldown))
            .await?;
        let reason = format!(
            "lease {} ({}) ended with no result after {silent_secs}s without a heartbeat; no new leases for {cooldown}s",
            lease.id, lease.class
        );
        tracing::warn!("host lost on {}: {reason}", lease.drone);
        self.store
            .record_health_event(now, &lease.drone, "host_lost", &reason)
            .await
    }

    /// Remove drones silent past their `drone_expiry` window (never one named
    /// in `wake.hosts`, never one that has not heartbeated yet) and log each
    /// removal as a `drone_expired` health event.
    pub(super) async fn expire_drones(&self, now: i64) -> Result<(), StoreError> {
        let rows = self.store.drones().await?;
        for d in &rows {
            let Some(last) = d.last_heartbeat else {
                continue;
            };
            let Some(after) = self.cfg.drone_expiry.expiry_for(&d.name) else {
                continue;
            };
            let silent = now - last;
            if silent <= after.max(self.cfg.drone_stale_secs)
                || self.cfg.wake.hosts.iter().any(|h| h.name == d.name)
            {
                continue;
            }
            // Removal deletes the drone's leases: note the jobs it was running first.
            for lease in self.store.active_leases().await? {
                if lease.drone == d.name && lease.state == LeaseStatus::Running {
                    self.classify_lost(&lease, now, &rows, true).await?;
                }
            }
            if let Some(dropped) = self.store.remove_drone(&d.name).await? {
                let reason = format!(
                    "no heartbeat for {silent}s (expiry {after}s); {dropped} unfinished leases dropped"
                );
                tracing::warn!("expired drone {}: {reason}", d.name);
                self.store
                    .record_health_event(now, &d.name, "drone_expired", &reason)
                    .await?;
            }
        }
        Ok(())
    }
}
