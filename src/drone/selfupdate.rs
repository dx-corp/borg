//! The drone's half of self-update.
//!
//! The drone cannot replace its own binary or restart itself: it runs as an
//! unprivileged user, and stopping it is what drains it. So it only *asks*: it
//! writes `update-request` into its state directory, and a root-owned path unit
//! (systemd) or watch (launchd) starts `borg update --from-request`, which
//! downloads, verifies, drains, swaps, restarts and rolls back if needed. The
//! updater leaves `update-result.json`; the restarted drone reports it to the
//! queen in its next heartbeat. Health evidence for the updater is the
//! `health.json` this module writes after every successful heartbeat.

use std::fs;
use std::io;
use std::path::PathBuf;

use crate::api::UpdateReport;
use crate::update::health::{self, HealthRecord};
use crate::update::{Request, UpdateResult, request_path, result_path};

/// What to do about the queen's `desired_version` right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// Nothing to do: no instruction, or already on that version.
    Stay,
    /// Leases are held: take no new ones, finish these, update after.
    FinishJobsFirst,
    /// Idle: hand the update to the updater now.
    UpdateNow,
}

/// Between jobs the drone obeys `desired_version`, in either direction.
pub fn plan(desired: Option<&str>, own_version: &str, leases_held: usize) -> Plan {
    match desired {
        None => Plan::Stay,
        Some(d) if d == own_version => Plan::Stay,
        Some(_) if leases_held > 0 => Plan::FinishJobsFirst,
        Some(_) => Plan::UpdateNow,
    }
}

