//! The supersede janitor: cancels queued pull-request workflow runs whose head
//! commit is no longer the pull request's head.
//!
//! A push to a pull request branch makes every run of the older commit
//! pointless, but GitHub keeps them queued and they occupy runner demand. Two
//! further opt-ins remove other waste: `cancel_running` also cancels
//! superseded runs that already started, and `dedupe_same_sha` cancels queued
//! duplicates of a run of the same workflow, commit, event and pull request.
//! Duplicate groups never include `workflow_dispatch` or `merge_group` runs,
//! and a running duplicate is never cancelled. The
//! janitor is opt-in (`supersede_janitor` in the queen config) and uses its own
//! token, because cancelling runs needs `actions: write` and the queen's App
//! deliberately does not have it.
//!
//! Safety rules, all enforced here:
//! * only runs that are `queued` are candidates (`in_progress` ones too when
//!   `cancel_running` is set), and the run is fetched again immediately before
//!   cancelling, so a run whose status changed in between is left alone;
//! * only `pull_request` runs that name exactly one pull request are touched;
//!   runs from forks (no pull request attached) and merge groups never are;
//! * the pull request's current head comes from the pulls API, not from the
//!   run's own snapshot;
//! * at most `max_cancels_per_pass` cancellations per pass, one pass per
//!   `interval_secs`;
//! * a cancel that finds the run already finished (409) is a no-op;
//! * every cancellation (or, in dry-run, every would-be cancellation) is logged.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::time::Duration;

use reqwest::{Method, RequestBuilder};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::config::JanitorSettings;
use crate::github::GithubError;
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
    /// Queued superseded runs cancelled.
    pub cancelled: u32,
    /// Running superseded runs cancelled (`cancel_running`).
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
    /// Runs that changed state before the cancel and were left alone.
    pub raced: u32,
    /// Per-run or per-pull-request failures (logged, not fatal).
    pub errors: u32,
}

impl PassReport {
    fn add(&mut self, o: &PassReport) {
        self.considered += o.considered;
        self.superseded += o.superseded;
        self.cancelled += o.cancelled;
        self.cancelled_running += o.cancelled_running;
        self.duplicates += o.duplicates;
        self.dedup_cancelled += o.dedup_cancelled;
        self.would_cancel += o.would_cancel;
        self.deferred += o.deferred;
        self.raced += o.raced;
        self.errors += o.errors;
    }
}

/// The janitor as `/status` and `/metrics` show it.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct JanitorView {
    pub passes: u64,
    pub last: PassReport,
    /// Sums over every pass since the queen started.
    pub total: PassReport,
}

#[derive(Debug, Deserialize)]
struct Run {
    id: u64,
    #[serde(default)]
    workflow_id: u64,
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
    view: Mutex<JanitorView>,
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
            view: Mutex::default(),
        })
    }

    pub fn view(&self) -> JanitorView {
        self.view.lock().expect("janitor view lock").clone()
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
        tracing::debug!(
            "janitor pass: considered {} superseded {} cancelled {} cancelled_running {} duplicates {} dedup_cancelled {} would_cancel {} deferred {} raced {} errors {}",
            report.considered,
            report.superseded,
            report.cancelled,
            report.cancelled_running,
            report.duplicates,
            report.dedup_cancelled,
            report.would_cancel,
            report.deferred,
            report.raced,
            report.errors
        );
        let mut view = self.view.lock().expect("janitor view lock");
        view.passes += 1;
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
        let event = (!st.dedupe_same_sha).then_some("pull_request");
        let mut runs = self.list_runs(repo, "queued", event).await?;
        if st.cancel_running || st.dedupe_same_sha {
            runs.extend(self.list_runs(repo, "in_progress", event).await?);
            // A run that started between the two listings appears in both.
            runs.sort_by_key(|r| r.id);
            runs.dedup_by_key(|r| r.id);
        }
        let mut stale = BTreeSet::new();
        let mut heads: BTreeMap<u64, Option<String>> = BTreeMap::new();
        for run in &runs {
            let running = run.status == "in_progress";
            if run.event != "pull_request"
                || !(run.status == "queued" || running && st.cancel_running)
            {
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
            stale.insert(run.id);
            if *budget == 0 {
                report.deferred += 1;
                continue;
            }
            let why = format!(
                "for #{}: head {} superseded by {current}",
                pr.number, run.head_sha
            );
            let kind = if running { Kind::Running } else { Kind::Stale };
            let outcome = self.cancel_if_unchanged(repo, run, &why).await;
            tally(report, budget, repo, run.id, kind, outcome);
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
                let outcome = self.cancel_if_unchanged(repo, run, &why).await;
                tally(report, budget, repo, run.id, Kind::Duplicate, outcome);
            }
        }
        Ok(())
    }

    /// Cancel `run` unless a fresh read shows its status or head changed.
    async fn cancel_if_unchanged(
        &self,
        repo: &str,
        run: &Run,
        why: &str,
    ) -> Result<Outcome, GithubError> {
        let fresh: Run = self
            .get(&format!("/repos/{repo}/actions/runs/{}", run.id))
            .await?;
        if fresh.status != run.status || fresh.head_sha != run.head_sha {
            return Ok(Outcome::Raced);
        }
        let state = &run.status;
        if self.settings.dry_run {
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
            Ok(Outcome::Raced)
        }
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Stale,
    Running,
    Duplicate,
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
            }
        }
        Ok(Outcome::DryRun) => {
            *budget -= 1;
            report.would_cancel += 1;
        }
        Ok(Outcome::Raced) => report.raced += 1,
        Err(e) => {
            tracing::warn!("janitor: {repo} run {run}: {e}");
            report.errors += 1;
        }
    }
}

/// Runs to drop from each group of queued or running runs sharing workflow,
/// head commit, event and pull requests, each with the id of the run kept: the
/// lowest-id running run, else the lowest-id queued run. `workflow_dispatch`
/// and `merge_group` runs are never grouped, and `skip` ids are left out.
fn surplus<'a>(runs: &'a [Run], skip: &BTreeSet<u64>) -> Vec<(&'a Run, u64)> {
    type Key<'a> = (u64, &'a str, &'a str, Vec<u64>);
    let mut groups: BTreeMap<Key<'a>, Vec<&'a Run>> = BTreeMap::new();
    for run in runs {
        if skip.contains(&run.id)
            || matches!(run.event.as_str(), "workflow_dispatch" | "merge_group")
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
}

pub(crate) async fn status_error(
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

#[cfg(test)]
mod tests {
    use super::*;

    fn run(id: u64, wf: u64, status: &str, event: &str, sha: &str, prs: &[u64]) -> Run {
        Run {
            id,
            workflow_id: wf,
            status: status.into(),
            event: event.into(),
            head_sha: sha.into(),
            pull_requests: prs.iter().map(|&number| PullRef { number }).collect(),
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
