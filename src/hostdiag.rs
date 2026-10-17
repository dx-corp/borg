//! Per-lease host diagnostics: how loaded the drone was when a job started
//! and when it ended.
//!
//! The drone samples its host just before it starts the runner and again when
//! the runner exits, and sends both samples with the lease result
//! (`POST /v1/leases/{id}/result`, field `host`). The queen stores one row per
//! lease, adds the GitHub run id and job id when it sees the job on a runner it
//! minted, and serves the rows on `GET /jobs/{job_id}` and `GET /runs/{run_id}`.
//!
//! A slow job on an overloaded drone looks like a code failure unless someone
//! compares hosts by hand. These rows let a triage tool show which drone ran a
//! job and how loaded that drone was.

use std::path::Path;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::classes::{Class, Scope};
use crate::probe::{self, Host};
use crate::store::{LeaseResult, LeaseRow, Store, StoreError};

/// Rows are kept this long after the lease was granted.
pub(crate) const KEEP_SECS: i64 = 7 * 86_400;
/// At most this many rows are kept; the oldest go first.
pub(crate) const MAX_ROWS: i64 = 50_000;
/// CPU pressure (`some avg10`, percent) at job start that marks a slow host
/// when the queen has no `admission` block.
pub(crate) const SLOW_CPU_PSI_PCT: f64 = 30.0;
/// Memory pressure (`some avg10`, percent) at job start that marks a slow host.
const SLOW_MEM_PSI_PCT: f64 = 10.0;
/// Load average per CPU that marks a slow host. Only used when the host has no
/// CPU pressure reading, because an LXC's load average is computed by lxcfs and
/// over-reports.
const SLOW_LOAD_PER_CPU: f64 = 1.5;
/// Share of the host's CPUs busy that marks a slow host.
const SLOW_BUSY_SHARE: f64 = 0.9;
/// Free disk on the work or cache volume that marks a slow host.
const SLOW_FREE_DISK_MIB: u64 = 10 * 1024;

const MAX_NAME_CHARS: usize = 120;

/// One Linux PSI file (`/proc/pressure/*` or a cgroup `*.pressure`), in percent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Psi {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    some_avg10: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    some_avg60: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    full_avg10: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    full_avg60: Option<f64>,
}

impl Psi {
    /// Parse `some avg10=1.00 avg60=...` / `full ...` lines. `None` when no
    /// field could be read.
    fn parse(text: &str) -> Option<Self> {
        let field = |kind: &str, key: &str| -> Option<f64> {
            text.lines()
                .find(|l| l.starts_with(kind))?
                .split_whitespace()
                .find_map(|f| f.strip_prefix(key))?
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())
                .map(|v| v.clamp(0.0, 100.0))
        };
        let psi = Self {
            some_avg10: field("some ", "avg10="),
            some_avg60: field("some ", "avg60="),
            full_avg10: field("full ", "avg10="),
            full_avg60: field("full ", "avg60="),
        };
        (psi != Self::default()).then_some(psi)
    }

    fn sanitized(self) -> Self {
        let pct = |v: Option<f64>| v.filter(|v| v.is_finite()).map(|v| v.clamp(0.0, 100.0));
        Self {
            some_avg10: pct(self.some_avg10),
            some_avg60: pct(self.some_avg60),
            full_avg10: pct(self.full_avg10),
            full_avg60: pct(self.full_avg60),
        }
    }
}

/// The drone's host at one moment.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostSample {
    /// Unix seconds.
    pub at: i64,
    /// CPUs the drone process may use.
    #[serde(default)]
    pub cpus: Option<u32>,
    /// `/proc/loadavg` 1, 5 and 15 minute averages. Inside an LXC this is
    /// lxcfs's estimate and over-reports; prefer the pressure and busy fields.
    #[serde(default)]
    load: Option<[f64; 3]>,
    /// `/proc/pressure/cpu`.
    #[serde(default)]
    cpu_psi: Option<Psi>,
    /// `/proc/pressure/memory`.
    #[serde(default)]
    memory_psi: Option<Psi>,
    /// `/proc/pressure/io`.
    #[serde(default)]
    io_psi: Option<Psi>,
    /// `/sys/fs/cgroup/cpu.pressure`: the cgroup-namespace root, which is the
    /// whole container inside an LXC.
    #[serde(default)]
    cgroup_cpu_psi: Option<Psi>,
    /// `/sys/fs/cgroup/memory.pressure`.
    #[serde(default)]
    cgroup_memory_psi: Option<Psi>,
    /// `usage_usec` of `/sys/fs/cgroup/cpu.stat` (whole host or container).
    #[serde(default)]
    host_cpu_usage_usec: Option<u64>,
    /// Cores busy in that cgroup, averaged since the drone's previous sample
    /// (at most one heartbeat ago).
    #[serde(default)]
    host_cpu_busy_cores: Option<f64>,
    /// `usage_usec` of the drone's own cgroup; jobs run inside it.
    #[serde(default)]
    drone_cpu_usage_usec: Option<u64>,
    /// `throttled_usec` of the drone's own cgroup.
    #[serde(default)]
    drone_cpu_throttled_usec: Option<u64>,
    #[serde(default)]
    mem_available_mib: Option<u64>,
    /// Free space on the filesystem holding the drone's `work_root`.
    #[serde(default)]
    work_free_mib: Option<u64>,
    /// Free space on the filesystem holding the drone's `cache_root`.
    #[serde(default)]
    cache_free_mib: Option<u64>,
    /// Leases the drone held at this moment, this one included.
    #[serde(default)]
    pub active_leases: u32,
}

