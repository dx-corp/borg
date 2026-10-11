//! Bare git mirrors: one per configured repository under
//! `<cache_root>/git/<owner>/<repo>.git`, refreshed on a timer and before a
//! lease starts. Jobs learn the path from `BORG_GIT_MIRROR_<OWNER>_<REPO>`,
//! set only while the mirror exists, so they can fall back to a plain clone.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::drone::config::GitCaches;
use crate::drone::exec::Exec;
use crate::drone::fsops::FsOps;

/// A mirror refreshed this recently is not refreshed again for a new lease.
const MIN_REFRESH_GAP: Duration = Duration::from_secs(15);
/// Upper bound for one mirror refresh (the first clone of a large repository included).
const REFRESH_TIMEOUT: Duration = Duration::from_secs(1800);
const PREFIX: &str = "BORG_GIT_MIRROR_";

/// Environment variable naming the mirror of `owner/repo`: upper-cased with
/// every non-alphanumeric character replaced by `_`
/// (`example-org/app` becomes `BORG_GIT_MIRROR_EXAMPLE_ORG_APP`).
pub fn mirror_var(repo: &str) -> String {
    let body: String = repo
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("{PREFIX}{body}")
}

/// `<cache_root>/git/<owner>/<repo>.git`
pub fn mirror_path(cache_root: &Path, repo: &str) -> PathBuf {
    cache_root.join("git").join(format!("{repo}.git"))
}

/// The URL a mirror fetches from: `base` and `owner/repo.git`, joined by `/`
/// unless `base` already ends in `/` or `:` (scp-style `git@host:`).
pub fn remote_url(base: &str, repo: &str) -> String {
    if base.ends_with('/') || base.ends_with(':') {
        format!("{base}{repo}.git")
    } else {
        format!("{base}/{repo}.git")
    }
}

fn run_git(exec: &dyn Exec, git: &str, args: &[&str]) -> Result<(), String> {
    match exec.run(git, args) {
        Ok(out) if out.success => Ok(()),
        Ok(_) => Err(format!("`git {}` failed", args.join(" "))),
        Err(e) => Err(format!("{git}: {e}")),
    }
}

/// Create the mirror if needed, point it at `url`, and fetch every branch and
/// tag into it. Idempotent, so an interrupted first clone heals on the next run.
fn refresh_mirror(
    exec: &dyn Exec,
    fs_ops: &dyn FsOps,
    git: &str,
    path: &Path,
    url: &str,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs_ops
            .mkdir_p(parent)
            .map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let p = path.to_string_lossy();
    if !path.join("HEAD").exists() {
        run_git(exec, git, &["init", "--bare", "--quiet", &p])?;
    }
    run_git(exec, git, &["-C", &p, "config", "remote.origin.url", url])?;
    run_git(
        exec,
        git,
        &[
            "-C",
            &p,
            "config",
            "remote.origin.fetch",
            "+refs/heads/*:refs/heads/*",
        ],
    )?;
    // Objects only accumulate: a job borrowing them via alternates must never
    // see them pruned underneath it.
    run_git(exec, git, &["-C", &p, "config", "gc.auto", "0"])?;
    run_git(
        exec,
        git,
        &[
            "-C",
            &p,
            "-c",
            "http.lowSpeedLimit=1000",
            "-c",
            "http.lowSpeedTime=60",
            "fetch",
            "--prune",
            "--tags",
            "--force",
            "--quiet",
            "origin",
        ],
    )
}

struct Mirror {
    repo: String,
    path: PathBuf,
    url: String,
    refreshed: Mutex<Option<Instant>>,
}

pub struct GitMirrors {
    git: String,
    exec: Arc<dyn Exec>,
    fs_ops: Arc<dyn FsOps>,
    mirrors: Vec<Mirror>,
    /// Leases that were handed a mirror / ran without one.
    served: AtomicU64,
    missed: AtomicU64,
}

