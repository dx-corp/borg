//! GitHub App authentication and the handful of REST calls the queen needs.
//!
//! The App private key never leaves this module: it signs a short-lived JWT,
//! which is exchanged for an installation token, which authorizes everything
//! else. Drones only ever receive single-use JIT runner configs.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jsonwebtoken::{Algorithm, EncodingKey, Header};
use reqwest::{Method, RequestBuilder};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::secret::Secret;
use crate::telemetry::{Conclusion, FailureClass, JobTelemetry, StepTiming};

/// GitHub-owned job metadata; never contains logs, outputs or environment values.
#[derive(Debug, Clone, Deserialize)]
pub struct WorkflowJob {
    pub id: u64,
    #[serde(default)]
    pub run_id: u64,
    #[serde(default)]
    pub name: String,
    pub status: String,
    pub conclusion: Option<Conclusion>,
    pub labels: Option<Vec<String>>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub runner_id: Option<u64>,
    pub runner_name: Option<String>,
    #[serde(default)]
    pub steps: Vec<JobStep>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JobStep {
    pub name: String,
    pub number: u64,
    pub conclusion: Option<Conclusion>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
}

fn timestamp(value: &str) -> Option<i64> {
    OffsetDateTime::parse(value, &Rfc3339)
        .ok()
        .map(|t| t.unix_timestamp())
}

fn elapsed(start: Option<&str>, end: Option<&str>) -> Option<u64> {
    u64::try_from(timestamp(end?)?.checked_sub(timestamp(start?)?)?).ok()
}

impl WorkflowJob {
    pub fn queued(&self, repo: &str) -> Result<Option<QueuedJob>, GithubError> {
        if self.status != "queued" {
            return Ok(None);
        }
        let created_at = OffsetDateTime::parse(&self.created_at, &Rfc3339)
            .map_err(|e| GithubError::Decode(format!("job {} created_at: {e}", self.id)))?;
        Ok(Some(QueuedJob {
            id: self.id,
            repo: repo.to_owned(),
            labels: self.labels.clone().ok_or_else(|| {
                GithubError::Decode(format!("job {} missing queued labels", self.id))
            })?,
            created_at,
        }))
    }

