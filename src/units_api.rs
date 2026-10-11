//! HTTP endpoints for work units; see [`crate::units`].
//!
//! Clients (bearer: client token): `POST /v1/units`, `GET /v1/units/{id}`,
//! `GET /v1/units/{id}/log?from=N&wait=S`, `POST /v1/units/{id}/cancel`.
//! Drones (bearer: drone token): `POST /v1/units/{id}/log?offset=N` and
//! `POST /v1/units/{id}/result`; units reach a drone in its heartbeat
//! response.

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::api::{ApiError, authenticate};
use crate::enroll::hash_token;
use crate::queen::Queen;
use crate::units::{BundleStored, Receipt, SubmitUnit, Submitted, Unit};

/// Longest a log long-poll waits.
const MAX_WAIT_SECS: u64 = 30;
/// Most log bytes returned by one read.
const READ_CHUNK: usize = 256 * 1024;

pub fn routes() -> Router<Arc<Queen>> {
    Router::new()
        .route("/v1/units", post(submit))
        .route("/v1/units/{id}", get(show))
        .route("/v1/units/{id}/log", get(read_log).post(append_log))
        .route("/v1/units/{id}/bundle", put(put_bundle).get(get_bundle))
        .route("/v1/units/{id}/cancel", post(cancel))
        .route("/v1/units/{id}/result", post(result))
}

fn now() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

fn enabled(queen: &Queen) -> Result<(), ApiError> {
    if queen.config().units.enabled {
        Ok(())
    } else {
        Err(ApiError::NotFound)
    }
}

async fn authenticate_client(queen: &Queen, headers: &HeaderMap) -> Result<String, ApiError> {
    let presented = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(ApiError::Unauthorized)?;
    queen
        .store()
        .client_for_token_hash(&hash_token(presented))
        .await?
        .ok_or(ApiError::Unauthorized)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SubmitResponse {
    /// `new`, `joined` or `receipt`.
    pub outcome: String,
    pub unit_id: String,
    pub state: String,
    pub receipt: Option<Receipt>,
}

async fn submit(
    State(queen): State<Arc<Queen>>,
    headers: HeaderMap,
    Json(req): Json<SubmitUnit>,
) -> Result<(StatusCode, Json<SubmitResponse>), ApiError> {
    enabled(&queen)?;
    let client = authenticate_client(&queen, &headers).await?;
    req.validate().map_err(ApiError::BadRequest)?;
    let cfg = &queen.config().units;
    let timeout = req
        .timeout_secs
        .unwrap_or(cfg.default_timeout_secs)
        .clamp(1, cfg.max_timeout_secs);
    let store = queen.store();
    let t = now();
    // Joining or reusing a result costs nothing, so it is allowed even when the
    // client is at its limit of queued units.
    if store.client_active_units(&client).await? >= cfg.max_queued_per_client
        && store
            .find_reusable_unit(&req, cfg.receipt_ttl_secs, t)
            .await?
            .is_none()
    {
        return Err(ApiError::TooMany);
    }
    let id = crate::queen::new_unit_id();
    let submitted = store
        .submit_unit(&req, &client, &id, timeout, cfg.receipt_ttl_secs, t)
        .await?;
    let (outcome, unit, status) = match submitted {
        Submitted::New(u) => ("new", u, StatusCode::CREATED),
        Submitted::Joined(u) => {
            queen.count_unit_join();
            ("joined", u, StatusCode::OK)
        }
        Submitted::Receipt(u) => {
            queen.count_unit_receipt();
            ("receipt", u, StatusCode::OK)
        }
    };
    tracing::info!(
        "unit {} {outcome} for {} {} by {client}",
        unit.id,
        unit.repo,
        &unit.tree_sha[..unit.tree_sha.len().min(12)]
    );
    Ok((
        status,
        Json(SubmitResponse {
            outcome: outcome.to_owned(),
            unit_id: unit.id.clone(),
            state: unit.state.as_str().to_owned(),
            receipt: unit.receipt(),
        }),
    ))
}

#[derive(Debug, Serialize)]
pub struct UnitView {
    #[serde(flatten)]
    pub unit: Unit,
    pub receipt: Option<Receipt>,
}

async fn show(
    State(queen): State<Arc<Queen>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<UnitView>, ApiError> {
    enabled(&queen)?;
    authenticate_client(&queen, &headers).await?;
    let unit = queen.store().unit(&id).await?.ok_or(ApiError::NotFound)?;
    let receipt = unit.receipt();
    Ok(Json(UnitView { unit, receipt }))
}

async fn cancel(
    State(queen): State<Arc<Queen>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    enabled(&queen)?;
    authenticate_client(&queen, &headers).await?;
    if queen.store().cancel_unit(&id, now()).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound)
    }
}

#[derive(Debug, Deserialize)]
struct LogQuery {
    from: Option<u64>,
    wait: Option<u64>,
}

