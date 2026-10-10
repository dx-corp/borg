//! SQLite bookkeeping: enrolled drones and lease history.
//!
//! The file is small and disposable in the sense that the queen re-derives
//! truth from GitHub plus drone heartbeats after a restart. Enrollment hashes
//! are the one thing worth backing up.

use std::path::Path;
use std::str::FromStr;

use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use crate::allocator::HostCaps;
use crate::api::CacheStat;
use crate::classes::{Class, Scope};
use crate::probe::Capacity;
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
    cache_stats    TEXT
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
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done => "done",
            Self::Expired => "expired",
        }
    }

    fn parse(s: &str) -> Result<Self, StoreError> {
        Ok(match s {
            "pending" => Self::Pending,
            "running" => Self::Running,
            "done" => Self::Done,
            "expired" => Self::Expired,
            other => return Err(StoreError::Corrupt(format!("lease state {other}"))),
        })
    }

    pub fn is_active(self) -> bool {
        matches!(self, Self::Pending | Self::Running)
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

#[derive(Debug, Clone)]
pub struct LeaseResult {
    pub exit_code: i32,
    pub duration_secs: u64,
    pub peak_rss_mib: Option<u64>,
}

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
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
    pub async fn open(path: &Path) -> Result<Self, StoreError> {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        Self::connect(opts, 4).await
    }

    pub async fn open_in_memory() -> Result<Self, StoreError> {
        // One connection: every pooled in-memory connection is its own database.
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")?;
        Self::connect(opts, 1).await
    }

    async fn connect(opts: SqliteConnectOptions, max: u32) -> Result<Self, StoreError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(max)
            .connect_with(opts)
            .await?;
        sqlx::raw_sql(SCHEMA).execute(&pool).await?;
        // Databases created before cache stats existed lack the column.
        // sqlx-runtime-query: one-off additive migration, no data involved
        if let Err(e) = sqlx::query("ALTER TABLE drones ADD COLUMN cache_stats TEXT")
            .execute(&pool)
            .await
            && !e.to_string().contains("duplicate column")
        {
            return Err(e.into());
        }
        Ok(Self { pool })
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

    pub async fn drones(&self) -> Result<Vec<DroneRow>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let rows = sqlx::query(
            "SELECT name, last_heartbeat, capacity, caps, accepting, cache_stats FROM drones ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                let capacity: Option<String> = r.get("capacity");
                let caps: Option<String> = r.get("caps");
                let stats: Option<String> = r.get("cache_stats");
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
                })
            })
            .collect()
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

    fn lease_from_row(r: &sqlx::sqlite::SqliteRow) -> Result<LeaseRow, StoreError> {
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
            "SELECT * FROM leases WHERE state IN ('pending', 'running') ORDER BY created_at, id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::lease_from_row).collect()
    }

    pub async fn lease(&self, id: &str) -> Result<Option<LeaseRow>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let row = sqlx::query("SELECT * FROM leases WHERE id = ?")
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
             WHERE id = ? AND state IN ('pending', 'running')",
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
             WHERE id = ? AND state IN ('pending', 'running', 'expired')",
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
            "SELECT * FROM leases WHERE drone = ? AND state = 'pending' AND jit_config IS NOT NULL ORDER BY created_at, id",
        )
        .bind(drone)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::lease_from_row).collect()
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
        let d = &s.drones().await.unwrap()[0];
        assert_eq!((d.last_heartbeat, d.accepting), (Some(123), false));
        assert_eq!(d.cache_stats, [stat]);
        assert_eq!(d.capacity.as_ref(), Some(&cap));
    }
}