impl HostSample {
    /// Clamp what a drone sent: finite non-negative numbers, no future time.
    fn sanitized(mut self, now: i64) -> Self {
        let num = |v: f64| v.is_finite().then(|| v.max(0.0));
        self.at = self.at.min(now);
        self.load = self
            .load
            .and_then(|l| Some([num(l[0])?, num(l[1])?, num(l[2])?]));
        for psi in [
            &mut self.cpu_psi,
            &mut self.memory_psi,
            &mut self.io_psi,
            &mut self.cgroup_cpu_psi,
            &mut self.cgroup_memory_psi,
        ] {
            *psi = psi.map(Psi::sanitized);
        }
        self.host_cpu_busy_cores = self.host_cpu_busy_cores.and_then(num);
        self
    }

    /// The higher CPU `some avg10` of the host file and the cgroup file.
    fn cpu_some_avg10(&self) -> Option<f64> {
        [self.cpu_psi, self.cgroup_cpu_psi]
            .into_iter()
            .flatten()
            .filter_map(|p| p.some_avg10)
            .reduce(f64::max)
    }

    fn mem_some_avg10(&self) -> Option<f64> {
        [self.memory_psi, self.cgroup_memory_psi]
            .into_iter()
            .flatten()
            .filter_map(|p| p.some_avg10)
            .reduce(f64::max)
    }
}

/// What a drone sends in its lease result.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LeaseHostReport {
    #[serde(default)]
    pub(crate) start: Option<HostSample>,
    #[serde(default)]
    pub(crate) end: Option<HostSample>,
}

/// Turns successive `cpu.stat` readings into a busy-cores rate.
#[derive(Debug, Default)]
pub struct CpuMeter(Mutex<Option<MeterState>>);

#[derive(Debug, Clone, Copy)]
struct MeterState {
    at_ms: i64,
    usage_usec: u64,
    busy_cores: Option<f64>,
}

/// Readings closer together than this keep the previous rate.
const METER_MIN_GAP_MS: i64 = 2_000;

impl CpuMeter {
    /// Record a reading and return the latest rate. A reading that arrives
    /// within two seconds of the stored one returns the stored rate, so a lease
    /// that starts right after a heartbeat still gets a measured value.
    fn observe(&self, at_ms: i64, usage_usec: u64) -> Option<f64> {
        let mut slot = self.0.lock().ok()?;
        let next = match *slot {
            Some(prev) if at_ms - prev.at_ms < METER_MIN_GAP_MS && at_ms >= prev.at_ms => {
                return prev.busy_cores;
            }
            Some(prev) if at_ms > prev.at_ms && usage_usec >= prev.usage_usec => MeterState {
                at_ms,
                usage_usec,
                busy_cores: Some(
                    (usage_usec - prev.usage_usec) as f64 / ((at_ms - prev.at_ms) as f64 * 1000.0),
                ),
            },
            _ => MeterState {
                at_ms,
                usage_usec,
                busy_cores: None,
            },
        };
        *slot = Some(next);
        next.busy_cores
    }
}

/// `usage_usec` and `throttled_usec` from a cgroup v2 `cpu.stat`.
fn cpu_stat(host: &dyn Host, dir: &Path) -> (Option<u64>, Option<u64>) {
    let Some(text) = host.read_to_string(&dir.join("cpu.stat")) else {
        return (None, None);
    };
    let field = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key)?.trim().parse::<u64>().ok())
    };
    (field("usage_usec "), field("throttled_usec "))
}