/// Log bytes from `from`. Waits up to `wait` seconds (at most 30) for new
/// bytes while the unit is still active. Headers: `x-borg-next` (offset to
/// ask for next), `x-borg-state`, and `x-borg-exit` once finished.
async fn read_log(
    State(queen): State<Arc<Queen>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<LogQuery>,
) -> Result<Response, ApiError> {
    enabled(&queen)?;
    authenticate_client(&queen, &headers).await?;
    let from = q.from.unwrap_or(0);
    let deadline =
        tokio::time::Instant::now() + Duration::from_secs(q.wait.unwrap_or(0).min(MAX_WAIT_SECS));
    loop {
        let unit = queen.store().unit(&id).await?.ok_or(ApiError::NotFound)?;
        let data = queen.store().read_unit_log(&id, from, READ_CHUNK).await?;
        let finished = !unit.state.is_active();
        // Fully drained and finished, or data to hand over, or out of time.
        if !data.is_empty() || finished || tokio::time::Instant::now() >= deadline {
            let next = from + u64::try_from(data.len()).unwrap_or(0);
            let mut resp = (StatusCode::OK, data).into_response();
            let h = resp.headers_mut();
            let mut put = |k: &'static str, v: String| {
                if let Ok(v) = v.parse() {
                    h.insert(k, v);
                }
            };
            put("x-borg-next", next.to_string());
            put("x-borg-state", unit.state.as_str().to_owned());
            // Only report the end once every byte has been handed out.
            if finished && next >= unit.log_bytes {
                if let Some(code) = unit.exit_code {
                    put("x-borg-exit", code.to_string());
                }
                if let Some(why) = &unit.failure {
                    put("x-borg-failure", why.clone());
                }
            }
            return Ok(resp);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[derive(Debug, Deserialize)]
struct AppendQuery {
    offset: u64,
}

async fn drone_owns(queen: &Queen, drone: &str, id: &str) -> Result<Unit, ApiError> {
    let unit = queen.store().unit(id).await?.ok_or(ApiError::NotFound)?;
    if unit.drone.as_deref() == Some(drone) {
        Ok(unit)
    } else {
        Err(ApiError::NotFound)
    }
}

async fn append_log(
    State(queen): State<Arc<Queen>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<AppendQuery>,
    body: Bytes,
) -> Result<Response, ApiError> {
    enabled(&queen)?;
    let drone = authenticate(&queen, &headers).await?;
    drone_owns(&queen, &drone, &id).await?;
    let max = queen.config().units.max_log_bytes;
    match queen
        .store()
        .append_unit_log(&id, q.offset, &body, max)
        .await?
    {
        Some(len) => Ok((StatusCode::OK, len.to_string()).into_response()),
        // The drone is ahead of what the queen has: it must resend from the
        // length the queen holds.
        None => Ok(StatusCode::CONFLICT.into_response()),
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UnitResult {
    pub exit_code: i32,
    pub duration_secs: u64,
}

async fn result(
    State(queen): State<Arc<Queen>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<UnitResult>,
) -> Result<StatusCode, ApiError> {
    enabled(&queen)?;
    let drone = authenticate(&queen, &headers).await?;
    drone_owns(&queen, &drone, &id).await?;
    if queen
        .store()
        .finish_unit(&id, &drone, req.exit_code, req.duration_secs, now())
        .await?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound)
    }
}

/// `PUT /v1/units/{id}/bundle`: the client uploads a git bundle holding the
/// objects of a tree that is not on any remote (a unit submitted with
/// `bundle: true`). Authenticated as the unit's client and capped at
/// `units.max_bundle_bytes`; deleted with the unit when its receipt expires (or
/// at once if the unit fails or is cancelled).
async fn put_bundle(
    State(queen): State<Arc<Queen>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Body,
) -> Result<StatusCode, ApiError> {
    enabled(&queen)?;
    let client = authenticate_client(&queen, &headers).await?;
    let cap = queen.config().units.max_bundle_bytes;
    let declared = headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > cap) {
        return Err(ApiError::TooLarge);
    }
    let data = axum::body::to_bytes(body, usize::try_from(cap).unwrap_or(usize::MAX))
        .await
        .map_err(|_| ApiError::TooLarge)?;
    if data.is_empty() {
        return Err(ApiError::BadRequest("empty bundle".into()));
    }
    match queen
        .store()
        .put_unit_bundle(&id, &client, &data, now())
        .await?
    {
        BundleStored::Stored => Ok(StatusCode::NO_CONTENT),
        BundleStored::NotFound => Err(ApiError::NotFound),
        BundleStored::TooLate => Err(ApiError::Conflict),
    }
}

/// `GET /v1/units/{id}/bundle`: the drone running the unit fetches the bundle
/// when the tree is not in its git mirror.
async fn get_bundle(
    State(queen): State<Arc<Queen>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    enabled(&queen)?;
    let drone = authenticate(&queen, &headers).await?;
    drone_owns(&queen, &drone, &id).await?;
    let data = queen
        .store()
        .unit_bundle(&id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(([("content-type", "application/octet-stream")], data).into_response())
}
