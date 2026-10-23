//! Superseded and stale promotion runs (`superseded_promotions`).
//!
//! A promotion workflow keeps one queue per service set and source revision,
//! so a burst of image publications leaves many older runs pending behind a
//! newer one that will promote the same services anyway, and an old run can
//! sit for days on its verification jobs after newer revisions went live.
//! Rules, applied per configured workflow every `sweep_secs`:
//!
//! * **superseded**: a pending run whose apply job never executed is cancelled
//!   when a newer run that covers all of its services (same `source-event`)
//!   has started or succeeded. Requiring a started successor keeps a cohort
//!   from being pushed to the back of the runner queue by its own successors.
//! * **stale**: a run older than `stale_after_hours`, in any unfinished state,
//!   is cancelled when a newer covering run has succeeded. Its own
//!   verification would check a revision production no longer runs.
//! * **ghost**: a queued run with no job records older than `ghost_after_hours`
//!   is cancelled, whatever its run name.
//! * **force**: a run the janitor cancelled that is still unfinished after
//!   `force_cancel_after_mins` is force-cancelled (a runner that never
//!   acknowledges the cancel keeps holding the run otherwise).
//!
//! The superseded and stale rules also need the newer run's sha to be the same
//! as or ahead of the older one in the event's `source_repos` entry; without
//! an entry, or when the compare fails, the run is kept. No run is touched
//! while one of its `mutation_jobs` is in progress.

use super::*;

const PENDING: [&str; 4] = ["queued", "pending", "waiting", "requested"];
const ACTIVE: [&str; 5] = ["queued", "pending", "waiting", "requested", "in_progress"];