/// The drone's own cgroup directory under `/sys/fs/cgroup`, from the cgroup v2
/// line of `/proc/self/cgroup`.
fn own_cgroup(host: &dyn Host) -> Option<std::path::PathBuf> {
    let text = host.read_to_string(Path::new("/proc/self/cgroup"))?;
    let rel = text.lines().find_map(|l| l.strip_prefix("0::"))?.trim();
    let rel = rel.trim_start_matches('/');
    if rel.contains("..") {
        return None;
    }
    Some(Path::new("/sys/fs/cgroup").join(rel))
}

fn loadavg(host: &dyn Host) -> Option<[f64; 3]> {
    let text = host.read_to_string(Path::new("/proc/loadavg"))?;
    let mut it = text
        .split_whitespace()
        .map(|f| f.parse::<f64>().ok().filter(|v| v.is_finite()));
    Some([it.next()??, it.next()??, it.next()??])
}

/// Feed the meter from the root cgroup and return the busy-cores rate. Called
/// on every heartbeat so a lease start has a recent rate.
pub(crate) fn observe_cpu(host: &dyn Host, meter: &CpuMeter, at_ms: i64) -> Option<f64> {
    let (usage, _) = cpu_stat(host, Path::new("/sys/fs/cgroup"));
    meter.observe(at_ms, usage?)
}

/// Sample the host for a lease start or end. Every field degrades to `None`
/// on its own; this never fails.
pub(crate) fn sample(
    host: &dyn Host,
    work_root: &Path,
    cache_root: &Path,
    active_leases: u32,
    meter: &CpuMeter,
    at_ms: i64,
) -> HostSample {
    let psi = |path: &str| Psi::parse(&host.read_to_string(Path::new(path))?);
    let (host_usage, _) = cpu_stat(host, Path::new("/sys/fs/cgroup"));
    let (drone_usage, drone_throttled) = own_cgroup(host)
        .map(|dir| cpu_stat(host, &dir))
        .unwrap_or_default();
    HostSample {
        at: at_ms.div_euclid(1000),
        cpus: host.available_parallelism(),
        load: loadavg(host),
        cpu_psi: psi("/proc/pressure/cpu"),
        memory_psi: psi("/proc/pressure/memory"),
        io_psi: psi("/proc/pressure/io"),
        cgroup_cpu_psi: psi("/sys/fs/cgroup/cpu.pressure"),
        cgroup_memory_psi: psi("/sys/fs/cgroup/memory.pressure"),
        host_cpu_usage_usec: host_usage,
        host_cpu_busy_cores: host_usage.and_then(|u| meter.observe(at_ms, u)),
        drone_cpu_usage_usec: drone_usage,
        drone_cpu_throttled_usec: drone_throttled,
        mem_available_mib: probe::mem_available_mib(host),
        work_free_mib: probe::free_disk_mib_at(host, work_root),
        cache_free_mib: probe::free_disk_mib_at(host, cache_root),
        active_leases,
    }
}

// ---------------------------------------------------------------- queen side

/// Averages over the job, from the start and end samples.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct During {
    secs: i64,
    /// Cores busy on the whole host (or container).
    #[serde(skip_serializing_if = "Option::is_none")]
    host_cpu_busy_cores: Option<f64>,
    /// Cores used by the drone's cgroup: this job and any other lease.
    #[serde(skip_serializing_if = "Option::is_none")]
    drone_cpu_busy_cores: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    drone_cpu_throttled_secs: Option<f64>,
}

fn during(start: &HostSample, end: &HostSample) -> Option<During> {
    let secs = end.at - start.at;
    if secs <= 0 {
        return None;
    }
    let rate =
        |a: Option<u64>, b: Option<u64>| Some(b?.checked_sub(a?)? as f64 / 1e6 / secs as f64);
    Some(During {
        secs,
        host_cpu_busy_cores: rate(start.host_cpu_usage_usec, end.host_cpu_usage_usec),
        drone_cpu_busy_cores: rate(start.drone_cpu_usage_usec, end.drone_cpu_usage_usec),
        drone_cpu_throttled_secs: end
            .drone_cpu_throttled_usec
            .zip(start.drone_cpu_throttled_usec)
            .and_then(|(b, a)| b.checked_sub(a))
            .map(|d| d as f64 / 1e6),
    })
}

