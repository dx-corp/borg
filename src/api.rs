//! HTTP surface of the queen.
//!
//! * `GET /status`, `GET /metrics`, `GET /report`, `GET /jobs/{job_id}` and
//!   `GET /runs/{run_id}` are read-only and unauthenticated; bind them to a
//!   private interface.
//! * `POST /v1/heartbeat` and `POST /v1/leases/{id}/result` are the drone API,
//!   authenticated with `Authorization: Bearer <enrollment token>`.
//!
//! The queen speaks plain HTTP; terminate TLS in front of it.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::allocator::{
    ActiveLease, AdmissionState, HostCaps, LeaseState, Reservation, Unmet, free_room, free_slots,
    max_slots,
};
use crate::bid::{ClassPressure, DroneBid};
use crate::classes::{Class, Scope, Trust};
use crate::enroll::hash_token;
use crate::envcontract::{CheckState, EnvFacts, Need, Verdict, blocked_classes, evaluate};
use crate::health::{DroneHealth, HealthState};
use crate::homeostasis::KeyReport;
use crate::hostdiag::{JobHostView, LeaseHostReport};
use crate::probe::Capacity;
use crate::queen::RolloutView;
use crate::queen::{Queen, QueenError, QueueEntry};
use crate::reclaim::ReclaimStats;
use crate::rollout::Phase as RolloutPhase;
use crate::secret::Secret;
use crate::store::{LeaseResult, LeaseStatus, StoreError};
use crate::tailnet::drone_name_for;
use crate::telemetry::{JobTelemetry, MAX_REPORT_ROWS, Report, build_report};
use crate::units::UnitGrant;

#[derive(Debug, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    pub capacity: Capacity,
    #[serde(default)]
    pub caps: HostCaps,
    /// False while the drone drains or yields to its owner.
    #[serde(default = "yes")]
    pub accepting: bool,
    /// Lease ids the drone currently holds; acknowledges their JIT configs.
    #[serde(default)]
    pub active_leases: Vec<String>,
    /// Size and garbage-collection state of the drone's host caches.
    #[serde(default)]
    pub cache_stats: Vec<CacheStat>,
    /// Labels the drone asks to have added to its runners.
    #[serde(default)]
    pub extra_labels: Vec<String>,
    /// `shared` (default), `deploy` or `deploy-only`; see [`Trust`].
    #[serde(default)]
    pub trust: Trust,
    /// Standby lease ids the drone holds unstarted; acknowledges their JIT
    /// configs. Leases the drone started are in `active_leases` instead.
    #[serde(default)]
    pub held_standby: Vec<String>,
    /// Outcome of an update this drone just ran. The queen may use it as
    /// rollout evidence or ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_report: Option<UpdateReport>,
    /// The borg version the drone runs (reported for rollout tracking).
    #[serde(default)]
    pub version: Option<String>,
    /// One bid per `(class, scope)` the drone is willing to serve; see
    /// [`crate::bid`]. Empty for drones that do not bid.
    #[serde(default)]
    pub bids: Vec<DroneBid>,
    /// Build-box reclaim stats and new audit entries; see [`crate::reclaim`].
    #[serde(default)]
    pub reclaim: Option<ReclaimStats>,
    /// Work units the drone currently holds; acknowledges new ones.
    #[serde(default)]
    pub active_units: Vec<String>,
    /// Facts about the drone's build environment; see [`crate::envcontract`].
    #[serde(default)]
    pub env_facts: Option<EnvFacts>,
}

/// Bound the free text a drone reports; the observation time cannot be in the
/// future.
fn sanitize_facts(f: &EnvFacts, now: i64) -> EnvFacts {
    let clip = |s: &String| {
        s.chars()
            .filter(|c| !c.is_control())
            .take(300)
            .collect::<String>()
    };
    let mut out = f.clone();
    out.observed_at = out.observed_at.min(now);
    out.cpu_busy_pm = out.cpu_busy_pm.map(|b| b.min(1000));
    for fact in out.capabilities.values_mut() {
        fact.observed_at = fact.observed_at.min(now);
    }
    out.rustc_wrapper = out.rustc_wrapper.as_ref().map(clip);
    out.sccache_dir = out.sccache_dir.as_ref().map(clip);
    if let Some(list) = &mut out.toolchains {
        list.truncate(32);
        for t in list {
            t.channel = clip(&t.channel);
            t.rustc_version = clip(&t.rustc_version);
        }
    }
    // Only probe-list names, with bounded printable versions.
    out.tools = out.tools.as_ref().map(|m| {
        m.iter()
            .filter(|(k, _)| crate::envcontract::PROBE_TOOLS.contains(&k.as_str()))
            .map(|(k, v)| {
                (
                    k.clone(),
                    v.chars().filter(|c| !c.is_control()).take(80).collect(),
                )
            })
            .collect()
    });
    out
}

/// At most one bid per key (the first), clamped, at most one per key overall.
fn sanitize_bids(bids: &[DroneBid]) -> Vec<DroneBid> {
    let mut out: Vec<DroneBid> = Vec::new();
    for b in bids {
        if !out.iter().any(|o| o.class == b.class && o.scope == b.scope) {
            out.push(b.clamped());
        }
    }
    out
}

/// A drone-reported version is shown in metric labels, so accept only short
/// printable ASCII.
fn sanitize_version(v: Option<&str>) -> Option<&str> {
    v.filter(|v| !v.is_empty() && v.len() <= 64 && v.bytes().all(|b| b.is_ascii_graphic()))
}

/// One host cache as measured by its drone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheStat {
    pub name: String,
    pub size_bytes: u64,
    /// The cap garbage collection enforces (sccache: its own cache size).
    pub max_bytes: Option<u64>,
    pub evicted_bytes: u64,
    pub last_gc_at: Option<i64>,
    /// Git: leases that were handed a mirror / ran without one. sccache:
    /// the host server's totals (`caches.sccache.server`). See `counters`.
    pub hits: Option<u64>,
    pub misses: Option<u64>,
    /// Whether `size_bytes` covers the whole cache.
    #[serde(default)]
    pub size_status: SizeStatus,
    /// Whether `hits` and `misses` are real counts.
    #[serde(default)]
    pub counters: Counters,
}

/// How complete a cache's `size_bytes` is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SizeStatus {
    /// Every directory was listed.
    Measured,
    /// Some directories could not be listed by the drone's user (for
    /// example the job user's 0750 sccache directories): a lower bound.
    Partial,
    /// The drone did not say (a drone older than this field).
    #[default]
    Unknown,
}

/// What a cache's `hits` and `misses` mean.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Counters {
    /// Real counts.
    Reported,
    /// Counts exist but the drone cannot read them: sccache without a host
    /// server keeps its counters in each job's own server, which exits with
    /// the job. Read the job's `sccache --show-stats` output instead.
    #[default]
    Unknown,
    /// The cache kind has no hit counters.
    NotTracked,
}

/// A running unit the drone stops listing is considered lost after this.
const LOST_UNIT_GRACE_SECS: i64 = 60;

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseGrant {
    pub lease_id: String,
    pub class: Class,
    pub scope: Scope,
    pub labels: Vec<String>,
    pub runner_name: String,
    pub jit_config: Secret,
    pub expires_at: i64,
}

/// A drone's report of an update it ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateReport {
    /// The version the update aimed for.
    pub target: String,
    /// `installed`, `runner_updated`, `up_to_date`, `rolled_back` or `failed`.
    pub state: String,
    pub detail: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HeartbeatResponse {
    /// Existing Queen disk floor; older Queens omit it and drones use 10 GiB.
    #[serde(default = "crate::envcontract::default_min_disk_free_bytes")]
    pub min_free_disk_bytes: u64,
    pub leases: Vec<LeaseGrant>,
    /// Work units to run (resent until acknowledged in `active_units`).
    #[serde(default)]
    pub units: Vec<UnitGrant>,
    /// Units the client cancelled; stop them.
    #[serde(default)]
    pub cancel_units: Vec<String>,
    /// The borg version this drone should run, from the rollout gate (or the
    /// configured target when the rollout is not gated). Absent: no
    /// instruction, keep the current version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired_version: Option<String>,
    /// Demand pressure per class, for the drone to set its bids against.
    #[serde(default)]
    pub pressure: Vec<ClassPressure>,
    /// Leases the queen preempted for higher-priority demand; stop their jobs
    /// (resent until the lease ends).
    #[serde(default)]
    pub cancel_leases: Vec<String>,
    /// Standby deploy runners to hold unstarted (resent until the drone lists
    /// them in `held_standby`).
    #[serde(default)]
    pub standby: Vec<LeaseGrant>,
    /// Held standby leases to start now: deploy jobs are queued for them.
    #[serde(default)]
    pub start_standby: Vec<String>,
    /// Held standby leases that ended on the queen's side; forget them.
    #[serde(default)]
    pub drop_standby: Vec<String>,
    /// A short-lived read-only token for the git mirrors, present on every
    /// heartbeat while the queen has `mirror_auth` configured. Keep it in
    /// memory only and never log it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror_credential: Option<crate::mirror_auth::MirrorCredential>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ResultRequest {
    pub exit_code: i32,
    pub duration_secs: u64,
    #[serde(default)]
    pub peak_rss_mib: Option<u64>,
    /// Compact per-job summary; see [`crate::telemetry`] for the wire format.
    #[serde(default)]
    pub telemetry: Option<JobTelemetry>,
    /// The drone stopped the job because the queen preempted the lease.
    #[serde(default)]
    pub preempted: bool,
    /// Host load when the runner started and when it exited; see
    /// [`crate::hostdiag`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<LeaseHostReport>,
}

