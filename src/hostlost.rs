//! Jobs whose runner host was lost, and their one automatic rerun.
//!
//! A host that dies (a stopped or reclaimed VM, memory exhaustion, a lost
//! network) takes its runner with it. GitHub notices minutes later and ends
//! the job as an ordinary `failure`: its running step stays `in_progress`
//! with no conclusion and there is no exit code. Until now a person had to
//! notice and re-run it.
//!
//! The queen already holds the authoritative signal: the drone stopped
//! heartbeating and the lease ended with no result. This module turns that
//! into a `host_lost` record, distinct from a job failure, and re-runs the job
//! once through the jobs rerun API ([`crate::requeue::Actions`], the same call
//! the preemption requeuer makes).
//!
//! # Classification (the queen's own evidence)
//!
//! A lease is `host_lost` when it was running a job, ended without a result
//! from its drone, was not preempted by the queen, and its drone was not
//! heard from: silent for longer than `drone_stale_secs` when the lease ended,
//! back from such a silence that began after the lease was granted, or removed
//! as expired. Two guards keep the queen's own outages from reading as host
//! loss: the queen must have been listening for longer than `drone_stale_secs`
//! itself, and when other drones exist at least one of them must have been
//! heard inside that window. See [`classify_end`].
//!
//! The drone is marked suspect for `suspect_cooldown_secs`: it gets no new
//! leases until then, so the rerun lands on another drone. A result the drone
//! reports later for the lease withdraws the record.
//!
//! # Rerun (GitHub's evidence, read at rerun time)
//!
//! A `host_lost` job is re-run only when all of these hold; see [`judge`]:
//!
//! * it is still the newest attempt of that job in its run (nobody re-ran it);
//! * GitHub concluded it `failure` and no step of it concluded `failure` or
//!   `timed_out`: a step that failed is the job's own verdict, whatever
//!   happened to the host afterwards;
//! * it was not cancelled (by a person, a concurrency group, or the supersede
//!   janitor) and did not succeed;
//! * its commit is still wanted: a `pull_request` run names an open pull
//!   request whose head is the run's commit, and a `push` run has no newer run
//!   of the same workflow on its branch;
//! * its run has completed (GitHub refuses a rerun before that).
//!
//! # Caps and backoff ([`gate`])
//!
//! * one automatic rerun per job attempt: one record per lease, decided once;
//! * at most `max_reruns_per_run_per_hour` per workflow run, after which
//!   further host-lost jobs of that run are left alone;
//! * at most `max_reruns_per_hour` across the fleet, after which reruns wait;
//! * a rerun waits `backoff_secs` after the loss, doubling for each rerun the
//!   run already had, up to `max_backoff_secs`;
//! * a record older than `max_age_secs` is left alone.
//!
//! Nothing here updates a branch or re-runs a job that failed on its own.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::classes::{Class, Scope};
use crate::github::{GithubError, RateLimit, TokenSource};
use crate::requeue::Actions;
use crate::store::{LeaseRow, Store, StoreError, enum_from, enum_text};
use crate::telemetry::Conclusion;

/// Pending records read per pass.
const BATCH: i64 = 200;
/// Pages of a run's jobs read at most (100 per page).
const MAX_JOB_PAGES: u32 = 5;
const HOUR: i64 = 3600;
/// Records are kept this long for `borg report` (30 days).
const KEEP_SECS: i64 = 30 * 86_400;
/// A rerun claim still in flight after this long was interrupted (the queen
/// stopped before GitHub answered) and is released.
const CLAIM_GRACE_SECS: i64 = 600;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HostLostSettings {
    /// Re-run host-lost jobs. `false` still classifies, marks the drone
    /// suspect and reports.
    pub rerun: bool,
    /// Log what would be re-run without calling GitHub's rerun.
    pub dry_run: bool,
    /// Seconds between rerun passes.
    pub interval_secs: u64,
    /// Wait this long after the loss before the first rerun of a run.
    pub backoff_secs: u64,
    /// The backoff doubles per earlier rerun of the run, up to this.
    pub max_backoff_secs: u64,
    /// Automatic reruns per workflow run per hour.
    pub max_reruns_per_run_per_hour: u32,
    /// Automatic reruns per hour across every repository.
    pub max_reruns_per_hour: u32,
    /// A drone that lost a lease gets no new leases for this long.
    pub suspect_cooldown_secs: u64,
    /// A record this old is left alone.
    pub max_age_secs: u64,
}

impl Default for HostLostSettings {
    fn default() -> Self {
        Self {
            rerun: true,
            dry_run: false,
            interval_secs: 30,
            backoff_secs: 60,
            max_backoff_secs: 900,
            max_reruns_per_run_per_hour: 2,
            max_reruns_per_hour: 30,
            suspect_cooldown_secs: 900,
            max_age_secs: 6 * 3600,
        }
    }
}

impl HostLostSettings {
    pub fn validate(&self) -> Result<(), String> {
        if self.interval_secs == 0 || self.max_age_secs == 0 {
            return Err("host_lost interval_secs and max_age_secs must be positive".into());
        }
        if self.max_backoff_secs < self.backoff_secs {
            return Err("host_lost max_backoff_secs may not be below backoff_secs".into());
        }
        Ok(())
    }

    /// Seconds a rerun waits after the loss when its run already had
    /// `earlier` automatic reruns.
    pub fn backoff_for(&self, earlier: u32) -> i64 {
        let doubled = self
            .backoff_secs
            .saturating_mul(1u64.checked_shl(earlier).unwrap_or(u64::MAX));
        secs(doubled.min(self.max_backoff_secs))
    }
}

