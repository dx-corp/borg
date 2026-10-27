//! SQLite bookkeeping: enrolled drones and lease history.
//!
//! Back up enrollment hashes and pending job bindings. The queen re-derives
//! active capacity from GitHub and drone heartbeats, but losing a binding can
//! lose the completion sample once its workflow leaves the active queue.

use std::path::Path;
use std::str::FromStr;

use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{ConnectOptions, Connection, Row, SqlitePool};

use crate::allocator::{DroneState, HostCaps};
use crate::api::CacheStat;
use crate::bid::DroneBid;
use crate::classes::{Class, Scope, Trust};
use crate::envcontract::EnvFacts;
use crate::health::{Action, Hold};
use crate::probe::Capacity;
use crate::reclaim::{DeletionRecord, ReclaimStats};
use crate::rollout::RolloutState;
use crate::secret::Secret;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS drones (
    name           TEXT PRIMARY KEY,
    token_hash     TEXT NOT NULL UNIQUE,
    enrolled_at    INTEGER NOT NULL,
    last_heartbeat INTEGER,
    capacity       TEXT,
    caps           TEXT,
    accepting      INTEGER NOT NULL DEFAULT 1,
    cache_stats    TEXT,
    quarantined    INTEGER NOT NULL DEFAULT 0,
    quarantine_reason TEXT,
    version        TEXT,
    extra_labels   TEXT,
    bids           TEXT,
    quarantine_source TEXT,
    quarantined_at INTEGER,
    probation_since INTEGER,
    quarantine_strikes INTEGER NOT NULL DEFAULT 0,
    health_cleared_at INTEGER,
    version_since INTEGER,
    reclaim TEXT,
    env_facts TEXT,
    trust TEXT NOT NULL DEFAULT 'shared'
);
CREATE TABLE IF NOT EXISTS reclaim_log (
    drone      TEXT NOT NULL,
    seq        INTEGER NOT NULL,
    at         INTEGER NOT NULL,
    path       TEXT NOT NULL,
    kind       TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    reason     TEXT NOT NULL,
    outcome    TEXT NOT NULL,
    error      TEXT,
    PRIMARY KEY (drone, seq)
);
CREATE INDEX IF NOT EXISTS reclaim_log_at ON reclaim_log(at);
-- Diagnostic metadata only, owned and pruned with the queen's existing store.
CREATE TABLE IF NOT EXISTS placement_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at INTEGER NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('runner', 'unit')),
    key TEXT NOT NULL,
    client TEXT,
    outcome TEXT NOT NULL,
    decision TEXT NOT NULL,
    CHECK ((kind = 'runner' AND client IS NULL) OR (kind = 'unit' AND client IS NOT NULL))
);
CREATE INDEX IF NOT EXISTS placement_history_key ON placement_history(kind, key, id);
CREATE TABLE IF NOT EXISTS clients (
    name       TEXT PRIMARY KEY,
    token_hash TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS units (
    id               TEXT PRIMARY KEY,
    key              TEXT NOT NULL,
    repo             TEXT NOT NULL,
    tree_sha         TEXT NOT NULL,
    argv             TEXT NOT NULL,
    platform         TEXT NOT NULL,
    toolchain        TEXT NOT NULL,
    rust_toolchain   TEXT,
    class            TEXT NOT NULL,
    timeout_secs     INTEGER NOT NULL,
    client           TEXT NOT NULL,
    state            TEXT NOT NULL,
    drone            TEXT,
    attempt          INTEGER NOT NULL DEFAULT 0,
    created_at       INTEGER NOT NULL,
    leased_at        INTEGER,
    started_at       INTEGER,
    finished_at      INTEGER,
    exit_code        INTEGER,
    duration_secs    INTEGER,
    cancel_requested INTEGER NOT NULL DEFAULT 0,
    failure          TEXT,
    log_bytes        INTEGER NOT NULL DEFAULT 0,
    awaiting_bundle  INTEGER NOT NULL DEFAULT 0,
    bundle_bytes     INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS unit_bundles (
    unit_id    TEXT PRIMARY KEY,
    data       BLOB NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS units_active_key ON units(key) WHERE state IN ('queued', 'leased', 'running');
CREATE INDEX IF NOT EXISTS units_state ON units(state, created_at);
CREATE TABLE IF NOT EXISTS unit_logs (
    unit_id TEXT NOT NULL,
    offset  INTEGER NOT NULL,
    data    BLOB NOT NULL,
    PRIMARY KEY (unit_id, offset)
);
CREATE TABLE IF NOT EXISTS rollout_state (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    state      TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS rollout_log (
    id    INTEGER PRIMARY KEY AUTOINCREMENT,
    at    INTEGER NOT NULL,
    event TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS health_log (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    at       INTEGER NOT NULL,
    drone    TEXT NOT NULL,
    event    TEXT NOT NULL,
    reason   TEXT,
    evidence TEXT
);
CREATE TABLE IF NOT EXISTS leases (
    id           TEXT PRIMARY KEY,
    drone        TEXT NOT NULL REFERENCES drones(name),
    class        TEXT NOT NULL,
    scope        TEXT NOT NULL,
    labels       TEXT NOT NULL,
    runner_name  TEXT NOT NULL,
    runner_id    INTEGER NOT NULL,
    jit_config   TEXT,
    state        TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    expires_at   INTEGER NOT NULL,
    finished_at  INTEGER,
    exit_code    INTEGER,
    duration_secs INTEGER,
    peak_rss_mib INTEGER
);
CREATE INDEX IF NOT EXISTS leases_state ON leases(state);
CREATE INDEX IF NOT EXISTS leases_runner ON leases(runner_id, runner_name);
-- Recover observed job identity after a queen restart, until GitHub reports completion.
CREATE TABLE IF NOT EXISTS lease_jobs (
    lease_id TEXT PRIMARY KEY REFERENCES leases(id) ON DELETE CASCADE,
    repo TEXT NOT NULL,
    job_id INTEGER NOT NULL,
    observed_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS job_telemetry (
    lease_id        TEXT PRIMARY KEY,
    drone           TEXT NOT NULL,
    class           TEXT NOT NULL,
    scope           TEXT NOT NULL,
    repo            TEXT NOT NULL,
    run_id          INTEGER NOT NULL,
    job_name        TEXT NOT NULL,
    conclusion      TEXT NOT NULL,
    failure_class   TEXT NOT NULL,
    duration_secs   INTEGER NOT NULL,
    queue_wait_secs INTEGER,
    steps           TEXT NOT NULL,
    finished_at     INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS job_telemetry_finished ON job_telemetry(finished_at);
-- Host load at lease start and end, with the GitHub job the lease ran; see
-- hostdiag.rs. Pruned after seven days and to a row cap.
CREATE TABLE IF NOT EXISTS lease_hosts (
    lease_id      TEXT PRIMARY KEY,
    drone         TEXT NOT NULL,
    class         TEXT NOT NULL,
    scope         TEXT NOT NULL,
    leased_at     INTEGER NOT NULL,
    repo          TEXT,
    run_id        INTEGER,
    job_id        INTEGER,
    job_name      TEXT,
    finished_at   INTEGER,
    exit_code     INTEGER,
    duration_secs INTEGER,
    start_sample  TEXT,
    end_sample    TEXT
);
CREATE INDEX IF NOT EXISTS lease_hosts_job ON lease_hosts(job_id);
CREATE INDEX IF NOT EXISTS lease_hosts_run ON lease_hosts(run_id);
CREATE INDEX IF NOT EXISTS lease_hosts_leased ON lease_hosts(leased_at);
CREATE TABLE IF NOT EXISTS preemptions (
    lease_id     TEXT PRIMARY KEY,
    drone        TEXT NOT NULL,
    class        TEXT NOT NULL,
    scope        TEXT NOT NULL,
    for_scope    TEXT NOT NULL,
    repo         TEXT NOT NULL,
    run_id       INTEGER NOT NULL,
    job_id       INTEGER NOT NULL,
    at           INTEGER NOT NULL,
    requeued_at  INTEGER
);
CREATE TABLE IF NOT EXISTS burst_vms (
    drone        TEXT PRIMARY KEY,
    vm           TEXT NOT NULL,
    zone         TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    draining_at  INTEGER,
    ended_at     INTEGER,
    scrubbed_at  INTEGER
);
";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
    #[error("stored value is corrupt: {0}")]
    Corrupt(String),
    #[error("drone {0} is already enrolled")]
    AlreadyEnrolled(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LeaseStatus {
    /// A deploy runner the drone holds unstarted: started when deploy demand
    /// is queued or the queen is unreachable. Never expires by lease TTL.
    Standby,
    /// Runner minted, no job yet.
    Pending,
    /// Runner is executing a job.
    Running,
    Done,
    Expired,
}

impl LeaseStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standby => "standby",
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done => "done",
            Self::Expired => "expired",
        }
    }

    fn parse(s: &str) -> Result<Self, StoreError> {
        Ok(match s {
            "standby" => Self::Standby,
            "pending" => Self::Pending,
            "running" => Self::Running,
            "done" => Self::Done,
            "expired" => Self::Expired,
            other => return Err(StoreError::Corrupt(format!("lease state {other}"))),
        })
    }

    pub fn is_active(self) -> bool {
        matches!(self, Self::Standby | Self::Pending | Self::Running)
    }
}

#[derive(Debug, Clone)]
pub struct DroneRow {
    pub name: String,
    pub last_heartbeat: Option<i64>,
    pub capacity: Option<Capacity>,
    pub caps: HostCaps,
    pub accepting: bool,
    pub cache_stats: Vec<CacheStat>,
    /// Set by an operator; a quarantined drone gets no new leases.
    pub quarantined: bool,
    pub quarantine_reason: Option<String>,
    /// The borg version the drone last reported, if it reports one.
    pub version: Option<String>,
    pub extra_labels: Vec<String>,
    /// The trust the drone last reported.
    pub trust: Trust,
    /// The drone's last reported bids (may have expired; see `expires_at`).
    pub bids: Vec<DroneBid>,
    /// `operator` or `auto` while quarantined.
    pub quarantine_source: Option<String>,
    pub quarantined_at: Option<i64>,
    /// Set while an auto-quarantined drone is on probation.
    pub probation_since: Option<i64>,
    pub quarantine_strikes: u32,
    /// Evidence at or before this time was cleared by a return to service.
    pub health_cleared_at: Option<i64>,
    /// When the queen first saw the drone report its current `version`.
    pub version_since: Option<i64>,
    /// The drone's last reported build-box reclaim stats (audit entries are
    /// in `reclaim_log`, not here).
    pub reclaim: Option<ReclaimStats>,
    /// The build environment the drone last reported.
    pub env_facts: Option<EnvFacts>,
}

impl DroneRow {
    /// Quarantined and not on probation: no new leases at all.
    pub fn held(&self) -> bool {
        self.quarantined && self.probation_since.is_none()
    }

    /// On probation after an automatic quarantine: one lease at a time.
    pub fn on_probation(&self) -> bool {
        self.quarantined && self.probation_since.is_some()
    }

    /// Whether the allocator may hand this drone new leases.
    pub fn schedulable(&self) -> bool {
        self.accepting && !self.held()
    }

    /// Why the drone is held, for the health evaluation.
    pub fn hold(&self) -> Hold {
        if !self.quarantined {
            return Hold::None;
        }
        if self.quarantine_source.as_deref() != Some("auto") {
            return Hold::Operator;
        }
        let strikes = self.quarantine_strikes;
        match (self.probation_since, self.quarantined_at) {
            (Some(since), _) => Hold::Probation { since, strikes },
            (None, since) => Hold::Auto {
                since: since.unwrap_or(0),
                strikes,
            },
        }
    }

    /// The allocator's view of a fresh drone with a known capacity.
    pub fn allocator_state(&self, cap: &Capacity, now: i64) -> DroneState {
        let mut s = DroneState::from_capacity(self.name.clone(), cap, &self.caps);
        s.accepting = self.schedulable();
        s.trust = self.trust;
        if self.on_probation() {
            s.max_leases = Some(s.max_leases.map_or(1, |m| m.min(1)));
        }
        s.bids = self
            .bids
            .iter()
            .filter(|b| b.expires_at > now)
            .copied()
            .collect();
        s
    }
}

/// One stored build-box reclaim audit entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReclaimLogRow {
    pub drone: String,
    #[serde(flatten)]
    pub record: DeletionRecord,
}

/// One entry of the quarantine and health audit trail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HealthLogRow {
    pub at: i64,
    pub drone: String,
    pub event: String,
    pub reason: Option<String>,
    /// The failures that justified an automatic action (JSON).
    pub evidence: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct LeaseRow {
    pub id: String,
    pub drone: String,
    pub class: Class,
    pub scope: Scope,
    pub labels: Vec<String>,
    pub runner_name: String,
    pub runner_id: u64,
    /// Present until the drone acknowledges the lease.
    pub jit_config: Option<Secret>,
    pub state: LeaseStatus,
    pub created_at: i64,
    pub expires_at: i64,
}

/// A lease cancelled for higher-priority demand, and the job it was running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreemptionRow {
    pub lease_id: String,
    pub drone: String,
    pub class: Class,
    /// The cancelled lease's scope.
    pub scope: Scope,
    /// The starving demand's scope.
    pub for_scope: Scope,
    pub repo: String,
    pub run_id: u64,
    pub job_id: u64,
    pub at: i64,
    /// When the job was re-run; unset until then.
    pub requeued_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct LeaseResult {
    pub exit_code: i32,
    pub duration_secs: u64,
    pub peak_rss_mib: Option<u64>,
}

#[derive(Clone)]
pub struct Store {
    pub(crate) pool: SqlitePool,
}

fn to_text<T: Serialize>(v: &T) -> Result<String, StoreError> {
    serde_json::to_string(v).map_err(|e| StoreError::Corrupt(e.to_string()))
}

fn from_text<T: DeserializeOwned>(s: &str) -> Result<T, StoreError> {
    serde_json::from_str(s).map_err(|e| StoreError::Corrupt(e.to_string()))
}

fn enum_text<T: Serialize>(v: &T) -> Result<String, StoreError> {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => Ok(s),
        _ => Err(StoreError::Corrupt("enum is not a string".to_owned())),
    }
}

fn enum_from<T: DeserializeOwned>(s: &str) -> Result<T, StoreError> {
    from_text(&format!("\"{s}\""))
}

impl Store {
    /// Historical placement cannot be derived from current heartbeats. Keep it
    /// in this existing store, bounded by rows and serialized bytes, pruning in
    /// the same transaction as insertion. Unit ownership is checked against its
    /// authoritative row before persisting the diagnostic owner.
    pub async fn record_placement(
        &self,
        record: crate::explain::PlacementRecord<'_>,
    ) -> Result<(), StoreError> {
        self.record_placements(&[record]).await
    }

    /// Persist one bounded placement pass atomically, pruning once after its
    /// inserts. Invalid bounds or a mismatched unit owner reject the whole batch.
    pub async fn record_placements(
        &self,
        records: &[crate::explain::PlacementRecord<'_>],
    ) -> Result<(), StoreError> {
        use crate::explain::{MAX_DECISION_BYTES, MAX_HISTORY, PlacementKind};
        if records.len() > MAX_HISTORY as usize {
            return Err(StoreError::Corrupt(
                "placement batch exceeds diagnostic bounds".into(),
            ));
        }
        if records.is_empty() {
            return Ok(());
        }
        let mut encoded = Vec::with_capacity(records.len());
        for record in records {
            if record.key.is_empty() || record.key.len() > 256 {
                return Err(StoreError::Corrupt(
                    "placement key exceeds diagnostic bounds".into(),
                ));
            }
            let decision = to_text(record.decision)?;
            if decision.len() > MAX_DECISION_BYTES
                || record.decision.candidates.len() > crate::allocator::MAX_CANDIDATES
            {
                return Err(StoreError::Corrupt(
                    "placement exceeds diagnostic bounds".into(),
                ));
            }
            encoded.push(decision);
        }
        let mut tx = self.pool.begin().await?;
        for (record, decision) in records.iter().zip(encoded) {
            match record.kind {
                PlacementKind::Runner if record.client.is_some() => {
                    return Err(StoreError::Corrupt("runner placement has a client".into()));
                }
                PlacementKind::Unit => {
                    let owner: Option<String> =
                        // sqlx-runtime-query: standalone SQLite diagnostic owner check
                        sqlx::query_scalar("SELECT client FROM units WHERE id = ?")
                            .bind(record.key)
                            .fetch_optional(&mut *tx)
                            .await?;
                    if record.client.is_none() || owner.as_deref() != record.client {
                        return Err(StoreError::Corrupt("unit placement owner mismatch".into()));
                    }
                }
                PlacementKind::Runner => {}
            }
            // sqlx-runtime-query: standalone-buildable bounded SQLite diagnostics
            sqlx::query("INSERT INTO placement_history (at, kind, key, client, outcome, decision) VALUES (?, ?, ?, ?, ?, ?)")
                .bind(record.at).bind(record.kind.as_str()).bind(record.key).bind(record.client)
                .bind(enum_text(&record.outcome)?).bind(decision).execute(&mut *tx).await?;
        }
        // sqlx-runtime-query: all batch inserts and retention form one transaction
        sqlx::query("DELETE FROM placement_history WHERE id NOT IN (SELECT id FROM placement_history ORDER BY id DESC LIMIT ?)")
            .bind(MAX_HISTORY).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// `client` restricts unit history to its persisted owner. `None` for a unit
    /// is deliberately queen-local administration; HTTP callers must supply
    /// their authenticated client. Runner reads always exclude unit records.
    pub async fn placement_history(
        &self,
        kind: crate::explain::PlacementKind,
        key: &str,
        client: Option<&str>,
    ) -> Result<Vec<crate::explain::HistoryEntry>, StoreError> {
        // sqlx-runtime-query: typed bounded query; no command payloads selected
        let rows = sqlx::query("SELECT id, at, kind, key, outcome, decision FROM placement_history WHERE kind = ? AND key = ? AND (? IS NULL OR client = ?) ORDER BY id DESC LIMIT ?")
            .bind(kind.as_str()).bind(key).bind(client).bind(client)
            .bind(crate::explain::MAX_QUERY_ROWS).fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|row| {
                Ok(crate::explain::HistoryEntry {
                    id: row.try_get("id")?,
                    at: row.try_get("at")?,
                    kind: enum_from(&row.try_get::<String, _>("kind")?)?,
                    key: row.try_get("key")?,
                    outcome: enum_from(&row.try_get::<String, _>("outcome")?)?,
                    decision: from_text(&row.try_get::<String, _>("decision")?)?,
                })
            })
            .collect()
    }

    pub async fn open(path: &Path) -> Result<Self, StoreError> {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        // Migrate on a dedicated connection first so every pooled connection
        // prepares its statements against the final schema.
        let mut conn = opts.connect().await?;
        Self::migrate(&mut conn).await?;
        conn.close().await?;
        Self::connect(opts, 4).await
    }

    pub async fn open_in_memory() -> Result<Self, StoreError> {
        // One connection: every pooled in-memory connection is its own database.
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")?;
        let store = Self::connect(opts, 1).await?;
        // The pool has one connection, so migrating through it is safe.
        Self::migrate(&mut *store.pool.acquire().await?).await?;
        Ok(store)
    }

    async fn connect(opts: SqliteConnectOptions, max: u32) -> Result<Self, StoreError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(max)
            .connect_with(opts)
            .await?;
        Ok(Self { pool })
    }

    async fn migrate(conn: &mut SqliteConnection) -> Result<(), StoreError> {
        sqlx::raw_sql(SCHEMA).execute(&mut *conn).await?;
        // Databases created before these columns existed lack them.
        for ddl in [
            "ALTER TABLE drones ADD COLUMN cache_stats TEXT",
            "ALTER TABLE drones ADD COLUMN quarantined INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE drones ADD COLUMN quarantine_reason TEXT",
            "ALTER TABLE drones ADD COLUMN version TEXT",
            "ALTER TABLE drones ADD COLUMN extra_labels TEXT",
            "ALTER TABLE drones ADD COLUMN bids TEXT",
            "ALTER TABLE drones ADD COLUMN quarantine_source TEXT",
            "ALTER TABLE drones ADD COLUMN quarantined_at INTEGER",
            "ALTER TABLE drones ADD COLUMN probation_since INTEGER",
            "ALTER TABLE drones ADD COLUMN quarantine_strikes INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE drones ADD COLUMN health_cleared_at INTEGER",
            "ALTER TABLE drones ADD COLUMN version_since INTEGER",
            "ALTER TABLE drones ADD COLUMN reclaim TEXT",
            "ALTER TABLE drones ADD COLUMN env_facts TEXT",
            "ALTER TABLE leases ADD COLUMN preempted_at INTEGER",
            "ALTER TABLE drones ADD COLUMN trust TEXT NOT NULL DEFAULT 'shared'",
            "ALTER TABLE units ADD COLUMN scope TEXT NOT NULL DEFAULT 'pr'",
            "ALTER TABLE units ADD COLUMN supersede_key TEXT",
            "ALTER TABLE units ADD COLUMN record INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE clients ADD COLUMN max_scope TEXT NOT NULL DEFAULT 'pr'",
        ] {
            // sqlx-runtime-query: one-off additive migration, no data involved
            if let Err(e) = sqlx::query(ddl).execute(&mut *conn).await
                && !e.to_string().contains("duplicate column")
            {
                return Err(e.into());
            }
        }
        Ok(())
    }

    pub async fn enroll(&self, name: &str, token_hash: &str, now: i64) -> Result<(), StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let r = sqlx::query("INSERT INTO drones (name, token_hash, enrolled_at) VALUES (?, ?, ?)")
            .bind(name)
            .bind(token_hash)
            .bind(now)
            .execute(&self.pool)
            .await;
        match r {
            Ok(_) => Ok(()),
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                Err(StoreError::AlreadyEnrolled(name.to_owned()))
            }
            Err(e) => Err(e.into()),
        }
    }

    pub async fn drone_for_token_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<String>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let row = sqlx::query("SELECT name FROM drones WHERE token_hash = ?")
            .bind(token_hash)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<String, _>("name")))
    }

    pub async fn record_heartbeat(
        &self,
        name: &str,
        capacity: &Capacity,
        caps: &HostCaps,
        accepting: bool,
        cache_stats: &[CacheStat],
        now: i64,
    ) -> Result<(), StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query("UPDATE drones SET last_heartbeat = ?, capacity = ?, caps = ?, accepting = ?, cache_stats = ? WHERE name = ?")
            .bind(now)
            .bind(to_text(capacity)?)
            .bind(to_text(caps)?)
            .bind(i64::from(accepting))
            .bind(to_text(&cache_stats)?)
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Labels the drone asks to have added to its runners.
    pub async fn set_extra_labels(&self, name: &str, labels: &[String]) -> Result<(), StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query("UPDATE drones SET extra_labels = ? WHERE name = ?")
            .bind(to_text(&labels)?)
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The trust the drone reports for itself.
    pub async fn set_trust(&self, name: &str, trust: Trust) -> Result<(), StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query("UPDATE drones SET trust = ? WHERE name = ?")
            .bind(trust.as_str())
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Delete a drone (its token hash with it) and its lease records. Returns
    /// the number of unfinished leases dropped, or `None` if there is no such
    /// drone. With the lease rows gone the queen no longer treats those
    /// runners as owned, so it deregisters any that are still offline.
    pub async fn remove_drone(&self, name: &str) -> Result<Option<u32>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let exists = sqlx::query("SELECT 1 FROM drones WHERE name = ?")
            .bind(name)
            .fetch_optional(&self.pool)
            .await?
            .is_some();
        if !exists {
            return Ok(None);
        }
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let unfinished: i64 = sqlx::query(
            "SELECT COUNT(*) AS n FROM leases WHERE drone = ? AND state IN ('pending', 'running')",
        )
        .bind(name)
        .fetch_one(&self.pool)
        .await?
        .get("n");
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query("DELETE FROM leases WHERE drone = ?")
            .bind(name)
            .execute(&self.pool)
            .await?;
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query("DELETE FROM drones WHERE name = ?")
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(Some(u32::try_from(unfinished).unwrap_or(u32::MAX)))
    }

    pub async fn drones(&self) -> Result<Vec<DroneRow>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let rows = sqlx::query(
            "SELECT name, last_heartbeat, capacity, caps, accepting, cache_stats, quarantined, quarantine_reason, version, extra_labels, bids, quarantine_source, quarantined_at, probation_since, quarantine_strikes, health_cleared_at, version_since, reclaim, env_facts, trust FROM drones ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                let capacity: Option<String> = r.get("capacity");
                let caps: Option<String> = r.get("caps");
                let stats: Option<String> = r.get("cache_stats");
                let labels: Option<String> = r.get("extra_labels");
                Ok(DroneRow {
                    name: r.get("name"),
                    last_heartbeat: r.get("last_heartbeat"),
                    capacity: capacity.as_deref().map(from_text).transpose()?,
                    caps: caps
                        .as_deref()
                        .map(from_text)
                        .transpose()?
                        .unwrap_or_default(),
                    accepting: r.get::<i64, _>("accepting") != 0,
                    cache_stats: stats
                        .as_deref()
                        .map(from_text)
                        .transpose()?
                        .unwrap_or_default(),
                    quarantined: r.get::<i64, _>("quarantined") != 0,
                    quarantine_reason: r.get("quarantine_reason"),
                    version: r.get("version"),
                    extra_labels: labels
                        .as_deref()
                        .map(from_text)
                        .transpose()?
                        .unwrap_or_default(),
                    trust: enum_from(&r.get::<String, _>("trust"))?,
                    quarantine_source: r.get("quarantine_source"),
                    quarantined_at: r.get("quarantined_at"),
                    probation_since: r.get("probation_since"),
                    quarantine_strikes: u32::try_from(r.get::<i64, _>("quarantine_strikes"))
                        .unwrap_or(0),
                    health_cleared_at: r.get("health_cleared_at"),
                    version_since: r.get("version_since"),
                    env_facts: r
                        .get::<Option<String>, _>("env_facts")
                        .as_deref()
                        .map(from_text)
                        .transpose()?,
                    reclaim: r
                        .get::<Option<String>, _>("reclaim")
                        .as_deref()
                        .map(from_text)
                        .transpose()?,
                    bids: r
                        .get::<Option<String>, _>("bids")
                        .as_deref()
                        .map(from_text)
                        .transpose()?
                        .unwrap_or_default(),
                })
            })
            .collect()
    }

    /// The burst VMs the queen created and still accounts for (live mode).
    pub(crate) async fn burst_vms(&self) -> Result<Vec<crate::burst::BurstVm>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let rows = sqlx::query(
            "SELECT drone, vm, zone, created_at, draining_at, ended_at, scrubbed_at FROM burst_vms ORDER BY created_at, drone",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| crate::burst::BurstVm {
                drone: r.get("drone"),
                vm: r.get("vm"),
                zone: r.get("zone"),
                created_at: r.get("created_at"),
                draining_at: r.get("draining_at"),
                ended_at: r.get("ended_at"),
                scrubbed_at: r.get("scrubbed_at"),
            })
            .collect())
    }

    /// Replace the burst VM list with `vms` (a handful of rows, written
    /// whole after every live pass so a restart keeps the spend record).
    pub(crate) async fn save_burst_vms(
        &self,
        vms: &[crate::burst::BurstVm],
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query("DELETE FROM burst_vms")
            .execute(&mut *tx)
            .await?;
        for v in vms {
            // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
            sqlx::query(
                "INSERT INTO burst_vms (drone, vm, zone, created_at, draining_at, ended_at, scrubbed_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&v.drone)
            .bind(&v.vm)
            .bind(&v.zone)
            .bind(v.created_at)
            .bind(v.draining_at)
            .bind(v.ended_at)
            .bind(v.scrubbed_at)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Quarantine (`Some(reason)`) or release (`None`) a drone on an
    /// operator's say-so. A release forgets earlier health evidence. False if
    /// no such drone is enrolled.
    pub async fn set_quarantine(
        &self,
        name: &str,
        reason: Option<&str>,
    ) -> Result<bool, StoreError> {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        const HOLD: &str = "UPDATE drones SET quarantined = 1, quarantine_reason = ?, quarantine_source = 'operator', quarantined_at = ?, probation_since = NULL, quarantine_strikes = 0 WHERE name = ?";
        const FREE: &str = "UPDATE drones SET quarantined = 0, quarantine_reason = NULL, quarantine_source = NULL, quarantined_at = NULL, probation_since = NULL, quarantine_strikes = 0, health_cleared_at = ? WHERE name = ?";
        let r = match reason {
            // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
            Some(why) => sqlx::query(HOLD).bind(why).bind(now).bind(name),
            // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
            None => sqlx::query(FREE).bind(now).bind(name),
        }
        .execute(&self.pool)
        .await?;
        if r.rows_affected() == 0 {
            return Ok(false);
        }
        let event = if reason.is_some() {
            "manual_quarantine"
        } else {
            "manual_release"
        };
        self.log_health(now, name, event, reason, None).await?;
        Ok(true)
    }

    /// Append a fleet or drone event (stale, expired, capacity) to the
    /// health audit trail that `/status` shows as `health_events`.
    pub(crate) async fn record_health_event(
        &self,
        at: i64,
        drone: &str,
        event: &str,
        reason: &str,
    ) -> Result<(), StoreError> {
        self.log_health(at, drone, event, Some(reason), None).await
    }

    async fn log_health(
        &self,
        at: i64,
        drone: &str,
        event: &str,
        reason: Option<&str>,
        evidence: Option<&str>,
    ) -> Result<(), StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query(
            "INSERT INTO health_log (at, drone, event, reason, evidence) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(at)
        .bind(drone)
        .bind(event)
        .bind(reason)
        .bind(evidence)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Apply one automatic health action and record it in the audit trail
    /// (which is never pruned).
    pub async fn apply_health_action(&self, action: &Action, now: i64) -> Result<(), StoreError> {
        const HOLD: &str = "UPDATE drones SET quarantined = 1, quarantine_reason = ?, quarantine_source = 'auto', quarantined_at = ?, probation_since = NULL, quarantine_strikes = ? WHERE name = ?";
        const PROBATION: &str =
            "UPDATE drones SET probation_since = ? WHERE name = ? AND quarantine_source = 'auto'";
        const FREE: &str = "UPDATE drones SET quarantined = 0, quarantine_reason = NULL, quarantine_source = NULL, quarantined_at = NULL, probation_since = NULL, quarantine_strikes = 0, health_cleared_at = ? WHERE name = ? AND quarantine_source = 'auto'";
        let (reason, evidence) = match action {
            Action::Quarantine {
                drone,
                reason,
                evidence,
                strikes,
            }
            | Action::Requarantine {
                drone,
                reason,
                evidence,
                strikes,
            } => {
                // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
                sqlx::query(HOLD)
                    .bind(reason)
                    .bind(now)
                    .bind(i64::from(*strikes))
                    .bind(drone)
                    .execute(&self.pool)
                    .await?;
                (Some(reason.as_str()), Some(to_text(evidence)?))
            }
            Action::BeginProbation { drone } => {
                // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
                sqlx::query(PROBATION)
                    .bind(now)
                    .bind(drone)
                    .execute(&self.pool)
                    .await?;
                (None, None)
            }
            Action::Release { drone } => {
                // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
                sqlx::query(FREE)
                    .bind(now)
                    .bind(drone)
                    .execute(&self.pool)
                    .await?;
                (None, None)
            }
        };
        self.log_health(
            now,
            action.drone(),
            action.event(),
            reason,
            evidence.as_deref(),
        )
        .await
    }

    /// The newest `limit` health audit entries, newest first.
    pub async fn health_log(&self, limit: i64) -> Result<Vec<HealthLogRow>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let rows = sqlx::query(
            "SELECT at, drone, event, reason, evidence FROM health_log ORDER BY id DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                let evidence: Option<String> = r.get("evidence");
                Ok(HealthLogRow {
                    at: r.get("at"),
                    drone: r.get("drone"),
                    event: r.get("event"),
                    reason: r.get("reason"),
                    evidence: evidence.as_deref().map(from_text).transpose()?,
                })
            })
            .collect()
    }

    /// Store a drone's reclaim stats and append its new audit entries. Entries
    /// already stored (same drone and `seq`) are ignored, so a repeated
    /// heartbeat cannot duplicate them. Text is bounded.
    pub async fn record_reclaim(&self, name: &str, stats: &ReclaimStats) -> Result<(), StoreError> {
        const MAX_TEXT: usize = 500;
        const MAX_ENTRIES: usize = 500;
        let clip = |s: &str| {
            s.chars()
                .filter(|c| !c.is_control())
                .take(MAX_TEXT)
                .collect::<String>()
        };
        for r in stats.recent.iter().take(MAX_ENTRIES) {
            let size = i64::try_from(r.size_bytes).unwrap_or(i64::MAX);
            let seq = i64::try_from(r.seq).unwrap_or(i64::MAX);
            // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
            sqlx::query("INSERT OR IGNORE INTO reclaim_log (drone, seq, at, path, kind, size_bytes, reason, outcome, error) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)")
                .bind(name)
                .bind(seq)
                .bind(r.at)
                .bind(clip(&r.path))
                .bind(to_text(&r.kind)?.trim_matches('"').to_owned())
                .bind(size)
                .bind(clip(&r.reason))
                .bind(to_text(&r.outcome)?.trim_matches('"').to_owned())
                .bind(r.error.as_deref().map(clip))
                .execute(&self.pool)
                .await?;
        }
        let mut kept = stats.clone();
        kept.recent.clear();
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query("UPDATE drones SET reclaim = ? WHERE name = ?")
            .bind(to_text(&kept)?)
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The newest reclaim audit entries across drones, newest first.
    pub async fn reclaim_log(&self, limit: i64) -> Result<Vec<ReclaimLogRow>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let rows = sqlx::query("SELECT drone, seq, at, path, kind, size_bytes, reason, outcome, error FROM reclaim_log ORDER BY at DESC, seq DESC LIMIT ?")
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|r| {
                Ok(ReclaimLogRow {
                    drone: r.get("drone"),
                    record: DeletionRecord {
                        seq: u64::try_from(r.get::<i64, _>("seq")).unwrap_or(0),
                        at: r.get("at"),
                        path: r.get("path"),
                        kind: enum_from(&r.get::<String, _>("kind"))?,
                        size_bytes: u64::try_from(r.get::<i64, _>("size_bytes")).unwrap_or(0),
                        reason: r.get("reason"),
                        outcome: enum_from(&r.get::<String, _>("outcome"))?,
                        error: r.get("error"),
                    },
                })
            })
            .collect()
    }

    /// Total bytes recorded as really deleted, across all drones and time.
    pub async fn reclaim_bytes_logged(&self) -> Result<u64, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let row = sqlx::query(
            "SELECT COALESCE(SUM(size_bytes), 0) AS n FROM reclaim_log WHERE outcome = 'deleted'",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(u64::try_from(row.get::<i64, _>("n")).unwrap_or(0))
    }

    pub async fn prune_reclaim_log(&self, before: i64) -> Result<u64, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let r = sqlx::query("DELETE FROM reclaim_log WHERE at < ?")
            .bind(before)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected())
    }

    /// Store the build environment a drone last reported.
    pub async fn set_env_facts(&self, name: &str, facts: &EnvFacts) -> Result<(), StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query("UPDATE drones SET env_facts = ? WHERE name = ?")
            .bind(to_text(facts)?)
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Replace the bids a drone last reported.
    pub async fn set_bids(&self, name: &str, bids: &[DroneBid]) -> Result<(), StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query("UPDATE drones SET bids = ? WHERE name = ?")
            .bind(to_text(&bids)?)
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Record the borg version a drone reported in its heartbeat.
    pub async fn set_version(
        &self,
        name: &str,
        version: Option<&str>,
        now: i64,
    ) -> Result<(), StoreError> {
        // `version_since` moves only when the reported version changes.
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query("UPDATE drones SET version_since = CASE WHEN version IS ? THEN version_since ELSE ? END, version = ? WHERE name = ?")
            .bind(version)
            .bind(now)
            .bind(version)
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The persisted state of the current rollout, if any.
    pub async fn rollout_state(&self) -> Result<Option<RolloutState>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let row = sqlx::query("SELECT state FROM rollout_state WHERE id = 1")
            .fetch_optional(&self.pool)
            .await?;
        row.map(|r| from_text(&r.get::<String, _>("state")))
            .transpose()
    }

    pub async fn save_rollout_state(&self, st: &RolloutState, now: i64) -> Result<(), StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query(
            "INSERT OR REPLACE INTO rollout_state (id, state, updated_at) VALUES (1, ?, ?)",
        )
        .bind(to_text(st)?)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Forget the current rollout; the next pass starts it over.
    pub async fn clear_rollout_state(&self) -> Result<(), StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query("DELETE FROM rollout_state WHERE id = 1")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn log_rollout(&self, at: i64, event: &str) -> Result<(), StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query("INSERT INTO rollout_log (at, event) VALUES (?, ?)")
            .bind(at)
            .bind(event)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The newest rollout audit lines, newest first.
    pub async fn rollout_log(&self, limit: i64) -> Result<Vec<(i64, String)>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let rows = sqlx::query("SELECT at, event FROM rollout_log ORDER BY id DESC LIMIT ?")
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|r| (r.get("at"), r.get("event"))).collect())
    }

    pub async fn insert_lease(&self, l: &LeaseRow) -> Result<(), StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query(
            "INSERT INTO leases (id, drone, class, scope, labels, runner_name, runner_id, jit_config, state, created_at, expires_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&l.id)
        .bind(&l.drone)
        .bind(enum_text(&l.class)?)
        .bind(enum_text(&l.scope)?)
        .bind(to_text(&l.labels)?)
        .bind(&l.runner_name)
        .bind(i64::try_from(l.runner_id).map_err(|e| StoreError::Corrupt(e.to_string()))?)
        .bind(l.jit_config.as_ref().map(Secret::expose))
        .bind(l.state.as_str())
        .bind(l.created_at)
        .bind(l.expires_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub(crate) fn lease_from_row(r: &sqlx::sqlite::SqliteRow) -> Result<LeaseRow, StoreError> {
        let jit: Option<String> = r.get("jit_config");
        Ok(LeaseRow {
            id: r.get("id"),
            drone: r.get("drone"),
            class: enum_from(&r.get::<String, _>("class"))?,
            scope: enum_from(&r.get::<String, _>("scope"))?,
            labels: from_text(&r.get::<String, _>("labels"))?,
            runner_name: r.get("runner_name"),
            runner_id: u64::try_from(r.get::<i64, _>("runner_id"))
                .map_err(|e| StoreError::Corrupt(e.to_string()))?,
            jit_config: jit.map(Secret::new),
            state: LeaseStatus::parse(&r.get::<String, _>("state"))?,
            created_at: r.get("created_at"),
            expires_at: r.get("expires_at"),
        })
    }

    /// Leases that are pending or running.
    pub async fn active_leases(&self) -> Result<Vec<LeaseRow>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let rows = sqlx::query(
            "SELECT id, drone, class, scope, labels, runner_name, runner_id, jit_config, state, created_at, expires_at FROM leases WHERE state IN ('standby', 'pending', 'running') ORDER BY created_at, id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::lease_from_row).collect()
    }

    pub async fn lease(&self, id: &str) -> Result<Option<LeaseRow>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let row = sqlx::query("SELECT id, drone, class, scope, labels, runner_name, runner_id, jit_config, state, created_at, expires_at FROM leases WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(Self::lease_from_row).transpose()
    }

    /// Move an active lease to `to`. Finished leases never change again.
    pub async fn set_state(&self, id: &str, to: LeaseStatus, now: i64) -> Result<(), StoreError> {
        let finished = (!to.is_active()).then_some(now);
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query(
            "UPDATE leases SET state = ?, finished_at = COALESCE(?, finished_at), jit_config = CASE WHEN ? THEN NULL ELSE jit_config END
             WHERE id = ? AND state IN ('standby', 'pending', 'running')",
        )
        .bind(to.as_str())
        .bind(finished)
        .bind(!to.is_active())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record a drone's result for its own lease. Returns false if `drone`
    /// does not own `id`. Re-reporting a finished lease is accepted.
    pub async fn finish_lease(
        &self,
        id: &str,
        drone: &str,
        result: &LeaseResult,
        now: i64,
    ) -> Result<bool, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let owned = sqlx::query("SELECT 1 FROM leases WHERE id = ? AND drone = ?")
            .bind(id)
            .bind(drone)
            .fetch_optional(&self.pool)
            .await?
            .is_some();
        if !owned {
            return Ok(false);
        }
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query(
            "UPDATE leases SET state = 'done', finished_at = ?, jit_config = NULL, exit_code = ?, duration_secs = ?, peak_rss_mib = ?
             WHERE id = ? AND state IN ('standby', 'pending', 'running', 'expired')",
        )
        .bind(now)
        .bind(i64::from(result.exit_code))
        .bind(i64::try_from(result.duration_secs).unwrap_or(i64::MAX))
        .bind(result.peak_rss_mib.map(|v| i64::try_from(v).unwrap_or(i64::MAX)))
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(true)
    }

    /// Mark an active lease preempted and record the job it was running, once.
    /// Returns false if it was already marked or is no longer active.
    pub async fn record_preemption(&self, p: &PreemptionRow, now: i64) -> Result<bool, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let marked = sqlx::query(
            "UPDATE leases SET preempted_at = ? WHERE id = ? AND preempted_at IS NULL AND state IN ('pending', 'running')",
        )
        .bind(now)
        .bind(&p.lease_id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if marked == 0 {
            return Ok(false);
        }
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query(
            "INSERT OR IGNORE INTO preemptions (lease_id, drone, class, scope, for_scope, repo, run_id, job_id, at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&p.lease_id)
        .bind(&p.drone)
        .bind(enum_text(&p.class)?)
        .bind(enum_text(&p.scope)?)
        .bind(enum_text(&p.for_scope)?)
        .bind(&p.repo)
        .bind(i64::try_from(p.run_id).unwrap_or(i64::MAX))
        .bind(i64::try_from(p.job_id).unwrap_or(i64::MAX))
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(true)
    }

    /// `(lease id, drone)` of preempted leases that have not ended: the drone
    /// is told to cancel each on every heartbeat until the lease finishes.
    pub async fn preempted_active(&self) -> Result<Vec<(String, String)>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let rows = sqlx::query(
            "SELECT id, drone FROM leases WHERE preempted_at IS NOT NULL AND state IN ('pending', 'running') ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| (r.get("id"), r.get("drone"))).collect())
    }

    /// The newest `limit` preemptions, newest first.
    pub async fn preemptions(&self, limit: i64) -> Result<Vec<PreemptionRow>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let rows = sqlx::query("SELECT lease_id, drone, class, scope, for_scope, repo, run_id, job_id, at, requeued_at FROM preemptions ORDER BY at DESC, lease_id LIMIT ?")
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(Self::preemption_from_row).collect()
    }

    /// Up to `limit` preemptions whose job has not been re-run, oldest first.
    pub async fn unrequeued_preemptions(
        &self,
        limit: i64,
    ) -> Result<Vec<PreemptionRow>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let rows = sqlx::query("SELECT lease_id, drone, class, scope, for_scope, repo, run_id, job_id, at, requeued_at FROM preemptions WHERE requeued_at IS NULL ORDER BY at, lease_id LIMIT ?")
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(Self::preemption_from_row).collect()
    }

    /// Record that a preempted job needs no further requeue attempts.
    pub async fn mark_requeued(&self, lease_id: &str, now: i64) -> Result<(), StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query(
            "UPDATE preemptions SET requeued_at = ? WHERE lease_id = ? AND requeued_at IS NULL",
        )
        .bind(now)
        .bind(lease_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    fn preemption_from_row(r: &sqlx::sqlite::SqliteRow) -> Result<PreemptionRow, StoreError> {
        Ok(PreemptionRow {
            lease_id: r.get("lease_id"),
            drone: r.get("drone"),
            class: enum_from(&r.get::<String, _>("class"))?,
            scope: enum_from(&r.get::<String, _>("scope"))?,
            for_scope: enum_from(&r.get::<String, _>("for_scope"))?,
            repo: r.get("repo"),
            run_id: u64::try_from(r.get::<i64, _>("run_id")).unwrap_or(0),
            job_id: u64::try_from(r.get::<i64, _>("job_id")).unwrap_or(0),
            at: r.get("at"),
            requeued_at: r.get("requeued_at"),
        })
    }

    /// Preemption counts by `(victim scope, starving scope)`, for metrics.
    pub async fn preemption_counts(&self) -> Result<Vec<(Scope, Scope, u64)>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let rows = sqlx::query(
            "SELECT scope, for_scope, COUNT(*) AS n FROM preemptions GROUP BY scope, for_scope ORDER BY scope, for_scope",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok((
                    enum_from(&r.get::<String, _>("scope"))?,
                    enum_from(&r.get::<String, _>("for_scope"))?,
                    u64::try_from(r.get::<i64, _>("n")).unwrap_or(0),
                ))
            })
            .collect()
    }

    /// Drop the JIT config of leases `drone` says it holds.
    pub async fn acknowledge(&self, drone: &str, lease_ids: &[String]) -> Result<(), StoreError> {
        for id in lease_ids {
            // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
            sqlx::query("UPDATE leases SET jit_config = NULL WHERE id = ? AND drone = ?")
                .bind(id)
                .bind(drone)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    /// Active leases of `drone` whose JIT config has not been acknowledged.
    pub async fn undelivered(&self, drone: &str) -> Result<Vec<LeaseRow>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let rows = sqlx::query(
            "SELECT id, drone, class, scope, labels, runner_name, runner_id, jit_config, state, created_at, expires_at FROM leases WHERE drone = ? AND state = 'pending' AND jit_config IS NOT NULL ORDER BY created_at, id",
        )
        .bind(drone)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::lease_from_row).collect()
    }

    /// Unstarted standby leases of `drone`, oldest first.
    pub async fn standby_leases(&self, drone: &str) -> Result<Vec<LeaseRow>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let rows = sqlx::query(
            "SELECT id, drone, class, scope, labels, runner_name, runner_id, jit_config, state, created_at, expires_at FROM leases WHERE drone = ? AND state = 'standby' ORDER BY created_at, id",
        )
        .bind(drone)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::lease_from_row).collect()
    }

    /// Turn the standby leases `ids` of `drone` into pending ones: the drone
    /// started them. The usual TTL applies from `now`, and the JIT config is
    /// dropped. Returns how many changed.
    pub async fn promote_standby(
        &self,
        drone: &str,
        ids: &[String],
        now: i64,
        ttl_secs: i64,
    ) -> Result<u64, StoreError> {
        let mut changed = 0;
        for id in ids {
            // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
            changed += sqlx::query(
                "UPDATE leases SET state = 'pending', expires_at = ?, jit_config = NULL
                 WHERE id = ? AND drone = ? AND state = 'standby'",
            )
            .bind(now.saturating_add(ttl_secs))
            .bind(id)
            .bind(drone)
            .execute(&self.pool)
            .await?
            .rows_affected();
        }
        Ok(changed)
    }

    /// Lease counts by (class, state), for metrics.
    pub async fn lease_counts(&self) -> Result<Vec<(Class, LeaseStatus, u32)>, StoreError> {
        let rows =
            // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
            sqlx::query("SELECT class, state, COUNT(*) AS n FROM leases GROUP BY class, state")
                .fetch_all(&self.pool)
                .await?;
        rows.iter()
            .map(|r| {
                Ok((
                    enum_from(&r.get::<String, _>("class"))?,
                    LeaseStatus::parse(&r.get::<String, _>("state"))?,
                    u32::try_from(r.get::<i64, _>("n")).unwrap_or(u32::MAX),
                ))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(id: &str, drone: &str) -> LeaseRow {
        LeaseRow {
            id: id.into(),
            drone: drone.into(),
            class: Class::Medium,
            scope: Scope::Pr,
            labels: vec!["a".into(), "b".into()],
            runner_name: format!("borg-{drone}-{id}"),
            runner_id: 7,
            jit_config: Some(Secret::new("jit")),
            state: LeaseStatus::Pending,
            created_at: 100,
            expires_at: 400,
        }
    }

    async fn store_with(drone: &str) -> Store {
        let s = Store::open_in_memory().await.unwrap();
        s.enroll(drone, &format!("hash-{drone}"), 1).await.unwrap();
        s
    }

    /// A database created before the newest columns existed must open, migrate
    /// and read back without sqlx tripping over a stale column-name map.
    #[tokio::test]
    async fn opens_a_pre_migration_database_and_migrates_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("borg.db");
        let opts = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        let mut conn = opts.connect().await.unwrap();
        sqlx::raw_sql(
            "
CREATE TABLE drones (
    name           TEXT PRIMARY KEY,
    token_hash     TEXT NOT NULL UNIQUE,
    enrolled_at    INTEGER NOT NULL,
    last_heartbeat INTEGER,
    capacity       TEXT,
    caps           TEXT,
    accepting      INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE leases (
    id           TEXT PRIMARY KEY,
    drone        TEXT NOT NULL REFERENCES drones(name),
    class        TEXT NOT NULL,
    scope        TEXT NOT NULL,
    labels       TEXT NOT NULL,
    runner_name  TEXT NOT NULL,
    runner_id    INTEGER NOT NULL,
    jit_config   TEXT,
    state        TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    expires_at   INTEGER NOT NULL,
    finished_at  INTEGER,
    exit_code    INTEGER,
    duration_secs INTEGER,
    peak_rss_mib INTEGER
);
INSERT INTO drones (name, token_hash, enrolled_at) VALUES ('d1', 'hash-d1', 1);
INSERT INTO leases (id, drone, class, scope, labels, runner_name, runner_id, jit_config, state, created_at, expires_at)
VALUES ('l1', 'd1', 'medium', 'pr', '[\"a\"]', 'borg-d1-l1', 7, 'jit', 'pending', 100, 400);
",
        )
        .execute(&mut conn)
        .await
        .unwrap();
        conn.close().await.unwrap();

        for _ in 0..2 {
            let s = Store::open(&path).await.unwrap();
            // Concurrent reads warm several pooled connections.
            let (a, b, c, d) = tokio::join!(
                s.active_leases(),
                s.active_leases(),
                s.active_leases(),
                s.drones()
            );
            assert_eq!(a.unwrap().len(), 1);
            assert_eq!(b.unwrap().len(), 1);
            assert_eq!(c.unwrap().len(), 1);
            let drones = d.unwrap();
            assert_eq!(drones.len(), 1);
            assert_eq!(drones[0].name, "d1");
            assert_eq!(s.lease("l1").await.unwrap().unwrap().runner_id, 7);
        }
    }

    #[tokio::test]
    async fn enrollment_is_unique_and_lookup_is_by_hash() {
        let s = store_with("d1").await;
        assert_eq!(
            s.drone_for_token_hash("hash-d1").await.unwrap().as_deref(),
            Some("d1")
        );
        assert_eq!(s.drone_for_token_hash("nope").await.unwrap(), None);
        assert!(matches!(
            s.enroll("d1", "other", 2).await,
            Err(StoreError::AlreadyEnrolled(_))
        ));
        assert!(matches!(
            s.enroll("d2", "hash-d1", 2).await,
            Err(StoreError::AlreadyEnrolled(_))
        ));
    }

    #[tokio::test]
    async fn jit_config_is_delivered_until_acknowledged() {
        let s = store_with("d1").await;
        s.insert_lease(&lease("l1", "d1")).await.unwrap();
        assert_eq!(s.undelivered("d1").await.unwrap().len(), 1);
        s.acknowledge("d1", &["l1".to_owned()]).await.unwrap();
        assert!(s.undelivered("d1").await.unwrap().is_empty());
        assert_eq!(
            s.active_leases().await.unwrap().len(),
            1,
            "ack does not end the lease"
        );
    }

    #[tokio::test]
    async fn only_the_owning_drone_can_finish_and_finishing_is_idempotent() {
        let s = store_with("d1").await;
        s.enroll("d2", "hash-d2", 1).await.unwrap();
        s.insert_lease(&lease("l1", "d1")).await.unwrap();
        let r = LeaseResult {
            exit_code: 0,
            duration_secs: 5,
            peak_rss_mib: Some(100),
        };
        assert!(!s.finish_lease("l1", "d2", &r, 500).await.unwrap());
        assert!(s.finish_lease("l1", "d1", &r, 500).await.unwrap());
        assert!(s.finish_lease("l1", "d1", &r, 600).await.unwrap());
        let l = s.lease("l1").await.unwrap().unwrap();
        assert_eq!(l.state, LeaseStatus::Done);
        assert!(l.jit_config.is_none(), "secret dropped when the lease ends");
        assert!(s.active_leases().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn preemption_is_recorded_once_and_listed_until_the_lease_ends() {
        let s = store_with("host-a").await;
        s.insert_lease(&lease("l1", "host-a")).await.unwrap();
        let row = PreemptionRow {
            lease_id: "l1".into(),
            drone: "host-a".into(),
            class: Class::Medium,
            scope: Scope::Pr,
            for_scope: Scope::Main,
            repo: "example-org/app".into(),
            run_id: 5,
            job_id: 6,
            at: 0,
            requeued_at: None,
        };
        assert!(s.record_preemption(&row, 500).await.unwrap());
        assert!(!s.record_preemption(&row, 501).await.unwrap());
        assert_eq!(
            s.preempted_active().await.unwrap(),
            [("l1".to_owned(), "host-a".to_owned())]
        );
        assert_eq!(
            s.preemptions(20).await.unwrap(),
            [PreemptionRow { at: 500, ..row }]
        );
        assert_eq!(
            s.preemption_counts().await.unwrap(),
            [(Scope::Pr, Scope::Main, 1)]
        );
        s.set_state("l1", LeaseStatus::Done, 600).await.unwrap();
        assert!(s.preempted_active().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn unrequeued_preemptions_list_oldest_first_until_marked() {
        let s = store_with("host-a").await;
        for (id, at) in [("l1", 300), ("l2", 100), ("l3", 200)] {
            s.insert_lease(&lease(id, "host-a")).await.unwrap();
            let row = PreemptionRow {
                lease_id: id.into(),
                drone: "host-a".into(),
                class: Class::Medium,
                scope: Scope::Pr,
                for_scope: Scope::Main,
                repo: "example-org/app".into(),
                run_id: 5,
                job_id: 6,
                at: 0,
                requeued_at: None,
            };
            assert!(s.record_preemption(&row, at).await.unwrap());
        }
        let ids = |rows: Vec<PreemptionRow>| -> Vec<String> {
            rows.into_iter().map(|r| r.lease_id).collect()
        };
        assert_eq!(
            ids(s.unrequeued_preemptions(10).await.unwrap()),
            ["l2", "l3", "l1"]
        );
        assert_eq!(
            ids(s.unrequeued_preemptions(2).await.unwrap()),
            ["l2", "l3"]
        );
        s.mark_requeued("l2", 900).await.unwrap();
        s.mark_requeued("l2", 950).await.unwrap();
        assert_eq!(
            ids(s.unrequeued_preemptions(10).await.unwrap()),
            ["l3", "l1"]
        );
        let all = s.preemptions(10).await.unwrap();
        let l2 = all.iter().find(|r| r.lease_id == "l2").unwrap();
        assert_eq!(l2.requeued_at, Some(900), "the first mark sticks");
    }

    #[tokio::test]
    async fn standby_leases_round_trip_stay_active_and_promote() {
        let s = store_with("d1").await;
        let mut l = lease("sb1", "d1");
        l.state = LeaseStatus::Standby;
        l.expires_at = i64::MAX;
        s.insert_lease(&l).await.unwrap();
        s.insert_lease(&lease("p1", "d1")).await.unwrap();
        let got = s.lease("sb1").await.unwrap().unwrap();
        assert_eq!(got.state, LeaseStatus::Standby);
        assert_eq!(got.expires_at, i64::MAX);
        assert_eq!(s.active_leases().await.unwrap().len(), 2);
        let standby = s.standby_leases("d1").await.unwrap();
        assert_eq!(standby.len(), 1);
        assert!(standby[0].jit_config.is_some());
        // A standby is not delivered as an ordinary pending lease.
        let undelivered = s.undelivered("d1").await.unwrap();
        assert!(undelivered.iter().all(|u| u.id != "sb1"));
        assert_eq!(
            s.promote_standby("d1", &["sb1".into()], 1000, 60)
                .await
                .unwrap(),
            1
        );
        let got = s.lease("sb1").await.unwrap().unwrap();
        assert_eq!((got.state, got.expires_at), (LeaseStatus::Pending, 1060));
        assert!(got.jit_config.is_none());
        assert_eq!(
            s.promote_standby("d1", &["sb1".into()], 1000, 60)
                .await
                .unwrap(),
            0
        );
        // A standby can end without ever starting.
        let mut l2 = lease("sb2", "d1");
        l2.state = LeaseStatus::Standby;
        s.insert_lease(&l2).await.unwrap();
        s.set_state("sb2", LeaseStatus::Done, 1100).await.unwrap();
        assert_eq!(
            s.lease("sb2").await.unwrap().unwrap().state,
            LeaseStatus::Done
        );
        assert!(s.standby_leases("d1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn set_state_never_revives_a_finished_lease() {
        let s = store_with("d1").await;
        s.insert_lease(&lease("l1", "d1")).await.unwrap();
        s.set_state("l1", LeaseStatus::Running, 200).await.unwrap();
        assert_eq!(
            s.lease("l1").await.unwrap().unwrap().state,
            LeaseStatus::Running
        );
        s.set_state("l1", LeaseStatus::Expired, 300).await.unwrap();
        s.set_state("l1", LeaseStatus::Running, 400).await.unwrap();
        assert_eq!(
            s.lease("l1").await.unwrap().unwrap().state,
            LeaseStatus::Expired
        );
    }

    #[tokio::test]
    async fn quarantine_and_version_round_trip_and_survive_heartbeats() {
        let s = store_with("d1").await;
        assert!(!s.set_quarantine("nope", Some("x")).await.unwrap());
        assert!(s.set_quarantine("d1", Some("bad disk")).await.unwrap());
        s.set_version("d1", Some("1.2.3"), 50).await.unwrap();
        s.set_version("d1", Some("1.2.3"), 99).await.unwrap();
        assert_eq!(s.drones().await.unwrap()[0].version_since, Some(50));
        s.set_version("d1", Some("1.2.4"), 120).await.unwrap();
        assert_eq!(s.drones().await.unwrap()[0].version_since, Some(120));
        s.set_version("d1", Some("1.2.3"), 130).await.unwrap();
        let d = &s.drones().await.unwrap()[0];
        assert!(d.accepting && d.quarantined && !d.schedulable());
        assert_eq!(d.quarantine_reason.as_deref(), Some("bad disk"));
        assert_eq!(d.version.as_deref(), Some("1.2.3"));
        assert!(s.set_quarantine("d1", None).await.unwrap());
        let d = &s.drones().await.unwrap()[0];
        assert!(!d.quarantined && d.schedulable());
        assert_eq!(d.quarantine_reason, None);
    }

    #[tokio::test]
    async fn probation_caps_a_drone_at_one_lease_and_a_hold_makes_it_unschedulable() {
        use crate::health::{Action, Hold};
        let s = store_with("d1").await;
        let cap = crate::probe::Capacity {
            hostname: "h".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            kind: crate::probe::HostKind::Vm,
            vcpu: 8,
            mem_mib: 16384,
            free_disk_mib: None,
            docker_usable: true,
            mise_version: None,
            git_version: None,
        };
        let row = || async { s.drones().await.unwrap().remove(0) };
        assert!(row().await.schedulable());
        assert_eq!(row().await.hold(), Hold::None);

        let hold = Action::Quarantine {
            drone: "d1".into(),
            reason: "auto: test".into(),
            evidence: vec![],
            strikes: 2,
        };
        s.apply_health_action(&hold, 100).await.unwrap();
        let r = row().await;
        assert!(!r.schedulable() && r.held());
        assert_eq!(
            r.hold(),
            Hold::Auto {
                since: 100,
                strikes: 2
            }
        );

        s.apply_health_action(&Action::BeginProbation { drone: "d1".into() }, 200)
            .await
            .unwrap();
        let r = row().await;
        assert!(r.schedulable() && r.on_probation());
        assert_eq!(
            r.hold(),
            Hold::Probation {
                since: 200,
                strikes: 2
            }
        );
        assert_eq!(r.allocator_state(&cap, 0).max_leases, Some(1));

        s.apply_health_action(&Action::Release { drone: "d1".into() }, 300)
            .await
            .unwrap();
        let r = row().await;
        assert_eq!(
            (r.hold(), r.health_cleared_at, r.quarantine_strikes),
            (Hold::None, Some(300), 0)
        );
        assert_eq!(r.allocator_state(&cap, 0).max_leases, None);
        let log = s.health_log(10).await.unwrap();
        let events: Vec<&str> = log.iter().map(|e| e.event.as_str()).collect();
        assert_eq!(events, ["release", "probation_start", "auto_quarantine"]);
    }

    #[tokio::test]
    async fn heartbeat_round_trips_capacity() {
        let s = store_with("d1").await;
        let cap = Capacity {
            hostname: "h".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            kind: crate::probe::HostKind::Vm,
            vcpu: 4,
            mem_mib: 8192,
            free_disk_mib: Some(10),
            docker_usable: true,
            mise_version: None,
            git_version: None,
        };
        let stat = CacheStat {
            name: "cargo".into(),
            size_bytes: 5,
            max_bytes: Some(10),
            evicted_bytes: 1,
            last_gc_at: Some(100),
            hits: None,
            misses: None,
            size_status: crate::api::SizeStatus::Measured,
            counters: crate::api::Counters::NotTracked,
        };
        s.record_heartbeat(
            "d1",
            &cap,
            &HostCaps::default(),
            false,
            std::slice::from_ref(&stat),
            123,
        )
        .await
        .unwrap();
        s.set_extra_labels("d1", &["on-prem".to_owned()])
            .await
            .unwrap();
        assert_eq!(s.drones().await.unwrap()[0].trust, Trust::Shared);
        s.set_trust("d1", Trust::Deploy).await.unwrap();
        let d = &s.drones().await.unwrap()[0];
        assert_eq!(d.trust, Trust::Deploy);
        assert_eq!(d.allocator_state(&cap, 0).trust, Trust::Deploy);
        assert_eq!((d.last_heartbeat, d.accepting), (Some(123), false));
        assert_eq!(d.cache_stats, [stat]);
        assert_eq!(d.extra_labels, ["on-prem"]);
        assert_eq!(d.capacity.as_ref(), Some(&cap));
    }
}
