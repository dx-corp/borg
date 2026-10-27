//! Re-runs jobs the queen preempted.
//!
//! Preemption cancels a running job so a higher-priority one can have its
//! slot. The queen records `(repo, run, job)` in the `preemptions` table with
//! `requeued_at` unset. This pass re-runs each recorded job through the jobs
//! rerun API once its run has completed (GitHub refuses to re-run a job of a
//! run that is still in progress). It uses the supersede janitor's token
//! source (the queen's App token by default), which has `actions: write`, and
//! the janitor's `dry_run` and `max_cancels_per_pass` settings.
//!
//! * a run that is not `completed` is left for a later pass;
//! * a rerun answered a permission 403, 404 or 409 (job gone, retention expired, or already
//!   re-run) is terminal: the row is marked and a warning is logged;
//! * any other error is logged and retried on the next pass;
//! * a row older than 24 hours is marked without a call, so a backlog never
//!   re-runs ancient jobs;
//! * in dry-run nothing is called and nothing is marked.

use std::sync::Mutex;
use std::time::Duration;

use reqwest::{Method, RequestBuilder};
use serde::{Deserialize, Serialize};

use crate::config::JanitorSettings;
use crate::github::{GithubError, RateLimit, TokenSource};
use crate::janitor::status_error;
use crate::secret::Secret;
use crate::store::{Store, StoreError};

/// Rows older than this are marked stale instead of re-run.
const MAX_AGE_SECS: i64 = 24 * 3600;
/// Most unrequeued rows read per pass.
const BATCH: i64 = 200;

/// What one pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct PassReport {
    /// Preemption rows examined.
    pub checked: u32,
    /// Jobs re-run.
    pub rerun: u32,
    /// Rows whose run has not completed yet.
    pub waiting: u32,
    /// Rows older than 24 hours, marked without a call.
    pub stale: u32,
    /// Rows whose rerun answered a permission 403, 404 or 409.
    pub terminal: u32,
    /// Failures that will be retried next pass.
    pub errors: u32,
}

impl PassReport {
    fn add(&mut self, o: &PassReport) {
        self.checked += o.checked;
        self.rerun += o.rerun;
        self.waiting += o.waiting;
        self.stale += o.stale;
        self.terminal += o.terminal;
        self.errors += o.errors;
    }
}

/// The requeuer as `/status` and `/metrics` show it.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct RequeueView {
    pub passes: u64,
    pub last: PassReport,
    /// Sums over every pass since the queen started.
    pub total: PassReport,
}

#[derive(Debug, Deserialize)]
struct Run {
    status: String,
}

pub struct Requeuer {
    rate_limit: RateLimit,
    http: reqwest::Client,
    api_base: String,
    token: TokenSource,
    dry_run: bool,
    max_per_pass: u32,
    view: Mutex<RequeueView>,
}

impl Requeuer {
    pub fn with_rate_limit(mut self, rate_limit: RateLimit) -> Self {
        self.rate_limit = rate_limit;
        self
    }

    /// Built from the janitor's token and settings.
    pub fn new(
        api_base: &str,
        token: Secret,
        settings: &JanitorSettings,
    ) -> Result<Self, GithubError> {
        Self::with_token_source(api_base, TokenSource::Static(token), settings)
    }

    pub fn with_token_source(
        api_base: &str,
        token: TokenSource,
        settings: &JanitorSettings,
    ) -> Result<Self, GithubError> {
        let http = reqwest::Client::builder()
            .user_agent("borg-queen-requeue")
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            rate_limit: token.rate_limit(),
            http,
            api_base: api_base.trim_end_matches('/').to_owned(),
            token,
            dry_run: settings.dry_run,
            max_per_pass: settings.max_cancels_per_pass,
            view: Mutex::default(),
        })
    }

    pub fn view(&self) -> RequeueView {
        self.view.lock().expect("requeue view lock").clone()
    }

    async fn request(&self, method: Method, path: &str) -> Result<RequestBuilder, GithubError> {
        let token = self.token.bearer().await?;
        Ok(self
            .http
            .request(method, format!("{}{path}", self.api_base))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .bearer_auth(token.expose()))
    }

    async fn run_status(&self, repo: &str, run: u64) -> Result<String, GithubError> {
        let path = format!("/repos/{repo}/actions/runs/{run}");
        let resp = self
            .rate_limit
            .send(self.request(Method::GET, &path).await?)
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(status_error(&self.rate_limit, "GET", &path, status.as_u16(), resp).await);
        }
        let bytes = resp.bytes().await?;
        serde_json::from_slice::<Run>(&bytes)
            .map(|r| r.status)
            .map_err(|e| GithubError::Decode(format!("{path}: {e}")))
    }

    /// Re-run a job. `Ok(Some(code))` is a terminal refusal (permission 403, 404, 409).
    async fn rerun(&self, repo: &str, job: u64) -> Result<Option<u16>, GithubError> {
        let path = format!("/repos/{repo}/actions/jobs/{job}/rerun");
        let resp = self
            .rate_limit
            .send(self.request(Method::POST, &path).await?)
            .await?;
        match resp.status().as_u16() {
            200..=299 => Ok(None),
            403 => {
                let error = status_error(&self.rate_limit, "POST", &path, 403, resp).await;
                if matches!(error, GithubError::RateLimited { .. }) {
                    Err(error)
                } else {
                    Ok(Some(403))
                }
            }
            code @ (404 | 409) => Ok(Some(code)),
            code => Err(status_error(&self.rate_limit, "POST", &path, code, resp).await),
        }
    }

    /// One pass over the preemptions not yet re-run.
    pub async fn pass(&self, store: &Store, now: i64) -> Result<PassReport, StoreError> {
        let mut report = PassReport::default();
        let mut budget = self.max_per_pass;
        for row in store.unrequeued_preemptions(BATCH).await? {
            report.checked += 1;
            let (repo, run, job) = (row.repo.as_str(), row.run_id, row.job_id);
            if now - row.at > MAX_AGE_SECS {
                tracing::info!("requeue: {repo} job {job} (run {run}) is stale, not re-running");
                store.mark_requeued(&row.lease_id, now).await?;
                report.stale += 1;
                continue;
            }
            if budget == 0 {
                break;
            }
            if self.dry_run {
                tracing::info!(
                    "requeue (dry-run): would rerun {repo} job {job} once run {run} completes"
                );
                budget -= 1;
                continue;
            }
            match self.run_status(repo, run).await {
                Ok(status) if status == "completed" => {}
                Ok(_) => {
                    report.waiting += 1;
                    continue;
                }
                Err(e) => {
                    tracing::warn!("requeue: {repo} run {run}: {e}");
                    report.errors += 1;
                    continue;
                }
            }
            budget -= 1;
            match self.rerun(repo, job).await {
                Ok(None) => {
                    tracing::info!("requeue: re-ran {repo} job {job} (run {run})");
                    store.mark_requeued(&row.lease_id, now).await?;
                    report.rerun += 1;
                }
                Ok(Some(code)) => {
                    tracing::warn!(
                        "requeue: {repo} job {job} (run {run}) rerun answered {code}, giving up"
                    );
                    store.mark_requeued(&row.lease_id, now).await?;
                    report.terminal += 1;
                }
                Err(e) => {
                    tracing::warn!("requeue: {repo} job {job} (run {run}): {e}");
                    report.errors += 1;
                }
            }
        }
        let mut view = self.view.lock().expect("requeue view lock");
        view.passes += 1;
        view.total.add(&report);
        view.last = report.clone();
        drop(view);
        Ok(report)
    }
}