/// Why a job's host was overloaded, or nothing. `cpu_psi_pct` is the CPU
/// pressure threshold (the queen's admission soft threshold when set).
fn slow_host_reasons(
    start: Option<&HostSample>,
    during: Option<&During>,
    cpu_psi_pct: f64,
) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(s) = start {
        let cpus = s.cpus.filter(|&c| c > 0).map(f64::from);
        match s.cpu_some_avg10() {
            Some(p) if p >= cpu_psi_pct => {
                out.push(format!(
                    "cpu pressure {p:.1}% at start (threshold {cpu_psi_pct:.0}%)"
                ));
            }
            Some(_) => {}
            None => {
                if let (Some(l), Some(c)) = (s.load, cpus)
                    && l[0] / c >= SLOW_LOAD_PER_CPU
                {
                    out.push(format!(
                        "load {:.1} on {c:.0} cpus at start ({:.1} per cpu, threshold {SLOW_LOAD_PER_CPU})",
                        l[0],
                        l[0] / c
                    ));
                }
            }
        }
        if let Some(p) = s.mem_some_avg10()
            && p >= SLOW_MEM_PSI_PCT
        {
            out.push(format!(
                "memory pressure {p:.1}% at start (threshold {SLOW_MEM_PSI_PCT:.0}%)"
            ));
        }
        if let (Some(b), Some(c)) = (s.host_cpu_busy_cores, cpus)
            && b / c >= SLOW_BUSY_SHARE
        {
            out.push(format!("{b:.1} of {c:.0} cpus busy at start"));
        }
        for (name, free) in [("work", s.work_free_mib), ("cache", s.cache_free_mib)] {
            if let Some(f) = free
                && f < SLOW_FREE_DISK_MIB
            {
                out.push(format!("{name} volume {f} MiB free at start"));
            }
        }
        if let (Some(d), Some(c)) = (during, cpus)
            && let Some(b) = d.host_cpu_busy_cores
            && b / c >= SLOW_BUSY_SHARE
        {
            out.push(format!(
                "{b:.1} of {c:.0} cpus busy on average during the job"
            ));
        }
    }
    out
}

/// One lease as `GET /jobs/{id}` and `GET /runs/{id}` show it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobHostView {
    lease_id: String,
    pub drone: String,
    class: Class,
    scope: Scope,
    repo: Option<String>,
    run_id: Option<u64>,
    job_id: Option<u64>,
    job_name: Option<String>,
    /// When the queen granted the lease.
    leased_at: i64,
    /// When the drone reported the result.
    finished_at: Option<i64>,
    /// Runner wall time the drone reported.
    duration_secs: Option<u64>,
    pub exit_code: Option<i32>,
    pub start: Option<HostSample>,
    pub end: Option<HostSample>,
    during: Option<During>,
    /// The drone was overloaded when the job started or while it ran.
    slow_host: bool,
    slow_host_reasons: Vec<String>,
}

fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn json<T: Serialize>(v: Option<&T>) -> Result<Option<String>, StoreError> {
    v.map(|v| serde_json::to_string(v).map_err(|e| StoreError::Corrupt(e.to_string())))
        .transpose()
}

fn enum_text<T: Serialize>(v: &T) -> Result<String, StoreError> {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => Ok(s),
        _ => Err(StoreError::Corrupt("enum is not a string".to_owned())),
    }
}

fn enum_from<T: for<'de> Deserialize<'de>>(s: &str) -> Result<T, StoreError> {
    serde_json::from_value(serde_json::Value::String(s.to_owned()))
        .map_err(|e| StoreError::Corrupt(e.to_string()))
}

fn clean(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME_CHARS)
        .collect::<String>()
        .trim()
        .to_owned()
}

const SELECT: &str = "SELECT lease_id, drone, class, scope, repo, run_id, job_id, job_name, leased_at, finished_at, exit_code, duration_secs, start_sample, end_sample FROM lease_hosts";