/// What the janitor remembers between passes.
#[derive(Default)]
pub(super) struct State {
    /// Unix time of the last sweep per `repo workflow`.
    last_sweep: BTreeMap<String, i64>,
    /// Runs this janitor cancelled, with the unix time of the cancel.
    requested: BTreeMap<u64, i64>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Intent {
    sha: String,
    services: BTreeSet<String>,
    event: String,
}

/// Parse `Promote runtime images sha-<40 hex> services=<a,b> ... source-event=<e>`.
pub(super) fn parse_title(title: &str) -> Option<Intent> {
    let rest = title.strip_prefix("Promote runtime images sha-")?;
    let mut tokens = rest.split_whitespace();
    let sha = tokens.next()?;
    if sha.len() != 40 || !sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let (mut services, mut event) = (None, None);
    for token in tokens {
        if let Some(v) = token.strip_prefix("services=") {
            services = Some(
                v.split(',')
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect::<BTreeSet<_>>(),
            );
        } else if let Some(v) = token.strip_prefix("source-event=") {
            event = Some(v.to_owned());
        }
    }
    Some(Intent {
        sha: sha.to_owned(),
        services: services.filter(|s| !s.is_empty())?,
        event: event.filter(|e| !e.is_empty())?,
    })
}

/// Whether `newer` promotes every service of `older` for the same event.
fn covers(newer: &Intent, older: &Intent) -> bool {
    newer.event == older.event && older.services.is_subset(&newer.services)
}

#[derive(Debug, Deserialize)]
struct Job {
    #[serde(default)]
    name: String,
    status: String,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    runner_name: Option<String>,
    #[serde(default)]
    steps: Vec<serde_json::Value>,
}

/// A job that ran or is running (a runner, a step, or a non-skipped result).
fn executed(job: &Job) -> bool {
    job.status == "in_progress"
        || job.runner_name.as_deref().is_some_and(|r| !r.is_empty())
        || !job.steps.is_empty()
        || job.status == "completed" && job.conclusion.as_deref() != Some("skipped")
}

fn unix(rfc3339: &str) -> Option<i64> {
    time::OffsetDateTime::parse(rfc3339, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(time::OffsetDateTime::unix_timestamp)
}

impl Janitor {
    /// Every `superseded_promotions` block whose sweep is due.
    pub(super) async fn promotion_passes(&self, budget: &mut u32, report: &mut PassReport) {
        let now = now();
        for p in &self.settings.superseded_promotions {
            let key = format!("{} {}", p.repo, p.workflow);
            {
                let mut st = self.promotion.lock().expect("promotion state lock");
                if st
                    .last_sweep
                    .get(&key)
                    .is_some_and(|t| now - t < p.sweep_secs as i64)
                {
                    continue;
                }
                st.last_sweep.insert(key, now);
            }
            if let Err(e) = self.promotion_pass(p, now, budget, report).await {
                tracing::warn!("janitor: {} {}: {e}", p.repo, p.workflow);
                report.errors += 1;
            }
        }
    }

    async fn promotion_jobs(&self, repo: &str, run: u64) -> Result<Vec<Job>, GithubError> {
        #[derive(Deserialize)]
        struct Jobs {
            jobs: Vec<Job>,
        }
        let mut out = Vec::new();
        for page in 1..=MAX_PAGES {
            let jobs: Jobs = self
                .get(&format!(
                    "/repos/{repo}/actions/runs/{run}/jobs?filter=latest&per_page=100&page={page}"
                ))
                .await?;
            let n = jobs.jobs.len();
            out.extend(jobs.jobs);
            if n < 100 {
                break;
            }
        }
        Ok(out)
    }

    /// Whether `head` is the same commit as, or ahead of, `base` in `repo`.
    async fn same_or_ahead(
        &self,
        repo: &str,
        base: &str,
        head: &str,
        cache: &mut BTreeMap<(String, String), bool>,
    ) -> Result<bool, GithubError> {
        if base == head {
            return Ok(true);
        }
        if let Some(v) = cache.get(&(base.to_owned(), head.to_owned())) {
            return Ok(*v);
        }
        #[derive(Deserialize)]
        struct Compare {
            status: String,
        }
        let c: Compare = self
            .get_contents(&format!("/repos/{repo}/compare/{base}...{head}?per_page=1"))
            .await?;
        let ok = matches!(c.status.as_str(), "ahead" | "identical");
        cache.insert((base.to_owned(), head.to_owned()), ok);
        Ok(ok)
    }

    async fn force_cancel(&self, repo: &str, run: u64, dry_run: bool) -> Result<bool, GithubError> {
        if dry_run {
            tracing::info!("janitor (dry-run): would force-cancel {repo} run {run}");
            return Ok(false);
        }
        let path = format!("/repos/{repo}/actions/runs/{run}/force-cancel");
        let resp = self.request(Method::POST, &path).await?.send().await?;
        match resp.status().as_u16() {
            200..=299 => Ok(true),
            409 => Ok(false),
            code => Err(status_error("POST", &path, code, resp).await),
        }
    }

    async fn promotion_pass(
        &self,
        p: &crate::config::PromotionSupersede,
        now: i64,
        budget: &mut u32,
        report: &mut PassReport,
    ) -> Result<(), GithubError> {
        let repo = p.repo.as_str();
        let dry_run = self.settings.dry_run || p.dry_run;
        let mut active = Vec::new();
        for status in ACTIVE {
            active.extend(
                self.list_runs(repo, status, Some("repository_dispatch"))
                    .await?,
            );
        }
        active.retain(|r| r.path == p.workflow);
        active.sort_by_key(|r| r.id);
        active.dedup_by_key(|r| r.id);
        let mut succeeded = self
            .list_runs(repo, "success", Some("repository_dispatch"))
            .await?;
        succeeded.retain(|r| r.path == p.workflow);

        // Escalate cancels that did not take; forget runs that finished.
        let requested: Vec<(u64, i64)> = {
            let st = self.promotion.lock().expect("promotion state lock");
            st.requested.iter().map(|(k, v)| (*k, *v)).collect()
        };
        for (id, at) in requested {
            if !active.iter().any(|r| r.id == id) {
                self.forget(id);
                continue;
            }
            if now - at < p.force_cancel_after_mins as i64 * 60 {
                continue;
            }
            match self.force_cancel(repo, id, dry_run).await {
                Ok(true) => {
                    tracing::info!(
                        "janitor: force-cancelled {repo} run {id}: cancel requested {}s ago did not finish it",
                        now - at
                    );
                    report.force_cancelled += 1;
                    self.forget(id);
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!("janitor: {repo} run {id}: {e}");
                    report.errors += 1;
                }
            }
        }

        let parsed_succeeded: Vec<(&Run, Intent)> = succeeded
            .iter()
            .filter_map(|r| parse_title(&r.display_title).map(|i| (r, i)))
            .collect();
        let parsed_started: Vec<(&Run, Intent)> = active
            .iter()
            .filter(|r| r.status == "in_progress")
            .filter_map(|r| parse_title(&r.display_title).map(|i| (r, i)))
            .collect();
        let mut compares = BTreeMap::new();
        for run in &active {
            if self
                .promotion
                .lock()
                .expect("promotion state lock")
                .requested
                .contains_key(&run.id)
            {
                continue;
            }
            let age = unix(&run.created_at).map_or(0, |t| now - t);
            let pending = PENDING.contains(&run.status.as_str());
            let Some(intent) = parse_title(&run.display_title) else {
                if !pending || age < p.ghost_after_hours as i64 * 3600 {
                    continue;
                }
                report.promotion_considered += 1;
                match self.promotion_jobs(repo, run.id).await {
                    Ok(jobs) if jobs.is_empty() => {
                        report.promotion_ghost += 1;
                        let why = format!("queued {age}s with no jobs");
                        self.promotion_cancel(repo, run, &why, dry_run, budget, report)
                            .await;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!("janitor: {repo} run {}: {e}", run.id);
                        report.errors += 1;
                    }
                }
                continue;
            };
            let stale = age >= p.stale_after_hours as i64 * 3600;
            if !pending && !stale {
                continue;
            }
            report.promotion_considered += 1;
            let jobs = match self.promotion_jobs(repo, run.id).await {
                Ok(jobs) => jobs,
                Err(e) => {
                    tracing::warn!("janitor: {repo} run {}: {e}", run.id);
                    report.errors += 1;
                    continue;
                }
            };
            if jobs
                .iter()
                .any(|j| j.status == "in_progress" && p.mutation_jobs.contains(&j.name))
            {
                continue;
            }
            let applied = jobs.iter().any(|j| j.name == p.apply_job && executed(j));
            // Successors newest first: a successful one serves both rules, a
            // started one only the pending rule.
            let mut successors: Vec<(&Run, &Intent, bool)> = parsed_succeeded
                .iter()
                .map(|(r, i)| (*r, i, true))
                .chain(parsed_started.iter().map(|(r, i)| (*r, i, false)))
                .filter(|(r, i, _)| r.id > run.id && covers(i, &intent))
                .collect();
            successors.sort_by_key(|(r, _, _)| std::cmp::Reverse(r.id));
            let superseded_rule = pending && !applied;
            let Some(source) = p.source_repos.get(&intent.event) else {
                if !successors.is_empty() {
                    report.promotion_unprovable += 1;
                }
                continue;
            };
            let mut chosen = None;
            for (r, i, success) in successors {
                if !(success && stale || superseded_rule) {
                    continue;
                }
                match self
                    .same_or_ahead(source, &intent.sha, &i.sha, &mut compares)
                    .await
                {
                    Ok(true) => {
                        chosen = Some((r.id, success));
                        break;
                    }
                    Ok(false) => {}
                    Err(e) => {
                        tracing::warn!("janitor: {source} compare {}...{}: {e}", intent.sha, i.sha);
                        report.errors += 1;
                        break;
                    }
                }
            }
            let Some((newer, success)) = chosen else {
                continue;
            };
            let why = if superseded_rule {
                report.promotion_superseded += 1;
                format!(
                    "pending {} superseded by {} run {newer}",
                    intent.sha,
                    if success { "successful" } else { "started" }
                )
            } else {
                report.promotion_stale += 1;
                format!("{age}s old, superseded by successful run {newer}")
            };
            self.promotion_cancel(repo, run, &why, dry_run, budget, report)
                .await;
        }
        Ok(())
    }

    async fn promotion_cancel(
        &self,
        repo: &str,
        run: &Run,
        why: &str,
        dry_run: bool,
        budget: &mut u32,
        report: &mut PassReport,
    ) {
        if *budget == 0 {
            report.deferred += 1;
            return;
        }
        let outcome = self.cancel_if_unchanged(repo, run, why, dry_run).await;
        if matches!(outcome, Ok(Outcome::Cancelled)) {
            self.promotion
                .lock()
                .expect("promotion state lock")
                .requested
                .insert(run.id, now());
        }
        tally(report, budget, repo, run.id, Kind::Promotion, outcome);
    }

    fn forget(&self, run: u64) {
        self.promotion
            .lock()
            .expect("promotion state lock")
            .requested
            .remove(&run);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "834792e1c990796237c149e5d79d4151ba42bdd7";

    #[test]
    fn parses_the_promotion_run_name() {
        let i = parse_title(&format!(
            "Promote runtime images sha-{SHA} services=memory,ingest dex-governance=false source-event=platform_runtime_images_published source-run=38749160525"
        ))
        .unwrap();
        assert_eq!(i.sha, SHA);
        assert_eq!(
            i.services.iter().map(String::as_str).collect::<Vec<_>>(),
            ["ingest", "memory"]
        );
        assert_eq!(i.event, "platform_runtime_images_published");
    }

    #[test]
    fn rejects_other_names() {
        for t in [
            "platform_runtime_images_published",
            "Promote runtime images sha-abc services=a source-event=e",
            &format!("Promote runtime images sha-{SHA} source-event=e"),
            &format!("Promote runtime images sha-{SHA} services= source-event=e"),
            &format!(
                "Promote runtime images sha-{} services=a source-event=e",
                SHA.to_uppercase()
            ),
        ] {
            assert_eq!(parse_title(t), None, "{t}");
        }
    }

    #[test]
    fn covering_needs_the_same_event_and_every_service() {
        let i = |s: &str, e: &str| {
            parse_title(&format!(
                "Promote runtime images sha-{SHA} services={s} source-event={e}"
            ))
            .unwrap()
        };
        assert!(covers(&i("a,b", "x"), &i("a", "x")));
        assert!(covers(&i("a", "x"), &i("a", "x")));
        assert!(!covers(&i("a", "x"), &i("a,b", "x")));
        assert!(!covers(&i("a,b", "y"), &i("a", "x")));
    }
}