#[derive(Debug)]
pub enum ApiError {
    BadRequest(String),
    TooMany,
    TooLarge,
    Unauthorized,
    NotFound,
    Forbidden,
    /// 403 with a message the client can show.
    ForbiddenWith(String),
    Conflict,
    Unprocessable,
    Internal(String),
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        Self::Internal(e.to_string())
    }
}

impl From<QueenError> for ApiError {
    fn from(e: QueenError) -> Self {
        Self::Internal(e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            Self::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg).into_response(),
            Self::TooMany => StatusCode::TOO_MANY_REQUESTS.into_response(),
            Self::TooLarge => StatusCode::PAYLOAD_TOO_LARGE.into_response(),
            Self::Unauthorized => StatusCode::UNAUTHORIZED.into_response(),
            Self::NotFound => StatusCode::NOT_FOUND.into_response(),
            Self::Forbidden => StatusCode::FORBIDDEN.into_response(),
            Self::ForbiddenWith(msg) => (StatusCode::FORBIDDEN, msg).into_response(),
            Self::Conflict => StatusCode::CONFLICT.into_response(),
            Self::Unprocessable => StatusCode::UNPROCESSABLE_ENTITY.into_response(),
            Self::Internal(msg) => {
                tracing::error!("api error: {msg}");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        }
    }
}

pub fn router(queen: Arc<Queen>) -> Router {
    Router::new()
        .route("/status", get(status))
        .route("/metrics", get(metrics))
        .route("/report", get(report))
        .route("/explain", get(explain))
        .route("/jobs/{job_id}", get(job_host))
        .route("/runs/{run_id}", get(run_hosts))
        .route("/v1/enroll", post(tailnet_enroll))
        .route("/v1/heartbeat", post(heartbeat))
        .route("/v1/leases/{id}/result", post(lease_result))
        .merge(crate::units_api::routes())
        .with_state(queen)
}

#[derive(Debug, Deserialize)]
struct ExplainQuery {
    key: String,
}

/// Public label queries contain runner decisions only. Unit diagnostics require
/// the existing client token and a row owned by that client; another client's
/// unit is indistinguishable from an unknown key.
async fn explain(
    State(queen): State<Arc<Queen>>,
    headers: HeaderMap,
    Query(query): Query<ExplainQuery>,
) -> Result<Json<crate::explain::Explanation>, ApiError> {
    use crate::explain::{Explanation, PlacementKind};
    if query.key.is_empty() || query.key.len() > 256 {
        return Err(ApiError::BadRequest(
            "explain key must be 1-256 bytes".into(),
        ));
    }
    let (kind, key, client) =
        if let Some((class, scope)) = queen.config().classes.parse_label(&query.key) {
            (
                PlacementKind::Runner,
                queen.config().classes.job_label(class, scope),
                None,
            )
        } else {
            if !queen.config().units.enabled {
                return Err(ApiError::NotFound);
            }
            let owner = crate::units_api::authenticate_client(&queen, &headers).await?;
            (PlacementKind::Unit, query.key.clone(), Some(owner))
        };
    let history = queen
        .store()
        .placement_history(kind, &key, client.as_deref())
        .await?;
    Ok(Json(Explanation::new(query.key, history)))
}

/// The drone name behind the request's bearer token.
pub(crate) async fn authenticate(queen: &Queen, headers: &HeaderMap) -> Result<String, ApiError> {
    let presented = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(ApiError::Unauthorized)?;
    queen
        .store()
        .drone_for_token_hash(&hash_token(presented))
        .await?
        .ok_or(ApiError::Unauthorized)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EnrollResponse {
    pub name: String,
    /// The drone token. Shown once; the queen stores only its hash.
    pub token: Secret,
}

/// `POST /v1/enroll`: zero-touch enrollment of an allowlisted tailnet node.
/// 404 when not configured, 403 when the caller is not an allowed tailnet
/// identity, 409 when the node's drone name is already enrolled.
async fn tailnet_enroll(
    State(queen): State<Arc<Queen>>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
) -> Result<Json<EnrollResponse>, ApiError> {
    let Some(enroll) = queen.tailnet_enroll() else {
        return Err(ApiError::NotFound);
    };
    let identity = match enroll.verifier.whois(peer).await {
        Ok(Some(id)) if enroll.settings.allows(&id) => id,
        Ok(found) => {
            tracing::warn!(
                "tailnet enroll refused for {peer}: {}",
                found.map_or_else(
                    || "not a tailnet node".to_owned(),
                    |id| format!("{} (tags {:?}) is not allowlisted", id.node_name, id.tags)
                )
            );
            return Err(ApiError::Forbidden);
        }
        Err(e) => {
            tracing::warn!("tailnet enroll: whois for {peer} failed: {e}");
            return Err(ApiError::Internal(format!("whois: {e}")));
        }
    };
    let Some(name) = drone_name_for(&identity.node_name) else {
        return Err(ApiError::Unprocessable);
    };
    let token = crate::enroll::new_token();
    let now = OffsetDateTime::now_utc().unix_timestamp();
    match queen
        .store()
        .enroll(&name, &crate::enroll::hash_token(&token), now)
        .await
    {
        Ok(()) => {
            tracing::info!(
                "tailnet enroll: enrolled drone {name} for {} (tags {:?}, user {:?}) from {peer}",
                identity.node_name,
                identity.tags,
                identity.user
            );
            Ok(Json(EnrollResponse {
                name,
                token: Secret::new(token),
            }))
        }
        Err(StoreError::AlreadyEnrolled(_)) => Err(ApiError::Conflict),
        Err(e) => Err(e.into()),
    }
}

async fn heartbeat(
    State(queen): State<Arc<Queen>>,
    headers: HeaderMap,
    Json(req): Json<HeartbeatRequest>,
) -> Result<Json<HeartbeatResponse>, ApiError> {
    let drone = authenticate(&queen, &headers).await?;
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let store = queen.store();
    let disk = crate::envcontract::disk_admission(
        queen.config().env_contract.min_cache_dir_free_bytes,
        req.capacity.free_disk_mib,
        req.env_facts
            .as_ref()
            .filter(|facts| {
                (0..=queen.config().env_contract.facts_max_age_secs)
                    .contains(&(now - facts.observed_at))
            })
            .and_then(|facts| facts.cache_dir_free_bytes),
    );
    let accepting = req.accepting && !disk.blocked();
    store
        .record_heartbeat(
            &drone,
            &req.capacity,
            &req.caps,
            accepting,
            &req.cache_stats,
            now,
        )
        .await?;
    store
        .set_version(&drone, sanitize_version(req.version.as_deref()), now)
        .await?;
    store.set_extra_labels(&drone, &req.extra_labels).await?;
    store.set_trust(&drone, req.trust).await?;
    store.set_bids(&drone, &sanitize_bids(&req.bids)).await?;
    if let Some(facts) = &req.env_facts {
        store
            .set_env_facts(&drone, &sanitize_facts(facts, now))
            .await?;
    }
    if let Some(stats) = &req.reclaim {
        store.record_reclaim(&drone, stats).await?;
    }
    store.acknowledge(&drone, &req.active_leases).await?;
    store
        .promote_standby(
            &drone,
            &req.active_leases,
            now,
            queen.config().lease_ttl_secs,
        )
        .await?;
    let standby_rows = store.standby_leases(&drone).await?;
    let start = queen.snapshot().start_standby;
    let start_standby: Vec<String> = standby_rows
        .iter()
        .filter(|l| accepting && start.contains(&l.id))
        .map(|l| l.id.clone())
        .collect();
    let drop_standby: Vec<String> = req
        .held_standby
        .iter()
        .filter(|id| !standby_rows.iter().any(|l| &l.id == *id))
        .cloned()
        .collect();
    let standby: Vec<LeaseGrant> = standby_rows
        .into_iter()
        .filter(|l| !req.held_standby.contains(&l.id))
        .filter_map(|l| {
            Some(LeaseGrant {
                lease_id: l.id,
                class: l.class,
                scope: l.scope,
                labels: l.labels,
                runner_name: l.runner_name,
                jit_config: l.jit_config?,
                expires_at: l.expires_at,
            })
        })
        .collect();
    let (units, cancel_units) = if queen.config().units.enabled {
        store.ack_units(&drone, &req.active_units, now).await?;
        store
            .requeue_lost_units(
                &drone,
                &req.active_units,
                now - LOST_UNIT_GRACE_SECS,
                queen.config().units.max_attempts,
                now,
            )
            .await?;
        let grants = store
            .unacked_units(&drone)
            .await?
            .into_iter()
            .filter(|_| accepting)
            .map(|u| UnitGrant {
                unit_id: u.id,
                repo: u.repo,
                tree_sha: u.tree_sha,
                argv: u.argv,
                class: u.class,
                toolchain: u.toolchain,
                timeout_secs: u.timeout_secs,
                attempt: u.attempt,
                bundle_bytes: u.bundle_bytes,
            })
            .collect();
        (grants, store.cancel_requests(&drone).await?)
    } else {
        (Vec::new(), Vec::new())
    };
    let leases = store
        .undelivered(&drone)
        .await?
        .into_iter()
        .filter(|_| accepting)
        .filter_map(|l| {
            Some(LeaseGrant {
                lease_id: l.id,
                class: l.class,
                scope: l.scope,
                labels: l.labels,
                runner_name: l.runner_name,
                jit_config: l.jit_config?,
                expires_at: l.expires_at,
            })
        })
        .collect();
    let mirror_credential = match queen.mirror_auth() {
        Some(mirror) => match mirror.credential().await {
            Ok(credential) => Some(credential),
            Err(e) => {
                tracing::warn!("mint mirror token: {e}");
                None
            }
        },
        None => None,
    };
    Ok(Json(HeartbeatResponse {
        min_free_disk_bytes: queen.config().env_contract.min_cache_dir_free_bytes,
        leases,
        units,
        cancel_units,
        desired_version: queen.desired_version(&drone),
        pressure: queen.snapshot().pressure,
        cancel_leases: store
            .preempted_active()
            .await?
            .into_iter()
            .filter_map(|(id, d)| (d == drone).then_some(id))
            .collect(),
        standby,
        start_standby,
        drop_standby,
        mirror_credential,
    }))
}

async fn lease_result(
    State(queen): State<Arc<Queen>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<ResultRequest>,
) -> Result<StatusCode, ApiError> {
    let drone = authenticate(&queen, &headers).await?;
    let result = LeaseResult {
        exit_code: req.exit_code,
        duration_secs: req.duration_secs,
        peak_rss_mib: req.peak_rss_mib,
    };
    let now = OffsetDateTime::now_utc().unix_timestamp();
    if queen
        .store()
        .finish_lease(&id, &drone, &result, now)
        .await?
    {
        if let Some(lease) = queen.store().lease(&id).await?
            && let Err(e) = queen
                .store()
                .record_lease_host(&lease, &result, req.host.as_ref(), now)
                .await
        {
            // Diagnostics never fail a result.
            tracing::warn!("lease {id}: host diagnostics not stored: {e}");
        }
        if let Some(t) = req.telemetry {
            match t.sanitized() {
                Some(t) => {
                    if let Some(lease) = queen.store().lease(&id).await? {
                        queen
                            .store()
                            .record_telemetry(&lease, &t, req.duration_secs, now)
                            .await?;
                    }
                }
                None => tracing::warn!("lease {id}: dropped unusable telemetry"),
            }
        }
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound)
    }
}

