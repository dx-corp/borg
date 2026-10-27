//! The supersede janitor: cancels queued pull-request workflow runs whose head
//! commit is no longer the pull request's head, runs for pull requests that
//! were closed or merged (any workflow once a pull-request read credential is
//! configured; `closed_pr_workflows` without one), and queued push runs on
//! listed branches that a newer run of the same workflow supersedes
//! (`superseded_push_branches`).
//!
//! A push to a pull request branch makes every run of the older commit
//! pointless, but GitHub keeps them queued and they occupy runner demand. Two
//! further opt-ins remove other waste: `cancel_running` also cancels
//! superseded runs that already started, and `dedupe_same_sha` cancels queued
//! duplicates of a run of the same workflow, commit, event and pull request.
//! Duplicate groups never include `workflow_dispatch` or `merge_group` runs,
//! and a running duplicate is never cancelled. The
//! janitor is opt-in (`supersede_janitor` in the queen config). Cancelling
//! runs needs `actions: write`: the janitor uses the queen's App installation
//! token (minted and refreshed on demand) unless the config names a static
//! token.
//!
//! Safety rules, all enforced here:
//! * only runs that are `queued` are candidates (`in_progress` ones too when
//!   `cancel_running` is set), and the run is fetched again immediately before
//!   cancelling, so a run whose status changed in between is left alone;
//! * only `pull_request` runs that name exactly one pull request are touched;
//!   runs from forks (no pull request attached) and merge groups never are;
//! * the pull request's current head comes from the run's `pull_requests`
//!   entry, which GitHub fills at read time with the open pull requests whose
//!   head matches, and from the pulls API only when that entry has no head;
//! * a `pull_request` run with no open pull request is treated as belonging
//!   to a closed one only when its head repository is the repository itself
//!   (forks never) and either its workflow is listed in `closed_pr_workflows`
//!   or the pulls API shows the branch's pull request closed *after* the run
//!   was created (a run created after the close is the `closed` event's own
//!   handler and is kept), and a fresh read must still show no pull request;
//! * the pulls API is read with `compare_auth` when configured (tokens carry
//!   `contents: read` and `pull_requests: read`), else with the janitor's own
//!   token; a refused read is counted once per pass and keeps every run;
//! * at most `max_cancels_per_pass` cancellations per pass, one pass per
//!   `interval_secs`;
//! * a cancel that GitHub refuses with 409 is a no-op, counted as
//!   `uncancellable`;
//! * every cancellation (or, in dry-run, every would-be cancellation) is logged.
//!
//! `superseded_promotions` blocks thin one promotion workflow each (see
//! `promotions`): a pending run whose apply never ran yields to a newer
//! started or successful run covering its services, a run older than
//! `stale_after_hours` yields to a newer successful one, a nameless queued run
//! with no jobs is dropped after `ghost_after_hours`, and a run the janitor
//! cancelled that is still unfinished `force_cancel_after_mins` later is
//! force-cancelled. A newer run must be proven the same as or ahead of the
//! older one in its source repository, and no run is touched while a mutation
//! job of it runs.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::time::Duration;

use reqwest::{Method, RequestBuilder};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::config::JanitorSettings;

#[path = "janitor_promotions.rs"]
mod promotions;
use crate::github::{GithubError, RateLimit, TokenSource};
use crate::secret::Secret;

/// Upper bound on pages of queued runs read per repository (100 per page).
const MAX_PAGES: u32 = 5;