impl Store {
    /// Record a drone's result and host samples for its lease.
    pub(crate) async fn record_lease_host(
        &self,
        lease: &LeaseRow,
        result: &LeaseResult,
        report: Option<&LeaseHostReport>,
        now: i64,
    ) -> Result<(), StoreError> {
        let start = report
            .and_then(|r| r.start.clone())
            .map(|s| s.sanitized(now));
        let end = report.and_then(|r| r.end.clone()).map(|s| s.sanitized(now));
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query(
            "INSERT INTO lease_hosts (lease_id, drone, class, scope, leased_at, finished_at, exit_code, duration_secs, start_sample, end_sample)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(lease_id) DO UPDATE SET
               finished_at = excluded.finished_at, exit_code = excluded.exit_code,
               duration_secs = excluded.duration_secs,
               start_sample = COALESCE(excluded.start_sample, start_sample),
               end_sample = COALESCE(excluded.end_sample, end_sample)",
        )
        .bind(&lease.id)
        .bind(&lease.drone)
        .bind(enum_text(&lease.class)?)
        .bind(enum_text(&lease.scope)?)
        .bind(lease.created_at)
        .bind(now)
        .bind(i64::from(result.exit_code))
        .bind(to_i64(result.duration_secs))
        .bind(json(start.as_ref())?)
        .bind(json(end.as_ref())?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Attach the GitHub job a lease's runner picked up.
    pub(crate) async fn bind_lease_host_job(
        &self,
        lease: &LeaseRow,
        repo: &str,
        run_id: u64,
        job_id: u64,
        job_name: &str,
    ) -> Result<(), StoreError> {
        let name = clean(job_name);
        let run_id = (run_id != 0).then(|| to_i64(run_id));
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query(
            "INSERT INTO lease_hosts (lease_id, drone, class, scope, leased_at, repo, run_id, job_id, job_name)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(lease_id) DO UPDATE SET
               repo = excluded.repo, run_id = COALESCE(excluded.run_id, run_id),
               job_id = excluded.job_id, job_name = COALESCE(excluded.job_name, job_name)
             WHERE job_id IS NOT excluded.job_id OR run_id IS NOT COALESCE(excluded.run_id, run_id)
               OR job_name IS NOT COALESCE(excluded.job_name, job_name)",
        )
        .bind(&lease.id)
        .bind(&lease.drone)
        .bind(enum_text(&lease.class)?)
        .bind(enum_text(&lease.scope)?)
        .bind(lease.created_at)
        .bind(repo)
        .bind(run_id)
        .bind(to_i64(job_id))
        .bind((!name.is_empty()).then_some(name))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The lease that ran GitHub job `job_id` (the newest, if several did).
    pub(crate) async fn job_host(
        &self,
        job_id: u64,
        cpu_psi_pct: f64,
    ) -> Result<Option<JobHostView>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let row = sqlx::query(&format!(
            "{SELECT} WHERE job_id = ? ORDER BY leased_at DESC LIMIT 1"
        ))
        .bind(to_i64(job_id))
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(|r| view(r, cpu_psi_pct)).transpose()
    }

    /// One lease's row, by lease id.
    pub async fn lease_host(
        &self,
        lease_id: &str,
        cpu_psi_pct: f64,
    ) -> Result<Option<JobHostView>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let row = sqlx::query(&format!("{SELECT} WHERE lease_id = ?"))
            .bind(lease_id)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(|r| view(r, cpu_psi_pct)).transpose()
    }

    /// Every lease that ran a job of workflow run `run_id`, oldest first.
    pub(crate) async fn run_hosts(
        &self,
        run_id: u64,
        cpu_psi_pct: f64,
    ) -> Result<Vec<JobHostView>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let rows = sqlx::query(&format!(
            "{SELECT} WHERE run_id = ? ORDER BY leased_at, lease_id LIMIT 1000"
        ))
        .bind(to_i64(run_id))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(|r| view(r, cpu_psi_pct)).collect()
    }

    /// Drop rows granted before `before`, then all but the newest `max_rows`.
    pub(crate) async fn prune_lease_hosts(
        &self,
        before: i64,
        max_rows: i64,
    ) -> Result<u64, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let old = sqlx::query("DELETE FROM lease_hosts WHERE leased_at < ?")
            .bind(before)
            .execute(&self.pool)
            .await?
            .rows_affected();
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let over = sqlx::query(
            "DELETE FROM lease_hosts WHERE lease_id IN
             (SELECT lease_id FROM lease_hosts ORDER BY leased_at DESC, lease_id DESC LIMIT -1 OFFSET ?)",
        )
        .bind(max_rows)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(old + over)
    }
}