    /// A completed job's own execution time, excluding runner startup and idle time.
    /// Missing or invalid timing is unavailable evidence, never a zero-duration sample.
    pub fn telemetry(&self, repo: &str) -> Option<(JobTelemetry, u64, i64)> {
        if self.status != "completed" {
            return None;
        }
        let duration = elapsed(self.started_at.as_deref(), self.completed_at.as_deref())?;
        let finished = timestamp(self.completed_at.as_deref()?)?;
        let mut steps: Vec<_> = self.steps.iter().collect();
        steps.sort_by_key(|step| step.number);
        let telemetry = JobTelemetry {
            job_name: self.name.clone(),
            repo: repo.to_owned(),
            run_id: self.run_id,
            conclusion: self.conclusion.unwrap_or_default(),
            failure_class: FailureClass::Unknown,
            queue_wait_secs: elapsed(Some(&self.created_at), self.started_at.as_deref()),
            steps: steps
                .into_iter()
                .filter_map(|step| {
                    Some(StepTiming {
                        name: step.name.clone(),
                        conclusion: step.conclusion.unwrap_or_default(),
                        duration_secs: elapsed(
                            step.started_at.as_deref(),
                            step.completed_at.as_deref(),
                        )?,
                    })
                })
                .collect(),
        }
        .sanitized()?;
        Some((telemetry, duration, finished))
    }
}

/// Upper bound on pages fetched per list call (100 items per page).
const MAX_PAGES: u32 = 20;
/// Refresh the installation token this long before it expires.
const TOKEN_SLACK_SECS: i64 = 60;

#[derive(Debug, thiserror::Error)]
pub enum GithubError {
    #[error("request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("{method} {path} returned {status}: {body}")]
    Status {
        method: &'static str,
        path: String,
        status: u16,
        body: String,
    },
    #[error("sign app jwt: {0}")]
    Jwt(#[from] jsonwebtoken::errors::Error),
    #[error("bad private key: {0}")]
    Key(String),
    #[error("unexpected response: {0}")]
    Decode(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedJob {
    pub id: u64,
    pub repo: String,
    pub labels: Vec<String>,
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Runner {
    pub id: u64,
    pub name: String,
    pub status: String,
    pub busy: bool,
    /// Labels GitHub reports for the runner (`self-hosted`, `Linux`, ...).
    #[serde(default)]
    pub labels: Vec<RunnerLabel>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RunnerLabel {
    pub name: String,
}

#[derive(Debug)]
pub struct Minted {
    pub runner_id: u64,
    pub jit_config: Secret,
}

struct CachedToken {
    value: Secret,
    expires_at: i64,
}

#[derive(Default)]
struct AuthState {
    installation_id: Option<u64>,
    token: Option<CachedToken>,
}

pub struct GithubClient {
    http: reqwest::Client,
    api_base: String,
    org: String,
    app_id: String,
    key: EncodingKey,
    auth: tokio::sync::Mutex<AuthState>,
}

#[derive(serde::Serialize)]
struct Claims<'a> {
    iat: i64,
    exp: i64,
    iss: &'a str,
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

/// Where a GitHub API caller gets its bearer token: a fixed secret, or the
/// queen's own App installation token, minted and refreshed on demand.
#[derive(Clone)]
pub enum TokenSource {
    Static(Secret),
    App(std::sync::Arc<GithubClient>),
}

impl TokenSource {
    pub async fn bearer(&self) -> Result<Secret, GithubError> {
        match self {
            TokenSource::Static(s) => Ok(s.clone()),
            TokenSource::App(c) => c.installation_token().await,
        }
    }
}

impl GithubClient {
    pub fn new(
        api_base: &str,
        org: &str,
        app_id: &str,
        private_key_pem: &[u8],
    ) -> Result<Self, GithubError> {
        let key = EncodingKey::from_rsa_pem(private_key_pem)
            .map_err(|e| GithubError::Key(e.to_string()))?;
        let http = reqwest::Client::builder()
            .user_agent("borg-queen")
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            http,
            api_base: api_base.trim_end_matches('/').to_owned(),
            org: org.to_owned(),
            app_id: app_id.to_owned(),
            key,
            auth: tokio::sync::Mutex::new(AuthState::default()),
        })
    }

    fn app_jwt(&self) -> Result<String, GithubError> {
        let now = unix_now();
        // GitHub allows at most 10 minutes; backdate iat for clock skew.
        let claims = Claims {
            iat: now - 60,
            exp: now + 9 * 60,
            iss: &self.app_id,
        };
        Ok(jsonwebtoken::encode(
            &Header::new(Algorithm::RS256),
            &claims,
            &self.key,
        )?)
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.api_base))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
    }

    async fn send<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        rb: RequestBuilder,
    ) -> Result<T, GithubError> {
        let resp = rb.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(GithubError::Status {
                method: static_method(&method),
                path: path.to_owned(),
                status: status.as_u16(),
                body: body.chars().take(200).collect(),
            });
        }
        let bytes = resp.bytes().await?;
        serde_json::from_slice(&bytes).map_err(|e| GithubError::Decode(format!("{path}: {e}")))
    }

    /// The App's installation id in the configured org, looked up once.
    async fn installation_id(&self, auth: &mut AuthState, jwt: &str) -> Result<u64, GithubError> {
        #[derive(Deserialize)]
        struct Installation {
            id: u64,
        }
        if let Some(id) = auth.installation_id {
            return Ok(id);
        }
        let path = format!("/orgs/{}/installation", self.org);
        let rb = self.request(Method::GET, &path).bearer_auth(jwt);
        let found: Installation = self.send(Method::GET, &path, rb).await?;
        auth.installation_id = Some(found.id);
        Ok(found.id)
    }

    /// POST `/app/installations/{id}/access_tokens`; a `body` down-scopes the token.
    async fn mint(
        &self,
        auth: &mut AuthState,
        body: Option<&serde_json::Value>,
    ) -> Result<(Secret, i64), GithubError> {
        #[derive(Deserialize)]
        struct Issued {
            token: String,
            expires_at: String,
        }
        let jwt = self.app_jwt()?;
        let installation_id = self.installation_id(auth, &jwt).await?;
        let path = format!("/app/installations/{installation_id}/access_tokens");
        let mut rb = self.request(Method::POST, &path).bearer_auth(&jwt);
        if let Some(body) = body {
            rb = rb.json(body);
        }
        let issued: Issued = self.send(Method::POST, &path, rb).await?;
        let expires_at = OffsetDateTime::parse(&issued.expires_at, &Rfc3339)
            .map_err(|e| GithubError::Decode(format!("expires_at: {e}")))?
            .unix_timestamp();
        Ok((Secret::new(issued.token), expires_at))
    }

