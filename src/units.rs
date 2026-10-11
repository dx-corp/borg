//! Work units: whole commands (gate and hook runs) that clients submit to the
//! queen and drones execute, with single-flight and receipts.
//!
//! A *unit* is `(repo, tree SHA, argv, platform, toolchain digest)`. Clients
//! (`borg run`, the pre-commit hook, `mq`) submit one with `POST /v1/units`:
//!
//! * **Single-flight.** The unit key is a SHA-256 over those fields. A
//!   submission whose key is already queued, leased or running *joins* that
//!   unit; one whose key has a successful (exit 0) result younger than
//!   `receipt_ttl_secs` returns that *receipt* without running anything. Only
//!   successes are reused, and only for the exact tree SHA, so a stale pass can
//!   never stand in for a different tree. `force` skips reuse.
//! * **Placement.** Queued units are placed by the same allocator that places
//!   runner leases (class capacity, owner caps, bids, learned profiles,
//!   quarantine and probation), on drones of the unit's platform, after CI
//!   runner demand has been served.
//! * **Execution** is the drone's: it is told about a unit in its heartbeat
//!   response (`units`), acknowledges it with `active_units`, appends log bytes
//!   with `POST /v1/units/{id}/log?offset=N`, and ends it with
//!   `POST /v1/units/{id}/result`. A unit that was never acknowledged is
//!   re-queued after `lease_ttl_secs`; one on a silent drone is re-queued (up to
//!   `max_attempts`); one over its timeout fails.
//! * **Logs** are stored in SQLite up to `max_log_bytes` and read by clients
//!   with `GET /v1/units/{id}/log?from=N&wait=S` (long poll).
//!
//! Clients authenticate with a client token (`borg queen enroll-client`);
//! drones with their drone token.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;

use crate::classes::Class;
use crate::store::{Store, StoreError};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UnitSettings {
    /// Off unless the queen opts in; the endpoints answer 404 otherwise.
    pub enabled: bool,
    /// A successful result is reused for this long.
    pub receipt_ttl_secs: i64,
    /// A placed unit the drone has not acknowledged within this is re-queued.
    pub lease_ttl_secs: i64,
    /// Time limit when the client gives none.
    pub default_timeout_secs: u64,
    pub max_timeout_secs: u64,
    /// Placement attempts before a unit fails.
    pub max_attempts: u32,
    /// Units queued or in flight, per client.
    pub max_queued_per_client: u32,
    pub max_log_bytes: u64,
    /// Largest git bundle a client may upload for a unit.
    pub max_bundle_bytes: u64,
    /// A unit announced with a bundle waits this long for it, then fails.
    pub bundle_wait_secs: i64,
    /// Finished units and their logs are deleted after this long.
    pub keep_secs: i64,
}

impl Default for UnitSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            receipt_ttl_secs: 7 * 86_400,
            lease_ttl_secs: 120,
            default_timeout_secs: 3600,
            max_timeout_secs: 4 * 3600,
            max_attempts: 3,
            max_queued_per_client: 200,
            max_log_bytes: 8 << 20,
            max_bundle_bytes: 64 << 20,
            bundle_wait_secs: 60,
            keep_secs: 7 * 86_400,
        }
    }
}

