//! Per-lease job telemetry: the wire format drones report in a lease result,
//! its validation, and its storage.
//!
//! # Wire format
//!
//! `POST /v1/leases/{id}/result` accepts an optional `telemetry` object next to
//! `exit_code`, `duration_secs` and `peak_rss_mib`:
//!
//! ```json
//! {
//!   "exit_code": 0,
//!   "duration_secs": 412,
//!   "telemetry": {
//!     "job_name": "rust-test (shard 2)",
//!     "repo": "example-org/app",
//!     "run_id": 123456789,
//!     "conclusion": "failure",
//!     "failure_class": "host",
//!     "queue_wait_secs": 18,
//!     "steps": [
//!       { "name": "Set up job", "duration_secs": 3, "conclusion": "success" },
//!       { "name": "cargo test", "duration_secs": 401, "conclusion": "failure" }
//!     ]
//!   }
//! }
//! ```
//!
//! * `job_name` (required, at most 120 characters) and `repo` (required,
//!   `owner/name`) identify the job; `run_id` is the workflow run id.
//! * `conclusion`: `success`, `failure`, `cancelled`, `skipped`, `timed_out` or
//!   `unknown` (the default; any unrecognized value also reads as `unknown`).
//! * `failure_class`: `host` (the machine or runner failed: out of disk,
//!   runner lost, docker daemon down), `code` (the job's own steps failed) or
//!   `unknown` (the default). Only meaningful for a job that did not succeed;
//!   the queen stores `unknown` for `success` and `skipped`.
//! * `queue_wait_secs` (optional): seconds the job waited between being queued
//!   and starting on this runner.
//! * `steps` (optional, at most 64, extra entries are dropped): name (at most
//!   120 characters), `duration_secs`, `conclusion`.
//!
//! Everything is optional from the queen's point of view: a result without
//! `telemetry` is accepted exactly as before, and a `telemetry` object the
//! queen cannot use (empty `job_name`, malformed `repo`) is dropped with a
//! warning while the result itself is still recorded. Control characters are
//! stripped from every string. Reporting the same lease twice replaces the
//! first report.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::classes::{Class, Scope};
use crate::store::{LeaseRow, Store, StoreError};

pub const MAX_STEPS: usize = 64;
pub const MAX_NAME_CHARS: usize = 120;
/// Rows the report reads at most (newest first).
pub const MAX_REPORT_ROWS: i64 = 100_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Conclusion {
    Success,
    Failure,
    Cancelled,
    Skipped,
    TimedOut,
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FailureClass {
    Host,
    Code,
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepTiming {
    pub name: String,
    pub duration_secs: u64,
    #[serde(default)]
    pub conclusion: Conclusion,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobTelemetry {
    pub job_name: String,
    pub repo: String,
    #[serde(default)]
    pub run_id: u64,
    #[serde(default)]
    pub conclusion: Conclusion,
    #[serde(default)]
    pub failure_class: FailureClass,
    #[serde(default)]
    pub queue_wait_secs: Option<u64>,
    #[serde(default)]
    pub steps: Vec<StepTiming>,
}

fn clean(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME_CHARS)
        .collect::<String>()
        .trim()
        .to_owned()
}

impl JobTelemetry {
    /// The report as the queen stores it, or `None` if it is unusable.
    pub fn sanitized(mut self) -> Option<Self> {
        self.job_name = clean(&self.job_name);
        self.repo = clean(&self.repo);
        let repo_ok = self.repo.split('/').filter(|p| !p.is_empty()).count() == 2
            && !self.repo.contains(char::is_whitespace);
        if self.job_name.is_empty() || !repo_ok {
            return None;
        }
        self.steps.truncate(MAX_STEPS);
        for s in &mut self.steps {
            s.name = clean(&s.name);
        }
        if matches!(self.conclusion, Conclusion::Success | Conclusion::Skipped) {
            self.failure_class = FailureClass::Unknown;
        }
        Some(self)
    }
}

/// One stored report with the lease facts the queen added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetryRow {
    pub lease_id: String,
    pub drone: String,
    pub class: Class,
    pub scope: Scope,
    pub duration_secs: u64,
    pub finished_at: i64,
    pub telemetry: JobTelemetry,
}

fn text<T: Serialize>(v: &T) -> Result<String, StoreError> {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => Ok(s),
        _ => Err(StoreError::Corrupt("enum is not a string".to_owned())),
    }
}

