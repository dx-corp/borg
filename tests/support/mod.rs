//! A fake GitHub REST API and a running queen, for integration tests.
//! Compiled once per integration suite; individual modules use parts of it.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use aws_lc_rs::encoding::AsDer;
use aws_lc_rs::rsa::{KeyPair, KeySize};
use aws_lc_rs::signature::KeyPair as _;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use base64::Engine as _;
use borg::api;
use borg::config::QueenConfig;
use borg::enroll;
use borg::github::GithubClient;
use borg::probe::{Capacity, HostKind};
use borg::queen::Queen;
use borg::store::Store;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub const INSTALLATION_TOKEN: &str = "ghs_installation_for_tests";
pub const MIRROR_TOKEN: &str = "ghs_mirror_for_tests";
pub const JANITOR_TOKEN: &str = "ghp_janitor_for_tests";
pub const ORG: &str = "example-org";

pub type HttpReply = (u16, Vec<(String, String)>, String);

#[derive(Default)]
pub struct MockState {
    /// Scripted API replies and counts; keys include the request path only.
    pub http_replies: HashMap<String, HttpReply>,
    pub http_requests: HashMap<String, u64>,
    public_pem: String,
    /// Compares answer 403 unless called with the contents-read mirror token.
    pub compare_needs_contents: bool,
    /// Bearer of every compare request.
    pub compare_tokens: Vec<String>,
    /// Queen job listings return only the jobs whose `run_id` matches the run.
    pub jobs_by_run: bool,
    /// Delay each jobs listing, so concurrent listings overlap.
    pub jobs_delay_ms: u64,
    pub jobs_inflight: u32,
    /// Most jobs listings that were in flight at once.
    pub jobs_max_inflight: u32,
    pub queued_runs: Vec<u64>,
    /// Queen jobs listings of these runs answer with this status instead.
    pub jobs_fail_runs: HashMap<u64, u16>,
    pub jobs: Vec<Value>,
    pub runners: Vec<Value>,
    pub mints: Vec<Value>,
    pub deleted: Vec<u64>,
    pub token_requests: u32,
    /// Bodies of down-scoped token requests (the mirror App's).
    pub scoped_requests: Vec<Value>,
    /// Lifetime of issued tokens in seconds (default one hour).
    pub token_ttl_secs: Option<i64>,
    pub jwt_rejections: u32,
    pub fail_mint: bool,
    pub fail_runner_list: bool,
    pub job_requests: u32,
    pub hide_completed_jobs: bool,
    pub job_gate: Option<Arc<tokio::sync::Notify>>,
    /// Queued `pull_request` runs as the janitor lists them.
    pub pr_runs: Vec<Value>,
    /// Current head sha per pull request number.
    pub pulls: HashMap<u64, String>,
    /// Pull requests per head branch as `GET /pulls?head=` lists them,
    /// newest first; a branch without an entry lists none.
    pub pulls_by_head: HashMap<String, Vec<Value>>,
    /// Pull-request reads answer 403 unless called with the mirror token.
    pulls_need_mirror: bool,
    /// Status the single-run endpoint reports instead of the listed one.
    pub fresh_status: HashMap<u64, String>,
    /// Runs the janitor cancelled.
    pub cancelled: Vec<u64>,
    /// Runs whose cancel answers 409.
    pub uncancellable: Vec<u64>,
    /// Runs whose cancel is accepted but which stay listed (a runner that
    /// never acknowledges the cancel).
    pub stuck_on_cancel: Vec<u64>,
    /// Runs the janitor force-cancelled.
    pub force_cancelled: Vec<u64>,
    /// Compare status per `base...head`; missing pairs answer 404.
    pub compares: HashMap<String, String>,
    /// Jobs the requeuer re-ran.
    pub reruns: Vec<u64>,
    /// Status code a job's rerun answers instead of 201.
    pub rerun_status: HashMap<u64, u16>,
    /// Reads of a single run (janitor or requeuer).
    pub run_reads: u32,
    next_runner_id: u64,
    /// Fake Compute Engine: live instances as `{name, zone, body}`.
    pub gce_instances: Vec<Value>,
    /// Instance names deleted, in order.
    pub gce_deleted: Vec<String>,
    /// `setMetadata` request bodies, in order.
    pub gce_set_metadata: Vec<Value>,
    /// The next instance insert fails with this operation error code.
    pub gce_fail_create: Option<String>,
    /// Operation waits answer with this HTTP status (the VM is still made).
    pub gce_wait_status: Option<u16>,
    /// Instance deletes answer 500 and keep the instance.
    pub gce_fail_delete: bool,
    /// Labeled instance listings served.
    pub gce_lists: u32,
    pub gce_token_requests: u32,
}