impl GitMirrors {
    pub fn new(
        cfg: &GitCaches,
        cache_root: &Path,
        exec: Arc<dyn Exec>,
        fs_ops: Arc<dyn FsOps>,
    ) -> Self {
        let mirrors = cfg
            .mirrors
            .iter()
            .map(|repo| Mirror {
                repo: repo.clone(),
                path: mirror_path(cache_root, repo),
                url: remote_url(&cfg.remote_base, repo),
                refreshed: Mutex::new(None),
            })
            .collect();
        Self {
            git: cfg.bin.clone(),
            exec,
            fs_ops,
            mirrors,
            served: AtomicU64::new(0),
            missed: AtomicU64::new(0),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.mirrors.is_empty()
    }

    async fn refresh(&self, mirror: &Mirror, force: bool) {
        let mut last = mirror.refreshed.lock().await;
        if !force && last.is_some_and(|t| t.elapsed() < MIN_REFRESH_GAP) {
            return;
        }
        let (exec, fs_ops, git, path, url) = (
            Arc::clone(&self.exec),
            Arc::clone(&self.fs_ops),
            self.git.clone(),
            mirror.path.clone(),
            mirror.url.clone(),
        );
        let job = tokio::task::spawn_blocking(move || {
            refresh_mirror(exec.as_ref(), fs_ops.as_ref(), &git, &path, &url)
        });
        match tokio::time::timeout(REFRESH_TIMEOUT, job).await {
            Ok(Ok(Ok(()))) => *last = Some(Instant::now()),
            Ok(Ok(Err(e))) => tracing::warn!("git mirror {} refresh failed: {e}", mirror.repo),
            Ok(Err(e)) => tracing::warn!("git mirror {} refresh task failed: {e}", mirror.repo),
            Err(_) => tracing::warn!("git mirror {} refresh timed out", mirror.repo),
        }
    }

    pub async fn refresh_stale(&self) {
        for m in &self.mirrors {
            self.refresh(m, false).await;
        }
    }

    pub async fn refresh_all(&self) {
        for m in &self.mirrors {
            self.refresh(m, true).await;
        }
    }

    /// The `BORG_GIT_MIRROR_*` variables for mirrors that exist, counting the
    /// lease as served or not.
    pub fn env(&self) -> Vec<(String, String)> {
        let mut env = Vec::new();
        for m in &self.mirrors {
            if m.path.join("HEAD").exists() {
                self.served.fetch_add(1, Ordering::Relaxed);
                env.push((mirror_var(&m.repo), m.path.to_string_lossy().into_owned()));
            } else {
                self.missed.fetch_add(1, Ordering::Relaxed);
            }
        }
        env
    }

    pub fn hit_counts(&self) -> (u64, u64) {
        (
            self.served.load(Ordering::Relaxed),
            self.missed.load(Ordering::Relaxed),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drone::cleanup::testing::FakeExec;
    use crate::drone::fsops::LocalFs;

    fn cfg(mirrors: &[&str]) -> GitCaches {
        GitCaches {
            mirrors: mirrors.iter().map(|m| (*m).to_owned()).collect(),
            remote_base: "https://git.example.test".to_owned(),
            ..GitCaches::default()
        }
    }

    #[test]
    fn variable_names_paths_and_urls() {
        assert_eq!(
            mirror_var("example-org/app"),
            "BORG_GIT_MIRROR_EXAMPLE_ORG_APP"
        );
        assert_eq!(mirror_var("a.b/c_d"), "BORG_GIT_MIRROR_A_B_C_D");
        assert_eq!(
            mirror_path(Path::new("/cache"), "example-org/app"),
            Path::new("/cache/git/example-org/app.git")
        );
        assert_eq!(
            remote_url("https://git.example.test", "o/r"),
            "https://git.example.test/o/r.git"
        );
        assert_eq!(
            remote_url("https://git.example.test/", "o/r"),
            "https://git.example.test/o/r.git"
        );
        assert_eq!(
            remote_url("git@git.example.test:", "o/r"),
            "git@git.example.test:o/r.git"
        );
    }

    #[tokio::test]
    async fn refresh_runs_the_expected_steps_and_is_rate_limited() {
        let root = tempfile::tempdir().unwrap();
        let x = Arc::new(FakeExec::default());
        let m = GitMirrors::new(
            &cfg(&["example-org/app"]),
            root.path(),
            x.clone(),
            Arc::new(LocalFs),
        );
        let path = mirror_path(root.path(), "example-org/app")
            .display()
            .to_string();

        m.refresh_stale().await;
        let calls = x.calls();
        assert_eq!(calls[0], format!("git init --bare --quiet {path}"));
        assert_eq!(
            calls[1],
            format!(
                "git -C {path} config remote.origin.url https://git.example.test/example-org/app.git"
            )
        );
        assert_eq!(
            calls[2],
            format!("git -C {path} config remote.origin.fetch +refs/heads/*:refs/heads/*")
        );
        assert_eq!(calls[3], format!("git -C {path} config gc.auto 0"));
        assert!(
            calls[4].contains("fetch --prune --tags --force --quiet origin"),
            "{}",
            calls[4]
        );
        assert_eq!(calls.len(), 5);

        m.refresh_stale().await;
        assert_eq!(
            x.calls().len(),
            5,
            "a lease right after a refresh does not refresh again"
        );
        m.refresh_all().await;
        assert!(
            x.calls().len() > 5,
            "the timer refresh ignores the rate limit"
        );
    }

    #[tokio::test]
    async fn a_failed_refresh_is_retried_by_the_next_lease() {
        let root = tempfile::tempdir().unwrap();
        let x = Arc::new(FakeExec::default());
        let path = mirror_path(root.path(), "example-org/app")
            .display()
            .to_string();
        x.fail(&format!(
            "git -C {path} -c http.lowSpeedLimit=1000 -c http.lowSpeedTime=60 fetch --prune --tags --force --quiet origin"
        ));
        let m = GitMirrors::new(
            &cfg(&["example-org/app"]),
            root.path(),
            x.clone(),
            Arc::new(LocalFs),
        );
        m.refresh_stale().await;
        let after_first = x.calls().len();
        m.refresh_stale().await;
        assert!(
            x.calls().len() > after_first,
            "failure does not start the rate-limit window"
        );
    }

    #[test]
    fn env_lists_only_existing_mirrors_and_counts_hits_and_misses() {
        let root = tempfile::tempdir().unwrap();
        let m = GitMirrors::new(
            &cfg(&["example-org/app", "example-org/other"]),
            root.path(),
            Arc::new(FakeExec::default()),
            Arc::new(LocalFs),
        );
        let present = mirror_path(root.path(), "example-org/app");
        std::fs::create_dir_all(&present).unwrap();
        std::fs::write(present.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let env = m.env();
        assert_eq!(
            env,
            [(
                "BORG_GIT_MIRROR_EXAMPLE_ORG_APP".to_owned(),
                present.display().to_string()
            )]
        );
        assert_eq!(m.hit_counts(), (1, 1));
    }
}