impl UnitSettings {
    pub fn validate(&self) -> Result<(), String> {
        if self.receipt_ttl_secs <= 0
            || self.lease_ttl_secs <= 0
            || self.keep_secs <= 0
            || self.default_timeout_secs == 0
            || self.max_timeout_secs < self.default_timeout_secs
            || self.max_attempts == 0
            || self.max_queued_per_client == 0
            || self.max_log_bytes == 0
            || self.max_bundle_bytes == 0
            || self.bundle_wait_secs <= 0
        {
            return Err(
                "units settings must be positive and max_timeout_secs >= default_timeout_secs"
                    .into(),
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitState {
    Queued,
    /// Placed on a drone, not yet acknowledged.
    Leased,
    Running,
    Done,
    /// Ended without a result: timed out, cancelled or out of attempts.
    Failed,
}

impl UnitState {
    pub const ALL: [Self; 5] = [
        Self::Queued,
        Self::Leased,
        Self::Running,
        Self::Done,
        Self::Failed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Leased => "leased",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    fn parse(s: &str) -> Result<Self, StoreError> {
        Self::ALL
            .into_iter()
            .find(|v| v.as_str() == s)
            .ok_or_else(|| StoreError::Corrupt(format!("unit state {s}")))
    }

    pub fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Leased | Self::Running)
    }
}

/// What a client submits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitUnit {
    pub repo: String,
    pub tree_sha: String,
    pub argv: Vec<String>,
    /// `<os>-<arch>`, e.g. `linux-x86_64`.
    pub platform: String,
    /// Digest of the toolchain the result depends on; part of the key.
    #[serde(default)]
    pub toolchain: String,
    #[serde(default = "default_class")]
    pub class: Class,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Run even if a receipt exists.
    #[serde(default)]
    pub force: bool,
    /// The Rust toolchain the command needs (from `rust-toolchain.toml` at the
    /// tree); the unit is only placed on a drone that has it.
    #[serde(default)]
    pub rust_toolchain: Option<String>,
    /// The client will upload a git bundle holding the tree's objects
    /// (`PUT /v1/units/{id}/bundle`); the unit is not placed until it arrives.
    #[serde(default)]
    pub bundle: bool,
}

fn default_class() -> Class {
    Class::Medium
}

impl SubmitUnit {
    pub fn validate(&self) -> Result<(), String> {
        let hex_ok =
            |s: &str| matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit());
        if self.repo.split('/').filter(|p| !p.is_empty()).count() != 2
            || self.repo.len() > 200
            || self.repo.contains(char::is_whitespace)
        {
            return Err("repo must look like owner/name".into());
        }
        if !hex_ok(&self.tree_sha) {
            return Err("tree_sha must be a 40 or 64 digit hex object id".into());
        }
        if self.argv.is_empty()
            || self.argv.len() > 64
            || self.argv.iter().any(|a| a.len() > 4096 || a.contains('\0'))
        {
            return Err("argv must have 1 to 64 arguments of at most 4096 bytes".into());
        }
        let plat_ok = !self.platform.is_empty()
            && self.platform.len() <= 40
            && self
                .platform
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
        if !plat_ok {
            return Err("platform must look like linux-x86_64".into());
        }
        let tc_ok = |t: &str| {
            !t.is_empty()
                && t.len() <= 64
                && t.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        };
        if self.rust_toolchain.as_deref().is_some_and(|t| !tc_ok(t)) {
            return Err("rust_toolchain must look like 1.95.0 or nightly-2026-01-01".into());
        }
        if self.toolchain.len() > 200 || self.toolchain.chars().any(char::is_control) {
            return Err("toolchain must be at most 200 printable characters".into());
        }
        Ok(())
    }

    /// The single-flight key. Length-prefixed fields, so no two different
    /// submissions can collide by concatenation.
    pub fn key(&self) -> String {
        let mut h = Sha256::new();
        let mut field = |s: &str| {
            h.update(u64::try_from(s.len()).unwrap_or(0).to_be_bytes());
            h.update(s.as_bytes());
        };
        field(&self.repo);
        field(&self.tree_sha.to_lowercase());
        field(&self.platform);
        field(&self.toolchain);
        field(self.rust_toolchain.as_deref().unwrap_or(""));
        for a in &self.argv {
            field(a);
        }
        hex::encode(h.finalize())
    }
}

/// A unit as stored and shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Unit {
    pub id: String,
    pub key: String,
    pub repo: String,
    pub tree_sha: String,
    pub argv: Vec<String>,
    pub platform: String,
    pub toolchain: String,
    pub rust_toolchain: Option<String>,
    pub class: Class,
    pub timeout_secs: u64,
    pub client: String,
    pub state: UnitState,
    pub drone: Option<String>,
    pub attempt: u32,
    pub created_at: i64,
    pub leased_at: Option<i64>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub exit_code: Option<i32>,
    pub duration_secs: Option<u64>,
    pub cancel_requested: bool,
    pub failure: Option<String>,
    pub log_bytes: u64,
    /// Waiting for the client's bundle upload; not placeable yet.
    pub awaiting_bundle: bool,
    /// Size of the uploaded bundle, 0 if none.
    pub bundle_bytes: u64,
}

/// The proof a unit succeeded for exactly this tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub unit_id: String,
    pub repo: String,
    pub tree_sha: String,
    pub argv: Vec<String>,
    pub platform: String,
    pub toolchain: String,
    pub drone: String,
    pub exit_code: i32,
    pub finished_at: i64,
    pub duration_secs: u64,
    /// SHA-256 over the fields above; lets a client check it was not altered in
    /// transit or in a log.
    pub digest: String,
}

impl Unit {
    pub fn receipt(&self) -> Option<Receipt> {
        if self.state != UnitState::Done || self.exit_code != Some(0) {
            return None;
        }
        let mut r = Receipt {
            unit_id: self.id.clone(),
            repo: self.repo.clone(),
            tree_sha: self.tree_sha.clone(),
            argv: self.argv.clone(),
            platform: self.platform.clone(),
            toolchain: self.toolchain.clone(),
            drone: self.drone.clone()?,
            exit_code: 0,
            finished_at: self.finished_at?,
            duration_secs: self.duration_secs.unwrap_or(0),
            digest: String::new(),
        };
        r.digest = r.compute_digest();
        Some(r)
    }
}