/// What one pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct PassReport {
    /// Pull-request runs examined (queued, plus running with `cancel_running`).
    pub considered: u32,
    /// Runs whose head is no longer the pull request's head.
    pub superseded: u32,
    /// Runs of `closed_pr_workflows` whose pull request is closed or merged.
    pub closed_pr: u32,
    /// Queued push runs on `superseded_push_branches` with a newer run of the
    /// same workflow on the branch.
    pub superseded_push: u32,
    /// Queued runs cancelled as superseded, closed-PR or superseded-push.
    pub cancelled: u32,
    /// Running runs cancelled as superseded or closed-PR (`cancel_running`).
    pub cancelled_running: u32,
    /// Runs beyond the first in a same-workflow, same-commit group
    /// (`dedupe_same_sha`), queued or running.
    pub duplicates: u32,
    /// Queued duplicates cancelled.
    pub dedup_cancelled: u32,
    /// Runs that dry-run would have cancelled, of any kind.
    pub would_cancel: u32,
    /// Runs left for a later pass by the per-pass cap.
    pub deferred: u32,
    /// Superseded push runs left alone because one of their jobs is running.
    /// GitHub shows a run as `queued` while any job waits for a runner.
    pub started_push: u32,
    /// Runs that changed state between the listing and the cancel and were
    /// left alone.
    pub raced: u32,
    /// Runs whose cancel GitHub answered with 409 (already finished, or stuck
    /// in a state GitHub will not cancel).
    pub uncancellable: u32,
    /// Per-run or per-pull-request failures (logged, not fatal).
    pub errors: u32,
    /// Runs of `superseded_promotions` workflows examined.
    pub promotion_considered: u32,
    /// Pending promotion runs a newer started or successful run supersedes.
    pub promotion_superseded: u32,
    /// Promotion runs past `stale_after_hours` a newer successful run supersedes.
    pub promotion_stale: u32,
    /// Queued promotion runs with no jobs past `ghost_after_hours`.
    pub promotion_ghost: u32,
    /// Promotion runs cancelled for any of the three reasons above.
    pub promotion_cancelled: u32,
    /// Promotion runs whose superseding sha could not be proven (no source
    /// repository configured for the event).
    pub promotion_unprovable: u32,
    /// Janitor-cancelled runs still unfinished that were force-cancelled.
    pub force_cancelled: u32,
}

impl PassReport {
    fn add(&mut self, o: &PassReport) {
        self.considered += o.considered;
        self.superseded += o.superseded;
        self.closed_pr += o.closed_pr;
        self.superseded_push += o.superseded_push;
        self.cancelled += o.cancelled;
        self.cancelled_running += o.cancelled_running;
        self.duplicates += o.duplicates;
        self.dedup_cancelled += o.dedup_cancelled;
        self.would_cancel += o.would_cancel;
        self.deferred += o.deferred;
        self.started_push += o.started_push;
        self.raced += o.raced;
        self.uncancellable += o.uncancellable;
        self.errors += o.errors;
        self.promotion_considered += o.promotion_considered;
        self.promotion_superseded += o.promotion_superseded;
        self.promotion_stale += o.promotion_stale;
        self.promotion_ghost += o.promotion_ghost;
        self.promotion_cancelled += o.promotion_cancelled;
        self.promotion_unprovable += o.promotion_unprovable;
        self.force_cancelled += o.force_cancelled;
    }
}

/// The janitor as `/status` and `/metrics` show it.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct JanitorView {
    pub passes: u64,
    /// Unix time the queen started; `total` counts from here.
    since: i64,
    /// Unix time of the last pass; `None` before the first.
    last_pass_at: Option<i64>,
    /// What the last pass alone found. Runs still queued are found again by
    /// every pass, so this is the current state.
    pub last: PassReport,
    /// Sums over every pass since `since`. A run that stays queued is counted
    /// once per pass, so these grow with time even when nothing changes.
    pub total: PassReport,
}

#[derive(Debug, Deserialize)]
struct Run {
    id: u64,
    #[serde(default)]
    workflow_id: u64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    path: String,
    status: String,
    event: String,
    head_sha: String,
    #[serde(default)]
    head_branch: Option<String>,
    #[serde(default)]
    head_repository: Option<RepoRef>,
    #[serde(default)]
    pull_requests: Vec<PullRef>,
    #[serde(default)]
    display_title: String,
    #[serde(default)]
    created_at: String,
}

#[derive(Debug, Deserialize)]
struct RepoRef {
    full_name: String,
}

#[derive(Debug, Deserialize)]
struct PullRef {
    number: u64,
    #[serde(default)]
    head: Option<PullHead>,
}