fn parse<T: for<'de> Deserialize<'de>>(s: &str) -> Result<T, StoreError> {
    serde_json::from_str(&format!("\"{s}\"")).map_err(|e| StoreError::Corrupt(e.to_string()))
}

fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn to_u64(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

impl Store {
    /// Match both GitHub runner coordinates; never attribute a static or reused name.
    pub async fn lease_for_runner(
        &self,
        id: u64,
        name: &str,
    ) -> Result<Option<LeaseRow>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let row = sqlx::query("SELECT * FROM leases WHERE runner_id = ? AND runner_name = ?")
            .bind(to_i64(id))
            .bind(name)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(Self::lease_from_row).transpose()
    }

    /// Persist one GitHub job per ephemeral lease. Existing drone telemetry wins.
    pub async fn remember_job(
        &self,
        lease: &LeaseRow,
        repo: &str,
        job_id: u64,
        now: i64,
    ) -> Result<(), StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query(
            "INSERT OR IGNORE INTO lease_jobs (lease_id, repo, job_id, observed_at)
            SELECT ?, ?, ?, ? WHERE NOT EXISTS (SELECT 1 FROM job_telemetry WHERE lease_id = ?)",
        )
        .bind(&lease.id)
        .bind(repo)
        .bind(to_i64(job_id))
        .bind(now)
        .bind(&lease.id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn pending_jobs(&self) -> Result<Vec<(LeaseRow, String, u64)>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let rows = sqlx::query(
            "SELECT leases.*, lease_jobs.repo AS job_repo, lease_jobs.job_id AS observed_job_id
            FROM lease_jobs JOIN leases ON leases.id = lease_jobs.lease_id
            WHERE NOT EXISTS (SELECT 1 FROM job_telemetry WHERE job_telemetry.lease_id = leases.id)
            ORDER BY lease_jobs.observed_at, lease_jobs.lease_id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok((
                    Self::lease_from_row(row)?,
                    row.get("job_repo"),
                    to_u64(row.get("observed_job_id")),
                ))
            })
            .collect()
    }

    pub async fn forget_job(&self, lease_id: &str) -> Result<(), StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query("DELETE FROM lease_jobs WHERE lease_id = ?")
            .bind(lease_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Store (or replace) the telemetry of a finished lease.
    pub async fn record_telemetry(
        &self,
        lease: &LeaseRow,
        t: &JobTelemetry,
        duration_secs: u64,
        now: i64,
    ) -> Result<(), StoreError> {
        self.write_telemetry(lease, t, duration_secs, now, true)
            .await
    }

    /// Fill a missing report atomically; a concurrent drone report takes precedence.
    pub async fn record_github_telemetry(
        &self,
        lease: &LeaseRow,
        t: &JobTelemetry,
        duration_secs: u64,
        now: i64,
    ) -> Result<(), StoreError> {
        self.write_telemetry(lease, t, duration_secs, now, false)
            .await
    }

    async fn write_telemetry(
        &self,
        lease: &LeaseRow,
        t: &JobTelemetry,
        duration_secs: u64,
        now: i64,
        replace: bool,
    ) -> Result<(), StoreError> {
        let steps =
            serde_json::to_string(&t.steps).map_err(|e| StoreError::Corrupt(e.to_string()))?;
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query(
            "INSERT INTO job_telemetry
             (lease_id, drone, class, scope, repo, run_id, job_name, conclusion, failure_class,
              duration_secs, queue_wait_secs, steps, finished_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(lease_id) DO UPDATE SET
               drone=excluded.drone, class=excluded.class, scope=excluded.scope,
               repo=excluded.repo, run_id=excluded.run_id, job_name=excluded.job_name,
               conclusion=excluded.conclusion, failure_class=excluded.failure_class,
               duration_secs=excluded.duration_secs, queue_wait_secs=excluded.queue_wait_secs,
               steps=excluded.steps, finished_at=excluded.finished_at
             WHERE ?",
        )
        .bind(&lease.id)
        .bind(&lease.drone)
        .bind(text(&lease.class)?)
        .bind(text(&lease.scope)?)
        .bind(&t.repo)
        .bind(to_i64(t.run_id))
        .bind(&t.job_name)
        .bind(text(&t.conclusion)?)
        .bind(text(&t.failure_class)?)
        .bind(to_i64(duration_secs))
        .bind(t.queue_wait_secs.map(to_i64))
        .bind(steps)
        .bind(now)
        .bind(replace)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Reports finished at or after `since`, newest first, at most `limit`.
    pub async fn telemetry_since(
        &self,
        since: i64,
        limit: i64,
    ) -> Result<Vec<TelemetryRow>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let rows = sqlx::query(
            "SELECT * FROM job_telemetry WHERE finished_at >= ? ORDER BY finished_at DESC LIMIT ?",
        )
        .bind(since)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                let steps: String = r.get("steps");
                Ok(TelemetryRow {
                    lease_id: r.get("lease_id"),
                    drone: r.get("drone"),
                    class: parse(&r.get::<String, _>("class"))?,
                    scope: parse(&r.get::<String, _>("scope"))?,
                    duration_secs: to_u64(r.get("duration_secs")),
                    finished_at: r.get("finished_at"),
                    telemetry: JobTelemetry {
                        job_name: r.get("job_name"),
                        repo: r.get("repo"),
                        run_id: to_u64(r.get("run_id")),
                        conclusion: parse(&r.get::<String, _>("conclusion"))?,
                        failure_class: parse(&r.get::<String, _>("failure_class"))?,
                        queue_wait_secs: r.get::<Option<i64>, _>("queue_wait_secs").map(to_u64),
                        steps: serde_json::from_str(&steps)
                            .map_err(|e| StoreError::Corrupt(e.to_string()))?,
                    },
                })
            })
            .collect()
    }

    /// Delete reports finished before `before`; returns how many.
    pub async fn prune_telemetry(&self, before: i64) -> Result<u64, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query(
            "DELETE FROM lease_jobs WHERE observed_at < ? OR EXISTS
            (SELECT 1 FROM job_telemetry WHERE job_telemetry.lease_id = lease_jobs.lease_id)",
        )
        .bind(before)
        .execute(&self.pool)
        .await?;
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let r = sqlx::query("DELETE FROM job_telemetry WHERE finished_at < ?")
            .bind(before)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected())
    }
}

