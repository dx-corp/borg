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
    if q.get("event").map(String::as_str) == Some("pull_request") {
        check_janitor(&h)?;
        return Ok(Json(json!({"workflow_runs": lock(&s).pr_runs.clone()})));
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
    Ok(Json(json!({"jobs": lock(&s).jobs.clone()})))
}

async fn one_run(
    State(s): State<Shared>,
    h: HeaderMap,
    Path((_o, _r, id)): Path<(String, String, u64)>,
) -> Result<Json<Value>, StatusCode> {
    check_janitor(&h)?;
    let st = lock(&s);
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

pub struct Harness {
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
        let mock: Shared = Arc::default();
        let (private_pem, public_pem) = generate_key();
        lock(&mock).public_pem = public_pem;

        let app = Router::new()
            .route("/orgs/{org}/installation", get(installation))
            .route("/app/installations/{id}/access_tokens", post(access_token))
            .route("/repos/{owner}/{repo}/actions/runs", get(runs))
            .route("/repos/{owner}/{repo}/actions/runs/{run}/jobs", get(jobs))
            .route("/repos/{owner}/{repo}/actions/runs/{run}", get(one_run))
            .route(
                "/repos/{owner}/{repo}/actions/runs/{run}/cancel",
                post(cancel_run),
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
        let store = Store::open_in_memory().await.unwrap();
        let janitor = cfg.supersede_janitor.clone().map(|settings| {
            borg::janitor::Janitor::new(&gh_url, borg::secret::Secret::new(JANITOR_TOKEN), settings)
                .unwrap()
        });
        let mut queen = Queen::new(cfg, store, github);
        if let Some(j) = janitor {
            queen = queen.with_janitor(j);
        }
        let queen = Arc::new(queen);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let router = api::router(Arc::clone(&queen));
        tokio::spawn(async move { axum::serve(listener, router).await });
        Self {
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

    pub async fn janitor_pass(&self) -> borg::janitor::PassReport {
        let repos = self.queen.config().repos.clone();
        self.queen.janitor().unwrap().pass(&repos).await
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