async fn scripted_reply(
    State(state): State<Shared>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let reply = {
        let mut state = lock(&state);
        let path = request.uri().path().to_owned();
        *state.http_requests.entry(path.clone()).or_default() += 1;
        state.http_replies.get(&path).cloned()
    };
    if let Some((status, headers, body)) = reply {
        let mut response = axum::http::Response::builder().status(status);
        for (name, value) in headers {
            response = response.header(name, value);
        }
        return response.body(axum::body::Body::from(body)).unwrap();
    }
    next.run(request).await
}

pub type Shared = Arc<Mutex<MockState>>;

fn pem(label: &str, der: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let body: Vec<&str> = b64
        .as_bytes()
        .chunks(64)
        .map(|c| std::str::from_utf8(c).unwrap_or_default())
        .collect();
    format!(
        "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
        body.join("\n")
    )
}

/// A fresh RSA key pair as (private PEM, public PEM).
pub fn generate_key() -> (String, String) {
    let kp = KeyPair::generate(KeySize::Rsa2048).unwrap();
    let private = AsDer::<aws_lc_rs::encoding::Pkcs8V1Der>::as_der(&kp).unwrap();
    let public = AsDer::<aws_lc_rs::encoding::PublicKeyX509Der>::as_der(kp.public_key()).unwrap();
    (
        pem("PRIVATE KEY", private.as_ref()),
        pem("PUBLIC KEY", public.as_ref()),
    )
}

fn lock(s: &Shared) -> MutexGuard<'_, MockState> {
    s.lock().unwrap()
}

#[derive(serde::Deserialize)]
struct Claims {
    iss: String,
    iat: i64,
    exp: i64,
}

fn check_jwt(state: &Shared, headers: &HeaderMap) -> Result<(), StatusCode> {
    let presented = bearer(headers).ok_or(StatusCode::UNAUTHORIZED)?;
    let pem = lock(state).public_pem.clone();
    let mut v = Validation::new(Algorithm::RS256);
    v.set_required_spec_claims(&["exp", "iss"]);
    let ok = DecodingKey::from_rsa_pem(pem.as_bytes())
        .ok()
        .and_then(|k| jsonwebtoken::decode::<Claims>(presented, &k, &v).ok())
        .is_some_and(|d| d.claims.iss == "12345" && d.claims.exp - d.claims.iat <= 600);
    if ok {
        Ok(())
    } else {
        lock(state).jwt_rejections += 1;
        Err(StatusCode::UNAUTHORIZED)
    }
}

