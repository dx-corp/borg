//! HTTP surface of the queen.
//!
//! * `GET /status` and `GET /metrics` are read-only and unauthenticated; bind
//!   them to a private interface.
//! * `POST /v1/heartbeat` and `POST /v1/leases/{id}/result` are the drone API,
//!   authenticated with `Authorization: Bearer <enrollment token>`.
//!
//! The queen speaks plain HTTP; terminate TLS in front of it.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::allocator::{DroneState, HostCaps, Reservation, Unmet};
use crate::classes::{Class, Scope};
use crate::enroll::hash_token;
use crate::probe::Capacity;
use crate::queen::{Queen, QueenError, QueueEntry};
use crate::secret::Secret;
use crate::store::{LeaseResult, LeaseStatus, StoreError};

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
}

fn yes() -> bool {
    true
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LeaseGrant {
    pub lease_id: String,
    pub class: Class,
    pub scope: Scope,
    pub labels: Vec<String>,
    pub runner_name: String,
    pub jit_config: Secret,
    pub expires_at: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HeartbeatResponse {
    pub leases: Vec<LeaseGrant>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ResultRequest {
    pub exit_code: i32,
    pub duration_secs: u64,
    #[serde(default)]
    pub peak_rss_mib: Option<u64>,
}

#[derive(Debug)]
pub enum ApiError {
    Unauthorized,
    NotFound,
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
            Self::Unauthorized => StatusCode::UNAUTHORIZED.into_response(),
            Self::NotFound => StatusCode::NOT_FOUND.into_response(),
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
        .route("/v1/heartbeat", post(heartbeat))
        .route("/v1/leases/{id}/result", post(lease_result))
        .with_state(queen)
}

/// The drone name behind the request's bearer token.
async fn authenticate(queen: &Queen, headers: &HeaderMap) -> Result<String, ApiError> {
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

async fn heartbeat(
    State(queen): State<Arc<Queen>>,
    headers: HeaderMap,
    Json(req): Json<HeartbeatRequest>,
) -> Result<Json<HeartbeatResponse>, ApiError> {
    let drone = authenticate(&queen, &headers).await?;
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let store = queen.store();
    store
        .record_heartbeat(&drone, &req.capacity, &req.caps, req.accepting, now)
        .await?;
    store.acknowledge(&drone, &req.active_leases).await?;
    let leases = store
        .undelivered(&drone)
        .await?
        .into_iter()
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
    Ok(Json(HeartbeatResponse { leases }))
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
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound)
    }
}

#[derive(Debug, Serialize)]
pub struct ClassStatus {
    pub queued_main: u32,
    pub queued_pr: u32,
    pub oldest_age_secs_main: u64,
    pub oldest_age_secs_pr: u64,
    /// Leases of this class the fleet could hold if empty.
    pub slots: u32,
    pub leases_pending: u32,
    pub leases_running: u32,
}

#[derive(Debug, Serialize)]
pub struct DroneStatus {
    pub name: String,
    pub heartbeat_age_secs: Option<i64>,
    pub fresh: bool,
    pub accepting: bool,
    pub hostname: Option<String>,
    pub kind: Option<crate::probe::HostKind>,
    pub vcpu: Option<u32>,
    pub mem_mib: Option<u64>,
    pub active_leases: u32,
}

#[derive(Debug, Serialize)]
pub struct Status {
    pub now: i64,
    pub last_tick_at: Option<i64>,
    pub last_error: Option<String>,
    pub classes: BTreeMap<Class, ClassStatus>,
    pub queue: Vec<QueueEntry>,
    pub drones: Vec<DroneStatus>,
    pub unmet: Vec<Unmet>,
    /// Drones held back for starving demand.
    pub reserved_for: Vec<Reservation>,
}

pub async fn build_status(queen: &Queen, now: i64) -> Result<Status, StoreError> {
    let snap = queen.snapshot();
    let cfg = queen.config();
    let rows = queen.store().drones().await?;
    let active = queen.store().active_leases().await?;

    let mut states = Vec::new();
    let mut drones = Vec::new();
    for d in &rows {
        let age = d.last_heartbeat.map(|t| now - t);
        let fresh = age.is_some_and(|a| a <= cfg.drone_stale_secs);
        if let (true, Some(cap)) = (fresh, &d.capacity) {
            let mut s = DroneState::from_capacity(d.name.clone(), cap, &d.caps);
            s.accepting = d.accepting;
            states.push(s);
        }
        drones.push(DroneStatus {
            name: d.name.clone(),
            heartbeat_age_secs: age,
            fresh,
            accepting: d.accepting,
            hostname: d.capacity.as_ref().map(|c| c.hostname.clone()),
            kind: d.capacity.as_ref().map(|c| c.kind),
            vcpu: d.capacity.as_ref().map(|c| c.vcpu),
            mem_mib: d.capacity.as_ref().map(|c| c.mem_mib),
            active_leases: u32::try_from(active.iter().filter(|l| l.drone == d.name).count())
                .unwrap_or(u32::MAX),
        });
    }

    let slots = queen.class_slots(&states);
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
                queued_main: entry(Scope::Main).map_or(0, |q| q.queued),
                queued_pr: entry(Scope::Pr).map_or(0, |q| q.queued),
                oldest_age_secs_main: entry(Scope::Main).map_or(0, |q| q.oldest_age_secs),
                oldest_age_secs_pr: entry(Scope::Pr).map_or(0, |q| q.oldest_age_secs),
                slots: slots.get(&class).copied().unwrap_or(0),
                leases_pending: count(LeaseStatus::Pending),
                leases_running: count(LeaseStatus::Running),
            },
        );
    }
    Ok(Status {
        now,
        last_tick_at: snap.last_tick_at,
        last_error: snap.last_error,
        classes,
        queue: snap.queue,
        drones,
        unmet: snap.unmet,
        reserved_for: snap.reserved_for,
    })
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
        both(&|s, sc| {
            f64::from(if sc == Scope::Main {
                s.queued_main
            } else {
                s.queued_pr
            })
        }),
    );
    gauge(
        "borg_oldest_queued_age_seconds",
        "Age of the oldest queued job per class and scope.",
        both(&|s, sc| {
            #[allow(
                clippy::cast_precision_loss,
                reason = "ages are far below 2^52 seconds"
            )]
            let v = (if sc == Scope::Main {
                s.oldest_age_secs_main
            } else {
                s.oldest_age_secs_pr
            }) as f64;
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
        "borg_active_leases",
        "Active leases per class and state.",
        lease_rows,
    );
    gauge(
        "borg_drones",
        "Enrolled drones by freshness.",
        vec![
            (
                "state=\"fresh\"".to_owned(),
                f64::from(
                    u32::try_from(status.drones.iter().filter(|d| d.fresh).count()).unwrap_or(0),
                ),
            ),
            (
                "state=\"stale\"".to_owned(),
                f64::from(
                    u32::try_from(status.drones.iter().filter(|d| !d.fresh).count()).unwrap_or(0),
                ),
            ),
        ],
    );
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
    out
}

fn scope_name(s: Scope) -> &'static str {
    match s {
        Scope::Main => "main",
        Scope::Pr => "pr",
    }
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
    let served = axum::serve(listener, router(queen))
        .with_graceful_shutdown(async move {
            shutdown.await;
            let _ = stop_tx.send(true);
        })
        .await;
    let _ = ticker.await;
    served
}