fn secs(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

// ------------------------------------------------------- classification

/// What the queen knows when a lease ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseEnd {
    /// The lease's runner had picked up a job.
    pub running: bool,
    /// The drone reported a result for the lease.
    pub reported: bool,
    /// The queen cancelled the lease for higher-priority demand.
    pub preempted: bool,
    pub created_at: i64,
    pub ended_at: i64,
    /// The drone is being removed as expired.
    pub drone_removed: bool,
    pub last_heartbeat: Option<i64>,
    /// The drone's newest finished silence: `(last heartbeat before, heartbeat that ended it)`.
    pub silence: Option<(i64, i64)>,
    /// The newest heartbeat of any other drone; `None` when no other drone
    /// has ever heartbeated.
    pub others_heard: Option<i64>,
    /// When this queen process started listening; `None` when unknown.
    pub queen_started_at: Option<i64>,
    pub stale_secs: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndClass {
    /// No job was running: nothing was lost.
    Idle,
    /// The drone reported the job's result: the job's own outcome stands.
    Reported,
    /// The queen preempted it; [`crate::requeue`] re-runs those.
    Preempted,
    /// The queen could not have heard the drone (it just started, or it hears
    /// no drone at all): no evidence either way.
    QueenBlind,
    /// The drone was heard throughout: the host is alive.
    HostAlive,
    /// The drone went silent under the job.
    HostLost { silent_secs: i64 },
}

/// Classify a lease that ended. Only [`EndClass::HostLost`] is ever re-run.
// formal: begin borg_host_lost_rerun/HostLost.Detect
pub fn classify_end(e: &LeaseEnd) -> EndClass {
    if !e.running {
        return EndClass::Idle;
    }
    if e.reported {
        return EndClass::Reported;
    }
    if e.preempted {
        return EndClass::Preempted;
    }
    let listening_for = e
        .queen_started_at
        .map_or(i64::MAX, |t| e.ended_at.saturating_sub(t));
    let others_silent = e
        .others_heard
        .is_some_and(|t| e.ended_at.saturating_sub(t) > e.stale_secs);
    if listening_for <= e.stale_secs || others_silent {
        return EndClass::QueenBlind;
    }
    let Some(last) = e.last_heartbeat else {
        // A drone that never heartbeated holds no lease; a removed one is gone.
        return if e.drone_removed {
            EndClass::HostLost { silent_secs: 0 }
        } else {
            EndClass::HostAlive
        };
    };
    let silent = e.ended_at.saturating_sub(last);
    if e.drone_removed || silent > e.stale_secs {
        return EndClass::HostLost {
            silent_secs: silent.max(0),
        };
    }
    match e.silence {
        // Back from a silence that ended after the lease was granted: the
        // host (or its drone) restarted under the job.
        Some((from, until)) if until > e.created_at && until <= e.ended_at => EndClass::HostLost {
            silent_secs: until.saturating_sub(from).max(0),
        },
        _ => EndClass::HostAlive,
    }
}
// formal: end borg_host_lost_rerun/HostLost.Detect

// ------------------------------------------------------------- decision

/// Why a host-lost job is left alone. Stored as its `reason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Skip {
    /// A step of the job failed: the job's own verdict, never re-run.
    JobFailed,
    /// Cancelled by a person, a concurrency group or the supersede janitor.
    JobCancelled,
    /// The job did not fail.
    JobNotFailed,
    /// A newer attempt of the job exists: someone re-ran it already.
    AlreadyRerun,
    /// No open pull request has the run's commit as its head.
    HeadChanged,
    /// A newer push run of the same workflow owns the branch's verdict.
    Superseded,
    /// The run had its automatic reruns for the hour.
    RunCap,
    /// Older than `max_age_secs`.
    Stale,
    /// The queen never saw which job the lease ran.
    Unbound,
    /// The drone reported a result after all.
    DroneReported,
    /// GitHub no longer has the run or job.
    Gone,
    /// GitHub refused the rerun (403, 404 or 409).
    Refused,
    /// `host_lost.rerun` is off.
    RerunOff,
}

impl Skip {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::JobFailed => "job_failed",
            Self::JobCancelled => "job_cancelled",
            Self::JobNotFailed => "job_not_failed",
            Self::AlreadyRerun => "already_rerun",
            Self::HeadChanged => "head_changed",
            Self::Superseded => "superseded",
            Self::RunCap => "run_cap",
            Self::Stale => "stale",
            Self::Unbound => "unbound",
            Self::DroneReported => "drone_reported",
            Self::Gone => "gone",
            Self::Refused => "refused",
            Self::RerunOff => "rerun_off",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// Inside the backoff after the loss.
    Backoff,
    /// The fleet-wide hourly cap is spent.
    FleetCap,
    /// GitHub has not concluded the job or its run yet.
    InProgress,
    /// The job binding has not arrived yet.
    Unbound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Rerun,
    Wait(Wait),
    Skip(Skip),
}

/// Automatic reruns already made, for the caps.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Of this record's run, in the last hour.
    pub run_last_hour: u32,
    /// Of this record's run, ever.
    pub run_total: u32,
    /// Across the fleet, in the last hour.
    pub fleet_last_hour: u32,
}

/// The caps and backoff, from the queen's own records (no GitHub call).
/// `None` lets the record go on to [`judge`].
// formal: begin borg_host_lost_rerun/HostLost.Eligible
pub fn gate(lost_at: i64, c: &Counts, s: &HostLostSettings, now: i64) -> Option<Decision> {
    if now.saturating_sub(lost_at) > secs(s.max_age_secs) {
        return Some(Decision::Skip(Skip::Stale));
    }
    if !s.rerun {
        return Some(Decision::Skip(Skip::RerunOff));
    }
    if c.run_last_hour >= s.max_reruns_per_run_per_hour {
        return Some(Decision::Skip(Skip::RunCap));
    }
    if now < lost_at.saturating_add(s.backoff_for(c.run_total)) {
        return Some(Decision::Wait(Wait::Backoff));
    }
    if c.fleet_last_hour >= s.max_reruns_per_hour {
        return Some(Decision::Wait(Wait::FleetCap));
    }
    None
}

/// The job as GitHub reports it now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobFacts {
    pub completed: bool,
    pub conclusion: Conclusion,
    /// A step concluded `failure` or `timed_out`.
    pub step_failed: bool,
}

/// Whether the run's commit is still the one wanted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Head {
    Current,
    Changed,
    Superseded,
}

/// GitHub's evidence. `job` is `None` when the job is no longer the newest
/// attempt in its run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facts {
    pub job: Option<JobFacts>,
    pub run_completed: bool,
    pub head: Head,
}