    /// Mint a fresh installation token limited to `permissions` (for example
    /// `{"contents": "read"}`) on the named `repositories` (bare names in the
    /// org), returning it with its expiry (unix seconds). Not cached here: the
    /// caller decides how long to keep it.
    pub async fn installation_token_scoped(
        &self,
        permissions: &serde_json::Value,
        repositories: &[String],
    ) -> Result<(Secret, i64), GithubError> {
        let body = serde_json::json!({
            "permissions": permissions,
            "repositories": repositories,
        });
        let mut auth = self.auth.lock().await;
        self.mint(&mut auth, Some(&body)).await
    }

    /// A valid installation token, refreshed when near expiry.
    pub(crate) async fn installation_token(&self) -> Result<Secret, GithubError> {
        let mut auth = self.auth.lock().await;
        let now = unix_now();
        if let Some(t) = &auth.token
            && t.expires_at - TOKEN_SLACK_SECS > now
        {
            return Ok(t.value.clone());
        }
        let (value, expires_at) = self.mint(&mut auth, None).await?;
        auth.token = Some(CachedToken {
            value: value.clone(),
            expires_at,
        });
        Ok(value)
    }

    async fn authed(&self, method: Method, path: &str) -> Result<RequestBuilder, GithubError> {
        let token = self.installation_token().await?;
        Ok(self.request(method, path).bearer_auth(token.expose()))
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, GithubError> {
        let rb = self.authed(Method::GET, path).await?;
        self.send(Method::GET, path, rb).await
    }

    /// Jobs in queued and in-progress workflows, including their completed jobs.
    pub async fn workflow_jobs(&self, repo: &str) -> Result<Vec<WorkflowJob>, GithubError> {
        #[derive(Deserialize)]
        struct Runs {
            workflow_runs: Vec<RunRef>,
        }
        #[derive(Deserialize)]
        struct RunRef {
            id: u64,
        }
        #[derive(Deserialize)]
        struct Jobs {
            jobs: Vec<WorkflowJob>,
        }

        let mut run_ids = BTreeSet::new();
        for status in ["queued", "in_progress"] {
            for page in 1..=MAX_PAGES {
                let runs: Runs = self
                    .get(&format!(
                        "/repos/{repo}/actions/runs?status={status}&per_page=100&page={page}"
                    ))
                    .await?;
                let n = runs.workflow_runs.len();
                run_ids.extend(runs.workflow_runs.into_iter().map(|r| r.id));
                if n < 100 {
                    break;
                }
            }
        }
        // One jobs listing per run dominates a tick (hundreds of runs across
        // the watched repositories), so list several runs at once. Workers
        // borrow the client and pull run ids from a shared index; results are
        // put back in run-id order so the output does not depend on timing.
        let run_ids: Vec<u64> = run_ids.into_iter().collect();
        let next = AtomicUsize::new(0);
        let worker = || async {
            let mut got: Vec<(usize, Vec<WorkflowJob>)> = Vec::new();
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(&run) = run_ids.get(i) else {
                    return Ok::<_, GithubError>(got);
                };
                let mut jobs_of_run = Vec::new();
                for page in 1..=MAX_PAGES {
                    let jobs: Jobs = self
                        .get(&format!("/repos/{repo}/actions/runs/{run}/jobs?filter=latest&per_page=100&page={page}"))
                        .await?;
                    let n = jobs.jobs.len();
                    jobs_of_run.extend(jobs.jobs.into_iter().map(|mut job| {
                        // The enclosing run is authoritative even if an older API omits run_id.
                        job.run_id = run;
                        job
                    }));
                    if n < 100 {
                        break;
                    }
                }
                got.push((i, jobs_of_run));
            }
        };
        // Eight concurrent listings: enough to cut a ~330-run tick several-fold
        // while staying far below the installation rate limit per tick.
        let (a, b, c, d, e, f, g, h) = tokio::join!(
            worker(),
            worker(),
            worker(),
            worker(),
            worker(),
            worker(),
            worker(),
            worker()
        );
        let mut parts = Vec::with_capacity(run_ids.len());
        for w in [a, b, c, d, e, f, g, h] {
            parts.extend(w?);
        }
        parts.sort_unstable_by_key(|(i, _)| *i);
        Ok(parts.into_iter().flat_map(|(_, jobs)| jobs).collect())
    }

