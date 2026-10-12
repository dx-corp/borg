//! A fake GitHub REST API and a running queen, for integration tests.
//! Shared by several test binaries, each of which uses only part of it.
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
pub const JANITOR_TOKEN: &str = "ghp_janitor_for_tests";
pub const ORG: &str = "example-org";

#[derive(Default)]
pub struct MockState {
    public_pem: String,
    pub queued_runs: Vec<u64>,
    pub jobs: Vec<Value>,
    pub runners: Vec<Value>,
    pub mints: Vec<Value>,
    pub deleted: Vec<u64>,
    pub token_requests: u32,
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
    /// Status the single-run endpoint reports instead of the listed one.
    pub fresh_status: HashMap<u64, String>,
    /// Runs the janitor cancelled.
    pub cancelled: Vec<u64>,
    /// Runs whose cancel answers 409.
    pub uncancellable: Vec<u64>,
    /// Jobs the requeuer re-ran.
    pub reruns: Vec<u64>,
    /// Status code a job's rerun answers instead of 201.
    pub rerun_status: HashMap<u64, u16>,
    /// Reads of a single run (janitor or requeuer).
    pub run_reads: u32,
    next_runner_id: u64,
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

async fn access_token(State(s): State<Shared>, h: HeaderMap) -> Result<Json<Value>, StatusCode> {
    check_jwt(&s, &h)?;
    lock(&s).token_requests += 1;
    let expires = (OffsetDateTime::now_utc() + time::Duration::hours(1))
        .format(&Rfc3339)
        .unwrap();
    Ok(Json(
        json!({"token": INSTALLATION_TOKEN, "expires_at": expires}),
    ))
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
                ["status", "event"]
                    .iter()
                    .all(|k| q.get(*k).is_none_or(|v| r[*k].as_str() == Some(v)))
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
    Path(_p): Path<(String, String, u64)>,
) -> Result<Json<Value>, StatusCode> {
    check_token(&h)?;
    let state = lock(&s);
    let jobs: Vec<_> = state
        .jobs
        .iter()
        .filter(|job| !state.hide_completed_jobs || job["status"] != "completed")
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
    st.pr_runs.retain(|r| r["id"].as_u64() != Some(id));
    StatusCode::ACCEPTED
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

async fn pull(
    State(s): State<Shared>,
    h: HeaderMap,
    Path((_o, _r, n)): Path<(String, String, u64)>,
) -> Result<Json<Value>, StatusCode> {
    check_janitor(&h)?;
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
}

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
                "/repos/{owner}/{repo}/actions/jobs/{id}/rerun",
                post(rerun_job),
            )
            .route("/repos/{owner}/{repo}/pulls/{n}", get(pull))
            .route("/orgs/{org}/actions/runners", get(list_runners))
            .route(
                "/orgs/{org}/actions/runners/generate-jitconfig",
                post(jitconfig),
            )
            .route("/orgs/{org}/actions/runners/{id}", delete(delete_runner))
            .with_state(Arc::clone(&mock));
        let gh_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gh_url = format!("http://{}", gh_listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(gh_listener, app).await });

        let mut cfg = json!({
            "github": {"org": ORG, "app_id": "12345", "api_base": gh_url},
            "repos": [format!("{ORG}/app")],
            "db_path": "unused.db",
            "warm_floors": []
        });
        for (k, v) in extra.as_object().cloned().unwrap_or_default() {
            cfg[k] = v;
        }
        let cfg = QueenConfig::from_json(&cfg.to_string()).unwrap();
        let github = GithubClient::new(&gh_url, ORG, "12345", private_pem.as_bytes()).unwrap();
        let janitor = cfg.supersede_janitor.clone().map(|settings| {
            borg::janitor::Janitor::new(&gh_url, borg::secret::Secret::new(JANITOR_TOKEN), settings)
                .unwrap()
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
