//! One pass of the quorum rollout gate ([`crate::rollout`]): the fleet and
//! queue as the rollout sees them, and the persisted result.

use std::collections::HashSet;

use super::{MAX_REPORT_ROWS, Queen, QueenError, RolloutView, outcome_of};
use crate::allocator::{QueueAge, max_slots};
use crate::classes::Class;
use crate::health::Outcome;
use crate::rollout::{DroneView, advance};
use crate::store::{LeaseRow, LeaseStatus};
use crate::units::UnitState;

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
        // A drone is busy while it holds a runner lease or a work unit: it
        // updates only once all of its held jobs finish, so both keep the
        // update clock from starting and both hold it out of a paused wave.
        let units = self
            .store
            .units_in(&[UnitState::Leased, UnitState::Running])
            .await?;
        let busy_drones: HashSet<&str> = active
            .iter()
            .filter(|lease| lease.state != LeaseStatus::Standby)
            .map(|lease| lease.drone.as_str())
            .chain(units.iter().filter_map(|u| u.drone.as_deref()))
            .collect();
        let fresh = |t: Option<i64>| t.is_some_and(|t| now - t <= self.cfg.drone_stale_secs);
        let queen_blind = queen_blind(
            now,
            self.started_at(),
            self.cfg.drone_stale_secs,
            rows.iter().any(|d| fresh(d.last_heartbeat)),
        );
        let views: Vec<DroneView> = rows
            .iter()
            .map(|d| DroneView {
                name: d.name.clone(),
                os: d
                    .capacity
                    .as_ref()
                    .map(|c| c.os.clone())
                    .unwrap_or_default(),
                fresh: fresh(d.last_heartbeat),
                queen_blind,
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

/// Whether this queen could not have heard the fleet lately, so a drone's
/// silence is no evidence against the version it runs: it hears no drone at
/// all, or it has not listened long enough for every live drone to reach it
/// (`listening_since` is `None` when unknown). A host-lost rerun skips a queen
/// restart the same way ([`crate::hostlost::EndClass::QueenBlind`]).
fn queen_blind(now: i64, listening_since: Option<i64>, stale_secs: i64, any_heard: bool) -> bool {
    let listening_for = listening_since.map_or(i64::MAX, |t| now.saturating_sub(t));
    listening_for <= restart_grace_secs(stale_secs) || !any_heard
}

/// How long after the queen starts listening every live drone has reached it.
/// A drone heartbeats every `I` seconds, and `I` is at most `stale_secs`: a
/// slower drone is never fresh, so the queen grants it nothing. After failing
/// against a down queen it waits [`backoff`] (at most the larger of `I` and
/// [`MAX_BACKOFF_SECS`]) and retries on its next interval tick, up to `I`
/// later.
///
/// [`backoff`]: crate::drone::selfupdate::backoff
/// [`MAX_BACKOFF_SECS`]: crate::drone::selfupdate::MAX_BACKOFF_SECS
fn restart_grace_secs(stale_secs: i64) -> i64 {
    let retry_cap = i64::try_from(crate::drone::selfupdate::MAX_BACKOFF_SECS).unwrap_or(i64::MAX);
    stale_secs.max(retry_cap).saturating_add(stale_secs)
}

#[cfg(test)]
mod tests {
    use super::{queen_blind, restart_grace_secs};
    use crate::drone::selfupdate::backoff;

    #[test]
    fn the_restart_grace_covers_the_slowest_interval_a_fresh_drone_can_have() {
        for stale in [10_i64, 60, 120, 600] {
            // Every interval a drone can heartbeat at and still read fresh,
            // after any run of failures against a down queen: its retry wait
            // plus the wait for the next tick of its interval.
            for interval in 1..=stale {
                for failures in 1..=10 {
                    let wait = backoff(interval as u64, failures) as i64;
                    let heard_by = wait + interval;
                    assert!(
                        heard_by <= restart_grace_secs(stale),
                        "stale {stale}, interval {interval}, failures {failures}"
                    );
                }
            }
        }
        assert!(queen_blind(1_240, Some(1_000), 120, true));
        assert!(!queen_blind(1_241, Some(1_000), 120, true));
        assert!(queen_blind(1_241, Some(1_000), 120, false));
        assert!(!queen_blind(1_241, None, 120, true));
    }
}