/// Decide from GitHub's evidence. A job that failed on its own, was
/// cancelled, succeeded or was already re-run is never re-run.
pub fn judge(f: &Facts) -> Decision {
    let Some(job) = f.job else {
        return Decision::Skip(Skip::AlreadyRerun);
    };
    if !job.completed {
        return Decision::Wait(Wait::InProgress);
    }
    match job.conclusion {
        Conclusion::Failure if job.step_failed => return Decision::Skip(Skip::JobFailed),
        Conclusion::Failure => {}
        Conclusion::TimedOut => return Decision::Skip(Skip::JobFailed),
        Conclusion::Cancelled => return Decision::Skip(Skip::JobCancelled),
        Conclusion::Success | Conclusion::Skipped | Conclusion::Unknown => {
            return Decision::Skip(Skip::JobNotFailed);
        }
    }
    match f.head {
        Head::Changed => return Decision::Skip(Skip::HeadChanged),
        Head::Superseded => return Decision::Skip(Skip::Superseded),
        Head::Current => {}
    }
    if !f.run_completed {
        return Decision::Wait(Wait::InProgress);
    }
    Decision::Rerun
}
// formal: end borg_host_lost_rerun/HostLost.Eligible

// ---------------------------------------------------------------- store

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Rerun,
    Skipped,
}

/// One lease that ended because its drone was lost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostLostRow {
    pub lease_id: String,
    pub drone: String,
    pub class: Class,
    pub scope: Scope,
    /// The job the lease ran; unset until the queen has seen it.
    pub repo: Option<String>,
    pub run_id: Option<u64>,
    pub job_id: Option<u64>,
    pub job_name: Option<String>,
    /// When the lease ended.
    pub at: i64,
    /// How long the drone had been silent.
    pub silent_secs: u64,
    /// Unset while a rerun is still possible.
    pub outcome: Option<Outcome>,
    /// Why a skipped job was left alone (a [`Skip`] name).
    pub reason: Option<String>,
    pub decided_at: Option<i64>,
}

fn u(v: Option<i64>) -> Option<u64> {
    v.and_then(|v| u64::try_from(v).ok())
}

fn row(r: &sqlx::sqlite::SqliteRow) -> Result<HostLostRow, StoreError> {
    Ok(HostLostRow {
        lease_id: r.get("lease_id"),
        drone: r.get("drone"),
        class: enum_from(&r.get::<String, _>("class"))?,
        scope: enum_from(&r.get::<String, _>("scope"))?,
        repo: r.get("repo"),
        run_id: u(r.get("run_id")),
        job_id: u(r.get("job_id")),
        job_name: r.get("job_name"),
        at: r.get("at"),
        silent_secs: u(r.get("silent_secs")).unwrap_or(0),
        outcome: r
            .get::<Option<String>, _>("outcome")
            .as_deref()
            .map(enum_from)
            .transpose()?,
        reason: r.get("reason"),
        decided_at: r.get("decided_at"),
    })
}

const COLUMNS: &str = "lease_id, drone, class, scope, repo, run_id, job_id, job_name, at, silent_secs, outcome, reason, decided_at";