#[derive(Debug, Serialize)]
pub struct ClassStatus {
    pub queued_deploy: u32,
    pub queued_main: u32,
    pub queued_pr: u32,
    pub oldest_age_secs_deploy: u64,
    pub oldest_age_secs_main: u64,
    pub oldest_age_secs_pr: u64,
    /// Leases of this class the fleet could hold if it were empty (theoretical
    /// capacity, not what is free).
    pub slots: u32,
    /// Leases of this class that fit right now: capacity minus every active
    /// lease, on drones that are fresh and take new leases.
    pub free_slots: u32,
    pub demand_deploy: ScopeDemand,
    pub demand_main: ScopeDemand,
    pub demand_pr: ScopeDemand,
    pub leases_pending: u32,
    pub leases_running: u32,
}

impl ClassStatus {
    fn queued(&self, scope: Scope) -> u32 {
        match scope {
            Scope::Deploy => self.queued_deploy,
            Scope::Main => self.queued_main,
            Scope::Pr => self.queued_pr,
        }
    }

    fn oldest_age_secs(&self, scope: Scope) -> u64 {
        match scope {
            Scope::Deploy => self.oldest_age_secs_deploy,
            Scope::Main => self.oldest_age_secs_main,
            Scope::Pr => self.oldest_age_secs_pr,
        }
    }

    fn demand(&self, scope: Scope) -> &ScopeDemand {
        match scope {
            Scope::Deploy => &self.demand_deploy,
            Scope::Main => &self.demand_main,
            Scope::Pr => &self.demand_pr,
        }
    }
}

/// Queued demand for one label, split by who can serve it. The split is an
/// estimate: GitHub, not borg, picks which runner takes a job, and a queued job
/// carries no runner yet, so borg counts the idle online runners it did not mint
/// that carry the job's label.
#[derive(Debug, Default, Serialize)]
pub struct ScopeDemand {
    pub queued: u32,
    /// Online runners not minted by borg with this label.
    pub static_online: u32,
    /// Of those, the ones not running a job.
    pub static_idle: u32,
    /// `min(queued, static_idle)`: likely taken by those runners.
    pub claimable_by_static_estimate: u32,
    /// `queued - claimable_by_static_estimate`: what borg has to place.
    pub borg_placeable: u32,
}

#[derive(Debug, Serialize)]
pub struct DroneStatus {
    pub name: String,
    pub heartbeat_age_secs: Option<i64>,
    pub fresh: bool,
    /// `alive` (heartbeat within `drone_stale_secs`) or `stale`. A stale
    /// drone holds no schedulable capacity whatever its `health` says;
    /// `health` is the historical failure evidence only.
    pub availability: Availability,
    /// Seconds since the last heartbeat; `None` if it never sent one.
    pub last_seen_age_secs: Option<i64>,
    /// Seconds until a stale drone is removed (`drone_expiry`); `None` while
    /// alive or when it is never removed.
    pub expires_in_secs: Option<i64>,
    pub accepting: bool,
    /// Enforced independently of the optional environment contract.
    pub disk_admission: crate::envcontract::DiskAdmission,
    /// `shared` or `deploy`: which scopes the drone may run.
    pub trust: Trust,
    pub hostname: Option<String>,
    pub kind: Option<crate::probe::HostKind>,
    pub vcpu: Option<u32>,
    /// Raw host memory and vCPU as probed.
    pub mem_mib: Option<u64>,
    /// Memory and vCPU borg may hand out (host less the OS reserve, under the
    /// owner's caps): the theoretical budget.
    pub budget_mem_mib: Option<u64>,
    pub budget_vcpu: Option<u32>,
    /// Budget minus the leases the drone holds right now.
    pub free_mem_mib: Option<u64>,
    pub free_vcpu: Option<u32>,
    /// Leases per class that fit right now / that would fit if it were empty.
    pub free_slots: BTreeMap<Class, u32>,
    pub theoretical_slots: BTreeMap<Class, u32>,
    pub active_leases: u32,
    pub caches: Vec<CacheStat>,
    pub quarantined: bool,
    pub quarantine_reason: Option<String>,
    pub version: Option<String>,
    /// Evidence-based host health; absent until the first assessment.
    pub health: Option<DroneHealth>,
    /// Build-box reclaim stats, when the drone reports them.
    pub reclaim: Option<ReclaimStats>,
    /// The build environment contract as it stands for this drone; absent when
    /// the contract is off.
    pub contract: Option<ContractStatus>,
    /// CPU-pressure admission from the last pass; absent for drones the last
    /// pass did not consider.
    pub admission: Option<DroneAdmissionView>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    Alive,
    Stale,
}

/// One class's schedulable capacity next to its unmet demand.
#[derive(Debug, Serialize)]
pub struct CapacityStatus {
    /// Slots the alive, accepting drones could hold if empty.
    pub schedulable_slots: u32,
    /// Slots free right now on alive, accepting drones.
    pub free_slots: u32,
    /// Jobs borg could not place at the last pass (`unmet_placeable`).
    pub unmet_main: u32,
    pub unmet_pr: u32,
    pub unmet_deploy: u32,
    /// Since when main demand has been unmet with schedulable slots at or
    /// under `capacity_alarm.max_slots`.
    pub starved_since: Option<i64>,
    /// The `capacity_starved` health event is raised and not yet restored.
    pub alarm: bool,
}

/// What CPU pressure currently lets this drone take.
#[derive(Debug, Serialize)]
pub struct DroneAdmissionView {
    pub state: AdmissionState,
    pub cpu_psi_pm: Option<u16>,
    /// Pressure estimated from the load average; `state` uses it only when
    /// `cpu_psi_pm` is null.
    pub cpu_load_pm: Option<u16>,
}

/// One drone's build environment contract.
#[derive(Debug, Serialize)]
pub struct ContractStatus {
    pub reported: bool,
    pub age_secs: Option<i64>,
    /// The checks as evaluated for the lightest class (the least demanding
    /// resources need); the admission per class is below.
    pub checks: Vec<crate::envcontract::CheckOutcome>,
    /// Whether the drone is admitted for each class, and why not.
    pub admission: BTreeMap<Class, ClassAdmission>,
}