fn check_token(headers: &HeaderMap) -> Result<(), StatusCode> {
    (bearer(headers) == Some(INSTALLATION_TOKEN))
        .then_some(())
        .ok_or(StatusCode::UNAUTHORIZED)
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

async fn installation(State(s): State<Shared>, h: HeaderMap) -> Result<Json<Value>, StatusCode> {
    check_jwt(&s, &h)?;
    Ok(Json(json!({"id": 77})))
}

async fn access_token(
    State(s): State<Shared>,
    h: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, StatusCode> {
    check_jwt(&s, &h)?;
    let scoped: Option<Value> = serde_json::from_slice(&body).ok();
    let mut st = lock(&s);
    let ttl = st.token_ttl_secs.unwrap_or(3600);
    let expires = (OffsetDateTime::now_utc() + time::Duration::seconds(ttl))
        .format(&Rfc3339)
        .unwrap();
    let token = match scoped {
        Some(body) => {
            st.scoped_requests.push(body);
            MIRROR_TOKEN
        }
        None => {
            st.token_requests += 1;
            INSTALLATION_TOKEN
        }
    };
    Ok(Json(json!({"token": token, "expires_at": expires})))
}

fn check_janitor(headers: &HeaderMap) -> Result<(), StatusCode> {
    (bearer(headers) == Some(JANITOR_TOKEN))
        .then_some(())
        .ok_or(StatusCode::UNAUTHORIZED)
}

async fn runs(
    State(s): State<Shared>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, StatusCode> {
    if bearer(&h) == Some(JANITOR_TOKEN) {
        let runs: Vec<Value> = lock(&s)
            .pr_runs
            .iter()
            .filter(|r| {
                ["status", "event"].iter().all(|k| {
                    q.get(*k).is_none_or(|v| {
                        // GitHub's `status=success` filters on the conclusion.
                        if *k == "status" && v == "success" {
                            r["conclusion"].as_str() == Some("success")
                        } else {
                            r[*k].as_str() == Some(v)
                        }
                    })
                })
            })
            .cloned()
            .collect();
        return Ok(Json(json!({"workflow_runs": runs})));
    }
    check_token(&h)?;
    let ids: Vec<Value> = if q.get("status").map(String::as_str) == Some("queued") {
        lock(&s)
            .queued_runs
            .iter()
            .map(|id| json!({"id": id}))
            .collect()
    } else {
        vec![]
    };
    Ok(Json(json!({"workflow_runs": ids})))
}

async fn jobs(
    State(s): State<Shared>,
    h: HeaderMap,
    Path((_o, _r, run)): Path<(String, String, u64)>,
) -> Result<Json<Value>, StatusCode> {
    // The janitor reads a run's jobs with its own token; queen jobs carry no run_id.
    let janitor = check_janitor(&h).is_ok();
    if !janitor {
        check_token(&h)?;
    }
    let delay = {
        let mut state = lock(&s);
        state.jobs_inflight += 1;
        state.jobs_max_inflight = state.jobs_max_inflight.max(state.jobs_inflight);
        state.jobs_delay_ms
    };
    if delay > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    }
    let mut state = lock(&s);
    state.jobs_inflight -= 1;
    if !janitor && let Some(&code) = state.jobs_fail_runs.get(&run) {
        return Err(StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR));
    }
    let by_run = janitor || state.jobs_by_run;
    let jobs: Vec<_> = state
        .jobs
        .iter()
        .filter(|job| !state.hide_completed_jobs || job["status"] != "completed")
        .filter(|job| !by_run || job["run_id"] == run)
        .cloned()
        .collect();
    Ok(Json(json!({"jobs": jobs})))
}

async fn one_job(
    State(s): State<Shared>,
    h: HeaderMap,
    Path((_o, _r, id)): Path<(String, String, u64)>,
) -> Result<Json<Value>, StatusCode> {
    check_token(&h)?;
    let (job, gate) = {
        let mut state = lock(&s);
        state.job_requests += 1;
        (
            state
                .jobs
                .iter()
                .find(|job| job["id"].as_u64() == Some(id))
                .cloned(),
            state.job_gate.clone(),
        )
    };
    if let Some(gate) = gate {
        gate.notified().await;
    }
    job.map(Json).ok_or(StatusCode::NOT_FOUND)
}

async fn one_run(
    State(s): State<Shared>,
    h: HeaderMap,
    Path((_o, _r, id)): Path<(String, String, u64)>,
) -> Result<Json<Value>, StatusCode> {
    check_janitor(&h)?;
    let mut st = lock(&s);
    st.run_reads += 1;
    let mut run = st
        .pr_runs
        .iter()
        .find(|r| r["id"].as_u64() == Some(id))
        .cloned()
        .ok_or(StatusCode::NOT_FOUND)?;
    if let Some(status) = st.fresh_status.get(&id) {
        run["status"] = json!(status);
    }
    Ok(Json(run))
}

async fn cancel_run(
    State(s): State<Shared>,
    h: HeaderMap,
    Path((_o, _r, id)): Path<(String, String, u64)>,
) -> StatusCode {
    if check_janitor(&h).is_err() {
        return StatusCode::UNAUTHORIZED;
    }
    let mut st = lock(&s);
    if st.uncancellable.contains(&id) {
        return StatusCode::CONFLICT;
    }
    st.cancelled.push(id);
    if !st.stuck_on_cancel.contains(&id) {
        st.pr_runs.retain(|r| r["id"].as_u64() != Some(id));
    }
    StatusCode::ACCEPTED
}

async fn force_cancel_run(
    State(s): State<Shared>,
    h: HeaderMap,
    Path((_o, _r, id)): Path<(String, String, u64)>,
) -> StatusCode {
    if check_janitor(&h).is_err() {
        return StatusCode::UNAUTHORIZED;
    }
    let mut st = lock(&s);
    st.force_cancelled.push(id);
    st.pr_runs.retain(|r| r["id"].as_u64() != Some(id));
    StatusCode::ACCEPTED
}

async fn compare(
    State(s): State<Shared>,
    h: HeaderMap,
    Path((_o, _r, range)): Path<(String, String, String)>,
) -> Result<Json<Value>, StatusCode> {
    let token = bearer(&h).map(str::to_owned);
    let mut st = lock(&s);
    if st.compare_needs_contents {
        // Like GitHub for a token without `contents: read`.
        if token.as_deref() != Some(MIRROR_TOKEN) {
            return Err(StatusCode::FORBIDDEN);
        }
    } else {
        check_janitor(&h)?;
    }
    st.compare_tokens.push(token.unwrap_or_default());
    st.compares
        .get(&range)
        .map(|status| Json(json!({"status": status})))
        .ok_or(StatusCode::NOT_FOUND)
}

async fn rerun_job(
    State(s): State<Shared>,
    h: HeaderMap,
    Path((_o, _r, id)): Path<(String, String, u64)>,
) -> StatusCode {
    if check_janitor(&h).is_err() {
        return StatusCode::UNAUTHORIZED;
    }
    let mut st = lock(&s);
    if let Some(code) = st.rerun_status.get(&id) {
        return StatusCode::from_u16(*code).unwrap();
    }
    st.reruns.push(id);
    StatusCode::CREATED
}

fn check_pulls(st: &MockState, h: &HeaderMap) -> Result<(), StatusCode> {
    match bearer(h) {
        Some(MIRROR_TOKEN) => Ok(()),
        Some(JANITOR_TOKEN) if !st.pulls_need_mirror => Ok(()),
        Some(JANITOR_TOKEN) => Err(StatusCode::FORBIDDEN),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

async fn pulls_by_head(
    State(s): State<Shared>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, StatusCode> {
    let st = lock(&s);
    check_pulls(&st, &h)?;
    let branch = q
        .get("head")
        .and_then(|v| v.split_once(':'))
        .map(|(_, b)| b)
        .ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
    Ok(Json(Value::Array(
        st.pulls_by_head.get(branch).cloned().unwrap_or_default(),
    )))
}

async fn pull(
    State(s): State<Shared>,
    h: HeaderMap,
    Path((_o, _r, n)): Path<(String, String, u64)>,
) -> Result<Json<Value>, StatusCode> {
    check_pulls(&lock(&s), &h)?;
    let sha = lock(&s)
        .pulls
        .get(&n)
        .cloned()
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(json!({"state": "open", "head": {"sha": sha}})))
}

async fn list_runners(State(s): State<Shared>, h: HeaderMap) -> Result<Json<Value>, StatusCode> {
    check_token(&h)?;
    let st = lock(&s);
    if st.fail_runner_list {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    Ok(Json(
        json!({"total_count": st.runners.len(), "runners": st.runners.clone()}),
    ))
}

async fn jitconfig(
    State(s): State<Shared>,
    h: HeaderMap,
    Json(body): Json<Value>,
) -> Result<(StatusCode, Json<Value>), StatusCode> {
    check_token(&h)?;
    let mut st = lock(&s);
    if st.fail_mint {
        return Err(StatusCode::FORBIDDEN);
    }
    st.next_runner_id += 1;
    let id = st.next_runner_id;
    let name = body["name"].as_str().unwrap_or_default().to_owned();
    st.runners
        .push(json!({"id": id, "name": name, "status": "offline", "busy": false}));
    st.mints.push(body);
    Ok((
        StatusCode::CREATED,
        Json(
            json!({"runner": {"id": id, "name": name}, "encoded_jit_config": format!("jit-{name}")}),
        ),
    ))
}

async fn delete_runner(
    State(s): State<Shared>,
    h: HeaderMap,
    Path((_org, id)): Path<(String, u64)>,
) -> Result<StatusCode, StatusCode> {
    check_token(&h)?;
    let mut st = lock(&s);
    st.deleted.push(id);
    st.runners.retain(|r| r["id"].as_u64() != Some(id));
    Ok(StatusCode::NO_CONTENT)
}

/// Records wake-on-LAN sends instead of using the network.
#[derive(Default)]
pub struct RecordingWake(pub Mutex<Vec<([u8; 6], Vec<std::net::SocketAddrV4>)>>);

impl borg::wake::WakeSender for RecordingWake {
    fn send<'a>(
        &'a self,
        mac: [u8; 6],
        to: &'a [std::net::SocketAddrV4],
    ) -> borg::wake::BoxFuture<'a, Result<Vec<std::net::SocketAddrV4>, String>> {
        self.0.lock().unwrap().push((mac, to.to_vec()));
        let sent = to.to_vec();
        Box::pin(async move { Ok(sent) })
    }
}

pub struct Harness {
    pub wakes: Arc<RecordingWake>,
    pub mock: Shared,
    pub queen: Arc<Queen>,
    pub url: String,
    pub http: reqwest::Client,
    /// The fake GitHub API base and the App key it accepts.
    pub gh_url: String,
    pub private_pem: String,
    /// The fake Compute Engine base (`/token` and `/compute/v1/...`).
    pub gce_url: String,
    /// Holds the live-burst input files for the harness's lifetime.
    _bootstrap: Option<tempfile::TempDir>,
}

pub const GCE_TOKEN: &str = "gce-token-for-tests";
pub const TS_AUTHKEY: &str = "tskey-auth-for-tests";
pub const TOOLCHAIN_SCRIPT: &str = "#!/bin/bash\necho toolchain-for-tests\n";

pub fn capacity() -> Capacity {
    Capacity {
        hostname: "host-a".into(),
        os: "linux".into(),
        arch: "x86_64".into(),
        kind: HostKind::Vm,
        vcpu: 16,
        // 28 GiB budget after the default 2 GiB reserve.
        mem_mib: 30 * 1024,
        free_disk_mib: Some(100_000),
        docker_usable: true,
        mise_version: None,
        git_version: None,
    }
}

impl Harness {
    /// `extra` is merged into the queen config; warm floors default to none.
    pub async fn start(extra: Value) -> Self {
        Self::start_with(extra, borg::tailnet::StaticIdentities::default()).await
    }

    /// Like `start`, with a fake tailnet identity table (callers all connect
    /// from 127.0.0.1) behind `tailnet_enroll` when the config enables it.
    pub async fn start_with(extra: Value, identities: borg::tailnet::StaticIdentities) -> Self {
        Self::start_with_store(extra, identities, Store::open_in_memory().await.unwrap()).await
    }

    pub async fn start_file(extra: Value, path: &std::path::Path) -> Self {
        Self::start_with_store(
            extra,
            borg::tailnet::StaticIdentities::default(),
            Store::open(path).await.unwrap(),
        )
        .await
    }

    async fn start_with_store(
        extra: Value,
        identities: borg::tailnet::StaticIdentities,
        store: Store,
    ) -> Self {
        let mock: Shared = Arc::default();
        let (private_pem, public_pem) = generate_key();
        lock(&mock).public_pem = public_pem;

        let app = Router::new()
            .route("/orgs/{org}/installation", get(installation))
            .route("/app/installations/{id}/access_tokens", post(access_token))
            .route("/repos/{owner}/{repo}/actions/runs", get(runs))
            .route("/repos/{owner}/{repo}/actions/runs/{run}/jobs", get(jobs))
            .route("/repos/{owner}/{repo}/actions/jobs/{id}", get(one_job))
            .route("/repos/{owner}/{repo}/actions/runs/{run}", get(one_run))
            .route(
                "/repos/{owner}/{repo}/actions/runs/{run}/cancel",
                post(cancel_run),
            )
            .route(
                "/repos/{owner}/{repo}/actions/runs/{run}/force-cancel",
                post(force_cancel_run),
            )
            .route("/repos/{owner}/{repo}/compare/{range}", get(compare))
            .route(
                "/repos/{owner}/{repo}/actions/jobs/{id}/rerun",
                post(rerun_job),
            )
            .route("/repos/{owner}/{repo}/pulls", get(pulls_by_head))
            .route("/repos/{owner}/{repo}/pulls/{n}", get(pull))
            .route("/orgs/{org}/actions/runners", get(list_runners))
            .route(
                "/orgs/{org}/actions/runners/generate-jitconfig",
                post(jitconfig),
            )
            .route("/orgs/{org}/actions/runners/{id}", delete(delete_runner))
            .layer(axum::middleware::from_fn_with_state(
                Arc::clone(&mock),
                scripted_reply,
            ))
            .with_state(Arc::clone(&mock));
        let gh_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gh_url = format!("http://{}", gh_listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(gh_listener, app).await });

        // A fake Compute Engine for live burst configs: a token endpoint and
        // the instance calls the queen makes.
        let gce_app = Router::new()
            .route("/token", post(gce_token))
            .route(
                "/compute/v1/projects/{project}/zones/{zone}/instances",
                post(gce_insert),
            )
            .route(
                "/compute/v1/projects/{project}/aggregated/instances",
                get(gce_aggregated),
            )
            .route(
                "/compute/v1/projects/{project}/zones/{zone}/operations/{op}/wait",
                post(gce_wait),
            )
            .route(
                "/compute/v1/projects/{project}/zones/{zone}/instances/{name}",
                get(gce_get).delete(gce_delete),
            )
            .route(
                "/compute/v1/projects/{project}/zones/{zone}/instances/{name}/setMetadata",
                post(gce_set_metadata),
            )
            .with_state(Arc::clone(&mock));
        let gce_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gce_url = format!("http://{}", gce_listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(gce_listener, gce_app).await });

        let mut cfg = json!({
            "github": {"org": ORG, "app_id": "12345", "api_base": gh_url},
            "repos": [format!("{ORG}/app")],
            "db_path": "unused.db",
            "warm_floors": []
        });
        let mut extra = extra;
        let mut bootstrap = None;
        if extra["burst"]["dry_run"] == json!(false) {
            // Live burst: point the queen at the fake API and give it the
            // input files a real queen host would hold.
            let dir = tempfile::tempdir().unwrap();
            let key = dir.path().join("gce-sa.json");
            std::fs::write(
                &key,
                json!({
                    // The RSA key is generated per Harness and only authenticates to its local fake API.
                    // nosemgrep: generic.secrets.security.detected-google-gcm-service-account.detected-google-gcm-service-account
                    "type": "service_account",
                    "client_email": "borg-queen@p.iam.gserviceaccount.com",
                    "private_key": private_pem,
                    "token_uri": format!("{gce_url}/token"),
                })
                .to_string(),
            )
            .unwrap();
            let ts = dir.path().join("drone-ts.key");
            std::fs::write(&ts, format!("{TS_AUTHKEY}\n")).unwrap();
            let toolchain = dir.path().join("toolchain.sh");
            std::fs::write(&toolchain, TOOLCHAIN_SCRIPT).unwrap();
            let gce = &mut extra["burst"]["gce"];
            for (k, v) in [
                ("api_base", json!(format!("{gce_url}/compute/v1"))),
                ("credentials_path", json!(key)),
                ("queen_url", json!("https://queen.example.test")),
                ("tailscale_authkey_path", json!(ts)),
                ("toolchain_script_path", json!(toolchain)),
            ] {
                if gce.get(k).is_none() {
                    gce[k] = v;
                }
            }
            bootstrap = Some(dir);
        }
        for (k, v) in extra.as_object().cloned().unwrap_or_default() {
            cfg[k] = v;
        }
        let cfg = QueenConfig::from_json(&cfg.to_string()).unwrap();
        let github = GithubClient::new(&gh_url, ORG, "12345", private_pem.as_bytes()).unwrap();
        let janitor = cfg.supersede_janitor.clone().map(|settings| {
            let compare = settings.compare_auth.clone();
            let j = borg::janitor::Janitor::new(
                &gh_url,
                borg::secret::Secret::new(JANITOR_TOKEN),
                settings,
            )
            .unwrap();
            match compare {
                Some(c) => j.with_compare_auth(
                    borg::mirror_auth::MirrorAuth::new(
                        GithubClient::new(&gh_url, ORG, &c.app_id, private_pem.as_bytes()).unwrap(),
                        ORG,
                        &c.repositories,
                    )
                    .with_permissions(json!({"contents": "read", "pull_requests": "read"})),
                ),
                None => j,
            }
        });
        let requeuer = cfg
            .supersede_janitor
            .as_ref()
            .filter(|_| cfg.preemption.is_some())
            .map(|settings| {
                borg::requeue::Requeuer::new(
                    &gh_url,
                    borg::secret::Secret::new(JANITOR_TOKEN),
                    settings,
                )
                .unwrap()
            });
        let wakes = Arc::new(RecordingWake::default());
        let mut queen = Queen::new(cfg, store, github).with_wake_sender(wakes.clone());
        if let Some(j) = janitor {
            queen = queen.with_janitor(j);
        }
        if let Some(r) = requeuer {
            queen = queen.with_requeuer(r);
        }
        if let Some(settings) = queen.config().tailnet_enroll.clone() {
            queen = queen.with_tailnet_enroll(borg::tailnet::TailnetEnroll {
                verifier: Arc::new(identities),
                settings,
            });
        }
        if let Some(settings) = queen.config().mirror_auth.clone() {
            let client =
                GithubClient::new(&gh_url, ORG, &settings.app_id, private_pem.as_bytes()).unwrap();
            queen = queen.with_mirror_auth(borg::mirror_auth::MirrorAuth::new(
                client,
                ORG,
                &settings.repositories,
            ));
        }
        if let Some(b) = queen.config().burst.as_ref().filter(|b| b.live()) {
            let g = b.gce();
            let client = borg::gce::GceClient::from_key_file(
                g.credentials_path.as_ref().unwrap(),
                &g.project,
                &g.api_base,
            )
            .unwrap();
            queen = queen.with_gce(client);
        }
        let queen = Arc::new(queen);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let router = api::router(Arc::clone(&queen));
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
        });
        Self {
            wakes,
            mock,
            queen,
            url,
            http: reqwest::Client::new(),
            gh_url,
            private_pem,
            gce_url,
            _bootstrap: bootstrap,
        }
    }

    pub fn state(&self) -> MutexGuard<'_, MockState> {
        lock(&self.mock)
    }

    /// Enroll a drone directly in the store and return its token.
    pub async fn enroll(&self, name: &str) -> String {
        let token = enroll::new_token();
        self.queen
            .store()
            .enroll(name, &enroll::hash_token(&token), 0)
            .await
            .unwrap();
        token
    }

    pub async fn heartbeat(&self, token: &str, active: &[&str]) -> reqwest::Response {
        self.http
            .post(format!("{}/v1/heartbeat", self.url))
            .bearer_auth(token)
            .json(&json!({"capacity": capacity(), "active_leases": active}))
            .send()
            .await
            .unwrap()
    }

    /// Heartbeat reporting `version`.
    pub async fn heartbeat_version(&self, token: &str, version: &str) -> reqwest::Response {
        self.http
            .post(format!("{}/v1/heartbeat", self.url))
            .bearer_auth(token)
            .json(&json!({"capacity": capacity(), "version": version}))
            .send()
            .await
            .unwrap()
    }

    /// Heartbeat with extra request fields (e.g. `bids`) merged in.
    pub async fn heartbeat_with(&self, token: &str, extra: Value) -> Value {
        let mut body = json!({"capacity": capacity()});
        for (k, v) in extra.as_object().cloned().unwrap_or_default() {
            body[k] = v;
        }
        self.http
            .post(format!("{}/v1/heartbeat", self.url))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    pub async fn metrics(&self) -> String {
        self.http
            .get(format!("{}/metrics", self.url))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    }

    pub async fn leases(&self, token: &str, active: &[&str]) -> Vec<Value> {
        let body: Value = self.heartbeat(token, active).await.json().await.unwrap();
        body["leases"].as_array().cloned().unwrap_or_default()
    }

    pub async fn status(&self) -> Value {
        self.http
            .get(format!("{}/status", self.url))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    /// Pull-request reads answer 403 to the janitor's own token from now on.
    pub fn require_mirror_for_pulls(&self) {
        self.state().pulls_need_mirror = true;
    }

    /// Add a queued pull-request run on `sha` for pull request `pr`; `head`
    /// is the pull request's current head.
    pub fn queue_pr_run(&self, id: u64, pr: u64, sha: &str, head: &str) {
        let mut st = self.state();
        st.pulls.insert(pr, head.to_owned());
        st.pr_runs.push(json!({
            "id": id, "status": "queued", "event": "pull_request",
            "head_sha": sha, "pull_requests": [{"number": pr}]
        }));
    }

    /// Add a run of any status or event as the janitor lists it.
    pub fn add_run(&self, id: u64, status: &str, event: &str, sha: &str, pr: Option<u64>) {
        let prs: Vec<Value> = pr.iter().map(|n| json!({"number": n})).collect();
        self.state().pr_runs.push(json!({
            "id": id, "workflow_id": 1, "status": status, "event": event,
            "head_sha": sha, "pull_requests": prs
        }));
    }

    /// Add a promotion run as the janitor lists it.
    pub fn add_promotion_run(&self, run: Value) {
        self.state().pr_runs.push(run);
    }

    /// Add a job record of `run` as the janitor reads it.
    pub fn add_run_job(&self, run: u64, name: &str, status: &str, runner: Option<&str>) {
        let id = 9_000_000 + self.state().jobs.len() as u64;
        self.state().jobs.push(json!({
            "id": id, "run_id": run, "name": name, "status": status,
            "runner_name": runner, "steps": [], "labels": []
        }));
    }

    pub async fn janitor_pass(&self) -> borg::janitor::PassReport {
        let repos = self.queen.config().repos.clone();
        self.queen.janitor().unwrap().pass(&repos).await
    }

    /// Record a preemption of `job` in `run` at unix time `at`.
    pub async fn preempted(&self, lease: &str, run: u64, job: u64, at: i64) {
        use borg::classes::{Class, Scope};
        use borg::store::{LeaseRow, LeaseStatus, PreemptionRow};
        let store = self.queen.store();
        store
            .insert_lease(&LeaseRow {
                id: lease.into(),
                drone: "d1".into(),
                class: Class::Medium,
                scope: Scope::Pr,
                labels: vec![],
                runner_name: format!("borg-d1-{lease}"),
                runner_id: 1,
                jit_config: None,
                state: LeaseStatus::Running,
                created_at: at,
                expires_at: at + 600,
            })
            .await
            .unwrap();
        let row = PreemptionRow {
            lease_id: lease.into(),
            drone: "d1".into(),
            class: Class::Medium,
            scope: Scope::Pr,
            for_scope: Scope::Main,
            repo: format!("{ORG}/app"),
            run_id: run,
            job_id: job,
            at,
            requeued_at: None,
        };
        assert!(store.record_preemption(&row, at).await.unwrap());
    }

    pub async fn requeue_pass(&self) -> borg::requeue::PassReport {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        self.queen
            .requeuer()
            .unwrap()
            .pass(self.queen.store(), now)
            .await
            .unwrap()
    }

    pub async fn tick(&self) {
        self.queen.tick(OffsetDateTime::now_utc()).await.unwrap();
    }

    /// Queue one job of `label`, `age_secs` old.
    pub fn queue_job(&self, id: u64, label: &str, age_secs: i64) {
        let created = (OffsetDateTime::now_utc() - time::Duration::seconds(age_secs))
            .format(&Rfc3339)
            .unwrap();
        let mut st = self.state();
        if st.queued_runs.is_empty() {
            st.queued_runs.push(1);
        }
        st.jobs
            .push(json!({"id": id, "status": "queued", "labels": [label], "created_at": created}));
    }

    pub fn set_runner(&self, name_prefix: &str, status: &str, busy: bool) {
        let mut st = self.state();
        for r in st.runners.iter_mut().filter(|r| {
            r["name"]
                .as_str()
                .is_some_and(|n| n.starts_with(name_prefix))
        }) {
            r["status"] = json!(status);
            r["busy"] = json!(busy);
        }
    }
}

fn gce_check(h: &HeaderMap) -> Result<(), StatusCode> {
    let auth = h
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if auth == format!("Bearer {GCE_TOKEN}") {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

/// The OAuth token exchange: any signed assertion is accepted.
async fn gce_token(State(s): State<Shared>, body: String) -> Result<Json<Value>, StatusCode> {
    if !body.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer")
        || !body.contains("&assertion=ey")
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    lock(&s).gce_token_requests += 1;
    Ok(Json(
        json!({"access_token": GCE_TOKEN, "expires_in": 3600, "token_type": "Bearer"}),
    ))
}

async fn gce_insert(
    State(s): State<Shared>,
    h: HeaderMap,
    Path((_project, zone)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    gce_check(&h)?;
    let mut st = lock(&s);
    let name = body["name"].as_str().unwrap_or_default().to_owned();
    if let Some(code) = st.gce_fail_create.take() {
        return Ok(Json(json!({"name": format!("op-{name}"), "status": "DONE",
            "error": {"errors": [{"code": code, "message": "The zone does not have enough resources"}]}})));
    }
    if st.gce_instances.iter().any(|i| i["name"] == name) {
        return Err(StatusCode::CONFLICT);
    }
    st.gce_instances
        .push(json!({"name": name, "zone": zone, "body": body}));
    Ok(Json(
        json!({"name": format!("op-{name}"), "status": "RUNNING"}),
    ))
}

async fn gce_wait(
    State(s): State<Shared>,
    h: HeaderMap,
    Path((_project, _zone, op)): Path<(String, String, String)>,
) -> Result<Json<Value>, StatusCode> {
    gce_check(&h)?;
    if let Some(code) = lock(&s).gce_wait_status {
        return Err(StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR));
    }
    Ok(Json(json!({"name": op, "status": "DONE"})))
}

/// `instances.aggregatedList` filtered on `labels.borg-burst=queen`, grouped
/// by zone as GCE does.
async fn gce_aggregated(
    State(s): State<Shared>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, StatusCode> {
    gce_check(&h)?;
    if q.get("filter").map(String::as_str) != Some("labels.borg-burst=queen") {
        return Err(StatusCode::BAD_REQUEST);
    }
    let mut st = lock(&s);
    st.gce_lists += 1;
    let mut items = serde_json::Map::new();
    for i in st
        .gce_instances
        .iter()
        .filter(|i| i["body"]["labels"]["borg-burst"] == "queen")
    {
        let zone = i["zone"].as_str().unwrap_or_default();
        let entry = items
            .entry(format!("zones/{zone}"))
            .or_insert_with(|| json!({"instances": []}));
        entry["instances"].as_array_mut().unwrap().push(json!({
            "name": i["name"],
            "zone": format!("https://www.googleapis.com/compute/v1/projects/p/zones/{zone}"),
            "status": "RUNNING",
            "labels": i["body"]["labels"],
        }));
    }
    items.insert(
        "zones/us-east1-b".to_owned(),
        json!({"warning": {"code": "NO_RESULTS_ON_PAGE"}}),
    );
    Ok(Json(json!({"items": items})))
}

async fn gce_get(
    State(s): State<Shared>,
    h: HeaderMap,
    Path((_project, _zone, name)): Path<(String, String, String)>,
) -> Result<Json<Value>, StatusCode> {
    gce_check(&h)?;
    let st = lock(&s);
    let inst = st
        .gce_instances
        .iter()
        .find(|i| i["name"] == name)
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(json!({
        "name": name,
        "status": "RUNNING",
        "metadata": {"fingerprint": "fp-for-tests", "items": inst["body"]["metadata"]["items"]},
    })))
}

async fn gce_delete(
    State(s): State<Shared>,
    h: HeaderMap,
    Path((_project, _zone, name)): Path<(String, String, String)>,
) -> Result<Json<Value>, StatusCode> {
    gce_check(&h)?;
    let mut st = lock(&s);
    if st.gce_fail_delete {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    let before = st.gce_instances.len();
    st.gce_instances.retain(|i| i["name"] != name);
    if st.gce_instances.len() == before {
        return Err(StatusCode::NOT_FOUND);
    }
    st.gce_deleted.push(name.clone());
    Ok(Json(
        json!({"name": format!("op-delete-{name}"), "status": "RUNNING"}),
    ))
}

async fn gce_set_metadata(
    State(s): State<Shared>,
    h: HeaderMap,
    Path((_project, _zone, name)): Path<(String, String, String)>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    gce_check(&h)?;
    let mut st = lock(&s);
    if body["fingerprint"] != "fp-for-tests" {
        return Err(StatusCode::PRECONDITION_FAILED);
    }
    let inst = st
        .gce_instances
        .iter_mut()
        .find(|i| i["name"] == name)
        .ok_or(StatusCode::NOT_FOUND)?;
    inst["body"]["metadata"]["items"] = body["items"].clone();
    st.gce_set_metadata
        .push(json!({"name": name, "body": body}));
    Ok(Json(
        json!({"name": format!("op-meta-{name}"), "status": "DONE"}),
    ))
}