#[derive(Debug, Deserialize)]
struct Pull {
    head: PullHead,
}

#[derive(Debug, Deserialize)]
struct PullHead {
    sha: String,
}

/// A pull request as `GET /pulls?head=owner:branch&state=all` lists it.
#[derive(Debug, Deserialize)]
struct PullByHead {
    number: u64,
    state: String,
    head: PullHead,
    #[serde(default)]
    closed_at: Option<String>,
}

/// What a run of a branch with no open pull request in its listing deserves.
#[derive(Debug, PartialEq, Eq)]
enum BranchVerdict {
    /// The branch's pull request is open at another head.
    Superseded {
        number: u64,
        current: String,
    },
    /// The branch's newest pull request closed after the run was created.
    Closed {
        number: u64,
        closed_at: String,
    },
    Keep,
}

/// Decide for a run created at `created_at` (unix) from its branch's pull
/// requests, newest first.
fn branch_verdict(run_head: &str, created_at: Option<i64>, pulls: &[PullByHead]) -> BranchVerdict {
    if let Some(open) = pulls.iter().find(|p| p.state == "open") {
        return if open.head.sha == run_head {
            BranchVerdict::Keep
        } else {
            BranchVerdict::Superseded {
                number: open.number,
                current: open.head.sha.clone(),
            }
        };
    }
    let (Some(newest), Some(created)) = (pulls.first(), created_at) else {
        return BranchVerdict::Keep;
    };
    match newest.closed_at.as_deref().and_then(unix) {
        Some(closed) if closed > created => BranchVerdict::Closed {
            number: newest.number,
            closed_at: newest.closed_at.clone().unwrap_or_default(),
        },
        _ => BranchVerdict::Keep,
    }
}