impl Store {
    /// Remember a silence that this heartbeat ends: the drone was last heard
    /// (by this queen process, listening since `listening_since`) more than
    /// `stale_secs` ago. Call before the heartbeat is recorded.
    pub async fn note_silence(
        &self,
        drone: &str,
        now: i64,
        listening_since: Option<i64>,
        stale_secs: i64,
    ) -> Result<(), StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query(
            "UPDATE drones SET silent_from = last_heartbeat, silent_until = ?
             WHERE name = ? AND last_heartbeat IS NOT NULL AND ? - MAX(last_heartbeat, ?) > ?",
        )
        .bind(now)
        .bind(drone)
        .bind(now)
        .bind(listening_since.unwrap_or(i64::MIN))
        .bind(stale_secs)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Whether the queen preempted the lease.
    pub(crate) async fn lease_preempted(&self, lease_id: &str) -> Result<bool, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let row = sqlx::query("SELECT preempted_at FROM leases WHERE id = ?")
            .bind(lease_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some_and(|r| r.get::<Option<i64>, _>("preempted_at").is_some()))
    }

    /// Record a host-lost lease once. Returns false if it was already recorded.
    pub async fn record_host_lost(
        &self,
        lease: &LeaseRow,
        at: i64,
        silent_secs: i64,
    ) -> Result<bool, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let inserted = sqlx::query(
            "INSERT OR IGNORE INTO host_lost (lease_id, drone, class, scope, at, silent_secs)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&lease.id)
        .bind(&lease.drone)
        .bind(enum_text(&lease.class)?)
        .bind(enum_text(&lease.scope)?)
        .bind(at)
        .bind(silent_secs.max(0))
        .execute(&self.pool)
        .await?
        .rows_affected();
        self.bind_host_lost_jobs().await?;
        Ok(inserted > 0)
    }

    /// Fill the job of undecided records from the lease's host record, which
    /// the queen binds when it sees the runner's job.
    pub(crate) async fn bind_host_lost_jobs(&self) -> Result<(), StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query(
            "UPDATE host_lost SET
               repo = (SELECT h.repo FROM lease_hosts h WHERE h.lease_id = host_lost.lease_id),
               run_id = (SELECT h.run_id FROM lease_hosts h WHERE h.lease_id = host_lost.lease_id),
               job_id = (SELECT h.job_id FROM lease_hosts h WHERE h.lease_id = host_lost.lease_id),
               job_name = (SELECT h.job_name FROM lease_hosts h WHERE h.lease_id = host_lost.lease_id)
             WHERE job_id IS NULL AND outcome IS NULL AND EXISTS
               (SELECT 1 FROM lease_hosts h WHERE h.lease_id = host_lost.lease_id
                AND h.job_id IS NOT NULL AND h.run_id IS NOT NULL AND h.repo IS NOT NULL)",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// No new leases for `drone` until `until` (never shortens a cool-down).
    pub async fn suspect_drone(&self, drone: &str, until: i64) -> Result<(), StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query(
            "UPDATE drones SET suspect_until = MAX(COALESCE(suspect_until, 0), ?) WHERE name = ?",
        )
        .bind(until)
        .bind(drone)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// End a drone's cool-down: it reported the result of `reported`, so that
    /// lease's host was not lost. The cool-down stays while another lease of
    /// the drone lost at or after `since` (the start of the cool-down window)
    /// has not been reported: that loss still holds the drone.
    /// "Reported" is whether a result arrived ([`Store::withdraw_host_lost`]
    /// marks it), not the rerun decision, which keeps an earlier skip reason.
    pub async fn clear_suspect(
        &self,
        drone: &str,
        reported: &str,
        since: i64,
    ) -> Result<(), StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query(
            "UPDATE drones SET suspect_until = NULL WHERE name = ? AND NOT EXISTS
               (SELECT 1 FROM host_lost h WHERE h.drone = ? AND h.lease_id != ? AND h.at >= ?
                AND h.reported = 0 AND COALESCE(h.reason, '') != ?)",
        )
        .bind(drone)
        .bind(drone)
        .bind(reported)
        .bind(since)
        .bind(Skip::DroneReported.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Whether `drone` is inside a host-lost cool-down.
    pub async fn drone_suspect(&self, drone: &str, now: i64) -> Result<bool, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let row = sqlx::query("SELECT suspect_until FROM drones WHERE name = ?")
            .bind(drone)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row
            .and_then(|r| r.get::<Option<i64>, _>("suspect_until"))
            .is_some_and(|until| now < until))
    }

    /// Undecided records, oldest first.
    pub async fn pending_host_lost(&self, limit: i64) -> Result<Vec<HostLostRow>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let rows = sqlx::query(&format!(
            "SELECT {COLUMNS} FROM host_lost WHERE outcome IS NULL ORDER BY at, lease_id LIMIT ?"
        ))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row).collect()
    }

    /// Decide a record once. Returns false if it was already decided, so a
    /// job attempt is never re-run twice.
    pub async fn decide_host_lost(
        &self,
        lease_id: &str,
        outcome: Outcome,
        reason: Option<Skip>,
        now: i64,
    ) -> Result<bool, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let n = sqlx::query(
            "UPDATE host_lost SET outcome = ?, reason = ?, decided_at = ?
             WHERE lease_id = ? AND outcome IS NULL",
        )
        .bind(enum_text(&outcome)?)
        .bind(reason.map(Skip::as_str))
        .bind(now)
        .bind(lease_id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n > 0)
    }

    /// Claim an undecided record for a rerun before asking GitHub. The claim
    /// counts toward the caps like a rerun, and stays marked in flight until
    /// [`Store::confirm_host_lost_claim`]; one a crash leaves in flight is
    /// released by [`Store::release_interrupted_claims`]. Returns false if
    /// the record was already decided.
    pub async fn claim_host_lost(&self, lease_id: &str, now: i64) -> Result<bool, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let n = sqlx::query(
            "UPDATE host_lost SET outcome = ?, reason = NULL, decided_at = ?, claimed = 1
             WHERE lease_id = ? AND outcome IS NULL",
        )
        .bind(enum_text(&Outcome::Rerun)?)
        .bind(now)
        .bind(lease_id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n > 0)
    }

    /// GitHub accepted the claimed rerun: the record is a confirmed rerun.
    pub async fn confirm_host_lost_claim(&self, lease_id: &str) -> Result<(), StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query("UPDATE host_lost SET claimed = 0 WHERE lease_id = ? AND outcome = ?")
            .bind(lease_id)
            .bind(enum_text(&Outcome::Rerun)?)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Release claims still in flight that were made before `before`: the
    /// queen stopped between the claim and GitHub's answer. Each returns to
    /// undecided (or is skipped as `drone_reported` if its drone reported),
    /// so the next judgement reads GitHub again: a rerun that did reach
    /// GitHub left a newer attempt, which is never re-run twice. Returns how
    /// many were released.
    pub async fn release_interrupted_claims(&self, before: i64) -> Result<u64, StoreError> {
        let skipped = enum_text(&Outcome::Skipped)?;
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let n = sqlx::query(
            "UPDATE host_lost SET
               outcome = CASE WHEN reason = ? THEN ? ELSE NULL END,
               decided_at = CASE WHEN reason = ? THEN decided_at ELSE NULL END,
               claimed = 0
             WHERE outcome = ? AND claimed = 1 AND decided_at < ?",
        )
        .bind(Skip::DroneReported.as_str())
        .bind(&skipped)
        .bind(Skip::DroneReported.as_str())
        .bind(enum_text(&Outcome::Rerun)?)
        .bind(before)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n)
    }

    /// Settle a record the pass claimed as `rerun` whose rerun GitHub did not
    /// accept. `refused` makes it a terminal skip. `None` (the request failed
    /// and may be retried) returns it to undecided, unless its drone reported
    /// a result meanwhile: then it is skipped as `drone_reported`.
    pub async fn settle_host_lost_claim(
        &self,
        lease_id: &str,
        refused: Option<Skip>,
        now: i64,
    ) -> Result<(), StoreError> {
        let rerun = enum_text(&Outcome::Rerun)?;
        let skipped = enum_text(&Outcome::Skipped)?;
        if let Some(why) = refused {
            // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
            sqlx::query(
                "UPDATE host_lost SET outcome = ?, reason = ?, decided_at = ?, claimed = 0
                 WHERE lease_id = ? AND outcome = ?",
            )
            .bind(&skipped)
            .bind(why.as_str())
            .bind(now)
            .bind(lease_id)
            .bind(&rerun)
            .execute(&self.pool)
            .await?;
            return Ok(());
        }
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query(
            "UPDATE host_lost SET
               outcome = CASE WHEN reason = ? THEN ? ELSE NULL END,
               decided_at = CASE WHEN reason = ? THEN ? ELSE NULL END,
               claimed = 0
             WHERE lease_id = ? AND outcome = ?",
        )
        .bind(Skip::DroneReported.as_str())
        .bind(&skipped)
        .bind(Skip::DroneReported.as_str())
        .bind(now)
        .bind(lease_id)
        .bind(&rerun)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The drone of a lease reported its result. An undecided record is
    /// skipped as `drone_reported`; a decided one keeps its outcome, and a
    /// claimed rerun is marked so a failed request does not return it to the
    /// pass. One statement, so a claim released between two steps cannot
    /// slip through unmarked. Returns whether the lease has a host-lost
    /// record at all.
    pub async fn withdraw_host_lost(&self, lease_id: &str, now: i64) -> Result<bool, StoreError> {
        let rerun = enum_text(&Outcome::Rerun)?;
        // SQLite evaluates every SET expression against the row as it was.
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let n = sqlx::query(
            "UPDATE host_lost SET
               reason = CASE WHEN outcome IS NULL OR (outcome = ? AND reason IS NULL)
                        THEN ? ELSE reason END,
               decided_at = CASE WHEN outcome IS NULL THEN ? ELSE decided_at END,
               outcome = CASE WHEN outcome IS NULL THEN ? ELSE outcome END,
               reported = 1
             WHERE lease_id = ?",
        )
        .bind(&rerun)
        .bind(Skip::DroneReported.as_str())
        .bind(now)
        .bind(enum_text(&Outcome::Skipped)?)
        .bind(lease_id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n > 0)
    }

    /// Automatic reruns decided at or after `since`: of one run, or of every
    /// run when `run` is `None`.
    pub async fn host_lost_reruns(
        &self,
        run: Option<(&str, u64)>,
        since: i64,
    ) -> Result<u32, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let n: i64 = sqlx::query(
            "SELECT COUNT(*) AS n FROM host_lost WHERE outcome = 'rerun' AND decided_at >= ?
             AND (? IS NULL OR (repo = ? AND run_id = ?))",
        )
        .bind(since)
        .bind(run.map(|(repo, _)| repo))
        .bind(run.map(|(repo, _)| repo))
        .bind(run.map(|(_, id)| i64::try_from(id).unwrap_or(i64::MAX)))
        .fetch_one(&self.pool)
        .await?
        .get("n");
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    }

    /// Records of leases lost at or after `since`, newest first.
    pub async fn host_lost_since(
        &self,
        since: i64,
        limit: i64,
    ) -> Result<Vec<HostLostRow>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let rows = sqlx::query(&format!(
            "SELECT {COLUMNS} FROM host_lost WHERE at >= ? ORDER BY at DESC, lease_id LIMIT ?"
        ))
        .bind(since)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row).collect()
    }

    /// Delete records of leases lost before `before`; returns how many.
    pub async fn prune_host_lost(&self, before: i64) -> Result<u64, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let r = sqlx::query("DELETE FROM host_lost WHERE at < ?")
            .bind(before)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected())
    }
}