/// A request the updater has not consumed after this long is rewritten.
const STALE_REQUEST: std::time::Duration = std::time::Duration::from_secs(30 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handled {
    /// Already running the requested version.
    UpToDate,
    /// A request was written; the updater takes it from here.
    Requested,
    /// An update is already in flight; nothing more is asked.
    InFlight,
    /// The same target already failed or rolled back here; not retried.
    Blocked,
}

pub struct SelfUpdate {
    state_dir: PathBuf,
}

impl SelfUpdate {
    pub fn new(state_dir: PathBuf) -> Self {
        Self { state_dir }
    }

    fn result(&self) -> Option<UpdateResult> {
        serde_json::from_slice(&fs::read(result_path(&self.state_dir)).ok()?).ok()
    }

    /// The updater's outcome, until the queen has acknowledged it.
    pub fn pending_report(&self) -> Option<UpdateReport> {
        self.result().map(|r| UpdateReport {
            target: r.target,
            state: r.state,
            detail: r.detail,
        })
    }

    /// The queen has the report; forget it.
    pub fn acknowledge_report(&self) {
        let _ = fs::remove_file(result_path(&self.state_dir));
    }

    /// Act on the queen's `desired_version`. Any other version is a target,
    /// older ones included. At most one update is in flight: while a request
    /// file exists nothing further is written.
    pub fn handle(&self, desired: &str, own_version: &str) -> io::Result<Handled> {
        if desired == own_version {
            return Ok(Handled::UpToDate);
        }
        let request = request_path(&self.state_dir);
        if let Ok(meta) = fs::metadata(&request) {
            let age = meta.modified().ok().and_then(|m| m.elapsed().ok());
            if age.is_none_or(|a| a < STALE_REQUEST) {
                return Ok(Handled::InFlight);
            }
            // The updater never picked it up (or died); ask again.
            fs::remove_file(&request)?;
        }
        if self.result().is_some_and(|r| {
            r.target == desired && matches!(r.state.as_str(), "rolled_back" | "failed")
        }) {
            return Ok(Handled::Blocked);
        }
        let tmp = request.with_extension("tmp");
        fs::write(
            &tmp,
            serde_json::to_vec(&Request {
                version: desired.to_owned(),
            })
            .map_err(io::Error::other)?,
        )?;
        fs::rename(&tmp, &request)?;
        Ok(Handled::Requested)
    }

    /// Record a successful heartbeat for the updater's health check.
    pub fn record_health(&self, version: &str, now: i64) -> io::Result<()> {
        health::write(
            &self.health_path(),
            &HealthRecord {
                version: version.to_owned(),
                at: now,
                pid: std::process::id(),
            },
        )
    }

    pub fn health_path(&self) -> PathBuf {
        self.state_dir.join("health.json")
    }
}

/// Delay before the next heartbeat after `failures` consecutive failures: the
/// normal interval, doubling per failure, capped at a minute, so a queen that
/// is down is retried gently and one that comes back is found quickly.
pub fn backoff(interval_secs: u64, failures: u32) -> u64 {
    interval_secs
        .saturating_mul(1u64 << failures.min(6))
        .min(60)
        .max(interval_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directive_writes_one_request_and_then_waits() {
        let dir = tempfile::tempdir().unwrap();
        let su = SelfUpdate::new(dir.path().to_owned());
        assert_eq!(su.handle("1.0.0", "1.0.0").unwrap(), Handled::UpToDate);
        assert!(!request_path(dir.path()).exists());

        assert_eq!(su.handle("2.0.0", "1.0.0").unwrap(), Handled::Requested);
        let request: Request =
            serde_json::from_slice(&fs::read(request_path(dir.path())).unwrap()).unwrap();
        assert_eq!(request.version, "2.0.0");
        assert_eq!(
            su.handle("2.0.0", "1.0.0").unwrap(),
            Handled::InFlight,
            "at most one in flight"
        );
        assert_eq!(su.handle("3.0.0", "1.0.0").unwrap(), Handled::InFlight);
    }

    #[test]
    fn a_failed_target_is_not_retried_but_a_newer_one_is() {
        let dir = tempfile::tempdir().unwrap();
        let su = SelfUpdate::new(dir.path().to_owned());
        let result = UpdateResult {
            target: "2.0.0".into(),
            state: "rolled_back".into(),
            detail: "no health".into(),
            at: 1,
        };
        fs::write(
            result_path(dir.path()),
            serde_json::to_vec(&result).unwrap(),
        )
        .unwrap();
        assert_eq!(su.pending_report().unwrap().state, "rolled_back");
        assert_eq!(su.handle("2.0.0", "1.0.0").unwrap(), Handled::Blocked);
        assert_eq!(su.handle("2.1.0", "1.0.0").unwrap(), Handled::Requested);
        su.acknowledge_report();
        assert_eq!(su.pending_report(), None);
    }

    #[test]
    fn health_evidence_is_written_for_the_updater() {
        let dir = tempfile::tempdir().unwrap();
        let su = SelfUpdate::new(dir.path().to_owned());
        su.record_health("1.2.3", 77).unwrap();
        let rec = health::read(&su.health_path()).unwrap();
        assert_eq!((rec.version.as_str(), rec.at), ("1.2.3", 77));
    }

    #[test]
    fn the_drone_updates_only_between_jobs_and_in_either_direction() {
        assert_eq!(plan(None, "1.0.0", 3), Plan::Stay);
        assert_eq!(plan(Some("1.0.0"), "1.0.0", 0), Plan::Stay);
        assert_eq!(plan(Some("2.0.0"), "1.0.0", 2), Plan::FinishJobsFirst);
        assert_eq!(plan(Some("2.0.0"), "1.0.0", 0), Plan::UpdateNow);
        assert_eq!(
            plan(Some("0.9.0"), "1.0.0", 0),
            Plan::UpdateNow,
            "downgrades too"
        );
    }

    #[test]
    fn a_stale_request_is_asked_again() {
        let dir = tempfile::tempdir().unwrap();
        let su = SelfUpdate::new(dir.path().to_owned());
        assert_eq!(su.handle("2.0.0", "1.0.0").unwrap(), Handled::Requested);
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        fs::File::options()
            .write(true)
            .open(request_path(dir.path()))
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert_eq!(su.handle("2.0.0", "1.0.0").unwrap(), Handled::Requested);
        assert_eq!(su.handle("2.0.0", "1.0.0").unwrap(), Handled::InFlight);
    }

    #[test]
    fn heartbeat_backoff_grows_and_is_capped() {
        assert_eq!(backoff(10, 0), 10);
        assert_eq!(backoff(10, 1), 20);
        assert_eq!(backoff(10, 2), 40);
        assert_eq!(backoff(10, 3), 60);
        assert_eq!(backoff(10, 30), 60);
        assert_eq!(backoff(90, 4), 90, "never below the configured interval");
    }
}