    /// Compatibility surface for consumers that only need queued demand.
    pub async fn queued_jobs(&self, repo: &str) -> Result<Vec<QueuedJob>, GithubError> {
        self.workflow_jobs(repo)
            .await?
            .iter()
            .filter_map(|j| j.queued(repo).transpose())
            .collect()
    }

    pub async fn job(&self, repo: &str, job_id: u64) -> Result<WorkflowJob, GithubError> {
        self.get(&format!("/repos/{repo}/actions/jobs/{job_id}"))
            .await
    }

    /// Every self-hosted runner registered to the organization.
    pub async fn list_runners(&self) -> Result<Vec<Runner>, GithubError> {
        #[derive(Deserialize)]
        struct Runners {
            runners: Vec<Runner>,
        }
        let mut out = Vec::new();
        for page in 1..=MAX_PAGES {
            let r: Runners = self
                .get(&format!(
                    "/orgs/{}/actions/runners?per_page=100&page={page}",
                    self.org
                ))
                .await?;
            let n = r.runners.len();
            out.extend(r.runners);
            if n < 100 {
                break;
            }
        }
        Ok(out)
    }

    /// Register a one-shot (JIT) runner and return its config.
    pub async fn mint_jit_runner(
        &self,
        name: &str,
        runner_group_id: u64,
        labels: &[String],
        work_folder: &str,
    ) -> Result<Minted, GithubError> {
        #[derive(Deserialize)]
        struct Reply {
            runner: RunnerRef,
            encoded_jit_config: String,
        }
        #[derive(Deserialize)]
        struct RunnerRef {
            id: u64,
        }
        let path = format!("/orgs/{}/actions/runners/generate-jitconfig", self.org);
        let body = serde_json::json!({
            "name": name,
            "runner_group_id": runner_group_id,
            "labels": labels,
            "work_folder": work_folder,
        });
        let rb = self.authed(Method::POST, &path).await?.json(&body);
        let reply: Reply = self.send(Method::POST, &path, rb).await?;
        Ok(Minted {
            runner_id: reply.runner.id,
            jit_config: Secret::new(reply.encoded_jit_config),
        })
    }

    /// Delete a runner registration. A runner that is already gone is fine.
    pub async fn delete_runner(&self, runner_id: u64) -> Result<(), GithubError> {
        let path = format!("/orgs/{}/actions/runners/{runner_id}", self.org);
        let resp = self.authed(Method::DELETE, &path).await?.send().await?;
        let status = resp.status();
        if status.is_success() || status.as_u16() == 404 {
            return Ok(());
        }
        Err(GithubError::Status {
            method: "DELETE",
            path,
            status: status.as_u16(),
            body: resp
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect(),
        })
    }
}

fn static_method(m: &Method) -> &'static str {
    match *m {
        Method::GET => "GET",
        Method::POST => "POST",
        Method::DELETE => "DELETE",
        _ => "REQUEST",
    }
}

#[cfg(test)]
mod job_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn missing_and_reversed_timestamps_do_not_invent_timings() {
        let mut value = json!({"id": 7, "run_id": 1, "name": "lint", "status": "completed", "conclusion": "success",
        "created_at": "bad", "started_at": "2026-10-11T10:00:00Z", "completed_at": "2026-10-11T10:00:30Z",
        "steps": [
            {"name": "second", "number": 2, "conclusion": "success", "started_at": "2026-10-11T10:00:10Z", "completed_at": "2026-10-11T10:00:20Z"},
            {"name": "first", "number": 1, "conclusion": "success", "started_at": "2026-10-11T10:00:00Z", "completed_at": "2026-10-11T10:00:10Z"},
            {"name": "unavailable", "number": 3, "conclusion": null, "started_at": null, "completed_at": null}
        ]});
        let job: WorkflowJob = serde_json::from_value(value.clone()).unwrap();
        let (telemetry, duration, _) = job.telemetry("example-org/app").unwrap();
        assert_eq!(duration, 30);
        assert_eq!(telemetry.queue_wait_secs, None);
        assert_eq!(
            telemetry
                .steps
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        value["completed_at"] = json!("2026-10-11T09:00:00Z");
        assert!(
            serde_json::from_value::<WorkflowJob>(value.clone())
                .unwrap()
                .telemetry("example-org/app")
                .is_none()
        );
        value["started_at"] = serde_json::Value::Null;
        assert!(
            serde_json::from_value::<WorkflowJob>(value)
                .unwrap()
                .telemetry("example-org/app")
                .is_none()
        );
    }
}