#[derive(Debug, Serialize)]
pub struct ClassAdmission {
    pub admit: bool,
    pub why: Option<String>,
}

/// How many enrolled drones run the configured target version.
#[derive(Debug, Default, Serialize)]
pub struct RolloutStatus {
    pub target_version: Option<String>,
    /// Drones reporting exactly the target.
    pub current: u32,
    /// Drones reporting another version.
    pub outdated: u32,
    /// Drones that report no version.
    pub unknown: u32,
    /// The quorum rollout gate, when `rollout.previous_version` is set.
    pub gate: Option<RolloutView>,
    /// Recent rollout transitions, newest first.
    pub log: Vec<(i64, String)>,
}

#[derive(Debug, Serialize)]
pub struct Status {
    pub now: i64,
    pub last_tick_at: Option<i64>,
    pub last_error: Option<String>,
    pub classes: BTreeMap<Class, ClassStatus>,
    pub queue: Vec<QueueEntry>,
    pub drones: Vec<DroneStatus>,
    /// Per class: schedulable slots on alive drones next to unmet demand.
    pub capacity: BTreeMap<Class, CapacityStatus>,
    /// Names of drones whose heartbeat is older than `drone_stale_secs`.
    pub stale_drones: Vec<String>,
    /// Demand the allocator could not place. Counts jobs that non-borg runners
    /// may serve; see `unmet_placeable`.
    pub unmet: Vec<Unmet>,
    /// `unmet` less the idle non-borg runners carrying each label (an estimate).
    pub unmet_placeable: Vec<Unmet>,
    /// Drones held back for starving demand.
    pub reserved_for: Vec<Reservation>,
    pub rollout: RolloutStatus,
    pub warm_floors: Vec<crate::allocator::WarmFloor>,
    pub pressure: Vec<ClassPressure>,
    /// Host failures look fleet-wide; automatic quarantine is paused.
    pub health_systemic: bool,
    pub homeostasis: Vec<KeyReport>,
    pub profiles: Vec<crate::profile::ClassProfile>,
    pub wake: crate::wake::WakeView,
    /// Build-box reclaim: bytes ever really deleted and the newest audit
    /// entries.
    pub reclaim: ReclaimSummary,
    pub units: UnitsStatus,
    /// Present when the supersede janitor is enabled.
    pub janitor: Option<crate::janitor::JanitorView>,
    /// Whether drones are handed git mirror tokens, and when the current one
    /// expires. Never the token.
    pub mirror_auth: crate::mirror_auth::MirrorAuthView,
    /// Present when preempted jobs are re-run (preemption and the janitor
    /// are both configured).
    pub requeue: Option<crate::requeue::RequeueView>,
    /// The newest quarantine and health audit entries.
    pub health_events: Vec<crate::store::HealthLogRow>,
    /// The newest leases cancelled for higher-priority demand.
    pub preemptions: Vec<crate::store::PreemptionRow>,
    /// All-time preemption counts by victim scope and starving scope.
    pub preemption_counts: Vec<(Scope, Scope, u64)>,
}

#[derive(Debug, Serialize)]
pub struct ReclaimSummary {
    pub reclaimed_bytes_logged: u64,
    pub events: Vec<crate::store::ReclaimLogRow>,
}

#[derive(Debug, Serialize)]
pub struct UnitsStatus {
    pub enabled: bool,
    pub counts: std::collections::BTreeMap<&'static str, u32>,
    /// Submissions that joined a running unit since the queen started.
    pub joins: u64,
    /// Submissions answered with a stored receipt since the queen started.
    pub receipt_hits: u64,
    /// Units cancelled by a newer submission with the same `supersede_key`
    /// since the queen started.
    pub superseded: u64,
    pub oldest_queued_age_secs: u64,
    pub recent: Vec<crate::units::Unit>,
    /// Why queued units are not admitted on drones that otherwise match them
    /// (platform, fresh): the build environment contract's verdict.
    pub not_admitted: Vec<UnitBlock>,
}

#[derive(Debug, Serialize)]
pub struct UnitBlock {
    pub unit_id: String,
    pub drone: String,
    pub why: String,
}

fn scope_demand(
    snap: &crate::queen::Snapshot,
    cfg: &crate::config::QueenConfig,
    class: Class,
    scope: Scope,
) -> ScopeDemand {
    let queued = snap
        .queue
        .iter()
        .find(|q| q.class == class && q.scope == scope)
        .map_or(0, |q| q.queued);
    let statics = snap
        .static_runners
        .get(&cfg.classes.job_label(class, scope))
        .copied()
        .unwrap_or_default();
    let claimable = queued.min(statics.idle);
    ScopeDemand {
        queued,
        static_online: statics.online,
        static_idle: statics.idle,
        claimable_by_static_estimate: claimable,
        borg_placeable: queued - claimable,
    }
}

/// For each queued unit, the drones of its platform that the contract does not
/// admit for it, and why (at most 50 entries).
fn unit_blocks(
    cfg: &crate::config::QueenConfig,
    rows: &[crate::store::DroneRow],
    queued: &[crate::units::Unit],
    now: i64,
) -> Vec<UnitBlock> {
    if !cfg.env_contract.enabled {
        return Vec::new();
    }
    let mut out = Vec::new();
    for u in queued.iter().filter(|u| !u.awaiting_bundle) {
        for d in rows {
            let Some(cap) = &d.capacity else {
                continue;
            };
            let fresh = d
                .last_heartbeat
                .is_some_and(|t| now - t <= cfg.drone_stale_secs);
            if !fresh || crate::queen::platform_of(cap) != u.platform {
                continue;
            }
            let need = Need::for_class(
                &cfg.env_contract,
                u.class,
                cfg.classes.spec(u.class),
                u.rust_toolchain.as_deref(),
            );
            let v = evaluate(&cfg.env_contract, d.env_facts.as_ref(), &need, now);
            if let (false, Some(why)) = (v.admit, v.why) {
                out.push(UnitBlock {
                    unit_id: u.id.clone(),
                    drone: d.name.clone(),
                    why,
                });
            }
        }
    }
    out.truncate(50);
    out
}