impl Receipt {
    pub fn compute_digest(&self) -> String {
        let mut h = Sha256::new();
        for s in [
            self.unit_id.as_str(),
            &self.repo,
            &self.tree_sha,
            &self.platform,
            &self.toolchain,
            &self.drone,
        ] {
            h.update(u64::try_from(s.len()).unwrap_or(0).to_be_bytes());
            h.update(s.as_bytes());
        }
        for a in &self.argv {
            h.update(u64::try_from(a.len()).unwrap_or(0).to_be_bytes());
            h.update(a.as_bytes());
        }
        h.update(self.exit_code.to_be_bytes());
        h.update(self.finished_at.to_be_bytes());
        h.update(self.duration_secs.to_be_bytes());
        hex::encode(h.finalize())
    }

    pub fn verifies(&self) -> bool {
        self.digest == self.compute_digest()
    }
}

/// What the drone is told to run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitGrant {
    pub unit_id: String,
    pub repo: String,
    pub tree_sha: String,
    pub argv: Vec<String>,
    pub class: Class,
    pub toolchain: String,
    pub timeout_secs: u64,
    pub attempt: u32,
    /// Size of the git bundle to fetch with `GET /v1/units/{id}/bundle` when
    /// the tree is not in the drone's mirror; 0 if there is none.
    #[serde(default)]
    pub bundle_bytes: u64,
}

/// The result of storing a bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleStored {
    Stored,
    /// No such unit, or it belongs to another client.
    NotFound,
    /// The unit has already been placed or ended.
    TooLate,
}

/// The outcome of a submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Submitted {
    /// A new unit was queued.
    New(Unit),
    /// An identical unit is already queued or running.
    Joined(Unit),
    /// An identical unit already succeeded recently.
    Receipt(Unit),
}

fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn unit_from_row(r: &sqlx::sqlite::SqliteRow) -> Result<Unit, StoreError> {
    let argv: String = r.get("argv");
    let class: String = r.get("class");
    Ok(Unit {
        id: r.get("id"),
        key: r.get("key"),
        repo: r.get("repo"),
        tree_sha: r.get("tree_sha"),
        argv: serde_json::from_str(&argv).map_err(|e| StoreError::Corrupt(e.to_string()))?,
        platform: r.get("platform"),
        toolchain: r.get("toolchain"),
        rust_toolchain: r.get("rust_toolchain"),
        class: serde_json::from_str(&format!("\"{class}\""))
            .map_err(|e| StoreError::Corrupt(e.to_string()))?,
        timeout_secs: u64::try_from(r.get::<i64, _>("timeout_secs")).unwrap_or(0),
        client: r.get("client"),
        state: UnitState::parse(&r.get::<String, _>("state"))?,
        drone: r.get("drone"),
        attempt: u32::try_from(r.get::<i64, _>("attempt")).unwrap_or(0),
        created_at: r.get("created_at"),
        leased_at: r.get("leased_at"),
        started_at: r.get("started_at"),
        finished_at: r.get("finished_at"),
        exit_code: r
            .get::<Option<i64>, _>("exit_code")
            .and_then(|v| i32::try_from(v).ok()),
        duration_secs: r
            .get::<Option<i64>, _>("duration_secs")
            .map(|v| u64::try_from(v).unwrap_or(0)),
        cancel_requested: r.get::<i64, _>("cancel_requested") != 0,
        failure: r.get("failure"),
        log_bytes: u64::try_from(r.get::<i64, _>("log_bytes")).unwrap_or(0),
        awaiting_bundle: r.get::<i64, _>("awaiting_bundle") != 0,
        bundle_bytes: u64::try_from(r.get::<i64, _>("bundle_bytes")).unwrap_or(0),
    })
}

const ACTIVE: &str = "('queued', 'leased', 'running')";

impl Store {
    pub async fn enroll_client(
        &self,
        name: &str,
        token_hash: &str,
        now: i64,
    ) -> Result<(), StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let r = sqlx::query("INSERT INTO clients (name, token_hash, created_at) VALUES (?, ?, ?)")
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

