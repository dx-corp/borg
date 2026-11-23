//! One pass of the quorum rollout gate ([`crate::rollout`]): the fleet and
//! queue as the rollout sees them, and the persisted result.

use std::collections::HashSet;

use super::{MAX_REPORT_ROWS, Queen, QueenError, RolloutView, outcome_of};
use crate::allocator::{QueueAge, max_slots};
use crate::classes::Class;
use crate::health::Outcome;
use crate::rollout::{DroneView, advance};
use crate::store::{LeaseRow, LeaseStatus};

impl Queen {
    /// One pass of the quorum rollout gate (only when both the target and
    /// `rollout.previous_version` are configured).
    pub(super) async fn advance_rollout(
        &self,
        now: i64,
        active: &[LeaseRow],
        queued: &[QueueAge],
    ) -> Result<(), QueenError> {
        let Some(target) = self.rollout_target() else {
            return Ok(());
        };
        if self.cfg.rollout.previous_version.is_none() && !self.cfg.rollout.follow_queen {
            return Ok(());
        }
        let stored = self.store.rollout_state().await?;
        let target = self.settled_target(target, stored.as_ref(), now);
        let rows = self.store.drones().await?;
        let busy_drones: HashSet<&str> = active
            .iter()
            .filter(|lease| lease.state != LeaseStatus::Standby)
            .map(|lease| lease.drone.as_str())
            .collect();
        let views: Vec<DroneView> = rows
            .iter()
            .map(|d| DroneView {
                name: d.name.clone(),
                os: d
                    .capacity
                    .as_ref()
                    .map(|c| c.os.clone())
                    .unwrap_or_default(),
                fresh: d
                    .last_heartbeat
                    .is_some_and(|t| now - t <= self.cfg.drone_stale_secs),
                held: d.held(),
                maintenance: d.hold() == crate::health::Hold::Operator,
                has_active_leases: busy_drones.contains(d.name.as_str()),
                version: d.version.clone(),
                version_since: d.version_since,
                shared: d.trust == crate::classes::Trust::Shared,
                runs_jobs: d.accepting,
                // What placement admits: a suspect cool-down or a contract
                // block clears `accepting` in the allocator's view.
                in_service: d.accepting
                    && d.capacity
                        .as_ref()
                        .is_some_and(|cap| d.allocator_state(cap, now).accepting),
                slots: d
                    .capacity
                    .as_ref()
                    .map(|cap| {
                        let state = d.allocator_state(cap, now);
                        Class::ALL
                            .into_iter()
                            .map(|c| (c, max_slots(&state, c, self.cfg.classes.spec(c))))
                            .filter(|&(_, n)| n > 0)
                            .collect()
                    })
                    .unwrap_or_default(),
            })
            .collect();
        let Some(previous) = self
            .cfg
            .rollout
            .previous_version
            .clone()
            .or_else(|| crate::rollout::previous_for(&target, stored.as_ref(), &views))
        else {
            // A reset can leave no baseline when all fresh drones already run
            // the target. Clear the old view and its heartbeat instructions.
            self.update_snapshot(|s| s.rollout = None);
            return Ok(());
        };
        let since = stored
            .as_ref()
            .filter(|s| s.target == target)
            .map_or(now.saturating_sub(86_400), |s| s.started_at);
        let outcomes: Vec<Outcome> = self
            .store
            .telemetry_since(since, MAX_REPORT_ROWS)
            .await?
            .iter()
            .filter_map(outcome_of)
            .collect();
        let adv = advance(
            now,
            &self.cfg.rollout,
            &previous,
            &target,
            stored.clone(),
            &views,
            &outcomes,
            queued,
        );
        if stored.as_ref() != Some(&adv.state)
            && !self
                .store
                .save_rollout_state_if_current(stored.as_ref(), &adv.state, now)
                .await?
        {
            return Ok(());
        }
        for e in &adv.events {
            tracing::warn!("rollout: {e}");
            self.store.log_rollout(now, e).await?;
        }
        self.update_snapshot(|s| {
            s.rollout = Some(RolloutView {
                state: adv.state,
                checks: adv.checks,
                waiting: adv.waiting,
                new_group: adv.new_group,
                control_group: adv.control_group,
            });
        });
        Ok(())
    }
}