// ---------------------------------------------------------------- report

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobTime {
    pub repo: String,
    pub job_name: String,
    pub runs: u32,
    pub total_secs: u64,
    pub avg_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepTime {
    pub job_name: String,
    pub step: String,
    pub runs: u32,
    pub total_secs: u64,
    pub avg_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueWait {
    pub class: Class,
    /// Jobs that reported a queue wait.
    pub samples: u32,
    pub avg_secs: u64,
    pub p95_secs: u64,
    pub max_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cancellation {
    pub jobs: u32,
    pub cancelled: u32,
    /// `cancelled / jobs`, 0 when there are no jobs.
    pub share: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DroneHealth {
    pub drone: String,
    pub jobs: u32,
    /// Jobs that failed or timed out (any class).
    pub failed: u32,
    /// Jobs whose failure was caused by the host.
    pub host_failures: u32,
    pub code_failures: u32,
    pub cancelled: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub now: i64,
    pub window_secs: i64,
    pub jobs: u32,
    pub by_job: Vec<JobTime>,
    pub by_step: Vec<StepTime>,
    pub queue_wait: Vec<QueueWait>,
    pub cancellation: Cancellation,
    pub drones: Vec<DroneHealth>,
}

#[derive(Default)]
struct Sum {
    runs: u32,
    total: u64,
}

impl Sum {
    fn add(&mut self, secs: u64) {
        self.runs += 1;
        self.total = self.total.saturating_add(secs);
    }

    fn avg(&self) -> u64 {
        self.total / u64::from(self.runs.max(1))
    }
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Aggregate `rows` (all finished within the last `window_secs`). The by-job
/// and by-step lists hold the `limit` entries with the most total time.
pub fn build_report(rows: &[TelemetryRow], now: i64, window_secs: i64, limit: usize) -> Report {
    let mut jobs: BTreeMap<(&str, &str), Sum> = BTreeMap::new();
    let mut steps: BTreeMap<(&str, &str), Sum> = BTreeMap::new();
    let mut waits: BTreeMap<Class, Vec<u64>> = BTreeMap::new();
    let mut drones: BTreeMap<&str, DroneHealth> = BTreeMap::new();
    let mut cancelled = 0u32;

    for r in rows {
        let t = &r.telemetry;
        jobs.entry((t.repo.as_str(), t.job_name.as_str()))
            .or_default()
            .add(r.duration_secs);
        for s in &t.steps {
            steps
                .entry((t.job_name.as_str(), s.name.as_str()))
                .or_default()
                .add(s.duration_secs);
        }
        if let Some(w) = t.queue_wait_secs {
            waits.entry(r.class).or_default().push(w);
        }
        let d = drones
            .entry(r.drone.as_str())
            .or_insert_with(|| DroneHealth {
                drone: r.drone.clone(),
                jobs: 0,
                failed: 0,
                host_failures: 0,
                code_failures: 0,
                cancelled: 0,
            });
        d.jobs += 1;
        let failed = matches!(t.conclusion, Conclusion::Failure | Conclusion::TimedOut);
        if failed {
            d.failed += 1;
        }
        if t.conclusion == Conclusion::Cancelled {
            d.cancelled += 1;
            cancelled += 1;
        }
        match t.failure_class {
            FailureClass::Host => d.host_failures += 1,
            FailureClass::Code => d.code_failures += 1,
            FailureClass::Unknown => {}
        }
    }

    let mut by_job: Vec<JobTime> = jobs
        .into_iter()
        .map(|((repo, job), s)| JobTime {
            repo: repo.to_owned(),
            job_name: job.to_owned(),
            runs: s.runs,
            total_secs: s.total,
            avg_secs: s.avg(),
        })
        .collect();
    by_job.sort_by_key(|j| std::cmp::Reverse(j.total_secs));
    by_job.truncate(limit);

    let mut by_step: Vec<StepTime> = steps
        .into_iter()
        .map(|((job, step), s)| StepTime {
            job_name: job.to_owned(),
            step: step.to_owned(),
            runs: s.runs,
            total_secs: s.total,
            avg_secs: s.avg(),
        })
        .collect();
    by_step.sort_by_key(|s| std::cmp::Reverse(s.total_secs));
    by_step.truncate(limit);

    let queue_wait = waits
        .into_iter()
        .map(|(class, mut v)| {
            v.sort_unstable();
            let n = v.len();
            // Nearest-rank p95.
            let rank = (n * 95).div_ceil(100).max(1);
            QueueWait {
                class,
                samples: count(n),
                avg_secs: v.iter().sum::<u64>() / u64::try_from(n).unwrap_or(1).max(1),
                p95_secs: v.get(rank - 1).copied().unwrap_or(0),
                max_secs: v.last().copied().unwrap_or(0),
            }
        })
        .collect();

    let total = count(rows.len());
    let share = if total == 0 {
        0.0
    } else {
        f64::from(cancelled) / f64::from(total)
    };
    let mut drones: Vec<DroneHealth> = drones.into_values().collect();
    drones.sort_by(|a, b| {
        b.host_failures
            .cmp(&a.host_failures)
            .then_with(|| a.drone.cmp(&b.drone))
    });
    Report {
        now,
        window_secs,
        jobs: total,
        by_job,
        by_step,
        queue_wait,
        cancellation: Cancellation {
            jobs: total,
            cancelled,
            share,
        },
        drones,
    }
}

fn dur(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m{:02}s", secs / 60, secs % 60),
        _ => format!("{}h{:02}m", secs / 3600, secs % 3600 / 60),
    }
}

/// Plain-text rendering for `borg report`.
pub fn render_text(r: &Report) -> String {
    use std::fmt::Write as _;
    let mut o = String::new();
    let _ = writeln!(
        o,
        "borg report: last {}, {} job(s) with telemetry\n",
        dur(u64::try_from(r.window_secs).unwrap_or(0)),
        r.jobs
    );
    let _ = writeln!(
        o,
        "Time by job\n  {:>9} {:>5} {:>8}  job",
        "total", "runs", "avg"
    );
    for j in &r.by_job {
        let _ = writeln!(
            o,
            "  {:>9} {:>5} {:>8}  {} / {}",
            dur(j.total_secs),
            j.runs,
            dur(j.avg_secs),
            j.repo,
            j.job_name
        );
    }
    let _ = writeln!(
        o,
        "\nTime by step\n  {:>9} {:>5} {:>8}  step",
        "total", "runs", "avg"
    );
    for s in &r.by_step {
        let _ = writeln!(
            o,
            "  {:>9} {:>5} {:>8}  {} / {}",
            dur(s.total_secs),
            s.runs,
            dur(s.avg_secs),
            s.job_name,
            s.step
        );
    }
    let _ = writeln!(
        o,
        "\nQueue wait by class\n  {:<8} {:>7} {:>8} {:>8} {:>8}",
        "class", "samples", "avg", "p95", "max"
    );
    for q in &r.queue_wait {
        let _ = writeln!(
            o,
            "  {:<8} {:>7} {:>8} {:>8} {:>8}",
            q.class,
            q.samples,
            dur(q.avg_secs),
            dur(q.p95_secs),
            dur(q.max_secs)
        );
    }
    let c = &r.cancellation;
    let _ = writeln!(
        o,
        "\nCancellations: {} of {} job(s) ({:.1}%)",
        c.cancelled,
        c.jobs,
        c.share * 100.0
    );
    let _ = writeln!(
        o,
        "\nDrones\n  {:<20} {:>5} {:>7} {:>5} {:>5} {:>9}",
        "drone", "jobs", "failed", "host", "code", "cancelled"
    );
    for d in &r.drones {
        let _ = writeln!(
            o,
            "  {:<20} {:>5} {:>7} {:>5} {:>5} {:>9}",
            d.drone, d.jobs, d.failed, d.host_failures, d.code_failures, d.cancelled
        );
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        drone: &str,
        class: Class,
        job: &str,
        conclusion: Conclusion,
        fc: FailureClass,
    ) -> TelemetryRow {
        TelemetryRow {
            lease_id: format!("{drone}-{job}"),
            drone: drone.to_owned(),
            class,
            scope: Scope::Pr,
            duration_secs: 100,
            finished_at: 1000,
            telemetry: JobTelemetry {
                job_name: job.to_owned(),
                repo: "o/r".to_owned(),
                run_id: 1,
                conclusion,
                failure_class: fc,
                queue_wait_secs: Some(10),
                steps: vec![
                    StepTiming {
                        name: "build".into(),
                        duration_secs: 60,
                        conclusion,
                    },
                    StepTiming {
                        name: "test".into(),
                        duration_secs: 30,
                        conclusion,
                    },
                ],
            },
        }
    }

    #[test]
    fn wire_format_reads_defaults_and_unknown_values() {
        let t: JobTelemetry = serde_json::from_str(
            r#"{"job_name":"j","repo":"o/r","conclusion":"weird","failure_class":"cosmic",
                "steps":[{"name":"s","duration_secs":3}]}"#,
        )
        .unwrap();
        assert_eq!(t.conclusion, Conclusion::Unknown);
        assert_eq!(t.failure_class, FailureClass::Unknown);
        assert_eq!(t.steps[0].conclusion, Conclusion::Unknown);
        assert_eq!((t.run_id, t.queue_wait_secs), (0, None));
        let timed: Conclusion = serde_json::from_str("\"timed_out\"").unwrap();
        assert_eq!(timed, Conclusion::TimedOut);
    }

    #[test]
    fn sanitizing_bounds_and_cleans() {
        let mut t = row(
            "d",
            Class::Light,
            "j",
            Conclusion::Success,
            FailureClass::Host,
        )
        .telemetry;
        t.job_name = format!("a\u{7}b{}", "x".repeat(500));
        t.steps = vec![
            StepTiming {
                name: "s".into(),
                duration_secs: 1,
                conclusion: Conclusion::Success
            };
            200
        ];
        let s = t.clone().sanitized().unwrap();
        assert_eq!(s.job_name.chars().count(), MAX_NAME_CHARS);
        assert!(!s.job_name.contains('\u{7}'));
        assert_eq!(s.steps.len(), MAX_STEPS);
        // A successful job has no failure class.
        assert_eq!(s.failure_class, FailureClass::Unknown);
        for bad in [("", "o/r"), ("j", "nope"), ("j", "a/b/c"), ("j", "a b/c")] {
            let mut t = t.clone();
            (t.job_name, t.repo) = (bad.0.into(), bad.1.into());
            assert!(t.sanitized().is_none(), "{bad:?}");
        }
    }

    #[test]
    fn report_aggregates_time_waits_cancellations_and_host_failures() {
        let rows = vec![
            row(
                "d1",
                Class::Light,
                "lint",
                Conclusion::Success,
                FailureClass::Unknown,
            ),
            row(
                "d1",
                Class::Light,
                "lint",
                Conclusion::Failure,
                FailureClass::Host,
            ),
            row(
                "d2",
                Class::Heavy,
                "test",
                Conclusion::Failure,
                FailureClass::Code,
            ),
            row(
                "d2",
                Class::Heavy,
                "test",
                Conclusion::Cancelled,
                FailureClass::Unknown,
            ),
        ];
        let r = build_report(&rows, 2000, 86_400, 10);
        assert_eq!(r.jobs, 4);
        assert_eq!(r.by_job.len(), 2);
        assert_eq!(
            (
                r.by_job[0].runs,
                r.by_job[0].total_secs,
                r.by_job[0].avg_secs
            ),
            (2, 200, 100)
        );
        let build = r
            .by_step
            .iter()
            .find(|s| s.job_name == "lint" && s.step == "build")
            .unwrap();
        assert_eq!((build.runs, build.total_secs, build.avg_secs), (2, 120, 60));
        assert_eq!(r.queue_wait.len(), 2);
        assert_eq!(
            (
                r.queue_wait[0].class,
                r.queue_wait[0].samples,
                r.queue_wait[0].p95_secs
            ),
            (Class::Light, 2, 10)
        );
        assert_eq!(r.cancellation.cancelled, 1);
        assert!((r.cancellation.share - 0.25).abs() < f64::EPSILON);
        // Worst host first.
        assert_eq!(r.drones[0].drone, "d1");
        assert_eq!((r.drones[0].host_failures, r.drones[0].failed), (1, 1));
        assert_eq!((r.drones[1].code_failures, r.drones[1].cancelled), (1, 1));
        let text = render_text(&r);
        assert!(
            text.contains("Cancellations: 1 of 4 job(s) (25.0%)"),
            "{text}"
        );
        assert!(text.contains("o/r / lint"), "{text}");
    }

    #[test]
    fn limit_keeps_the_biggest_and_an_empty_report_is_zeroed() {
        let mut rows = vec![row(
            "d",
            Class::Light,
            "small",
            Conclusion::Success,
            FailureClass::Unknown,
        )];
        let mut big = row(
            "d",
            Class::Light,
            "big",
            Conclusion::Success,
            FailureClass::Unknown,
        );
        big.duration_secs = 900;
        rows.push(big);
        let r = build_report(&rows, 0, 60, 1);
        assert_eq!(r.by_job.len(), 1);
        assert_eq!(r.by_job[0].job_name, "big");
        let empty = build_report(&[], 0, 60, 5);
        assert_eq!((empty.jobs, empty.cancellation.share), (0, 0.0));
        assert!(render_text(&empty).contains("0 job(s)"));
    }

    #[tokio::test]
    async fn store_round_trips_replaces_and_prunes() {
        let s = Store::open_in_memory().await.unwrap();
        s.enroll("d1", "h", 0).await.unwrap();
        let lease = LeaseRow {
            id: "l1".into(),
            drone: "d1".into(),
            class: Class::Medium,
            scope: Scope::Main,
            labels: vec![],
            runner_name: "r".into(),
            runner_id: 1,
            jit_config: None,
            state: crate::store::LeaseStatus::Done,
            created_at: 0,
            expires_at: 0,
        };
        let t = row(
            "d1",
            Class::Medium,
            "j",
            Conclusion::Failure,
            FailureClass::Host,
        )
        .telemetry;
        let mut fallback = t.clone();
        fallback.failure_class = FailureClass::Unknown;
        s.record_github_telemetry(&lease, &fallback, 20, 450)
            .await
            .unwrap();
        s.record_telemetry(&lease, &t, 77, 500).await.unwrap();
        s.record_github_telemetry(&lease, &fallback, 20, 550)
            .await
            .unwrap();
        assert_eq!(s.telemetry_since(0, 10).await.unwrap()[0].telemetry, t);
        s.record_telemetry(&lease, &t, 88, 600).await.unwrap();
        let rows = s.telemetry_since(0, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].duration_secs, rows[0].finished_at), (88, 600));
        assert_eq!(rows[0].telemetry, t);
        assert_eq!((rows[0].class, rows[0].scope), (Class::Medium, Scope::Main));
        assert!(s.telemetry_since(601, 10).await.unwrap().is_empty());
        assert_eq!(s.prune_telemetry(601).await.unwrap(), 1);
        assert!(s.telemetry_since(0, 10).await.unwrap().is_empty());
    }
    #[tokio::test]
    async fn pending_job_identity_survives_restart_and_expires_with_telemetry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queen.sqlite");
        let s = Store::open(&path).await.unwrap();
        s.enroll("d1", "h", 0).await.unwrap();
        let lease = LeaseRow {
            id: "l1".into(),
            drone: "d1".into(),
            class: Class::Light,
            scope: Scope::Pr,
            labels: vec![],
            runner_name: "borg-d1-l1".into(),
            runner_id: 123,
            jit_config: None,
            state: crate::store::LeaseStatus::Running,
            created_at: 1,
            expires_at: 500,
        };
        s.insert_lease(&lease).await.unwrap();
        s.remember_job(&lease, "example-org/app", 77, 100)
            .await
            .unwrap();
        s.remember_job(&lease, "other/repo", 88, 200).await.unwrap();
        s.pool.close().await;
        let reopened = Store::open(&path).await.unwrap();
        let pending = reopened.pending_jobs().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            (&pending[0].0.id, pending[0].1.as_str(), pending[0].2),
            (&lease.id, "example-org/app", 77)
        );
        reopened.prune_telemetry(101).await.unwrap();
        assert!(reopened.pending_jobs().await.unwrap().is_empty());
        reopened
            .remember_job(&lease, "example-org/app", 77, 200)
            .await
            .unwrap();
        assert_eq!(reopened.remove_drone("d1").await.unwrap(), Some(1));
        assert!(reopened.pending_jobs().await.unwrap().is_empty());
        assert!(reopened.lease(&lease.id).await.unwrap().is_none());
        assert!(reopened.drones().await.unwrap().is_empty());
    }
}