    pub async fn client_for_token_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<String>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let row = sqlx::query("SELECT name FROM clients WHERE token_hash = ?")
            .bind(token_hash)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get("name")))
    }

    pub async fn unit(&self, id: &str) -> Result<Option<Unit>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let row = sqlx::query("SELECT * FROM units WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(unit_from_row).transpose()
    }

    /// Units in `states`, oldest first.
    pub async fn units_in(&self, states: &[UnitState]) -> Result<Vec<Unit>, StoreError> {
        let list = states
            .iter()
            .map(|s| format!("'{}'", s.as_str()))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("SELECT * FROM units WHERE state IN ({list}) ORDER BY created_at, id");
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let rows = sqlx::query(&sql).fetch_all(&self.pool).await?;
        rows.iter().map(unit_from_row).collect()
    }

    pub async fn recent_units(&self, limit: i64) -> Result<Vec<Unit>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let rows = sqlx::query("SELECT * FROM units ORDER BY created_at DESC, id DESC LIMIT ?")
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(unit_from_row).collect()
    }

    /// Submit with single-flight. The caller has validated `s`.
    pub async fn submit_unit(
        &self,
        s: &SubmitUnit,
        client: &str,
        id: &str,
        timeout_secs: u64,
        receipt_ttl: i64,
        now: i64,
    ) -> Result<Submitted, StoreError> {
        let key = s.key();
        if let Some(existing) = self.find_reusable(&key, s.force, receipt_ttl, now).await? {
            return Ok(existing);
        }
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let inserted = sqlx::query("INSERT INTO units (id, key, repo, tree_sha, argv, platform, toolchain, rust_toolchain, class, timeout_secs, client, state, attempt, created_at, awaiting_bundle) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'queued', 0, ?, ?)")
            .bind(id)
            .bind(&key)
            .bind(&s.repo)
            .bind(s.tree_sha.to_lowercase())
            .bind(serde_json::to_string(&s.argv).map_err(|e| StoreError::Corrupt(e.to_string()))?)
            .bind(&s.platform)
            .bind(&s.toolchain)
            .bind(s.rust_toolchain.as_deref())
            .bind(s.class.as_str())
            .bind(to_i64(timeout_secs))
            .bind(client)
            .bind(now)
            .bind(i64::from(s.bundle))
            .execute(&self.pool)
            .await;
        match inserted {
            Ok(_) => {}
            // Another submission won the race for the same key: join it.
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                if let Some(existing) = self.find_reusable(&key, s.force, receipt_ttl, now).await? {
                    return Ok(existing);
                }
            }
            Err(e) => return Err(e.into()),
        }
        let unit = self
            .unit(id)
            .await?
            .ok_or_else(|| StoreError::Corrupt("unit vanished".into()))?;
        Ok(Submitted::New(unit))
    }

    /// An identical unit that is running or succeeded recently, if any (what a
    /// submission would join or reuse).
    pub async fn find_reusable_unit(
        &self,
        s: &SubmitUnit,
        receipt_ttl: i64,
        now: i64,
    ) -> Result<Option<Submitted>, StoreError> {
        self.find_reusable(&s.key(), s.force, receipt_ttl, now)
            .await
    }

    async fn find_reusable(
        &self,
        key: &str,
        force: bool,
        receipt_ttl: i64,
        now: i64,
    ) -> Result<Option<Submitted>, StoreError> {
        let sql = format!("SELECT * FROM units WHERE key = ? AND state IN {ACTIVE} LIMIT 1");
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let active = sqlx::query(&sql)
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        if let Some(r) = active {
            return Ok(Some(Submitted::Joined(unit_from_row(&r)?)));
        }
        if force {
            return Ok(None);
        }
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let done = sqlx::query("SELECT * FROM units WHERE key = ? AND state = 'done' AND exit_code = 0 AND finished_at >= ? ORDER BY finished_at DESC LIMIT 1")
            .bind(key)
            .bind(now - receipt_ttl)
            .fetch_optional(&self.pool)
            .await?;
        done.as_ref()
            .map(|r| unit_from_row(r).map(Submitted::Receipt))
            .transpose()
    }

    /// Number of a client's units that are queued or in flight.
    pub async fn client_active_units(&self, client: &str) -> Result<u32, StoreError> {
        let sql = format!("SELECT COUNT(*) AS n FROM units WHERE client = ? AND state IN {ACTIVE}");
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let row = sqlx::query(&sql).bind(client).fetch_one(&self.pool).await?;
        Ok(u32::try_from(row.get::<i64, _>("n")).unwrap_or(u32::MAX))
    }

    /// Place a queued unit on a drone.
    pub async fn lease_unit(&self, id: &str, drone: &str, now: i64) -> Result<bool, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let r = sqlx::query("UPDATE units SET state = 'leased', drone = ?, leased_at = ?, attempt = attempt + 1 WHERE id = ? AND state = 'queued'")
            .bind(drone)
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() > 0)
    }

    /// Units placed on `drone` that it has not acknowledged yet.
    pub async fn unacked_units(&self, drone: &str) -> Result<Vec<Unit>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let rows = sqlx::query("SELECT * FROM units WHERE drone = ? AND state = 'leased' AND cancel_requested = 0 ORDER BY created_at, id")
            .bind(drone)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(unit_from_row).collect()
    }

    /// The drone acknowledges units; leased ones start running.
    pub async fn ack_units(&self, drone: &str, ids: &[String], now: i64) -> Result<(), StoreError> {
        for id in ids {
            // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
            sqlx::query("UPDATE units SET state = 'running', started_at = ? WHERE id = ? AND drone = ? AND state = 'leased'")
                .bind(now)
                .bind(id)
                .bind(drone)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    /// Ids on `drone` that a client asked to cancel while queued, leased or
    /// running.
    pub async fn cancel_requests(&self, drone: &str) -> Result<Vec<String>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let rows = sqlx::query("SELECT id FROM units WHERE drone = ? AND cancel_requested = 1 AND state IN ('leased', 'running') ORDER BY id")
            .bind(drone)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|r| r.get("id")).collect())
    }

    /// Client cancel: queued and leased-but-unstarted units end at once;
    /// running ones are flagged for the drone. False if not found or already
    /// finished.
    pub async fn cancel_unit(&self, id: &str, now: i64) -> Result<bool, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let ended = sqlx::query("UPDATE units SET state = 'failed', failure = 'cancelled', finished_at = ?, cancel_requested = 1 WHERE id = ? AND state IN ('queued', 'leased')")
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await?;
        if ended.rows_affected() > 0 {
            self.delete_unit_bundle(id).await?;
            return Ok(true);
        }
        const FLAG: &str =
            "UPDATE units SET cancel_requested = 1 WHERE id = ? AND state = 'running'";
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let flagged = sqlx::query(FLAG).bind(id).execute(&self.pool).await?;
        Ok(flagged.rows_affected() > 0)
    }

    /// The drone reports a unit's end. Idempotent; false if the unit is not
    /// this drone's or already finished with a different outcome.
    pub async fn finish_unit(
        &self,
        id: &str,
        drone: &str,
        exit_code: i32,
        duration_secs: u64,
        now: i64,
    ) -> Result<bool, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let owned = sqlx::query("SELECT state FROM units WHERE id = ? AND drone = ?")
            .bind(id)
            .bind(drone)
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = owned else {
            return Ok(false);
        };
        let state: String = row.get("state");
        if state == "done" {
            return Ok(true);
        }
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query("UPDATE units SET state = 'done', exit_code = ?, duration_secs = ?, finished_at = ? WHERE id = ? AND drone = ? AND state IN ('leased', 'running', 'failed')")
            .bind(i64::from(exit_code))
            .bind(to_i64(duration_secs))
            .bind(now)
            .bind(id)
            .bind(drone)
            .execute(&self.pool)
            .await?;
        Ok(true)
    }

    /// Running units on `drone` that its heartbeat no longer lists (started
    /// before `cutoff`) are lost: re-queue them, or fail them once out of
    /// attempts. Returns how many were affected.
    pub async fn requeue_lost_units(
        &self,
        drone: &str,
        active: &[String],
        cutoff: i64,
        max_attempts: u32,
        now: i64,
    ) -> Result<u32, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let rows = sqlx::query("SELECT id, attempt FROM units WHERE drone = ? AND state = 'running' AND started_at < ?")
            .bind(drone)
            .bind(cutoff)
            .fetch_all(&self.pool)
            .await?;
        let mut n = 0;
        for r in rows {
            let id: String = r.get("id");
            if active.contains(&id) {
                continue;
            }
            let attempt = u32::try_from(r.get::<i64, _>("attempt")).unwrap_or(0);
            self.retry_or_fail(&id, attempt, max_attempts, "drone lost the unit", now)
                .await?;
            n += 1;
        }
        Ok(n)
    }

    /// Re-queue a unit, or fail it if it has used all its attempts.
    pub async fn retry_or_fail(
        &self,
        id: &str,
        attempt: u32,
        max_attempts: u32,
        why: &str,
        now: i64,
    ) -> Result<(), StoreError> {
        if attempt >= max_attempts {
            self.fail_unit(id, &format!("{why}; out of attempts"), now)
                .await
        } else {
            self.requeue_unit(id).await
        }
    }

    /// Put a unit back in the queue (its drone vanished or never started).
    pub async fn requeue_unit(&self, id: &str) -> Result<(), StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query("UPDATE units SET state = 'queued', drone = NULL, leased_at = NULL, started_at = NULL WHERE id = ? AND state IN ('leased', 'running')")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn fail_unit(&self, id: &str, why: &str, now: i64) -> Result<(), StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query("UPDATE units SET state = 'failed', failure = ?, finished_at = ? WHERE id = ? AND state IN ('queued', 'leased', 'running')")
            .bind(why)
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await?;
        self.delete_unit_bundle(id).await
    }

    /// Store the client's bundle for a unit that is still waiting to be
    /// placed. The bundle may replace an earlier upload.
    pub async fn put_unit_bundle(
        &self,
        id: &str,
        client: &str,
        data: &[u8],
        now: i64,
    ) -> Result<BundleStored, StoreError> {
        let Some(unit) = self.unit(id).await? else {
            return Ok(BundleStored::NotFound);
        };
        if unit.client != client {
            return Ok(BundleStored::NotFound);
        }
        if unit.state != UnitState::Queued {
            return Ok(BundleStored::TooLate);
        }
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query(
            "INSERT OR REPLACE INTO unit_bundles (unit_id, data, created_at) VALUES (?, ?, ?)",
        )
        .bind(id)
        .bind(data)
        .bind(now)
        .execute(&self.pool)
        .await?;
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query("UPDATE units SET awaiting_bundle = 0, bundle_bytes = ? WHERE id = ?")
            .bind(to_i64(u64::try_from(data.len()).unwrap_or(0)))
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(BundleStored::Stored)
    }

    pub async fn unit_bundle(&self, id: &str) -> Result<Option<Vec<u8>>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let row = sqlx::query("SELECT data FROM unit_bundles WHERE unit_id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get("data")))
    }

    pub async fn delete_unit_bundle(&self, id: &str) -> Result<(), StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query("DELETE FROM unit_bundles WHERE unit_id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Append drone log bytes at `offset`. Idempotent for a repeated chunk;
    /// an offset beyond the stored end (a gap) is rejected so the log never
    /// has holes. Returns the new length, or `None` for a gap.
    pub async fn append_unit_log(
        &self,
        id: &str,
        offset: u64,
        data: &[u8],
        max_bytes: u64,
    ) -> Result<Option<u64>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let row = sqlx::query("SELECT log_bytes FROM units WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let have = u64::try_from(row.get::<i64, _>("log_bytes")).unwrap_or(0);
        if offset > have {
            return Ok(None);
        }
        let skip = usize::try_from(have - offset)
            .unwrap_or(usize::MAX)
            .min(data.len());
        let fresh = &data[skip..];
        let room = usize::try_from(max_bytes.saturating_sub(have)).unwrap_or(usize::MAX);
        let fresh = &fresh[..fresh.len().min(room)];
        if fresh.is_empty() {
            return Ok(Some(have));
        }
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query("INSERT OR IGNORE INTO unit_logs (unit_id, offset, data) VALUES (?, ?, ?)")
            .bind(id)
            .bind(to_i64(have))
            .bind(fresh)
            .execute(&self.pool)
            .await?;
        let new_len = have + u64::try_from(fresh.len()).unwrap_or(0);
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query("UPDATE units SET log_bytes = ? WHERE id = ?")
            .bind(to_i64(new_len))
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(Some(new_len))
    }

    /// Log bytes from `from`, at most `limit`.
    pub async fn read_unit_log(
        &self,
        id: &str,
        from: u64,
        limit: usize,
    ) -> Result<Vec<u8>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let rows = sqlx::query("SELECT offset, data FROM unit_logs WHERE unit_id = ? AND offset + length(data) > ? ORDER BY offset")
            .bind(id)
            .bind(to_i64(from))
            .fetch_all(&self.pool)
            .await?;
        let mut out = Vec::new();
        for r in rows {
            let off = u64::try_from(r.get::<i64, _>("offset")).unwrap_or(0);
            let data: Vec<u8> = r.get("data");
            let skip = usize::try_from(from.saturating_sub(off))
                .unwrap_or(usize::MAX)
                .min(data.len());
            out.extend_from_slice(&data[skip..]);
            if out.len() >= limit {
                out.truncate(limit);
                break;
            }
        }
        Ok(out)
    }

    /// Delete finished units (and their logs) older than `before`.
    pub async fn prune_units(&self, before: i64) -> Result<u64, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query("DELETE FROM unit_bundles WHERE unit_id IN (SELECT id FROM units WHERE state IN ('done', 'failed') AND finished_at < ?)")
            .bind(before)
            .execute(&self.pool)
            .await?;
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        sqlx::query("DELETE FROM unit_logs WHERE unit_id IN (SELECT id FROM units WHERE state IN ('done', 'failed') AND finished_at < ?)")
            .bind(before)
            .execute(&self.pool)
            .await?;
        const DROP: &str =
            "DELETE FROM units WHERE state IN ('done', 'failed') AND finished_at < ?";
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let r = sqlx::query(DROP).bind(before).execute(&self.pool).await?;
        Ok(r.rows_affected())
    }

    /// Counts by state.
    pub async fn unit_counts(&self) -> Result<BTreeMap<&'static str, u32>, StoreError> {
        // sqlx-runtime-query: standalone-buildable SQLite store; the query! macros need a build-time database
        let rows = sqlx::query("SELECT state, COUNT(*) AS n FROM units GROUP BY state")
            .fetch_all(&self.pool)
            .await?;
        let mut out: BTreeMap<&'static str, u32> =
            UnitState::ALL.iter().map(|s| (s.as_str(), 0)).collect();
        for r in rows {
            let s = UnitState::parse(&r.get::<String, _>("state"))?;
            out.insert(
                s.as_str(),
                u32::try_from(r.get::<i64, _>("n")).unwrap_or(u32::MAX),
            );
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn submit() -> SubmitUnit {
        SubmitUnit {
            repo: "o/r".into(),
            tree_sha: "a".repeat(40),
            argv: vec!["cargo".into(), "check".into()],
            platform: "linux-x86_64".into(),
            toolchain: "rust-1.95".into(),
            class: Class::Medium,
            timeout_secs: None,
            force: false,
            rust_toolchain: None,
            bundle: false,
        }
    }

    #[test]
    fn the_key_covers_every_field_and_cannot_collide_by_concatenation() {
        let base = submit();
        let k = base.key();
        assert_eq!(k, submit().key());
        for changed in [
            SubmitUnit {
                repo: "o/x".into(),
                ..submit()
            },
            SubmitUnit {
                tree_sha: "b".repeat(40),
                ..submit()
            },
            SubmitUnit {
                platform: "macos-aarch64".into(),
                ..submit()
            },
            SubmitUnit {
                toolchain: "rust-1.96".into(),
                ..submit()
            },
            SubmitUnit {
                argv: vec!["cargo".into(), "test".into()],
                ..submit()
            },
            SubmitUnit {
                argv: vec!["cargo check".into()],
                ..submit()
            },
            SubmitUnit {
                argv: vec!["cargoc".into(), "heck".into()],
                ..submit()
            },
        ] {
            assert_ne!(changed.key(), k, "{changed:?}");
        }
        // Case of the object id is not part of the identity; class and force are not either.
        let upper = SubmitUnit {
            tree_sha: "A".repeat(40),
            ..submit()
        };
        assert_eq!(upper.key(), k);
        assert_eq!(
            SubmitUnit {
                class: Class::Heavy,
                force: true,
                ..submit()
            }
            .key(),
            k
        );
    }

    #[test]
    fn submissions_are_validated() {
        assert!(submit().validate().is_ok());
        for bad in [
            SubmitUnit {
                repo: "nope".into(),
                ..submit()
            },
            SubmitUnit {
                tree_sha: "xyz".into(),
                ..submit()
            },
            SubmitUnit {
                argv: vec![],
                ..submit()
            },
            SubmitUnit {
                argv: vec!["a\0b".into()],
                ..submit()
            },
            SubmitUnit {
                platform: "Linux X".into(),
                ..submit()
            },
            SubmitUnit {
                toolchain: "x\n".into(),
                ..submit()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }

    async fn store() -> Store {
        Store::open_in_memory().await.unwrap()
    }

    #[tokio::test]
    async fn single_flight_joins_running_units_and_reuses_only_fresh_successes() {
        let s = store().await;
        let first = s
            .submit_unit(&submit(), "c", "u1", 60, 1000, 100)
            .await
            .unwrap();
        assert!(matches!(first, Submitted::New(_)));
        // Queued, then running: still the same unit.
        let again = s
            .submit_unit(&submit(), "c2", "u2", 60, 1000, 101)
            .await
            .unwrap();
        let Submitted::Joined(u) = again else {
            panic!("{again:?}")
        };
        assert_eq!(u.id, "u1");

        s.enroll("d1", "h", 0).await.unwrap();
        assert!(s.lease_unit("u1", "d1", 102).await.unwrap());
        s.ack_units("d1", &["u1".into()], 103).await.unwrap();
        assert!(matches!(
            s.submit_unit(&submit(), "c", "u3", 60, 1000, 104)
                .await
                .unwrap(),
            Submitted::Joined(_)
        ));

        // A failure is not reused: a new unit runs.
        s.finish_unit("u1", "d1", 1, 5, 110).await.unwrap();
        let after_fail = s
            .submit_unit(&submit(), "c", "u4", 60, 1000, 111)
            .await
            .unwrap();
        assert!(matches!(after_fail, Submitted::New(_)), "{after_fail:?}");

        // A success is reused until its ttl ends, never with force.
        s.lease_unit("u4", "d1", 112).await.unwrap();
        s.finish_unit("u4", "d1", 0, 7, 120).await.unwrap();
        let hit = s
            .submit_unit(&submit(), "c", "u5", 60, 1000, 500)
            .await
            .unwrap();
        let Submitted::Receipt(u) = hit else {
            panic!("{hit:?}")
        };
        let r = u.receipt().unwrap();
        assert!(r.verifies());
        assert_eq!(
            (r.tree_sha.as_str(), r.exit_code, r.drone.as_str()),
            (&*"a".repeat(40), 0, "d1")
        );
        let forced = SubmitUnit {
            force: true,
            ..submit()
        };
        assert!(matches!(
            s.submit_unit(&forced, "c", "u6", 60, 1000, 500)
                .await
                .unwrap(),
            Submitted::New(_)
        ));
        // Expired receipt: a new unit. (u6 is active, so first end it.)
        s.cancel_unit("u6", 501).await.unwrap();
        let late = s
            .submit_unit(&submit(), "c", "u7", 60, 1000, 5000)
            .await
            .unwrap();
        assert!(matches!(late, Submitted::New(_)));
        // A different tree never matches.
        let other = SubmitUnit {
            tree_sha: "c".repeat(40),
            ..submit()
        };
        assert!(matches!(
            s.submit_unit(&other, "c", "u8", 60, 1000, 130)
                .await
                .unwrap(),
            Submitted::New(_)
        ));
    }

    #[tokio::test]
    async fn receipts_detect_tampering() {
        let s = store().await;
        s.enroll("d1", "h", 0).await.unwrap();
        s.submit_unit(&submit(), "c", "u1", 60, 1000, 100)
            .await
            .unwrap();
        s.lease_unit("u1", "d1", 101).await.unwrap();
        s.finish_unit("u1", "d1", 0, 3, 110).await.unwrap();
        let mut r = s.unit("u1").await.unwrap().unwrap().receipt().unwrap();
        assert!(r.verifies());
        r.tree_sha = "b".repeat(40);
        assert!(!r.verifies());
        // Nonzero exits have no receipt.
        s.submit_unit(
            &SubmitUnit {
                force: true,
                ..submit()
            },
            "c",
            "u2",
            60,
            1000,
            120,
        )
        .await
        .unwrap();
        s.lease_unit("u2", "d1", 121).await.unwrap();
        s.finish_unit("u2", "d1", 2, 3, 130).await.unwrap();
        assert!(s.unit("u2").await.unwrap().unwrap().receipt().is_none());
    }

    #[tokio::test]
    async fn logs_append_idempotently_without_gaps_and_read_from_any_offset() {
        let s = store().await;
        s.submit_unit(&submit(), "c", "u1", 60, 1000, 100)
            .await
            .unwrap();
        assert_eq!(
            s.append_unit_log("u1", 0, b"hello ", 100).await.unwrap(),
            Some(6)
        );
        // The same chunk again, and an overlapping one.
        assert_eq!(
            s.append_unit_log("u1", 0, b"hello ", 100).await.unwrap(),
            Some(6)
        );
        assert_eq!(
            s.append_unit_log("u1", 3, b"lo world", 100).await.unwrap(),
            Some(11)
        );
        // A gap is refused.
        assert_eq!(s.append_unit_log("u1", 50, b"x", 100).await.unwrap(), None);
        assert_eq!(
            s.read_unit_log("u1", 0, 1000).await.unwrap(),
            b"hello world"
        );
        assert_eq!(s.read_unit_log("u1", 4, 1000).await.unwrap(), b"o world");
        assert_eq!(s.read_unit_log("u1", 4, 3).await.unwrap(), b"o w");
        assert!(s.read_unit_log("u1", 11, 10).await.unwrap().is_empty());
        // The cap truncates.
        assert_eq!(
            s.append_unit_log("u1", 11, b"0123456789", 15)
                .await
                .unwrap(),
            Some(15)
        );
        assert_eq!(s.append_unit_log("nope", 0, b"x", 15).await.unwrap(), None);
    }

    #[tokio::test]
    async fn cancel_requeue_fail_and_prune() {
        let s = store().await;
        s.enroll("d1", "h", 0).await.unwrap();
        s.submit_unit(&submit(), "c", "u1", 60, 1000, 100)
            .await
            .unwrap();
        assert!(s.cancel_unit("u1", 101).await.unwrap());
        let u = s.unit("u1").await.unwrap().unwrap();
        assert_eq!(
            (u.state, u.failure.as_deref()),
            (UnitState::Failed, Some("cancelled"))
        );
        assert!(!s.cancel_unit("u1", 102).await.unwrap());

        s.submit_unit(&submit(), "c", "u2", 60, 1000, 110)
            .await
            .unwrap();
        s.lease_unit("u2", "d1", 111).await.unwrap();
        s.requeue_unit("u2").await.unwrap();
        let u = s.unit("u2").await.unwrap().unwrap();
        assert_eq!((u.state, u.drone, u.attempt), (UnitState::Queued, None, 1));
        s.lease_unit("u2", "d1", 112).await.unwrap();
        s.ack_units("d1", &["u2".into()], 113).await.unwrap();
        assert!(s.cancel_unit("u2", 114).await.unwrap());
        assert_eq!(s.cancel_requests("d1").await.unwrap(), ["u2"]);
        s.fail_unit("u2", "timed out", 200).await.unwrap();
        assert_eq!(s.client_active_units("c").await.unwrap(), 0);
        assert_eq!(s.prune_units(150).await.unwrap(), 1);
        assert_eq!(s.prune_units(300).await.unwrap(), 1);
        assert!(s.unit_counts().await.unwrap().values().all(|&n| n == 0));
    }

    #[tokio::test]
    async fn clients_enroll_once() {
        let s = store().await;
        s.enroll_client("agent", "hash", 1).await.unwrap();
        assert!(matches!(
            s.enroll_client("agent", "other", 2).await,
            Err(StoreError::AlreadyEnrolled(_))
        ));
        assert_eq!(
            s.client_for_token_hash("hash").await.unwrap().as_deref(),
            Some("agent")
        );
        assert!(s.client_for_token_hash("nope").await.unwrap().is_none());
    }
}