fn view(r: &sqlx::sqlite::SqliteRow, cpu_psi_pct: f64) -> Result<JobHostView, StoreError> {
    let sample = |col: &str| -> Result<Option<HostSample>, StoreError> {
        r.get::<Option<String>, _>(col)
            .map(|s| serde_json::from_str(&s).map_err(|e| StoreError::Corrupt(e.to_string())))
            .transpose()
    };
    let start = sample("start_sample")?;
    let end = sample("end_sample")?;
    let during = start
        .as_ref()
        .zip(end.as_ref())
        .and_then(|(s, e)| during(s, e));
    let reasons = slow_host_reasons(start.as_ref(), during.as_ref(), cpu_psi_pct);
    let u = |v: Option<i64>| v.and_then(|v| u64::try_from(v).ok());
    Ok(JobHostView {
        lease_id: r.get("lease_id"),
        drone: r.get("drone"),
        class: enum_from(&r.get::<String, _>("class"))?,
        scope: enum_from(&r.get::<String, _>("scope"))?,
        repo: r.get("repo"),
        run_id: u(r.get("run_id")),
        job_id: u(r.get("job_id")),
        job_name: r.get("job_name"),
        leased_at: r.get("leased_at"),
        finished_at: r.get("finished_at"),
        duration_secs: u(r.get("duration_secs")),
        exit_code: r
            .get::<Option<i64>, _>("exit_code")
            .and_then(|v| i32::try_from(v).ok()),
        start,
        end,
        during,
        slow_host: !reasons.is_empty(),
        slow_host_reasons: reasons,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::store::LeaseStatus;

    #[derive(Default)]
    struct FakeHost {
        files: HashMap<String, String>,
        df: HashMap<String, String>,
    }

    impl Host for FakeHost {
        fn read_to_string(&self, path: &Path) -> Option<String> {
            self.files.get(path.to_str()?).cloned()
        }
        fn exists(&self, path: &Path) -> bool {
            path.to_str().is_some_and(|p| self.files.contains_key(p))
        }
        fn run(&self, program: &str, args: &[&str]) -> Option<String> {
            (program == "df")
                .then(|| self.df.get(*args.last()?).cloned())
                .flatten()
        }
        fn os(&self) -> &'static str {
            "linux"
        }
        fn arch(&self) -> &'static str {
            "x86_64"
        }
        fn available_parallelism(&self) -> Option<u32> {
            Some(16)
        }
    }

    fn df(avail_kib: u64) -> String {
        format!("fs blocks used avail cap mount\nfs 999999999 0 {avail_kib} 0% /x")
    }

    /// An LXC build box shared with interactive builds, overloaded.
    fn lxc_host(root_usage: u64) -> FakeHost {
        let files = [
            ("/proc/loadavg", "93.40 78.56 69.31 90/3277 3570264\n".to_owned()),
            (
                "/proc/pressure/cpu",
                "some avg10=53.99 avg60=52.14 avg300=47.57 total=1\nfull avg10=23.75 avg60=25.01 avg300=21.45 total=1\n".to_owned(),
            ),
            (
                "/proc/pressure/memory",
                "some avg10=1.84 avg60=0.55 avg300=0.77 total=1\nfull avg10=0.00 avg60=0.00 avg300=0.21 total=1\n".to_owned(),
            ),
            (
                "/sys/fs/cgroup/cpu.pressure",
                "some avg10=54.89 avg60=52.37 avg300=47.65 total=1\nfull avg10=23.61 avg60=24.95 avg300=21.46 total=1\n".to_owned(),
            ),
            (
                "/sys/fs/cgroup/cpu.stat",
                format!("usage_usec {root_usage}\nuser_usec 1\nnr_throttled 0\nthrottled_usec 0\n"),
            ),
            ("/proc/self/cgroup", "0::/system.slice/borg-drone.service\n".to_owned()),
            (
                "/sys/fs/cgroup/system.slice/borg-drone.service/cpu.stat",
                "usage_usec 27600656603\nthrottled_usec 5\n".to_owned(),
            ),
            ("/proc/meminfo", "MemTotal: 1 kB\nMemAvailable: 4194304 kB\n".to_owned()),
        ];
        FakeHost {
            files: files.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
            df: HashMap::from([
                ("/work".to_owned(), df(30 * 1024 * 1024)),
                ("/cache".to_owned(), df(5 * 1024 * 1024)),
            ]),
        }
    }

    #[test]
    fn psi_parses_some_and_full_and_rejects_empty() {
        let p = Psi::parse("some avg10=53.99 avg60=52.14 avg300=1 total=2\nfull avg10=23.75 avg60=25.01 avg300=21.45 total").unwrap();
        assert_eq!(p.some_avg10, Some(53.99));
        assert_eq!(p.some_avg60, Some(52.14));
        assert_eq!(p.full_avg10, Some(23.75));
        assert_eq!(p.full_avg60, Some(25.01));
        assert_eq!(Psi::parse("nothing here"), None);
        assert_eq!(
            Psi::parse("some avg10=500 avg60=nan").unwrap().some_avg10,
            Some(100.0)
        );
    }

    #[test]
    fn sample_reads_load_pressure_cgroups_disk_and_meter() {
        let meter = CpuMeter::default();
        // A heartbeat ten seconds earlier primes the meter.
        assert_eq!(observe_cpu(&lxc_host(1_000_000_000), &meter, 0), None);
        // 120 core-seconds in 10 s: 12 cores busy.
        let s = sample(
            &lxc_host(1_120_000_000),
            Path::new("/work"),
            Path::new("/cache"),
            2,
            &meter,
            10_000,
        );
        assert_eq!(s.at, 10);
        assert_eq!(s.cpus, Some(16));
        assert_eq!(s.load, Some([93.40, 78.56, 69.31]));
        assert_eq!(s.cpu_psi.unwrap().some_avg10, Some(53.99));
        assert_eq!(s.cgroup_cpu_psi.unwrap().some_avg10, Some(54.89));
        assert_eq!(s.cpu_some_avg10(), Some(54.89));
        assert_eq!(s.memory_psi.unwrap().some_avg10, Some(1.84));
        assert_eq!(s.io_psi, None);
        assert_eq!(s.host_cpu_usage_usec, Some(1_120_000_000));
        assert_eq!(s.host_cpu_busy_cores, Some(12.0));
        assert_eq!(s.drone_cpu_usage_usec, Some(27_600_656_603));
        assert_eq!(s.drone_cpu_throttled_usec, Some(5));
        assert_eq!(s.mem_available_mib, Some(4096));
        assert_eq!(s.work_free_mib, Some(30 * 1024));
        assert_eq!(s.cache_free_mib, Some(5 * 1024));
        assert_eq!(s.active_leases, 2);
        // An empty host degrades every field to None.
        let bare = sample(
            &FakeHost::default(),
            Path::new("/work"),
            Path::new("/cache"),
            1,
            &CpuMeter::default(),
            5_000,
        );
        assert_eq!(bare.load, None);
        assert_eq!(bare.cpu_psi, None);
        assert_eq!(bare.host_cpu_busy_cores, None);
        assert_eq!(bare.drone_cpu_usage_usec, None);
    }

    #[test]
    fn meter_keeps_the_last_rate_for_close_readings_and_resets_on_counter_reset() {
        let m = CpuMeter::default();
        assert_eq!(m.observe(0, 0), None);
        assert_eq!(m.observe(10_000, 40_000_000), Some(4.0));
        // A lease starting 500 ms after the heartbeat reuses its rate.
        assert_eq!(m.observe(10_500, 40_100_000), Some(4.0));
        assert_eq!(m.observe(20_000, 120_000_000), Some(8.0));
        // Counter went backwards (cgroup recreated): no rate until next reading.
        assert_eq!(m.observe(30_000, 5), None);
        assert_eq!(m.observe(40_000, 10_000_005), Some(1.0));
    }

    #[test]
    fn own_cgroup_rejects_escapes() {
        let mut h = FakeHost::default();
        h.files
            .insert("/proc/self/cgroup".into(), "0::/../etc\n".into());
        assert_eq!(own_cgroup(&h), None);
        h.files.insert("/proc/self/cgroup".into(), "0::/\n".into());
        assert_eq!(own_cgroup(&h), Some(Path::new("/sys/fs/cgroup").into()));
    }

    #[test]
    fn slow_host_uses_pressure_and_ignores_lxc_load_when_pressure_exists() {
        let start = HostSample {
            at: 0,
            cpus: Some(16),
            load: Some([93.4, 78.0, 69.0]),
            cgroup_cpu_psi: Some(Psi {
                some_avg10: Some(10.0),
                ..Psi::default()
            }),
            ..HostSample::default()
        };
        // Pressure below threshold: lxcfs load average alone is not evidence.
        assert!(slow_host_reasons(Some(&start), None, 30.0).is_empty());
        let hot = HostSample {
            cgroup_cpu_psi: Some(Psi {
                some_avg10: Some(54.9),
                ..Psi::default()
            }),
            ..start.clone()
        };
        let r = slow_host_reasons(Some(&hot), None, 30.0);
        assert_eq!(r, vec!["cpu pressure 54.9% at start (threshold 30%)"]);
        // No pressure at all: fall back to load per cpu.
        let no_psi = HostSample {
            cgroup_cpu_psi: None,
            ..start.clone()
        };
        let r = slow_host_reasons(Some(&no_psi), None, 30.0);
        assert_eq!(r.len(), 1);
        assert!(r[0].starts_with("load 93.4 on 16 cpus"), "{r:?}");
        // A quiet burst drone is not slow.
        let quiet = HostSample {
            at: 0,
            cpus: Some(56),
            load: Some([20.0, 20.0, 20.0]),
            cpu_psi: Some(Psi {
                some_avg10: Some(0.5),
                ..Psi::default()
            }),
            host_cpu_busy_cores: Some(20.0),
            work_free_mib: Some(500_000),
            cache_free_mib: Some(500_000),
            ..HostSample::default()
        };
        assert!(slow_host_reasons(Some(&quiet), None, 30.0).is_empty());
        let busy = During {
            secs: 100,
            host_cpu_busy_cores: Some(55.0),
            drone_cpu_busy_cores: None,
            drone_cpu_throttled_secs: None,
        };
        assert_eq!(
            slow_host_reasons(Some(&quiet), Some(&busy), 30.0),
            vec!["55.0 of 56 cpus busy on average during the job"]
        );
        assert!(slow_host_reasons(None, None, 30.0).is_empty());
    }

    #[test]
    fn sanitize_drops_non_finite_and_future_times() {
        let s = HostSample {
            at: 999,
            load: Some([f64::NAN, 1.0, 1.0]),
            host_cpu_busy_cores: Some(-3.0),
            cpu_psi: Some(Psi {
                some_avg10: Some(f64::INFINITY),
                some_avg60: Some(250.0),
                ..Psi::default()
            }),
            ..HostSample::default()
        }
        .sanitized(100);
        assert_eq!(s.at, 100);
        assert_eq!(s.load, None);
        assert_eq!(s.host_cpu_busy_cores, Some(0.0));
        assert_eq!(s.cpu_psi.unwrap().some_avg10, None);
        assert_eq!(s.cpu_psi.unwrap().some_avg60, Some(100.0));
    }

    fn lease(id: &str, created_at: i64) -> LeaseRow {
        LeaseRow {
            id: id.into(),
            drone: "shared-box".into(),
            class: Class::Heavy,
            scope: Scope::Main,
            labels: vec![],
            runner_name: format!("borg-{id}"),
            runner_id: 1,
            jit_config: None,
            state: LeaseStatus::Running,
            created_at,
            expires_at: created_at + 600,
        }
    }

    #[tokio::test]
    async fn rows_bind_jobs_and_results_in_either_order_and_prune() {
        let s = Store::open_in_memory().await.unwrap();
        let (a, b, c) = (lease("a", 100), lease("b", 200), lease("c", 300));
        let result = LeaseResult {
            exit_code: 1,
            duration_secs: 90,
            peak_rss_mib: None,
        };
        let start = HostSample {
            at: 110,
            cpus: Some(16),
            cpu_psi: Some(Psi {
                some_avg10: Some(54.0),
                ..Psi::default()
            }),
            host_cpu_usage_usec: Some(0),
            drone_cpu_usage_usec: Some(0),
            ..HostSample::default()
        };
        let end = HostSample {
            at: 210,
            host_cpu_usage_usec: Some(1_500_000_000),
            drone_cpu_usage_usec: Some(400_000_000),
            ..start.clone()
        };
        let report = LeaseHostReport {
            start: Some(start),
            end: Some(end),
        };
        // a: job seen first, result later.
        s.bind_lease_host_job(&a, "o/r", 7, 70, "e2e (shard 3)")
            .await
            .unwrap();
        s.record_lease_host(&a, &result, Some(&report), 220)
            .await
            .unwrap();
        // b: result first (old drone, no samples), job later.
        s.record_lease_host(&b, &result, None, 230).await.unwrap();
        s.bind_lease_host_job(&b, "o/r", 7, 71, "lint")
            .await
            .unwrap();
        // c: another run.
        s.bind_lease_host_job(&c, "o/r", 8, 80, "x").await.unwrap();

        let v = s.job_host(70, 30.0).await.unwrap().unwrap();
        assert_eq!(v.drone, "shared-box");
        assert_eq!((v.run_id, v.job_id), (Some(7), Some(70)));
        assert_eq!(v.job_name.as_deref(), Some("e2e (shard 3)"));
        assert_eq!(
            (v.exit_code, v.duration_secs, v.finished_at),
            (Some(1), Some(90), Some(220))
        );
        let d = v.during.unwrap();
        assert_eq!(d.secs, 100);
        assert_eq!(d.host_cpu_busy_cores, Some(15.0));
        assert_eq!(d.drone_cpu_busy_cores, Some(4.0));
        assert!(v.slow_host);
        assert_eq!(v.slow_host_reasons.len(), 2, "{:?}", v.slow_host_reasons);

        let old = s.job_host(71, 30.0).await.unwrap().unwrap();
        assert_eq!(old.start, None);
        assert!(!old.slow_host);
        assert_eq!(old.exit_code, Some(1));

        let run = s.run_hosts(7, 30.0).await.unwrap();
        assert_eq!(
            run.iter().map(|v| v.lease_id.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert!(s.job_host(999, 30.0).await.unwrap().is_none());

        // Re-binding the same job is a no-op; a retried result keeps samples.
        s.bind_lease_host_job(&a, "o/r", 7, 70, "e2e (shard 3)")
            .await
            .unwrap();
        s.record_lease_host(&a, &result, None, 240).await.unwrap();
        assert!(s.job_host(70, 30.0).await.unwrap().unwrap().start.is_some());

        // Age prunes a; the row cap then keeps only the newest one (c).
        assert_eq!(s.prune_lease_hosts(150, 1).await.unwrap(), 2);
        assert!(s.job_host(70, 30.0).await.unwrap().is_none());
        assert!(s.job_host(71, 30.0).await.unwrap().is_none());
        assert!(s.job_host(80, 30.0).await.unwrap().is_some());
    }
}