fn unix(rfc3339: &str) -> Option<i64> {
    time::OffsetDateTime::parse(rfc3339, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(time::OffsetDateTime::unix_timestamp)
}

pub struct Janitor {
    rate_limit: RateLimit,
    http: reqwest::Client,
    api_base: String,
    token: TokenSource,
    settings: JanitorSettings,
    view: Mutex<JanitorView>,
    promotion: Mutex<promotions::State>,
    /// Contents-read credential for compares; `None` uses `token`.
    compare_auth: Option<crate::mirror_auth::MirrorAuth>,
}

impl Janitor {
    pub fn with_rate_limit(mut self, rate_limit: RateLimit) -> Self {
        self.rate_limit = rate_limit;
        self
    }

    /// A janitor with a fixed token.
    pub fn new(
        api_base: &str,
        token: Secret,
        settings: JanitorSettings,
    ) -> Result<Self, GithubError> {
        Self::with_token_source(api_base, TokenSource::Static(token), settings)
    }

    pub fn with_token_source(
        api_base: &str,
        token: TokenSource,
        settings: JanitorSettings,
    ) -> Result<Self, GithubError> {
        let http = reqwest::Client::builder()
            .user_agent("borg-queen-janitor")
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            rate_limit: token.rate_limit(),
            http,
            api_base: api_base.trim_end_matches('/').to_owned(),
            token,
            settings,
            view: Mutex::new(JanitorView {
                since: now(),
                ..JanitorView::default()
            }),
            promotion: Mutex::new(promotions::State::default()),
            compare_auth: None,
        })
    }

    /// Read source repositories (compares) with this credential instead of
    /// the janitor's own token.
    pub fn with_compare_auth(mut self, auth: crate::mirror_auth::MirrorAuth) -> Self {
        self.compare_auth = Some(auth);
        self
    }

    pub fn view(&self) -> JanitorView {
        self.view.lock().expect("janitor view lock").clone()
    }

    pub fn settings(&self) -> &JanitorSettings {
        &self.settings
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

    /// GET with the compare credential when one is configured.
    async fn get_contents<T: DeserializeOwned>(&self, path: &str) -> Result<T, GithubError> {
        let Some(auth) = &self.compare_auth else {
            return self.get(path).await;
        };
        let token = auth.credential().await?.token;
        let rate_limit = auth.rate_limit();
        let resp = rate_limit
            .send(
                self.http
                    .get(format!("{}{path}", self.api_base))
                    .header("Accept", "application/vnd.github+json")
                    .header("X-GitHub-Api-Version", "2022-11-28")
                    .bearer_auth(token.expose()),
            )
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(status_error(&rate_limit, "GET", path, status.as_u16(), resp).await);
        }
        let bytes = resp.bytes().await?;
        serde_json::from_slice(&bytes).map_err(|e| GithubError::Decode(format!("{path}: {e}")))
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, GithubError> {
        let resp = self
            .rate_limit
            .send(self.request(Method::GET, path).await?)
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(status_error(&self.rate_limit, "GET", path, status.as_u16(), resp).await);
        }
        let bytes = resp.bytes().await?;
        serde_json::from_slice(&bytes).map_err(|e| GithubError::Decode(format!("{path}: {e}")))
    }

    /// Cancel a run. `Ok(false)` means GitHub says it can no longer be
    /// cancelled (already finished), which is fine.
    async fn cancel(&self, repo: &str, run: u64) -> Result<bool, GithubError> {
        let path = format!("/repos/{repo}/actions/runs/{run}/cancel");
        let resp = self
            .rate_limit
            .send(self.request(Method::POST, &path).await?)
            .await?;
        let status = resp.status();
        match status.as_u16() {
            200..=299 => Ok(true),
            409 => Ok(false),
            code => Err(status_error(&self.rate_limit, "POST", &path, code, resp).await),
        }
    }

    async fn list_runs(
        &self,
        repo: &str,
        status: &str,
        event: Option<&str>,
    ) -> Result<Vec<Run>, GithubError> {
        #[derive(Deserialize)]
        struct Runs {
            workflow_runs: Vec<Run>,
        }
        let event = event.map_or_else(String::new, |e| format!("&event={e}"));
        let mut out = Vec::new();
        for page in 1..=MAX_PAGES {
            let runs: Runs = self
                .get(&format!(
                    "/repos/{repo}/actions/runs?status={status}{event}&per_page=100&page={page}"
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
        self.promotion_passes(&mut budget, &mut report).await;
        tracing::debug!(
            "janitor pass: considered {} superseded {} closed_pr {} superseded_push {} cancelled {} cancelled_running {} duplicates {} dedup_cancelled {} would_cancel {} deferred {} started_push {} raced {} uncancellable {} errors {}",
            report.considered,
            report.superseded,
            report.closed_pr,
            report.superseded_push,
            report.cancelled,
            report.cancelled_running,
            report.duplicates,
            report.dedup_cancelled,
            report.would_cancel,
            report.deferred,
            report.started_push,
            report.raced,
            report.uncancellable,
            report.errors
        );
        let mut view = self.view.lock().expect("janitor view lock");
        view.passes += 1;
        view.last_pass_at = Some(now());
        view.total.add(&report);
        view.last = report.clone();
        drop(view);
        report
    }

    async fn pass_repo(
        &self,
        repo: &str,
        budget: &mut u32,
        report: &mut PassReport,
    ) -> Result<(), GithubError> {
        let st = &self.settings;
        let all_events = st.dedupe_same_sha
            || !st.closed_pr_workflows.is_empty()
            || !st.superseded_push_branches.is_empty();
        let event = (!all_events).then_some("pull_request");
        let mut runs = self.list_runs(repo, "queued", event).await?;
        if st.cancel_running || st.dedupe_same_sha {
            runs.extend(self.list_runs(repo, "in_progress", event).await?);
            // A run that started between the two listings appears in both.
            runs.sort_by_key(|r| r.id);
            runs.dedup_by_key(|r| r.id);
        }
        let mut stale = BTreeSet::new();
        let mut heads: BTreeMap<u64, Option<String>> = BTreeMap::new();
        let mut branches = BranchPulls::default();
        for run in &runs {
            let running = run.status == "in_progress";
            if run.event != "pull_request"
                || !(run.status == "queued" || running && st.cancel_running)
            {
                continue;
            }
            let why = match run.pull_requests.as_slice() {
                // No open pull request matches this same-repository run: its
                // pull request was closed or merged, or moved to another head.
                // A fork's run never names a pull request either; forks are
                // left alone.
                [] if self.same_repository(repo, run) => {
                    if self.closed_pr_workflow(run) {
                        report.considered += 1;
                        report.closed_pr += 1;
                        format!(
                            "for branch {}: no open pull request",
                            run.head_branch.as_deref().unwrap_or("?")
                        )
                    } else {
                        let Some(branch) = run.head_branch.as_deref() else {
                            continue;
                        };
                        let Some(pulls) =
                            self.branch_pulls(repo, branch, &mut branches, report).await
                        else {
                            continue;
                        };
                        report.considered += 1;
                        match branch_verdict(&run.head_sha, unix(&run.created_at), &pulls) {
                            BranchVerdict::Superseded { number, current } => {
                                report.superseded += 1;
                                format!(
                                    "for #{number}: head {} superseded by {current}",
                                    run.head_sha
                                )
                            }
                            BranchVerdict::Closed { number, closed_at } => {
                                report.closed_pr += 1;
                                format!(
                                    "for branch {branch}: #{number} closed {closed_at}, run created {}",
                                    run.created_at
                                )
                            }
                            BranchVerdict::Keep => continue,
                        }
                    }
                }
                [pr] => {
                    report.considered += 1;
                    let Some(current) = self.pr_head(repo, pr, &mut heads, report).await else {
                        continue;
                    };
                    if run.head_sha == current {
                        continue;
                    }
                    report.superseded += 1;
                    format!(
                        "for #{}: head {} superseded by {current}",
                        pr.number, run.head_sha
                    )
                }
                // With more than one pull request the answer is ambiguous.
                _ => continue,
            };
            stale.insert(run.id);
            if *budget == 0 {
                report.deferred += 1;
                continue;
            }
            let kind = if running { Kind::Running } else { Kind::Stale };
            let outcome = self
                .cancel_if_unchanged(repo, run, &why, self.settings.dry_run)
                .await;
            tally(report, budget, repo, run.id, kind, outcome);
        }
        for (run, newer) in superseded_pushes(&runs, &st.superseded_push_branches) {
            report.superseded_push += 1;
            stale.insert(run.id);
            if *budget == 0 {
                report.deferred += 1;
                continue;
            }
            match self.has_running_job(repo, run.id).await {
                Ok(false) => {}
                Ok(true) => {
                    report.started_push += 1;
                    continue;
                }
                Err(e) => {
                    tracing::warn!("janitor: {repo} run {}: {e}", run.id);
                    report.errors += 1;
                    continue;
                }
            }
            let why = format!(
                "on {}: superseded by run {newer}",
                run.head_branch.as_deref().unwrap_or("?")
            );
            let outcome = self
                .cancel_if_unchanged(repo, run, &why, self.settings.dry_run)
                .await;
            tally(report, budget, repo, run.id, Kind::Stale, outcome);
        }
        if st.dedupe_same_sha {
            for (run, kept) in surplus(&runs, &stale) {
                report.duplicates += 1;
                if run.status != "queued" {
                    continue;
                }
                if *budget == 0 {
                    report.deferred += 1;
                    continue;
                }
                let why = format!(
                    "duplicate of run {kept} (workflow {}, head {})",
                    run.workflow_id, run.head_sha
                );
                let outcome = self
                    .cancel_if_unchanged(repo, run, &why, self.settings.dry_run)
                    .await;
                tally(report, budget, repo, run.id, Kind::Duplicate, outcome);
            }
        }
        Ok(())
    }

    /// Whether `run`'s head is in `repo` itself (not a fork).
    fn same_repository(&self, repo: &str, run: &Run) -> bool {
        run.head_repository
            .as_ref()
            .is_some_and(|r| r.full_name.eq_ignore_ascii_case(repo))
    }

    /// Whether `run` is a run of a `closed_pr_workflows` entry.
    fn closed_pr_workflow(&self, run: &Run) -> bool {
        self.settings
            .closed_pr_workflows
            .iter()
            .any(|w| *w == run.path || *w == run.name)
    }

    /// The pull requests of `branch` in `repo`, newest first, read once per
    /// branch per pass. `None` after a refused or failed read, which is
    /// logged and counted once per pass: every run of the repository is then
    /// kept.
    async fn branch_pulls(
        &self,
        repo: &str,
        branch: &str,
        cache: &mut BranchPulls,
        report: &mut PassReport,
    ) -> Option<std::sync::Arc<[PullByHead]>> {
        if cache.unreadable {
            return None;
        }
        if let Some(pulls) = cache.by_branch.get(branch) {
            return Some(pulls.clone());
        }
        let owner = repo.split('/').next().unwrap_or_default();
        let path = format!(
            "/repos/{repo}/pulls?state=all&head={owner}:{branch}&sort=updated&direction=desc&per_page=10"
        );
        match self.get_contents::<Vec<PullByHead>>(&path).await {
            Ok(pulls) => {
                let pulls: std::sync::Arc<[PullByHead]> = pulls.into();
                cache.by_branch.insert(branch.to_owned(), pulls.clone());
                Some(pulls)
            }
            Err(e) => {
                tracing::warn!(
                    "janitor: {repo} pulls for {branch}: {e}; runs of closed pull requests are kept this pass (set supersede_janitor.compare_auth to an App with pull_requests: read, or list the workflow in closed_pr_workflows)"
                );
                report.errors += 1;
                cache.unreadable = true;
                None
            }
        }
    }

    /// The pull request's current head: the run's own entry when GitHub filled
    /// it, else the pulls API (cached per pass; `None` after a logged error).
    async fn pr_head(
        &self,
        repo: &str,
        pr: &PullRef,
        heads: &mut BTreeMap<u64, Option<String>>,
        report: &mut PassReport,
    ) -> Option<String> {
        if let Some(head) = &pr.head {
            return Some(head.sha.clone());
        }
        if let Some(cached) = heads.get(&pr.number) {
            return cached.clone();
        }
        let fetched = match self
            .get_contents::<Pull>(&format!("/repos/{repo}/pulls/{}", pr.number))
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
    }

    /// Cancel `run` unless a fresh read shows its status or head changed.
    /// Whether any job of `run` is running. A run reads `queued` while one job
    /// waits for a runner, even when others have started.
    async fn has_running_job(&self, repo: &str, run: u64) -> Result<bool, GithubError> {
        #[derive(Deserialize)]
        struct Jobs {
            jobs: Vec<Job>,
        }
        #[derive(Deserialize)]
        struct Job {
            status: String,
        }
        for page in 1..=MAX_PAGES {
            let jobs: Jobs = self
                .get(&format!(
                    "/repos/{repo}/actions/runs/{run}/jobs?filter=latest&per_page=100&page={page}"
                ))
                .await?;
            if jobs.jobs.iter().any(|j| j.status == "in_progress") {
                return Ok(true);
            }
            if jobs.jobs.len() < 100 {
                break;
            }
        }
        Ok(false)
    }

    async fn cancel_if_unchanged(
        &self,
        repo: &str,
        run: &Run,
        why: &str,
        dry_run: bool,
    ) -> Result<Outcome, GithubError> {
        let fresh: Run = self
            .get(&format!("/repos/{repo}/actions/runs/{}", run.id))
            .await?;
        if fresh.status != run.status
            || fresh.head_sha != run.head_sha
            || fresh.pull_requests.len() != run.pull_requests.len()
        {
            return Ok(Outcome::Raced);
        }
        let state = &run.status;
        if dry_run {
            tracing::info!(
                "janitor (dry-run): would cancel {state} {repo} run {} {why}",
                run.id
            );
            return Ok(Outcome::DryRun);
        }
        if self.cancel(repo, run.id).await? {
            tracing::info!("janitor: cancelled {state} {repo} run {} {why}", run.id);
            Ok(Outcome::Cancelled)
        } else {
            tracing::debug!("janitor: {repo} run {} answered 409 to cancel", run.id);
            Ok(Outcome::Uncancellable)
        }
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Stale,
    Running,
    Duplicate,
    Promotion,
}

fn tally(
    report: &mut PassReport,
    budget: &mut u32,
    repo: &str,
    run: u64,
    kind: Kind,
    outcome: Result<Outcome, GithubError>,
) {
    match outcome {
        Ok(Outcome::Cancelled) => {
            *budget -= 1;
            match kind {
                Kind::Stale => report.cancelled += 1,
                Kind::Running => report.cancelled_running += 1,
                Kind::Duplicate => report.dedup_cancelled += 1,
                Kind::Promotion => report.promotion_cancelled += 1,
            }
        }
        Ok(Outcome::DryRun) => {
            *budget -= 1;
            report.would_cancel += 1;
        }
        Ok(Outcome::Raced) => report.raced += 1,
        Ok(Outcome::Uncancellable) => report.uncancellable += 1,
        Err(e) => {
            tracing::warn!("janitor: {repo} run {run}: {e}");
            report.errors += 1;
        }
    }
}

/// Runs to drop from each group of queued or running runs sharing workflow,
/// head commit, event and pull requests, each with the id of the run kept: the
/// lowest-id running run, else the lowest-id queued run. Dispatch payloads
/// are not represented by the head commit, so dispatch events and merge groups
/// are never grouped. `skip` ids are left out.
fn surplus<'a>(runs: &'a [Run], skip: &BTreeSet<u64>) -> Vec<(&'a Run, u64)> {
    type Key<'a> = (u64, &'a str, &'a str, Vec<u64>);
    let mut groups: BTreeMap<Key<'a>, Vec<&'a Run>> = BTreeMap::new();
    for run in runs {
        if skip.contains(&run.id)
            || matches!(
                run.event.as_str(),
                "workflow_dispatch" | "repository_dispatch" | "merge_group"
            )
            || !matches!(run.status.as_str(), "queued" | "in_progress")
        {
            continue;
        }
        let mut prs: Vec<u64> = run.pull_requests.iter().map(|p| p.number).collect();
        prs.sort_unstable();
        groups
            .entry((run.workflow_id, &run.head_sha, &run.event, prs))
            .or_default()
            .push(run);
    }
    let mut out = Vec::new();
    for mut group in groups.into_values() {
        group.sort_by_key(|r| (r.status != "in_progress", r.id));
        let kept = group[0].id;
        out.extend(group.into_iter().skip(1).map(|r| (r, kept)));
    }
    out
}

enum Outcome {
    Cancelled,
    DryRun,
    Raced,
    Uncancellable,
}

/// Pull requests read per branch during one pass of one repository.
#[derive(Default)]
struct BranchPulls {
    by_branch: BTreeMap<String, std::sync::Arc<[PullByHead]>>,
    /// The pulls API refused or failed once this pass: read no more.
    unreadable: bool,
}

fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

/// Queued `push` runs on `branches`, each with the id of the newest queued or
/// running run of the same workflow on the same branch, when that newer run
/// is on another commit.
fn superseded_pushes<'a>(runs: &'a [Run], branches: &[String]) -> Vec<(&'a Run, u64)> {
    let mut newest: BTreeMap<(u64, &str), &Run> = BTreeMap::new();
    let on_branch = |r: &&'a Run| {
        r.event == "push"
            && matches!(r.status.as_str(), "queued" | "in_progress")
            && r.head_branch.as_ref().is_some_and(|b| branches.contains(b))
    };
    for run in runs.iter().filter(on_branch) {
        let key = (
            run.workflow_id,
            run.head_branch.as_deref().unwrap_or_default(),
        );
        let e = newest.entry(key).or_insert(run);
        if run.id > e.id {
            *e = run;
        }
    }
    runs.iter()
        .filter(on_branch)
        .filter(|r| r.status == "queued")
        .filter_map(|r| {
            let n = newest[&(r.workflow_id, r.head_branch.as_deref().unwrap_or_default())];
            (n.id > r.id && n.head_sha != r.head_sha).then_some((r, n.id))
        })
        .collect()
}