pub async fn build_status(queen: &Queen, now: i64) -> Result<Status, StoreError> {
    let snap = queen.snapshot();
    let cfg = queen.config();
    let rows = queen.store().drones().await?;
    let active = queen.store().active_leases().await?;

    let mut held: Vec<ActiveLease> = active
        .iter()
        .map(|l| ActiveLease {
            drone_id: l.drone.clone(),
            class: l.class,
            scope: l.scope,
            state: LeaseState::Running,
        })
        .collect();
    // Placed work units hold capacity like running leases.
    if cfg.units.enabled {
        use crate::units::UnitState;
        for u in queen
            .store()
            .units_in(&[UnitState::Leased, UnitState::Running])
            .await?
        {
            if let Some(drone_id) = u.drone {
                held.push(ActiveLease {
                    drone_id,
                    class: u.class,
                    scope: Scope::Pr,
                    state: LeaseState::Running,
                });
            }
        }
    }
    let mut states = Vec::new();
    let mut drones = Vec::new();
    let mut free_by_class: BTreeMap<Class, u32> = BTreeMap::new();
    for d in &rows {
        let age = d.last_heartbeat.map(|t| now - t);
        let fresh = age.is_some_and(|a| a <= cfg.drone_stale_secs);
        let disk = crate::envcontract::disk_admission(
            cfg.env_contract.min_cache_dir_free_bytes,
            d.capacity
                .as_ref()
                .filter(|_| fresh)
                .and_then(|cap| cap.free_disk_mib),
            d.env_facts
                .as_ref()
                .filter(|facts| {
                    (0..=cfg.env_contract.facts_max_age_secs).contains(&(now - facts.observed_at))
                })
                .and_then(|facts| facts.cache_dir_free_bytes),
        );
        let mut budget = None;
        let mut free_slots_here = BTreeMap::new();
        let mut theoretical_here = BTreeMap::new();
        if let (true, Some(cap)) = (fresh, &d.capacity) {
            let mut state = d.allocator_state(cap, now);
            state.accepting &= !disk.blocked();
            state.blocked_classes = blocked_classes(
                &cfg.env_contract,
                d.env_facts.as_ref(),
                &cfg.classes,
                None,
                now,
            );
            let room = free_room(&state, &held, &cfg.classes);
            for class in Class::ALL {
                let spec = cfg.classes.spec(class);
                let free = if state.blocked_classes.contains(&class) {
                    0
                } else {
                    free_slots(&state, &room, spec)
                };
                free_slots_here.insert(class, free);
                theoretical_here.insert(
                    class,
                    if state.accepting {
                        max_slots(&state, spec)
                    } else {
                        0
                    },
                );
                *free_by_class.entry(class).or_default() += free;
            }
            budget = Some((state.mem_mib, state.vcpu, room));
            states.push(state);
        }
        drones.push(DroneStatus {
            name: d.name.clone(),
            heartbeat_age_secs: age,
            fresh,
            availability: if fresh {
                Availability::Alive
            } else {
                Availability::Stale
            },
            last_seen_age_secs: age,
            expires_in_secs: match (fresh, age, cfg.drone_expiry.expiry_for(&d.name)) {
                (false, Some(age), Some(after))
                    if !cfg.wake.hosts.iter().any(|h| h.name == d.name) =>
                {
                    Some((after.max(cfg.drone_stale_secs) - age).max(0))
                }
                _ => None,
            },
            accepting: d.accepting && !disk.blocked(),
            disk_admission: disk,
            trust: d.trust,
            hostname: d.capacity.as_ref().map(|c| c.hostname.clone()),
            kind: d.capacity.as_ref().map(|c| c.kind),
            vcpu: d.capacity.as_ref().map(|c| c.vcpu),
            mem_mib: d.capacity.as_ref().map(|c| c.mem_mib),
            budget_mem_mib: budget.map(|b| b.0),
            budget_vcpu: budget.map(|b| b.1),
            free_mem_mib: budget.map(|b| b.2.mem_mib),
            free_vcpu: budget.map(|b| b.2.vcpu),
            free_slots: free_slots_here,
            theoretical_slots: theoretical_here,
            active_leases: u32::try_from(active.iter().filter(|l| l.drone == d.name).count())
                .unwrap_or(u32::MAX),
            caches: d.cache_stats.clone(),
            quarantined: d.quarantined,
            quarantine_reason: d.quarantine_reason.clone(),
            version: d.version.clone(),
            health: snap.health.get(&d.name).cloned(),
            reclaim: d.reclaim.clone(),
            admission: snap
                .admission
                .iter()
                .find(|a| a.drone_id == d.name)
                .map(|a| DroneAdmissionView {
                    state: a.state,
                    cpu_psi_pm: a.cpu_psi_pm,
                    cpu_load_pm: a.cpu_load_pm,
                }),
            contract: cfg.env_contract.enabled.then(|| {
                let verdict = |class| {
                    let need =
                        Need::for_class(&cfg.env_contract, class, cfg.classes.spec(class), None);
                    evaluate(&cfg.env_contract, d.env_facts.as_ref(), &need, now)
                };
                let light: Verdict = verdict(Class::Light);
                ContractStatus {
                    reported: light.reported,
                    age_secs: light.age_secs,
                    checks: light.checks.clone(),
                    admission: Class::ALL
                        .into_iter()
                        .map(|c| {
                            let v = verdict(c);
                            (
                                c,
                                ClassAdmission {
                                    admit: v.admit,
                                    why: v.why,
                                },
                            )
                        })
                        .collect(),
                }
            }),
        });
    }

    let slots = queen.class_slots(&states);
    let unmet_of = |class: Class, scope: Scope| -> u32 {
        snap.unmet_placeable
            .iter()
            .filter(|u| u.class == class && u.scope == scope)
            .map(|u| u.count)
            .sum()
    };
    let capacity: BTreeMap<Class, CapacityStatus> = Class::ALL
        .into_iter()
        .map(|class| {
            (
                class,
                CapacityStatus {
                    schedulable_slots: slots.get(&class).copied().unwrap_or(0),
                    free_slots: free_by_class.get(&class).copied().unwrap_or(0),
                    unmet_main: unmet_of(class, Scope::Main),
                    unmet_pr: unmet_of(class, Scope::Pr),
                    unmet_deploy: unmet_of(class, Scope::Deploy),
                    starved_since: snap.starved_since.get(&class).copied(),
                    alarm: snap.starved.contains(&class),
                },
            )
        })
        .collect();
    let stale_drones: Vec<String> = drones
        .iter()
        .filter(|d| d.availability == Availability::Stale)
        .map(|d| d.name.clone())
        .collect();
    let mut classes = BTreeMap::new();
    for class in Class::ALL {
        let entry = |scope| {
            snap.queue
                .iter()
                .find(|q| q.class == class && q.scope == scope)
        };
        let count = |state| {
            u32::try_from(
                active
                    .iter()
                    .filter(|l| l.class == class && l.state == state)
                    .count(),
            )
            .unwrap_or(u32::MAX)
        };
        classes.insert(
            class,
            ClassStatus {
                queued_deploy: entry(Scope::Deploy).map_or(0, |q| q.queued),
                queued_main: entry(Scope::Main).map_or(0, |q| q.queued),
                queued_pr: entry(Scope::Pr).map_or(0, |q| q.queued),
                oldest_age_secs_deploy: entry(Scope::Deploy).map_or(0, |q| q.oldest_age_secs),
                oldest_age_secs_main: entry(Scope::Main).map_or(0, |q| q.oldest_age_secs),
                oldest_age_secs_pr: entry(Scope::Pr).map_or(0, |q| q.oldest_age_secs),
                slots: slots.get(&class).copied().unwrap_or(0),
                free_slots: free_by_class.get(&class).copied().unwrap_or(0),
                demand_deploy: scope_demand(&snap, cfg, class, Scope::Deploy),
                demand_main: scope_demand(&snap, cfg, class, Scope::Main),
                demand_pr: scope_demand(&snap, cfg, class, Scope::Pr),
                leases_pending: count(LeaseStatus::Pending),
                leases_running: count(LeaseStatus::Running),
            },
        );
    }
    let mut rollout = RolloutStatus {
        target_version: cfg.rollout_target_version.clone(),
        gate: snap.rollout.clone(),
        log: queen.store().rollout_log(20).await?,
        ..RolloutStatus::default()
    };
    for d in &drones {
        match (&d.version, &cfg.rollout_target_version) {
            (None, _) => rollout.unknown += 1,
            (Some(v), Some(t)) if v == t => rollout.current += 1,
            (Some(_), _) => rollout.outdated += 1,
        }
    }
    Ok(Status {
        now,
        last_tick_at: snap.last_tick_at,
        last_error: snap.last_error,
        classes,
        queue: snap.queue,
        drones,
        capacity,
        stale_drones,
        unmet: snap.unmet,
        unmet_placeable: snap.unmet_placeable,
        reserved_for: snap.reserved_for,
        rollout,
        warm_floors: snap.warm_floors,
        pressure: snap.pressure,
        health_systemic: snap.health_systemic,
        homeostasis: snap.homeostasis,
        profiles: snap.profiles,
        wake: snap.wake,
        // Disabled units show only `enabled: false`: old records are not
        // live state, and no unit is placed or served.
        units: if !cfg.units.enabled {
            UnitsStatus {
                enabled: false,
                counts: BTreeMap::new(),
                joins: 0,
                receipt_hits: 0,
                superseded: 0,
                oldest_queued_age_secs: 0,
                recent: Vec::new(),
                not_admitted: Vec::new(),
            }
        } else {
            let (joins, receipt_hits) = queen.unit_reuse_counts();
            let store = queen.store();
            let queued = store.units_in(&[crate::units::UnitState::Queued]).await?;
            UnitsStatus {
                enabled: cfg.units.enabled,
                counts: store.unit_counts().await?,
                joins,
                receipt_hits,
                superseded: queen.units_superseded(),
                oldest_queued_age_secs: queued
                    .first()
                    .map_or(0, |u| u64::try_from(now - u.created_at).unwrap_or(0)),
                recent: store.recent_units(20).await?,
                not_admitted: unit_blocks(cfg, &rows, &queued, now),
            }
        },
        janitor: queen.janitor().map(crate::janitor::Janitor::view),
        mirror_auth: queen.mirror_auth().map_or(
            crate::mirror_auth::MirrorAuthView {
                configured: false,
                expires_at: None,
            },
            crate::mirror_auth::MirrorAuth::view,
        ),
        requeue: queen.requeuer().map(crate::requeue::Requeuer::view),
        reclaim: ReclaimSummary {
            reclaimed_bytes_logged: queen.store().reclaim_bytes_logged().await?,
            events: queen.store().reclaim_log(20).await?,
        },
        health_events: queen.store().health_log(20).await?,
        preemptions: queen.store().preemptions(20).await?,
        preemption_counts: queen.store().preemption_counts().await?,
    })
}

#[derive(Debug, Deserialize)]
struct ReportQuery {
    since_secs: Option<i64>,
    limit: Option<usize>,
}

/// `GET /report?since_secs=86400&limit=20`: job time, queue wait, cancellation
/// share and host failures from stored telemetry. Unauthenticated like
/// `/status`.
async fn report(
    State(queen): State<Arc<Queen>>,
    Query(q): Query<ReportQuery>,
) -> Result<Json<Report>, ApiError> {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let max = i64::from(queen.config().telemetry_retention_days) * 86_400;
    let window = q.since_secs.unwrap_or(86_400).clamp(1, max);
    let rows = queen
        .store()
        .telemetry_since(now - window, MAX_REPORT_ROWS)
        .await?;
    Ok(Json(build_report(
        &rows,
        now,
        window,
        q.limit.unwrap_or(20).min(500),
    )))
}

