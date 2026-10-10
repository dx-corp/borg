//! The supersede janitor: cancels queued pull-request workflow runs whose head
//! commit is no longer the pull request's head.
//!
//! A push to a pull request branch makes every run of the older commit
//! pointless, but GitHub keeps them queued and they occupy runner demand. The
//! janitor is opt-in (`supersede_janitor` in the queen config) and uses its own
//! token, because cancelling runs needs `actions: write` and the queen's App
//! deliberately does not have it.
//!
//! Safety rules, all enforced here:
//! * only runs that are `queued` are candidates, and the run is fetched again
//!   immediately before cancelling, so a run that started in between is left
//!   alone;
//! * only `pull_request` runs that name exactly one pull request are touched;
//!   runs from forks (no pull request attached) and merge groups never are;
//! * the pull request's current head comes from the pulls API, not from the
//!   run's own snapshot;
//! * at most `max_cancels_per_pass` cancellations per pass, one pass per
//!   `interval_secs`;
//! * a cancel that finds the run already finished (409) is a no-op;
//! * every cancellation (or, in dry-run, every would-be cancellation) is logged.

use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::{Method, RequestBuilder};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::config::JanitorSettings;
use crate::github::GithubError;
use crate::secret::Secret;

/// Upper bound on pages of queued runs read per repository (100 per page).
const MAX_PAGES: u32 = 5;

/// What one pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PassReport {
    /// Queued pull-request runs examined.
    pub considered: u32,
    /// Runs whose head is no longer the pull request's head.
    pub superseded: u32,
    /// Runs cancelled.
    pub cancelled: u32,
    /// Runs that dry-run would have cancelled.
    pub would_cancel: u32,
    /// Superseded runs left for a later pass by the per-pass cap.
    pub deferred: u32,
    /// Runs that changed state before the cancel and were left alone.
    pub raced: u32,
    /// Per-run or per-pull-request failures (logged, not fatal).
    pub errors: u32,
}

#[derive(Debug, Deserialize)]
struct Run {
    id: u64,
    status: String,
    event: String,
    head_sha: String,
    #[serde(default)]
    pull_requests: Vec<PullRef>,
}

#[derive(Debug, Deserialize)]
struct PullRef {
    number: u64,
}

#[derive(Debug, Deserialize)]
struct Pull {
    head: PullHead,
}

#[derive(Debug, Deserialize)]
struct PullHead {
    sha: String,
}

pub struct Janitor {
    http: reqwest::Client,
    api_base: String,
    token: Secret,
    settings: JanitorSettings,
}