// ----------------------------------------------------------------- pass

/// What one pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct PassReport {
    /// Undecided records examined.
    pub checked: u32,
    /// Jobs re-run.
    pub rerun: u32,
    /// Jobs a dry-run pass would have re-run.
    pub would_rerun: u32,
    /// Records left for a later pass (backoff, caps, run still in progress).
    pub waiting: u32,
    /// Records left alone for good.
    pub skipped: u32,
    /// Failures that will be retried next pass.
    pub errors: u32,
}

impl PassReport {
    fn add(&mut self, o: &PassReport) {
        self.checked += o.checked;
        self.rerun += o.rerun;
        self.would_rerun += o.would_rerun;
        self.waiting += o.waiting;
        self.skipped += o.skipped;
        self.errors += o.errors;
    }
}

/// The host-lost rerunner as `/status` and `/metrics` show it.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct HostLostView {
    pub passes: u64,
    pub last: PassReport,
    /// Sums over every pass since the queen started.
    pub total: PassReport,
}

#[derive(Debug, Deserialize)]
struct Run {
    status: String,
    #[serde(default)]
    event: String,
    #[serde(default)]
    head_sha: String,
    #[serde(default)]
    head_branch: Option<String>,
    #[serde(default)]
    workflow_id: u64,
    #[serde(default)]
    run_number: u64,
    #[serde(default)]
    pull_requests: Vec<PullRef>,
}

#[derive(Debug, Deserialize)]
struct PullRef {
    #[serde(default)]
    head: Option<PullHead>,
}

#[derive(Debug, Deserialize)]
struct PullHead {
    #[serde(default)]
    sha: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Jobs {
    #[serde(default)]
    jobs: Vec<Job>,
}

#[derive(Debug, Deserialize)]
struct Job {
    id: u64,
    status: String,
    conclusion: Option<Conclusion>,
    #[serde(default)]
    steps: Vec<Step>,
}

#[derive(Debug, Deserialize)]
struct Step {
    conclusion: Option<Conclusion>,
}

#[derive(Debug, Deserialize)]
struct Runs {
    #[serde(default)]
    workflow_runs: Vec<RunRef>,
}

#[derive(Debug, Deserialize)]
struct RunRef {
    #[serde(default)]
    run_number: u64,
}

impl Job {
    fn facts(&self) -> JobFacts {
        JobFacts {
            completed: self.status == "completed",
            conclusion: self.conclusion.unwrap_or_default(),
            step_failed: self.steps.iter().any(|s| {
                matches!(
                    s.conclusion,
                    Some(Conclusion::Failure | Conclusion::TimedOut)
                )
            }),
        }
    }
}

/// Whether a pull-request run still names an open pull request at its commit.
/// GitHub fills a run's `pull_requests` at read time with the open pull
/// requests of its branch; a merged, closed or fork pull request lists none.
fn pull_request_head(run: &Run) -> Head {
    let current = run.pull_requests.iter().any(|p| {
        p.head
            .as_ref()
            .and_then(|h| h.sha.as_deref())
            .is_some_and(|sha| !sha.is_empty() && sha == run.head_sha)
    });
    if current {
        Head::Current
    } else {
        Head::Changed
    }
}

impl HostLostView {
    /// `(metric name, help, value)` for `/metrics`.
    pub(crate) fn metrics(&self) -> Vec<(String, String, u32)> {
        let t = &self.total;
        [
            ("checked", "Host-lost jobs the rerunner examined", t.checked),
            ("rerun", "Host-lost jobs re-run", t.rerun),
            (
                "would_rerun",
                "Host-lost jobs a dry-run pass would have re-run",
                t.would_rerun,
            ),
            (
                "waiting",
                "Host-lost jobs left for a later pass (backoff, caps, run in progress)",
                t.waiting,
            ),
            (
                "skipped",
                "Host-lost jobs left alone (own failure, cancelled, head changed, caps)",
                t.skipped,
            ),
            (
                "errors",
                "Host-lost rerun failures retried on a later pass",
                t.errors,
            ),
        ]
        .into_iter()
        .map(|(field, help, v)| {
            (
                format!("borg_host_lost_{field}_total"),
                format!("{help}, summed over passes since the queen started."),
                v,
            )
        })
        .collect()
    }
}

/// Run the rerun pass every `interval_secs` until `stop` changes. `None`
/// when the queen has no rerunner.
pub(crate) fn spawn_passes(
    queen: std::sync::Arc<crate::queen::Queen>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> Option<tokio::task::JoinHandle<()>> {
    queen.host_lost()?;
    Some(tokio::spawn(async move {
        let Some(rerunner) = queen.host_lost() else {
            return;
        };
        let mut every = tokio::time::interval(std::time::Duration::from_secs(
            rerunner.settings().interval_secs,
        ));
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            // Both arms are cancel-safe: `interval.tick` and `changed` hold no
            // state a dropped future could lose. The pass runs in the arm
            // body, outside the select, so shutdown waits for it and a rerun
            // is never requested without being recorded.
            tokio::select! {
                _ = every.tick() => {
                    let now = time::OffsetDateTime::now_utc().unix_timestamp();
                    if let Err(e) = rerunner.pass(queen.store(), now).await {
                        tracing::warn!("host-lost pass failed: {e}");
                    }
                }
                _ = stop.changed() => break,
            }
        }
    }))
}

pub struct HostLostRerunner {
    api: Actions,
    settings: HostLostSettings,
    view: Mutex<HostLostView>,
}

impl HostLostRerunner {
    pub fn new(
        api_base: &str,
        token: TokenSource,
        settings: HostLostSettings,
    ) -> Result<Self, GithubError> {
        Ok(Self {
            api: Actions::new(api_base, token, "borg-queen-host-lost")?,
            settings,
            view: Mutex::default(),
        })
    }