/// The CPU pressure (percent) above which a job's host counts as slow: the
/// admission soft threshold when admission is on.
fn slow_cpu_psi_pct(queen: &Queen) -> f64 {
    queen
        .config()
        .admission
        .as_ref()
        .map_or(crate::hostdiag::SLOW_CPU_PSI_PCT, |a| {
            f64::from(a.psi_soft_pm) / 10.0
        })
}

/// `GET /jobs/{job_id}`: the drone that ran GitHub job `job_id` and its host
/// load at start and end. 404 when the queen never saw that job on a runner it
/// minted, or the row aged out. Unauthenticated like `/status`.
async fn job_host(
    State(queen): State<Arc<Queen>>,
    Path(job_id): Path<u64>,
) -> Result<Json<JobHostView>, ApiError> {
    queen
        .store()
        .job_host(job_id, slow_cpu_psi_pct(&queen))
        .await?
        .map(Json)
        .ok_or(ApiError::NotFound)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RunHosts {
    pub run_id: u64,
    pub jobs: Vec<JobHostView>,
}

/// `GET /runs/{run_id}`: every job of a workflow run that ran on a borg
/// runner, oldest lease first. An unknown run has an empty `jobs` list.
async fn run_hosts(
    State(queen): State<Arc<Queen>>,
    Path(run_id): Path<u64>,
) -> Result<Json<RunHosts>, ApiError> {
    Ok(Json(RunHosts {
        run_id,
        jobs: queen
            .store()
            .run_hosts(run_id, slow_cpu_psi_pct(&queen))
            .await?,
    }))
}

async fn status(State(queen): State<Arc<Queen>>) -> Result<Json<Status>, ApiError> {
    Ok(Json(
        build_status(&queen, OffsetDateTime::now_utc().unix_timestamp()).await?,
    ))
}

/// Prometheus text exposition.
pub fn render_metrics(status: &Status) -> String {
    let mut out = String::new();
    let mut gauge = |name: &str, help: &str, rows: Vec<(String, f64)>| {
        let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} gauge");
        for (labels, v) in rows {
            let _ = if labels.is_empty() {
                writeln!(out, "{name} {v}")
            } else {
                writeln!(out, "{name}{{{labels}}} {v}")
            };
        }
    };
    let both = |f: &dyn Fn(&ClassStatus, Scope) -> f64| -> Vec<(String, f64)> {
        status
            .classes
            .iter()
            .flat_map(|(c, s)| {
                Scope::ALL.map(|sc| {
                    (
                        format!("class=\"{c}\",scope=\"{}\"", scope_name(sc)),
                        f(s, sc),
                    )
                })
            })
            .collect()
    };
    gauge(
        "borg_queued_jobs",
        "Queued jobs per class and scope.",
        both(&|s, sc| f64::from(s.queued(sc))),
    );
    gauge(
        "borg_oldest_queued_age_seconds",
        "Age of the oldest queued job per class and scope.",
        both(&|s, sc| {
            #[allow(
                clippy::cast_precision_loss,
                reason = "ages are far below 2^52 seconds"
            )]
            let v = s.oldest_age_secs(sc) as f64;
            v
        }),
    );
    gauge(
        "borg_class_slots",
        "Leases of a class the fleet could hold if empty.",
        status
            .classes
            .iter()
            .map(|(c, s)| (format!("class=\"{c}\""), f64::from(s.slots)))
            .collect(),
    );
    let mut lease_rows = Vec::new();
    for (c, s) in &status.classes {
        lease_rows.push((
            format!("class=\"{c}\",state=\"pending\""),
            f64::from(s.leases_pending),
        ));
        lease_rows.push((
            format!("class=\"{c}\",state=\"running\""),
            f64::from(s.leases_running),
        ));
    }
    gauge(
        "borg_env_contract_check",
        "Build environment contract checks per drone (evaluated for the lightest class): 1 for the current state.",
        status
            .drones
            .iter()
            .filter_map(|d| d.contract.as_ref().map(|c| (d, c)))
            .flat_map(|(d, c)| {
                c.checks.iter().flat_map(move |o| {
                    CheckState::ALL.into_iter().map(move |st| {
                        (
                            format!(
                                "drone=\"{}\",check=\"{}\",state=\"{}\"",
                                label_value(&d.name),
                                o.kind.as_str(),
                                st.as_str()
                            ),
                            f64::from(u8::from(o.state == st)),
                        )
                    })
                })
            })
            .collect(),
    );
    gauge(
        "borg_env_contract_admitted",
        "1 if the build environment contract admits the class on the drone.",
        status
            .drones
            .iter()
            .filter_map(|d| d.contract.as_ref().map(|c| (d, c)))
            .flat_map(|(d, c)| {
                c.admission.iter().map(move |(class, a)| {
                    (
                        format!("drone=\"{}\",class=\"{class}\"", label_value(&d.name)),
                        f64::from(u8::from(a.admit)),
                    )
                })
            })
            .collect(),
    );
    gauge(
        "borg_class_free_slots",
        "Leases of a class that fit right now (capacity minus active leases).",
        status
            .classes
            .iter()
            .map(|(c, s)| (format!("class=\"{c}\""), f64::from(s.free_slots)))
            .collect(),
    );
    gauge(
        "borg_unmet_jobs",
        "Jobs borg could not place at the last pass, less what idle non-borg runners can take.",
        status
            .capacity
            .iter()
            .flat_map(|(c, v)| {
                [
                    ("main", v.unmet_main),
                    ("pr", v.unmet_pr),
                    ("deploy", v.unmet_deploy),
                ]
                .map(|(scope, n)| (format!("class=\"{c}\",scope=\"{scope}\""), f64::from(n)))
            })
            .collect(),
    );
    gauge(
        "borg_capacity_starved",
        "1 while the capacity_starved health event is raised for a class.",
        status
            .capacity
            .iter()
            .map(|(c, v)| (format!("class=\"{c}\""), f64::from(u8::from(v.alarm))))
            .collect(),
    );
    gauge(
        "borg_drone_free_mem_mib",
        "Memory budget left on a drone after its active leases.",
        status
            .drones
            .iter()
            .filter_map(|d| {
                d.free_mem_mib.map(|m| {
                    #[allow(clippy::cast_precision_loss, reason = "MiB far below 2^52")]
                    let v = m as f64;
                    (format!("drone=\"{}\"", label_value(&d.name)), v)
                })
            })
            .collect(),
    );
    gauge(
        "borg_drone_free_slots",
        "Leases of a class that fit on a drone right now.",
        status
            .drones
            .iter()
            .flat_map(|d| {
                d.free_slots.iter().map(move |(c, n)| {
                    (
                        format!("drone=\"{}\",class=\"{c}\"", label_value(&d.name)),
                        f64::from(*n),
                    )
                })
            })
            .collect(),
    );
    gauge(
        "borg_queued_borg_placeable",
        "Queued jobs borg has to place: queued minus idle non-borg runners with the label (estimate).",
        both(&|s, sc| f64::from(s.demand(sc).borg_placeable)),
    );
    gauge(
        "borg_queued_claimable_by_static",
        "Queued jobs idle non-borg runners with the label can take (estimate).",
        both(&|s, sc| f64::from(s.demand(sc).claimable_by_static_estimate)),
    );
    gauge(
        "borg_active_leases",
        "Active leases per class and state.",
        lease_rows,
    );
    let count = |f: &dyn Fn(&DroneStatus) -> bool| {
        f64::from(u32::try_from(status.drones.iter().filter(|d| f(d)).count()).unwrap_or(0))
    };
    gauge(
        "borg_drones",
        "Enrolled drones by state. fresh and stale split by heartbeat age; quarantined \
         counts operator-quarantined drones regardless of heartbeat.",
        vec![
            ("state=\"fresh\"".to_owned(), count(&|d| d.fresh)),
            ("state=\"stale\"".to_owned(), count(&|d| !d.fresh)),
            (
                "state=\"quarantined\"".to_owned(),
                count(&|d| d.quarantined),
            ),
        ],
    );
    gauge(
        "borg_drone_quarantined",
        "1 while the drone is quarantined and gets no new leases.",
        status
            .drones
            .iter()
            .map(|d| {
                (
                    format!("drone=\"{}\"", label_value(&d.name)),
                    f64::from(u8::from(d.quarantined)),
                )
            })
            .collect(),
    );
    gauge(
        "borg_drone_health_state",
        "1 for the drone's current evidence-based health state (healthy, recommended, quarantined, probation).",
        status
            .drones
            .iter()
            .filter_map(|d| d.health.as_ref().map(|h| (d, h)))
            .flat_map(|(d, h)| {
                HealthState::ALL.into_iter().map(move |st| {
                    (
                        format!(
                            "drone=\"{}\",state=\"{}\"",
                            label_value(&d.name),
                            st.as_str()
                        ),
                        f64::from(u8::from(h.state == st)),
                    )
                })
            })
            .collect(),
    );
    gauge(
        "borg_drone_cpu_psi_pm",
        "Fresh CPU pressure (`some avg10`, permille) reported by the drone; absent when unknown or admission is off.",
        status
            .drones
            .iter()
            .filter_map(|d| {
                let psi = d.admission.as_ref()?.cpu_psi_pm?;
                Some((
                    format!("drone=\"{}\"", label_value(&d.name)),
                    f64::from(psi),
                ))
            })
            .collect(),
    );
    gauge(
        "borg_drone_cpu_load_pm",
        "CPU pressure (permille) estimated from the drone's load average; admission uses it when PSI is absent.",
        status
            .drones
            .iter()
            .filter_map(|d| {
                let load = d.admission.as_ref()?.cpu_load_pm?;
                Some((
                    format!("drone=\"{}\"", label_value(&d.name)),
                    f64::from(load),
                ))
            })
            .collect(),
    );
    gauge(
        "borg_drone_admission",
        "1 for the drone's CPU-pressure admission state (open, pr_blocked, light_only, closed).",
        status
            .drones
            .iter()
            .filter_map(|d| d.admission.as_ref().map(|a| (d, a)))
            .flat_map(|(d, a)| {
                AdmissionState::ALL.into_iter().map(move |st| {
                    (
                        format!(
                            "drone=\"{}\",state=\"{}\"",
                            label_value(&d.name),
                            st.as_str()
                        ),
                        f64::from(u8::from(a.state == st)),
                    )
                })
            })
            .collect(),
    );
    gauge(
        "borg_drone_host_failure_score",
        "Decayed host-caused failure evidence per drone (1 = one fresh failure).",
        status
            .drones
            .iter()
            .filter_map(|d| {
                d.health.as_ref().map(|h| {
                    (
                        format!("drone=\"{}\"", label_value(&d.name)),
                        f64::from(h.score_permille) / 1000.0,
                    )
                })
            })
            .collect(),
    );
    gauge(
        "borg_health_systemic",
        "1 while host failures look fleet-wide and automatic quarantine is paused.",
        vec![(String::new(), f64::from(u8::from(status.health_systemic)))],
    );
    gauge(
        "borg_drone_info",
        "The borg version each drone reported (always 1).",
        status
            .drones
            .iter()
            .filter_map(|d| {
                d.version.as_ref().map(|v| {
                    (
                        format!(
                            "drone=\"{}\",version=\"{}\"",
                            label_value(&d.name),
                            label_value(v)
                        ),
                        1.0,
                    )
                })
            })
            .collect(),
    );
    gauge(
        "borg_rollout_target_info",
        "The configured target borg version (always 1; absent when unset).",
        status
            .rollout
            .target_version
            .iter()
            .map(|v| (format!("version=\"{}\"", label_value(v)), 1.0))
            .collect(),
    );
    gauge(
        "borg_rollout_phase",
        "1 for the quorum rollout's current phase (canary, wave, complete, halted); absent without a gated rollout.",
        status
            .rollout
            .gate
            .iter()
            .flat_map(|g| {
                RolloutPhase::ALL.into_iter().map(move |p| {
                    (
                        format!("phase=\"{}\"", p.as_str()),
                        f64::from(u8::from(g.state.phase == p)),
                    )
                })
            })
            .collect(),
    );
    gauge(
        "borg_rollout_members",
        "Drones currently told to run the target version (gated rollout).",
        status
            .rollout
            .gate
            .iter()
            .map(|g| {
                (
                    String::new(),
                    f64::from(u32::try_from(g.state.members.len()).unwrap_or(0)),
                )
            })
            .collect(),
    );
    gauge(
        "borg_rollout_jobs_observed",
        "Jobs observed since the drones got the target version, by group (gated rollout).",
        status
            .rollout
            .gate
            .iter()
            .flat_map(|g| {
                [("new", g.new_group.jobs), ("control", g.control_group.jobs)]
                    .map(|(group, n)| (format!("group=\"{group}\""), f64::from(n)))
            })
            .collect(),
    );
    gauge(
        "borg_rollout_drones",
        "Drones by rollout state against the target version (absent when no target is set).",
        if status.rollout.target_version.is_some() {
            [
                ("current", status.rollout.current),
                ("outdated", status.rollout.outdated),
                ("unknown", status.rollout.unknown),
            ]
            .map(|(state, n)| (format!("state=\"{state}\""), f64::from(n)))
            .to_vec()
        } else {
            Vec::new()
        },
    );
    gauge(
        "borg_warm_floor",
        "Idle runners the queen keeps warm per class and scope right now (base floors plus the schedule).",
        status
            .warm_floors
            .iter()
            .map(|f| {
                (
                    format!("class=\"{}\",scope=\"{}\"", f.class, scope_name(f.scope)),
                    f64::from(f.count),
                )
            })
            .collect(),
    );
    gauge(
        "borg_demand_pressure",
        "Demand pressure per class (0-1000) published to drones for their bids.",
        status
            .pressure
            .iter()
            .map(|p| (format!("class=\"{}\"", p.class), f64::from(p.pressure)))
            .collect(),
    );
    gauge(
        "borg_warm_floor_dynamic",
        "The warm-floor controller's own level per class and scope (0 when idle or off).",
        status
            .homeostasis
            .iter()
            .map(|r| {
                (
                    format!("class=\"{}\",scope=\"{}\"", r.class, scope_name(r.scope)),
                    f64::from(r.level),
                )
            })
            .collect(),
    );
    gauge(
        "borg_warm_floor_changes_last_hour",
        "Warm-floor controller changes in the last hour per class and scope.",
        status
            .homeostasis
            .iter()
            .map(|r| {
                (
                    format!("class=\"{}\",scope=\"{}\"", r.class, scope_name(r.scope)),
                    f64::from(r.changes_last_hour),
                )
            })
            .collect(),
    );
    gauge(
        "borg_drone_profile_speed",
        "Learned speed of a drone for a class against the fleet median (1 = fleet speed).",
        status
            .profiles
            .iter()
            .map(|p| {
                (
                    format!("drone=\"{}\",class=\"{}\"", label_value(&p.drone), p.class),
                    f64::from(p.speed_permille) / 1000.0,
                )
            })
            .collect(),
    );
    gauge(
        "borg_drone_profile_reliability",
        "Learned reliability of a drone for a class (1 = no host-caused failures).",
        status
            .profiles
            .iter()
            .map(|p| {
                (
                    format!("drone=\"{}\",class=\"{}\"", label_value(&p.drone), p.class),
                    f64::from(p.reliability_permille) / 1000.0,
                )
            })
            .collect(),
    );
    let reclaim_rows = |f: &dyn Fn(&ReclaimStats) -> Option<f64>| -> Vec<(String, f64)> {
        status
            .drones
            .iter()
            .filter_map(|d| {
                d.reclaim
                    .as_ref()
                    .and_then(f)
                    .map(|v| (format!("drone=\"{}\"", label_value(&d.name)), v))
            })
            .collect()
    };
    #[allow(clippy::cast_precision_loss, reason = "byte counts far below 2^52")]
    {
        gauge(
            "borg_reclaim_bytes_total",
            "Bytes the drone's build-box reclaim really deleted since it started.",
            reclaim_rows(&|r| Some(r.reclaimed_bytes_total as f64)),
        );
        gauge(
            "borg_reclaim_would_reclaim_bytes",
            "Bytes the last dry-run reclaim cycle would have deleted.",
            reclaim_rows(&|r| Some(r.last_run_would_reclaim_bytes as f64)),
        );
    }
    gauge(
        "borg_reclaim_disk_usage_percent",
        "Disk usage the drone's last reclaim cycle saw.",
        reclaim_rows(&|r| r.usage_pct.map(f64::from)),
    );
    gauge(
        "borg_reclaim_errors",
        "Deletions that failed in the drone's last reclaim cycle.",
        reclaim_rows(&|r| Some(f64::from(r.last_run_errors))),
    );
    #[allow(
        clippy::cast_precision_loss,
        reason = "unix timestamps fit f64 exactly"
    )]
    gauge(
        "borg_reclaim_last_run_timestamp_seconds",
        "Unix time of the drone's last reclaim cycle.",
        reclaim_rows(&|r| r.last_run_at.map(|t| t as f64)),
    );
    gauge(
        "borg_wake_attempts_total",
        "Wake-on-LAN packets sent per host since the queen started.",
        status
            .wake
            .attempts
            .iter()
            .map(|(h, n)| {
                #[allow(clippy::cast_precision_loss, reason = "counts far below 2^52")]
                let v = *n as f64;
                (format!("host=\"{}\"", label_value(h)), v)
            })
            .collect(),
    );
    gauge(
        "borg_wake_unmet_seconds",
        "How long demand for a class has stayed unmet (wake-on-LAN trigger).",
        status
            .wake
            .unmet_secs
            .iter()
            .map(|(c, s)| {
                #[allow(clippy::cast_precision_loss, reason = "durations far below 2^52")]
                let v = *s as f64;
                (format!("class=\"{c}\""), v)
            })
            .collect(),
    );
    gauge(
        "borg_units",
        "Work units by state.",
        status
            .units
            .counts
            .iter()
            .map(|(s, n)| (format!("state=\"{s}\""), f64::from(*n)))
            .collect(),
    );
    #[allow(clippy::cast_precision_loss, reason = "counts and ages far below 2^52")]
    {
        gauge(
            "borg_unit_joins_total",
            "Unit submissions that joined an identical running unit since the queen started.",
            vec![(String::new(), status.units.joins as f64)],
        );
        gauge(
            "borg_unit_receipt_hits_total",
            "Unit submissions answered with a stored receipt since the queen started.",
            vec![(String::new(), status.units.receipt_hits as f64)],
        );
        gauge(
            "borg_units_superseded_total",
            "Work units cancelled by a newer submission with the same supersede key since the queen started.",
            vec![(String::new(), status.units.superseded as f64)],
        );
        gauge(
            "borg_unit_oldest_queued_age_seconds",
            "Age of the oldest queued work unit.",
            vec![(String::new(), status.units.oldest_queued_age_secs as f64)],
        );
    }
    if let Some(j) = &status.janitor {
        let t = &j.total;
        for (field, help, v) in [
            (
                "considered",
                "Pull-request runs the janitor examined",
                t.considered,
            ),
            (
                "superseded",
                "Runs whose head was no longer the pull request's head",
                t.superseded,
            ),
            (
                "closed_pr",
                "Runs of listed workflows whose pull request was closed or merged",
                t.closed_pr,
            ),
            (
                "superseded_push",
                "Queued push runs with a newer run of the same workflow on the branch",
                t.superseded_push,
            ),
            (
                "cancelled",
                "Queued superseded, closed-PR or superseded-push runs the janitor cancelled",
                t.cancelled,
            ),
            (
                "cancelled_running",
                "Running superseded or closed-PR runs the janitor cancelled",
                t.cancelled_running,
            ),
            (
                "duplicates",
                "Same-workflow same-commit runs beyond the one kept",
                t.duplicates,
            ),
            (
                "dedup_cancelled",
                "Queued duplicate runs the janitor cancelled",
                t.dedup_cancelled,
            ),
            (
                "would_cancel",
                "Runs a dry-run janitor would have cancelled",
                t.would_cancel,
            ),
            (
                "deferred",
                "Runs left for a later pass by the per-pass cap",
                t.deferred,
            ),
            (
                "raced",
                "Runs that changed state before the janitor cancelled them",
                t.raced,
            ),
            (
                "uncancellable",
                "Runs whose cancel GitHub answered with 409",
                t.uncancellable,
            ),
            (
                "errors",
                "Janitor per-run or per-pull-request failures",
                t.errors,
            ),
        ] {
            gauge(
                &format!("borg_janitor_{field}_total"),
                &format!("{help}, summed over passes since the queen started."),
                vec![(String::new(), f64::from(v))],
            );
        }
    }
    if let Some(r) = &status.requeue {
        let t = &r.total;
        for (field, help, v) in [
            ("checked", "Preempted jobs the requeuer examined", t.checked),
            ("rerun", "Preempted jobs re-run", t.rerun),
            (
                "waiting",
                "Preempted jobs whose run had not completed",
                t.waiting,
            ),
            (
                "stale",
                "Preempted jobs older than 24 hours, skipped",
                t.stale,
            ),
            (
                "terminal",
                "Preempted jobs GitHub refused to re-run (403, 404 or 409)",
                t.terminal,
            ),
            (
                "errors",
                "Requeue failures retried on a later pass",
                t.errors,
            ),
        ] {
            gauge(
                &format!("borg_requeue_{field}_total"),
                &format!("{help}, summed over passes since the queen started."),
                vec![(String::new(), f64::from(v))],
            );
        }
    }
    gauge(
        "borg_reservations",
        "Drones held back for starving demand.",
        vec![(
            String::new(),
            f64::from(u32::try_from(status.reserved_for.len()).unwrap_or(0)),
        )],
    );
    #[allow(
        clippy::cast_precision_loss,
        reason = "unix timestamps fit f64 exactly"
    )]
    gauge(
        "borg_last_tick_timestamp_seconds",
        "Unix time of the last reconciliation pass.",
        vec![(String::new(), status.last_tick_at.unwrap_or(0) as f64)],
    );
    let cache_rows = |f: &dyn Fn(&CacheStat) -> Option<f64>| -> Vec<(String, f64)> {
        status
            .drones
            .iter()
            .flat_map(|d| {
                d.caches.iter().filter_map(move |c| {
                    f(c).map(|v| {
                        (
                            format!(
                                "drone=\"{}\",cache=\"{}\"",
                                label_value(&d.name),
                                label_value(&c.name)
                            ),
                            v,
                        )
                    })
                })
            })
            .collect()
    };
    #[allow(clippy::cast_precision_loss, reason = "byte counts far below 2^52")]
    {
        gauge(
            "borg_cache_size_bytes",
            "Size of a drone host cache.",
            cache_rows(&|c| Some(c.size_bytes as f64)),
        );
        gauge(
            "borg_cache_max_bytes",
            "Size cap of a drone host cache.",
            cache_rows(&|c| c.max_bytes.map(|v| v as f64)),
        );
        gauge(
            "borg_cache_evicted_bytes",
            "Bytes evicted from a drone host cache by garbage collection.",
            cache_rows(&|c| Some(c.evicted_bytes as f64)),
        );
        gauge(
            "borg_cache_hits",
            "Leases that were handed a git mirror.",
            cache_rows(&|c| c.hits.map(|v| v as f64)),
        );
        gauge(
            "borg_cache_misses",
            "Leases that ran without a git mirror.",
            cache_rows(&|c| c.misses.map(|v| v as f64)),
        );
    }
    gauge(
        "borg_last_tick_ok",
        "1 if the last reconciliation pass had no error.",
        vec![(
            String::new(),
            if status.last_error.is_none() {
                1.0
            } else {
                0.0
            },
        )],
    );
    let _ = writeln!(
        out,
        "# HELP borg_preemptions_total Leases cancelled for higher-priority demand.\n# TYPE borg_preemptions_total counter"
    );
    for (from, to, n) in &status.preemption_counts {
        let _ = writeln!(
            out,
            "borg_preemptions_total{{from_scope=\"{}\",to_scope=\"{}\"}} {n}",
            from.as_str(),
            to.as_str()
        );
    }
    out
}