pub(crate) async fn status_error(
    rate_limit: &RateLimit,
    method: &'static str,
    path: &str,
    status: u16,
    resp: reqwest::Response,
) -> GithubError {
    // GitHub pretty-prints JSON errors over several lines; the journal shows
    // one line per record, so collapse the body to keep the message readable
    // in `journalctl | grep`.
    let body = resp
        .text()
        .await
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    rate_limit.error(method, path, status, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(id: u64, wf: u64, status: &str, event: &str, sha: &str, prs: &[u64]) -> Run {
        Run {
            id,
            workflow_id: wf,
            name: String::new(),
            path: String::new(),
            status: status.into(),
            event: event.into(),
            head_sha: sha.into(),
            head_branch: Some("main".into()),
            head_repository: None,
            pull_requests: prs
                .iter()
                .map(|&number| PullRef { number, head: None })
                .collect(),
            display_title: String::new(),
            created_at: String::new(),
        }
    }

    fn picked(runs: &[Run]) -> Vec<(u64, u64)> {
        let mut v: Vec<_> = surplus(runs, &BTreeSet::new())
            .into_iter()
            .map(|(r, k)| (r.id, k))
            .collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn only_older_queued_pushes_on_listed_branches_are_superseded() {
        let runs = [
            run(1, 1, "queued", "push", "a", &[]),
            run(2, 1, "in_progress", "push", "b", &[]),
            run(3, 1, "queued", "push", "c", &[]),
            run(4, 2, "queued", "push", "c", &[]),
            run(5, 2, "queued", "push", "c", &[]),
            run(6, 3, "queued", "pull_request", "a", &[9]),
            run(7, 3, "queued", "pull_request", "b", &[9]),
        ];
        let mut got: Vec<_> = superseded_pushes(&runs, &["main".into()])
            .into_iter()
            .map(|(r, n)| (r.id, n))
            .collect();
        got.sort_unstable();
        // Run 2 is running (never cancelled); run 4 shares run 5's commit.
        assert_eq!(got, [(1, 3)]);
        assert!(superseded_pushes(&runs, &["release".into()]).is_empty());
    }

    #[test]
    fn keeps_lowest_queued_run() {
        let runs = [
            run(7, 1, "queued", "pull_request", "a", &[3]),
            run(5, 1, "queued", "pull_request", "a", &[3]),
            run(9, 1, "queued", "pull_request", "a", &[3]),
        ];
        assert_eq!(picked(&runs), [(7, 5), (9, 5)]);
    }

    #[test]
    fn a_running_run_is_kept_over_lower_queued_ones() {
        let runs = [
            run(1, 1, "queued", "push", "a", &[]),
            run(8, 1, "in_progress", "push", "a", &[]),
            run(6, 1, "in_progress", "push", "a", &[]),
        ];
        assert_eq!(picked(&runs), [(1, 6), (8, 6)]);
    }

    #[test]
    fn repository_dispatches_at_same_head_preserve_distinct_rollout_verification() {
        // GitHub reports the default branch head, not the dispatched payload.
        // These runs may verify different rollout revisions and service digests.
        let runs = [
            run(1, 1, "in_progress", "repository_dispatch", "a", &[]),
            run(2, 1, "queued", "repository_dispatch", "a", &[]),
        ];
        assert!(picked(&runs).is_empty());
    }

    #[test]
    fn different_keys_and_exempt_events_do_not_group() {
        let runs = [
            run(1, 1, "queued", "pull_request", "a", &[3]),
            run(2, 2, "queued", "pull_request", "a", &[3]),
            run(3, 1, "queued", "pull_request", "b", &[3]),
            run(4, 1, "queued", "pull_request", "a", &[4]),
            run(5, 1, "queued", "workflow_dispatch", "a", &[]),
            run(6, 1, "queued", "workflow_dispatch", "a", &[]),
            run(7, 1, "queued", "merge_group", "a", &[]),
            run(8, 1, "queued", "merge_group", "a", &[]),
        ];
        assert!(picked(&runs).is_empty());
    }
}