    #[must_use]
    pub fn with_rate_limit(mut self, rate_limit: RateLimit) -> Self {
        self.api.set_rate_limit(rate_limit);
        self
    }

    pub fn settings(&self) -> &HostLostSettings {
        &self.settings
    }

    pub fn view(&self) -> HostLostView {
        self.view.lock().expect("host-lost view lock").clone()
    }

    /// The job as the run's newest attempt lists it; `None` when a newer
    /// attempt of the job replaced it.
    async fn latest_job(&self, repo: &str, run: u64, job: u64) -> Result<Option<Job>, GithubError> {
        let path = format!("/repos/{repo}/actions/runs/{run}/jobs");
        for page in 1..=MAX_JOB_PAGES {
            let page_text = page.to_string();
            let jobs: Jobs = self
                .api
                .get(
                    &path,
                    &[
                        ("filter", "latest"),
                        ("per_page", "100"),
                        ("page", &page_text),
                    ],
                )
                .await?;
            let last = jobs.jobs.len() < 100;
            if let Some(found) = jobs.jobs.into_iter().find(|j| j.id == job) {
                return Ok(Some(found));
            }
            if last {
                break;
            }
        }
        Ok(None)
    }

    async fn head(&self, repo: &str, run: &Run) -> Result<Head, GithubError> {
        match run.event.as_str() {
            "pull_request" | "pull_request_target" => Ok(pull_request_head(run)),
            "push" => {
                let Some(branch) = run.head_branch.as_deref().filter(|b| !b.is_empty()) else {
                    return Ok(Head::Current);
                };
                let path = format!("/repos/{repo}/actions/workflows/{}/runs", run.workflow_id);
                let newer: Runs = self
                    .api
                    .get(
                        &path,
                        &[("branch", branch), ("event", "push"), ("per_page", "10")],
                    )
                    .await?;
                let superseded = newer
                    .workflow_runs
                    .iter()
                    .any(|r| r.run_number > run.run_number);
                Ok(if superseded {
                    Head::Superseded
                } else {
                    Head::Current
                })
            }
            _ => Ok(Head::Current),
        }
    }

    async fn facts(
        &self,
        repo: &str,
        run_id: u64,
        job_id: u64,
    ) -> Result<Option<Facts>, GithubError> {
        let run: Run = match self
            .api
            .get(&format!("/repos/{repo}/actions/runs/{run_id}"), &[])
            .await
        {
            Ok(run) => run,
            Err(GithubError::Status { status: 404, .. }) => return Ok(None),
            Err(e) => return Err(e),
        };
        let job = self.latest_job(repo, run_id, job_id).await?;
        // The head is read only for a job that could otherwise be re-run.
        let candidate = job.as_ref().is_some_and(|j| {
            let f = j.facts();
            f.completed && f.conclusion == Conclusion::Failure && !f.step_failed
        });
        let head = if candidate {
            self.head(repo, &run).await?
        } else {
            Head::Current
        };
        Ok(Some(Facts {
            job: job.as_ref().map(Job::facts),
            run_completed: run.status == "completed",
            head,
        }))
    }