/// Escape a Prometheus label value.
fn label_value(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn scope_name(s: Scope) -> &'static str {
    s.as_str()
}

async fn metrics(State(queen): State<Arc<Queen>>) -> Result<Response, ApiError> {
    let status = build_status(&queen, OffsetDateTime::now_utc().unix_timestamp()).await?;
    Ok((
        [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
        render_metrics(&status),
    )
        .into_response())
}

/// Run the reconciliation loop and the HTTP server until `shutdown` resolves.
pub async fn serve(
    queen: Arc<Queen>,
    listener: tokio::net::TcpListener,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
    let stop_rx_janitor = stop_rx.clone();
    let ticker = {
        let queen = Arc::clone(&queen);
        tokio::spawn(async move {
            let mut every = tokio::time::interval(std::time::Duration::from_secs(
                queen.config().poll_interval_secs,
            ));
            every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                // Both arms are cancel-safe: `interval.tick` and `changed` hold
                // no state that a dropped future could lose. The tick itself
                // runs in the arm body, outside the select, so shutdown waits
                // for an in-progress pass instead of abandoning a mint.
                tokio::select! {
                    _ = every.tick() => {
                        let now = OffsetDateTime::now_utc();
                        if let Err(e) = queen.tick(now).await {
                            tracing::warn!("tick failed: {e}");
                            queen.record_tick_error(now, &e);
                        }
                    }
                    _ = stop_rx.changed() => break,
                }
            }
        })
    };
    let janitor_task = queen.janitor().map(|_| {
        let queen = Arc::clone(&queen);
        let mut stop_rx = stop_rx_janitor;
        tokio::spawn(async move {
            let Some(janitor) = queen.janitor() else {
                return;
            };
            let mut every = tokio::time::interval(std::time::Duration::from_secs(
                janitor.settings().interval_secs,
            ));
            every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                // Cancel-safe for the same reason as the ticker above.
                tokio::select! {
                    _ = every.tick() => {
                        janitor.pass(&queen.config().repos).await;
                        if let Some(requeuer) = queen.requeuer() {
                            let now = OffsetDateTime::now_utc().unix_timestamp();
                            if let Err(e) = requeuer.pass(queen.store(), now).await {
                                tracing::warn!("requeue pass failed: {e}");
                            }
                        }
                    }
                    _ = stop_rx.changed() => break,
                }
            }
        })
    });
    let served = axum::serve(
        listener,
        router(queen).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown.await;
        let _ = stop_tx.send(true);
    })
    .await;
    let _ = ticker.await;
    if let Some(task) = janitor_task {
        let _ = task.await;
    }
    served
}
