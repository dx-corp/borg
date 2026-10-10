//! GitHub App authentication and the handful of REST calls the queen needs.
//!
//! The App private key never leaves this module: it signs a short-lived JWT,
//! which is exchanged for an installation token, which authorizes everything
//! else. Drones only ever receive single-use JIT runner configs.

use std::collections::BTreeSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jsonwebtoken::{Algorithm, EncodingKey, Header};
use reqwest::{Method, RequestBuilder};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::secret::Secret;

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

    /// A valid installation token, refreshed when near expiry.
    async fn installation_token(&self) -> Result<Secret, GithubError> {
        #[derive(Deserialize)]
        struct Installation {
            id: u64,
        }
        #[derive(Deserialize)]
        struct Issued {
            token: String,
            expires_at: String,
        }

        let mut auth = self.auth.lock().await;
        let now = unix_now();
        if let Some(t) = &auth.token
            && t.expires_at - TOKEN_SLACK_SECS > now
        {
            return Ok(t.value.clone());
        }
        let jwt = self.app_jwt()?;
        let installation_id = match auth.installation_id {
            Some(id) => id,
            None => {
                let path = format!("/orgs/{}/installation", self.org);
                let rb = self.request(Method::GET, &path).bearer_auth(&jwt);
                let found: Installation = self.send(Method::GET, &path, rb).await?;
                auth.installation_id = Some(found.id);
                found.id
            }
        };
        let path = format!("/app/installations/{installation_id}/access_tokens");
        let rb = self.request(Method::POST, &path).bearer_auth(&jwt);
        let issued: Issued = self.send(Method::POST, &path, rb).await?;
        let expires_at = OffsetDateTime::parse(&issued.expires_at, &Rfc3339)
            .map_err(|e| GithubError::Decode(format!("expires_at: {e}")))?
            .unix_timestamp();
        let value = Secret::new(issued.token);
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

    /// Jobs that are `queued` in `repo`, across queued and in-progress runs.
    pub async fn queued_jobs(&self, repo: &str) -> Result<Vec<QueuedJob>, GithubError> {
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
            jobs: Vec<Job>,
        }
        #[derive(Deserialize)]
        struct Job {
            id: u64,
            status: String,
            labels: Vec<String>,
            created_at: String,
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
        let mut out = Vec::new();
        for run in run_ids {
            for page in 1..=MAX_PAGES {
                let jobs: Jobs = self
                    .get(&format!("/repos/{repo}/actions/runs/{run}/jobs?filter=latest&per_page=100&page={page}"))
                    .await?;
                let n = jobs.jobs.len();
                for j in jobs.jobs.into_iter().filter(|j| j.status == "queued") {
                    let created_at =
                        OffsetDateTime::parse(&j.created_at, &Rfc3339).map_err(|e| {
                            GithubError::Decode(format!("job {} created_at: {e}", j.id))
                        })?;
                    out.push(QueuedJob {
                        id: j.id,
                        repo: repo.to_owned(),
                        labels: j.labels,
                        created_at,
                    });
                }
                if n < 100 {
                    break;
                }
            }
        }
        Ok(out)
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
