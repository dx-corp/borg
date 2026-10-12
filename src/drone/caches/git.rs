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
use crate::mirror_auth::MirrorCredential;

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

const EXTRAHEADER: &str = "http.extraheader=";

/// `args` for a log line, with the credential header hidden.
fn redacted(args: &[&str]) -> String {
    args.iter()
        .map(|a| {
            if a.starts_with(EXTRAHEADER) {
                "http.extraheader=<redacted>"
            } else {
                a
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The `-c` value that authenticates a fetch from GitHub over https as an
/// App installation. Passed per command, so the token is never written to the
/// mirror's config or the stored remote URL.
fn auth_header_config(token: &str) -> String {
    let basic = base64(format!("x-access-token:{token}").as_bytes());
    format!("{EXTRAHEADER}AUTHORIZATION: basic {basic}")
}

fn run_git(exec: &dyn Exec, git: &str, args: &[&str]) -> Result<(), String> {
    match exec.run(git, args) {
        Ok(out) if out.success => Ok(()),
        Ok(_) => Err(format!("`git {}` failed", redacted(args))),
        Err(e) => Err(format!("{git}: {e}")),
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

/// Create the mirror if needed, point it at `url`, and fetch every branch and
/// tag into it. Idempotent, so an interrupted first clone heals on the next run.
fn refresh_mirror(
    exec: &dyn Exec,
    fs_ops: &dyn FsOps,
    git: &str,
    path: &Path,
    url: &str,
    auth: Option<&str>,
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
    let mut args = vec![
        "-C",
        &p,
        "-c",
        "http.lowSpeedLimit=1000",
        "-c",
        "http.lowSpeedTime=60",
    ];
    if let Some(header) = auth {
        args.extend(["-c", header]);
    }
    args.extend(["fetch", "--prune", "--tags", "--force", "--quiet", "origin"]);
    run_git(exec, git, &args)
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
    /// Fetch credentials apply only to an `https://` remote base.
    https: bool,
    /// The queen's latest token; memory only, replaced on each heartbeat.
    credential: std::sync::Mutex<Option<MirrorCredential>>,
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
            https: cfg.remote_base.starts_with("https://"),
            credential: std::sync::Mutex::new(None),
            served: AtomicU64::new(0),
            missed: AtomicU64::new(0),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.mirrors.is_empty()
    }

    /// Keep the queen's latest credential. Returns true when mirrors were
    /// configured and no usable credential was held before, so the caller can
    /// refresh them now instead of at the next timer tick.
    pub fn set_credential(&self, credential: MirrorCredential) -> bool {
        let now = unix_now();
        let Ok(mut held) = self.credential.lock() else {
            return false;
        };
        let was_usable = held.as_ref().is_some_and(|c| c.expires_at > now);
        *held = Some(credential);
        !was_usable && !self.mirrors.is_empty() && self.https
    }

    /// The `-c` auth setting for `repo`, when a held credential covers it and
    /// has not expired; an expired one is dropped.
    fn auth_for(&self, repo: &str) -> Option<String> {
        if !self.https {
            return None;
        }
        let mut held = self.credential.lock().ok()?;
        if held.as_ref().is_some_and(|c| c.expires_at <= unix_now()) {
            *held = None;
        }
        let c = held.as_ref()?;
        c.repositories
            .iter()
            .any(|r| r == repo)
            .then(|| auth_header_config(c.token.expose()))
    }

    async fn refresh(&self, mirror: &Mirror, force: bool) {
        let mut last = mirror.refreshed.lock().await;
        if !force && last.is_some_and(|t| t.elapsed() < MIN_REFRESH_GAP) {
            return;
        }
        let (exec, fs_ops, git, path, url, auth) = (
            Arc::clone(&self.exec),
            Arc::clone(&self.fs_ops),
            self.git.clone(),
            mirror.path.clone(),
            mirror.url.clone(),
            self.auth_for(&mirror.repo),
        );
        let job = tokio::task::spawn_blocking(move || {
            refresh_mirror(
                exec.as_ref(),
                fs_ops.as_ref(),
                &git,
                &path,
                &url,
                auth.as_deref(),
            )
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

    fn credential(expires_at: i64, repos: &[&str]) -> MirrorCredential {
        MirrorCredential {
            token: crate::secret::Secret::new("ghs_mirror_token"),
            expires_at,
            repositories: repos.iter().map(|r| (*r).to_owned()).collect(),
        }
    }

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"x-access-token:abc"), "eC1hY2Nlc3MtdG9rZW46YWJj");
    }

    #[tokio::test]
    async fn the_credential_rides_the_fetch_command_and_never_the_stored_url() {
        let root = tempfile::tempdir().unwrap();
        let x = Arc::new(FakeExec::default());
        let m = GitMirrors::new(
            &cfg(&["example-org/app", "example-org/other"]),
            root.path(),
            x.clone(),
            Arc::new(LocalFs),
        );
        let far = unix_now() + 3600;
        assert!(
            m.set_credential(credential(far, &["example-org/app"])),
            "the first usable credential asks for a refresh"
        );
        assert!(
            !m.set_credential(credential(far, &["example-org/app"])),
            "a replacement does not"
        );
        m.refresh_all().await;

        let header = format!(
            "http.extraheader=AUTHORIZATION: basic {}",
            base64(b"x-access-token:ghs_mirror_token")
        );
        let calls = x.calls();
        let fetch = |repo: &str| {
            calls
                .iter()
                .find(|c| c.contains("fetch --prune") && c.contains(&format!("{repo}.git")))
                .cloned()
                .unwrap()
        };
        assert!(fetch("example-org/app").contains(&format!("-c {header} fetch")));
        assert!(
            !fetch("example-org/other").contains("extraheader"),
            "repositories the credential does not name get no header"
        );
        for call in &calls {
            if call.contains("config remote.origin.url") {
                assert!(
                    call.ends_with("https://git.example.test/example-org/app.git")
                        || call.ends_with("https://git.example.test/example-org/other.git"),
                    "{call}"
                );
            }
            if !call.contains("fetch --prune") {
                assert!(!call.contains("ghs_mirror_token"), "{call}");
            }
        }
    }

    #[tokio::test]
    async fn an_expired_credential_is_dropped_and_ssh_bases_ignore_it() {
        let root = tempfile::tempdir().unwrap();
        let x = Arc::new(FakeExec::default());
        let m = GitMirrors::new(
            &cfg(&["example-org/app"]),
            root.path(),
            x.clone(),
            Arc::new(LocalFs),
        );
        m.set_credential(credential(unix_now() - 1, &["example-org/app"]));
        m.refresh_all().await;
        assert!(x.calls().iter().all(|c| !c.contains("extraheader")));
        assert!(m.credential.lock().unwrap().is_none());

        let ssh = GitCaches {
            remote_base: "git@git.example.test:".to_owned(),
            ..cfg(&["example-org/app"])
        };
        let m = GitMirrors::new(&ssh, root.path(), x.clone(), Arc::new(LocalFs));
        assert!(!m.set_credential(credential(unix_now() + 3600, &["example-org/app"])));
        m.refresh_all().await;
        assert!(x.calls().iter().all(|c| !c.contains("extraheader")));
    }

    #[test]
    fn failure_messages_hide_the_header() {
        let header = auth_header_config("ghs_mirror_token");
        let shown = redacted(&["-c", &header, "fetch"]);
        assert_eq!(shown, "-c http.extraheader=<redacted> fetch");
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