    /// One pass over the undecided host-lost records.
    pub async fn pass(&self, store: &Store, now: i64) -> Result<PassReport, StoreError> {
        let s = &self.settings;
        let mut report = PassReport::default();
        store.prune_host_lost(now - KEEP_SECS).await?;
        store.bind_host_lost_jobs().await?;
        // Passes run one at a time, so a claim this old is not in flight.
        store
            .release_interrupted_claims(now - CLAIM_GRACE_SECS)
            .await?;
        let mut fleet = store.host_lost_reruns(None, now - HOUR).await?;
        for row in store.pending_host_lost(BATCH).await? {
            report.checked += 1;
            let skip =
                |why: Skip| store.decide_host_lost(&row.lease_id, Outcome::Skipped, Some(why), now);
            let (Some(repo), Some(run), Some(job)) = (row.repo.as_deref(), row.run_id, row.job_id)
            else {
                if now.saturating_sub(row.at) > secs(s.max_age_secs) {
                    skip(Skip::Unbound).await?;
                    report.skipped += 1;
                } else {
                    report.waiting += 1;
                }
                continue;
            };
            let counts = Counts {
                run_last_hour: store
                    .host_lost_reruns(Some((repo, run)), now - HOUR)
                    .await?,
                run_total: store.host_lost_reruns(Some((repo, run)), i64::MIN).await?,
                fleet_last_hour: fleet,
            };
            let decision = match gate(row.at, &counts, s, now) {
                Some(d) => d,
                None => match self.facts(repo, run, job).await {
                    Ok(Some(facts)) => judge(&facts),
                    Ok(None) => Decision::Skip(Skip::Gone),
                    Err(e) => {
                        tracing::warn!("host-lost: {repo} job {job} (run {run}): {e}");
                        report.errors += 1;
                        continue;
                    }
                },
            };
            match decision {
                Decision::Wait(_) => report.waiting += 1,
                Decision::Skip(why) => {
                    tracing::info!(
                        "host-lost: {repo} job {job} (run {run}) on {} left alone: {}",
                        row.drone,
                        why.as_str()
                    );
                    skip(why).await?;
                    report.skipped += 1;
                }
                Decision::Rerun if s.dry_run => {
                    tracing::info!(
                        "host-lost (dry-run): would rerun {repo} job {job} (run {run}) lost on {}",
                        row.drone
                    );
                    report.would_rerun += 1;
                }
                Decision::Rerun => {
                    // Claim the record before asking GitHub: a drone result
                    // that arrived while this pass read GitHub has already
                    // decided it, and then nothing is re-run. A claimed
                    // record counts toward the caps from here on.
                    if !store.claim_host_lost(&row.lease_id, now).await? {
                        report.skipped += 1;
                        continue;
                    }
                    match self.api.rerun_job(repo, job).await {
                        Ok(None) => {
                            tracing::info!(
                                "host-lost: re-ran {repo} job {job} (run {run}) lost on {}",
                                row.drone
                            );
                            store.confirm_host_lost_claim(&row.lease_id).await?;
                            fleet += 1;
                            report.rerun += 1;
                        }
                        Ok(Some(code)) => {
                            tracing::warn!(
                                "host-lost: {repo} job {job} (run {run}) rerun answered {code}, giving up"
                            );
                            store
                                .settle_host_lost_claim(&row.lease_id, Some(Skip::Refused), now)
                                .await?;
                            report.skipped += 1;
                        }
                        Err(e) => {
                            tracing::warn!("host-lost: {repo} job {job} (run {run}): {e}");
                            store
                                .settle_host_lost_claim(&row.lease_id, None, now)
                                .await?;
                            report.errors += 1;
                        }
                    }
                }
            }
        }
        let mut view = self.view.lock().expect("host-lost view lock");
        view.passes += 1;
        view.total.add(&report);
        view.last = report.clone();
        drop(view);
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STALE: i64 = 60;

    /// A running lease granted at 1000 that ended at 2000 with no result, on
    /// a queen listening since 0 that heard another drone at 1995.
    fn end() -> LeaseEnd {
        LeaseEnd {
            running: true,
            reported: false,
            preempted: false,
            created_at: 1000,
            ended_at: 2000,
            drone_removed: false,
            last_heartbeat: Some(1995),
            silence: None,
            others_heard: Some(1995),
            queen_started_at: Some(0),
            stale_secs: STALE,
        }
    }

    #[test]
    fn a_silent_drone_with_no_result_is_host_lost() {
        let e = LeaseEnd {
            last_heartbeat: Some(1460),
            ..end()
        };
        assert_eq!(classify_end(&e), EndClass::HostLost { silent_secs: 540 });
    }

    #[test]
    fn a_heard_drone_is_alive_whatever_the_job_did() {
        assert_eq!(classify_end(&end()), EndClass::HostAlive);
        // Exactly at the threshold is still heard.
        let at_threshold = LeaseEnd {
            last_heartbeat: Some(2000 - STALE),
            ..end()
        };
        assert_eq!(classify_end(&at_threshold), EndClass::HostAlive);
    }

    #[test]
    fn a_reported_result_a_preemption_or_an_idle_runner_is_never_host_lost() {
        let silent = LeaseEnd {
            last_heartbeat: Some(1000),
            ..end()
        };
        assert!(matches!(classify_end(&silent), EndClass::HostLost { .. }));
        let reported = LeaseEnd {
            reported: true,
            ..silent
        };
        assert_eq!(classify_end(&reported), EndClass::Reported);
        let preempted = LeaseEnd {
            preempted: true,
            ..silent
        };
        assert_eq!(classify_end(&preempted), EndClass::Preempted);
        let idle = LeaseEnd {
            running: false,
            ..silent
        };
        assert_eq!(classify_end(&idle), EndClass::Idle);
    }

    #[test]
    fn a_drone_back_from_a_silence_under_the_lease_is_host_lost() {
        // Silent 1200..1500, heartbeating again when the lease ends.
        let back = LeaseEnd {
            silence: Some((1200, 1500)),
            ..end()
        };
        assert_eq!(classify_end(&back), EndClass::HostLost { silent_secs: 300 });
        // A silence that ended before the lease was granted says nothing.
        let earlier = LeaseEnd {
            silence: Some((700, 900)),
            ..end()
        };
        assert_eq!(classify_end(&earlier), EndClass::HostAlive);
    }

    #[test]
    fn a_queen_that_was_not_listening_classifies_nothing() {
        let silent = LeaseEnd {
            last_heartbeat: Some(1000),
            ..end()
        };
        // Restarted 30 s ago: every drone looks silent.
        let restarted = LeaseEnd {
            queen_started_at: Some(1970),
            ..silent
        };
        assert_eq!(classify_end(&restarted), EndClass::QueenBlind);
        // Listening, but no other drone was heard either: the queen's network.
        let deaf = LeaseEnd {
            others_heard: Some(1000),
            ..silent
        };
        assert_eq!(classify_end(&deaf), EndClass::QueenBlind);
        // A fleet of one has no other drone to compare with.
        let alone = LeaseEnd {
            others_heard: None,
            ..silent
        };
        assert!(matches!(classify_end(&alone), EndClass::HostLost { .. }));
    }

    #[test]
    fn a_removed_drone_lost_its_running_leases() {
        let removed = LeaseEnd {
            drone_removed: true,
            last_heartbeat: Some(100),
            ..end()
        };
        assert_eq!(
            classify_end(&removed),
            EndClass::HostLost { silent_secs: 1900 }
        );
    }

    fn lost_job() -> Facts {
        Facts {
            job: Some(JobFacts {
                completed: true,
                conclusion: Conclusion::Failure,
                step_failed: false,
            }),
            run_completed: true,
            head: Head::Current,
        }
    }

    fn with_job(conclusion: Conclusion, step_failed: bool) -> Facts {
        Facts {
            job: Some(JobFacts {
                completed: true,
                conclusion,
                step_failed,
            }),
            ..lost_job()
        }
    }

    #[test]
    fn only_a_failure_with_no_failed_step_is_rerun() {
        assert_eq!(judge(&lost_job()), Decision::Rerun);
        assert_eq!(
            judge(&with_job(Conclusion::Failure, true)),
            Decision::Skip(Skip::JobFailed)
        );
        assert_eq!(
            judge(&with_job(Conclusion::TimedOut, false)),
            Decision::Skip(Skip::JobFailed)
        );
        assert_eq!(
            judge(&with_job(Conclusion::Cancelled, false)),
            Decision::Skip(Skip::JobCancelled)
        );
        for c in [
            Conclusion::Success,
            Conclusion::Skipped,
            Conclusion::Unknown,
        ] {
            assert_eq!(
                judge(&with_job(c, false)),
                Decision::Skip(Skip::JobNotFailed)
            );
        }
    }

    #[test]
    fn a_moved_head_a_newer_push_or_an_earlier_rerun_is_left_alone() {
        for (head, why) in [
            (Head::Changed, Skip::HeadChanged),
            (Head::Superseded, Skip::Superseded),
        ] {
            let f = Facts { head, ..lost_job() };
            assert_eq!(judge(&f), Decision::Skip(why));
        }
        let rerun_already = Facts {
            job: None,
            ..lost_job()
        };
        assert_eq!(judge(&rerun_already), Decision::Skip(Skip::AlreadyRerun));
    }

    #[test]
    fn an_unfinished_job_or_run_waits() {
        let running_run = Facts {
            run_completed: false,
            ..lost_job()
        };
        assert_eq!(judge(&running_run), Decision::Wait(Wait::InProgress));
        let mut running_job = lost_job();
        running_job.job = Some(JobFacts {
            completed: false,
            conclusion: Conclusion::Unknown,
            step_failed: false,
        });
        assert_eq!(judge(&running_job), Decision::Wait(Wait::InProgress));
    }

    #[test]
    fn the_gate_applies_age_caps_and_backoff_in_order() {
        let s = HostLostSettings::default();
        let none = Counts::default();
        // Inside the backoff, then free.
        assert_eq!(
            gate(1000, &none, &s, 1059),
            Some(Decision::Wait(Wait::Backoff))
        );
        assert_eq!(gate(1000, &none, &s, 1060), None);
        // The run's hourly cap is terminal, the fleet's waits.
        let run_spent = Counts {
            run_last_hour: s.max_reruns_per_run_per_hour,
            run_total: s.max_reruns_per_run_per_hour,
            fleet_last_hour: 0,
        };
        assert_eq!(
            gate(1000, &run_spent, &s, 5000),
            Some(Decision::Skip(Skip::RunCap))
        );
        let fleet_spent = Counts {
            fleet_last_hour: s.max_reruns_per_hour,
            ..none
        };
        assert_eq!(
            gate(1000, &fleet_spent, &s, 5000),
            Some(Decision::Wait(Wait::FleetCap))
        );
        // Too old, whatever else holds.
        let old = 1000 + secs(s.max_age_secs) + 1;
        assert_eq!(
            gate(1000, &none, &s, old),
            Some(Decision::Skip(Skip::Stale))
        );
        // Switched off.
        let off = HostLostSettings {
            rerun: false,
            ..HostLostSettings::default()
        };
        assert_eq!(
            gate(1000, &none, &off, 5000),
            Some(Decision::Skip(Skip::RerunOff))
        );
    }

    #[test]
    fn the_backoff_doubles_per_earlier_rerun_up_to_the_cap() {
        let s = HostLostSettings::default();
        assert_eq!(s.backoff_for(0), 60);
        assert_eq!(s.backoff_for(1), 120);
        assert_eq!(s.backoff_for(2), 240);
        assert_eq!(s.backoff_for(4), 900);
        assert_eq!(s.backoff_for(200), 900);
        // The second rerun of a run waits the doubled backoff.
        let one = Counts {
            run_last_hour: 1,
            run_total: 1,
            fleet_last_hour: 1,
        };
        assert_eq!(
            gate(1000, &one, &s, 1119),
            Some(Decision::Wait(Wait::Backoff))
        );
        assert_eq!(gate(1000, &one, &s, 1120), None);
    }

    #[test]
    fn settings_reject_a_zero_interval_and_an_inverted_backoff() {
        assert!(HostLostSettings::default().validate().is_ok());
        let zero = HostLostSettings {
            interval_secs: 0,
            ..HostLostSettings::default()
        };
        assert!(zero.validate().is_err());
        let inverted = HostLostSettings {
            backoff_secs: 100,
            max_backoff_secs: 10,
            ..HostLostSettings::default()
        };
        assert!(inverted.validate().is_err());
    }

    #[test]
    fn a_pull_request_run_is_current_only_while_an_open_pull_request_has_its_head() {
        let run = |prs: &str| -> Run {
            serde_json::from_str(&format!(
                r#"{{"status":"completed","event":"pull_request","head_sha":"abc","pull_requests":{prs}}}"#
            ))
            .unwrap()
        };
        assert_eq!(
            pull_request_head(&run(r#"[{"number":1,"head":{"sha":"abc"}}]"#)),
            Head::Current
        );
        // The pull request moved on, was merged or closed, or comes from a fork.
        assert_eq!(
            pull_request_head(&run(r#"[{"number":1,"head":{"sha":"def"}}]"#)),
            Head::Changed
        );
        assert_eq!(pull_request_head(&run("[]")), Head::Changed);
        assert_eq!(pull_request_head(&run(r#"[{"number":1}]"#)), Head::Changed);
    }

    #[test]
    fn a_job_with_a_failed_step_is_the_jobs_own_failure() {
        let job = |steps: &str| -> Job {
            serde_json::from_str(&format!(
                r#"{{"id":1,"status":"completed","conclusion":"failure","steps":{steps}}}"#
            ))
            .unwrap()
        };
        // As GitHub lists a job whose runner was lost: the running step never concluded.
        let lost = job(
            r#"[{"conclusion":"success"},{"conclusion":null,"status":"in_progress"},{"conclusion":null,"status":"pending"}]"#,
        );
        assert!(!lost.facts().step_failed);
        let failed = job(r#"[{"conclusion":"success"},{"conclusion":"failure"}]"#);
        assert!(failed.facts().step_failed);
    }
}