impl Janitor {
    pub fn new(
        api_base: &str,
        token: Secret,
        settings: JanitorSettings,
    ) -> Result<Self, GithubError> {
        let http = reqwest::Client::builder()
            .user_agent("borg-queen-janitor")
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            http,
            api_base: api_base.trim_end_matches('/').to_owned(),
            token,
            settings,
        })
    }

    pub fn settings(&self) -> &JanitorSettings {
        &self.settings
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.api_base))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .bearer_auth(self.token.expose())
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, GithubError> {
        let resp = self.request(Method::GET, path).send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(status_error("GET", path, status.as_u16(), resp).await);
        }
        let bytes = resp.bytes().await?;
        serde_json::from_slice(&bytes).map_err(|e| GithubError::Decode(format!("{path}: {e}")))
    }

    /// Cancel a run. `Ok(false)` means GitHub says it can no longer be
    /// cancelled (already finished), which is fine.
    async fn cancel(&self, repo: &str, run: u64) -> Result<bool, GithubError> {
        let path = format!("/repos/{repo}/actions/runs/{run}/cancel");
        let resp = self.request(Method::POST, &path).send().await?;
        let status = resp.status();
        match status.as_u16() {
            200..=299 => Ok(true),
            409 => Ok(false),
            code => Err(status_error("POST", &path, code, resp).await),
        }
    }

    async fn queued_pr_runs(&self, repo: &str) -> Result<Vec<Run>, GithubError> {
        #[derive(Deserialize)]
        struct Runs {
            workflow_runs: Vec<Run>,
        }
        let mut out = Vec::new();
        for page in 1..=MAX_PAGES {
            let runs: Runs = self
                .get(&format!(
                    "/repos/{repo}/actions/runs?status=queued&event=pull_request&per_page=100&page={page}"
                ))
                .await?;
            let n = runs.workflow_runs.len();
            out.extend(runs.workflow_runs);
            if n < 100 {
                break;
            }
        }
        Ok(out)
    }

    /// One pass over `repos`. A failure to list a repository's runs aborts
    /// that repository only; the error is counted and logged.
    pub async fn pass(&self, repos: &[String]) -> PassReport {
        let mut report = PassReport::default();
        let mut budget = self.settings.max_cancels_per_pass;
        for repo in repos {
            if let Err(e) = self.pass_repo(repo, &mut budget, &mut report).await {
                tracing::warn!("janitor: {repo}: {e}");
                report.errors += 1;
            }
        }
        report
    }

    async fn pass_repo(
        &self,
        repo: &str,
        budget: &mut u32,
        report: &mut PassReport,
    ) -> Result<(), GithubError> {
        let runs = self.queued_pr_runs(repo).await?;
        let mut heads: BTreeMap<u64, Option<String>> = BTreeMap::new();
        for run in &runs {
            if run.status != "queued" || run.event != "pull_request" {
                continue;
            }
            // A fork's run carries no pull request; with more than one the
            // answer is ambiguous. Leave both alone.
            let [pr] = run.pull_requests.as_slice() else {
                continue;
            };
            report.considered += 1;
            let head = if let Some(cached) = heads.get(&pr.number) {
                cached.clone()
            } else {
                let fetched = match self
                    .get::<Pull>(&format!("/repos/{repo}/pulls/{}", pr.number))
                    .await
                {
                    Ok(p) => Some(p.head.sha),
                    Err(e) => {
                        tracing::warn!("janitor: {repo}#{}: {e}", pr.number);
                        report.errors += 1;
                        None
                    }
                };
                heads.insert(pr.number, fetched.clone());
                fetched
            };
            let Some(current) = head else {
                continue;
            };
            if run.head_sha == current {
                continue;
            }
            report.superseded += 1;
            if *budget == 0 {
                report.deferred += 1;
                continue;
            }
            match self.supersede(repo, run, pr.number, &current).await {
                Ok(Outcome::Cancelled) => {
                    *budget -= 1;
                    report.cancelled += 1;
                }
                Ok(Outcome::DryRun) => {
                    *budget -= 1;
                    report.would_cancel += 1;
                }
                Ok(Outcome::Raced) => report.raced += 1,
                Err(e) => {
                    tracing::warn!("janitor: {repo} run {}: {e}", run.id);
                    report.errors += 1;
                }
            }
        }
        Ok(())
    }

    async fn supersede(
        &self,
        repo: &str,
        run: &Run,
        pr: u64,
        current: &str,
    ) -> Result<Outcome, GithubError> {
        // Re-read: the run may have started since the list was fetched.
        let fresh: Run = self
            .get(&format!("/repos/{repo}/actions/runs/{}", run.id))
            .await?;
        if fresh.status != "queued" || fresh.head_sha != run.head_sha {
            return Ok(Outcome::Raced);
        }
        if self.settings.dry_run {
            tracing::info!(
                "janitor (dry-run): would cancel {repo} run {} for #{pr}: head {} superseded by {current}",
                run.id,
                run.head_sha
            );
            return Ok(Outcome::DryRun);
        }
        if self.cancel(repo, run.id).await? {
            tracing::info!(
                "janitor: cancelled {repo} run {} for #{pr}: head {} superseded by {current}",
                run.id,
                run.head_sha
            );
            Ok(Outcome::Cancelled)
        } else {
            Ok(Outcome::Raced)
        }
    }
}

enum Outcome {
    Cancelled,
    DryRun,
    Raced,
}

async fn status_error(
    method: &'static str,
    path: &str,
    status: u16,
    resp: reqwest::Response,
) -> GithubError {
    GithubError::Status {
        method,
        path: path.to_owned(),
        status,
        body: resp
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect(),
    }
}
