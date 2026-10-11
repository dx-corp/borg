//! `borg run -- <command>`: run a command where capacity is free, or here.
//!
//! The client resolves the tree it is asked to run against, submits a work unit
//! to the queen (`POST /v1/units`, see [`crate::units`]), streams the unit's log
//! to this process's stdout and exits with the unit's exit code. It never makes
//! things worse than running the command directly: when the queen is not
//! configured, unreachable, refuses the unit, or no drone picks it up within
//! `wait_secs`, the command runs locally with this process's own stdio.
//!
//! * The tree is `git write-tree` of the index (`--index`, for pre-commit
//!   hooks) or `HEAD^{tree}` (default), or an explicit SHA.
//! * An identical unit that is already running is joined (its output is
//!   streamed); one that already succeeded is not run at all: the receipt is
//!   reported and the exit code is 0.
//! * A unit that fails for infrastructure reasons (cancelled, timed out, out of
//!   attempts, or the queen lost track of it) is run locally rather than
//!   reported as the command's failure. A command that ran and exited nonzero
//!   on a drone is a real failure: its code is returned as is.
//! * The drone must be able to get the tree. A tree from a pushed commit is in
//!   its git mirror. Otherwise `--publish` uploads a `git bundle` of just the
//!   missing objects (everything since the merge base with `origin/main`) to the
//!   queen, which hands it to the drone; nothing is pushed to any remote. Without
//!   `--publish`, an unpublished tree runs locally.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::secret::Secret;

/// Where the tree comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeSource {
    /// `git write-tree`: the staged content (pre-commit hooks).
    Index,
    /// `HEAD^{tree}`.
    Head,
    Explicit(String),
}

#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Queen base URL; `None` runs locally.
    pub queen: Option<String>,
    pub token: Option<Secret>,
    pub dir: PathBuf,
    pub tree: TreeSource,
    /// `owner/name`; default derived from `origin`.
    pub repo: Option<String>,
    pub platform: String,
    pub class: String,
    pub toolchain: String,
    pub timeout_secs: Option<u64>,
    pub force: bool,
    /// How long to wait for a drone to pick the unit up before running locally.
    pub wait_secs: u64,
    pub publish: bool,
    pub argv: Vec<String>,
}

/// `<os>-<arch>` of this machine in the queen's notation.
pub fn local_platform() -> String {
    let arch = match std::env::consts::ARCH {
        "arm64" => "aarch64",
        "amd64" => "x86_64",
        other => other,
    };
    format!("{}-{arch}", std::env::consts::OS)
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    #[allow(
        clippy::disallowed_methods,
        reason = "fixed git subcommands that resolve the tree and repository for the user's own checkout"
    )]
    let out = StdCommand::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    } else {
        Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// The tree object id for `source`.
pub fn resolve_tree(dir: &Path, source: &TreeSource) -> Result<String, String> {
    match source {
        TreeSource::Index => git(dir, &["write-tree"]),
        TreeSource::Head => git(dir, &["rev-parse", "HEAD^{tree}"]),
        TreeSource::Explicit(sha) => {
            git(dir, &["rev-parse", "--verify", &format!("{sha}^{{tree}}")])
        }
    }
}

/// The channel a `rust-toolchain.toml` (or a legacy one-line `rust-toolchain`)
/// names.
pub fn parse_toolchain_channel(text: &str) -> Option<String> {
    let valid = |c: &str| {
        !c.is_empty()
            && c.len() <= 64
            && c.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
    };
    let mut lines = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'));
    let first = lines.clone().next()?;
    if !first.starts_with('[') && !first.contains('=') {
        return Some(first.to_owned()).filter(|c| valid(c));
    }
    lines
        .find_map(|l| {
            let (k, v) = l.split_once('=')?;
            (k.trim() == "channel").then(|| {
                v.split('#')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .trim_matches('"')
                    .to_owned()
            })
        })
        .filter(|c| valid(c))
}

/// The Rust toolchain the tree asks for, looked for at the repository root and
/// under `rust/`.
pub fn tree_rust_toolchain(dir: &Path, tree: &str) -> Option<String> {
    [
        "rust-toolchain.toml",
        "rust/rust-toolchain.toml",
        "rust-toolchain",
    ]
    .iter()
    .find_map(|p| git(dir, &["show", &format!("{tree}:{p}")]).ok())
    .and_then(|t| parse_toolchain_channel(&t))
}

/// `owner/name` from a remote URL (`git@host:owner/name.git`,
/// `https://host/owner/name`).
pub fn repo_from_remote(url: &str) -> Option<String> {
    let path = url.trim().trim_end_matches('/').trim_end_matches(".git");
    let tail = path.rsplit([':', '/']).take(2).collect::<Vec<_>>();
    match tail.as_slice() {
        [name, owner] if !name.is_empty() && !owner.is_empty() => Some(format!("{owner}/{name}")),
        _ => None,
    }
}

/// Whether the tree exists on a remote already: some commit reachable from a
/// remote-tracking ref has exactly this tree. Conservative: `false` when unsure.
fn tree_is_published(dir: &Path, tree: &str) -> bool {
    git(dir, &["log", "--remotes", "--format=%T", "-n", "2000"])
        .map(|t| t.lines().any(|l| l == tree))
        .unwrap_or(false)
}

fn git_bytes(dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    #[allow(
        clippy::disallowed_methods,
        reason = "fixed git subcommands that bundle the user's own objects"
    )]
    let out = StdCommand::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// The merge base of HEAD with the remote's default branch: what a drone's
/// mirror is certain to have.
fn bundle_base(dir: &Path) -> Result<String, String> {
    for upstream in ["origin/HEAD", "origin/main", "origin/master"] {
        if let Ok(base) = git(dir, &["merge-base", "HEAD", upstream]) {
            return Ok(base);
        }
    }
    Err("no merge base with origin/main".into())
}

/// A git bundle holding `tree`: a synthetic commit whose parent is the merge
/// base with the remote default branch, bundled with everything the merge base
/// already reaches excluded. The bundle's single ref is `refs/borg/unit/<tree>`.
/// Nothing is pushed anywhere.
pub fn build_bundle(dir: &Path, tree: &str) -> Result<Vec<u8>, String> {
    let base = bundle_base(dir)?;
    let commit = git(
        dir,
        &[
            "-c",
            "user.name=borg",
            "-c",
            "user.email=borg@localhost",
            "commit-tree",
            tree,
            "-p",
            &base,
            "-m",
            "borg unit tree",
        ],
    )?;
    let name = format!("refs/borg/unit/{tree}");
    git(dir, &["update-ref", &name, &commit])?;
    let bundle = git_bytes(dir, &["bundle", "create", "-", &name, &format!("^{base}")]);
    // Never leave the temporary ref behind, whatever the bundle did.
    let _ = git(dir, &["update-ref", "-d", &name]);
    bundle
}

/// Run the command here with this process's stdio; returns its exit code.
pub fn run_local(argv: &[String], dir: &Path) -> i32 {
    let Some((prog, args)) = argv.split_first() else {
        return 2;
    };
    #[allow(
        clippy::disallowed_methods,
        reason = "the fallback runs the command the user typed after --, on their own machine, with their own stdio"
    )]
    let status = StdCommand::new(prog).args(args).current_dir(dir).status();
    match status {
        Ok(s) => s.code().unwrap_or(128 + signal_of(&s)),
        Err(e) => {
            eprintln!("borg run: cannot start {prog}: {e}");
            127
        }
    }
}

#[cfg(unix)]
fn signal_of(s: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    s.signal().unwrap_or(0)
}

#[cfg(not(unix))]
fn signal_of(_: &std::process::ExitStatus) -> i32 {
    0
}

#[derive(Deserialize)]
struct Submitted {
    outcome: String,
    unit_id: String,
    receipt: Option<serde_json::Value>,
}

struct Remote<'a> {
    http: reqwest::Client,
    base: &'a str,
    token: &'a Secret,
}

impl Remote<'_> {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base.trim_end_matches('/'))
    }
}

async fn upload_bundle(q: &Remote<'_>, dir: &Path, tree: &str, unit: &str) -> Result<(), String> {
    let (dir, tree_id) = (dir.to_path_buf(), tree.to_owned());
    let bundle = tokio::task::spawn_blocking(move || build_bundle(&dir, &tree_id))
        .await
        .map_err(|e| format!("bundling failed ({e})"))?
        .map_err(|e| format!("bundling the tree failed ({e})"))?;
    let resp = q
        .http
        .put(q.url(&format!("/v1/units/{unit}/bundle")))
        .bearer_auth(q.token.expose())
        .timeout(Duration::from_secs(120))
        .body(bundle)
        .send()
        .await
        .map_err(|e| format!("uploading the bundle failed ({e})"))?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(format!("the queen refused the bundle ({})", resp.status()))
    }
}

/// Why a remote attempt did not produce the command's result.
enum Fallback {
    /// Run it here, with this reason on stderr.
    Local(String),
}

/// Run `opts.argv`: remotely when possible, otherwise here. Returns the exit
/// code to exit with. Remote log bytes go to `out`.
pub async fn run(opts: &RunOptions, out: &mut dyn Write) -> i32 {
    match remote(opts, out).await {
        Ok(code) => code,
        Err(Fallback::Local(why)) => {
            eprintln!("borg run: {why}; running locally");
            run_local(&opts.argv, &opts.dir)
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one linear submit, wait, stream, finish flow"
)]
async fn remote(opts: &RunOptions, out: &mut dyn Write) -> Result<i32, Fallback> {
    let local = |why: &str| Fallback::Local(why.to_owned());
    let (Some(base), Some(token)) = (&opts.queen, &opts.token) else {
        return Err(local("no queen configured"));
    };
    let tree = resolve_tree(&opts.dir, &opts.tree).map_err(Fallback::Local)?;
    let repo = match &opts.repo {
        Some(r) => r.clone(),
        None => git(&opts.dir, &["remote", "get-url", "origin"])
            .ok()
            .and_then(|u| repo_from_remote(&u))
            .ok_or_else(|| local("cannot tell the repository (no origin remote)"))?,
    };
    let needs_bundle = !tree_is_published(&opts.dir, &tree);
    if needs_bundle && !opts.publish {
        return Err(local("the tree is not on a remote (use --publish)"));
    }
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(40))
        .build()
        .map_err(|e| Fallback::Local(e.to_string()))?;
    let q = Remote { http, base, token };

    let body = serde_json::json!({
        "repo": repo, "tree_sha": tree, "argv": opts.argv, "platform": opts.platform,
        "toolchain": opts.toolchain, "class": opts.class,
        "timeout_secs": opts.timeout_secs, "force": opts.force, "bundle": needs_bundle,
        "rust_toolchain": tree_rust_toolchain(&opts.dir, &tree),
    });
    let resp = q
        .http
        .post(q.url("/v1/units"))
        .bearer_auth(q.token.expose())
        .timeout(Duration::from_secs(10))
        .json(&body)
        .send()
        .await
        .map_err(|e| Fallback::Local(format!("queen unreachable ({e})")))?;
    if !resp.status().is_success() {
        return Err(Fallback::Local(format!(
            "queen refused the unit ({})",
            resp.status()
        )));
    }
    let sub: Submitted = resp
        .json()
        .await
        .map_err(|e| Fallback::Local(format!("unreadable queen answer ({e})")))?;
    if sub.outcome == "receipt" {
        let drone = sub
            .receipt
            .as_ref()
            .and_then(|r| r["drone"].as_str())
            .unwrap_or("a drone");
        eprintln!(
            "borg run: tree {} already passed on {drone} (unit {}); not running again",
            &tree[..tree.len().min(12)],
            sub.unit_id
        );
        return Ok(0);
    }
    let created = sub.outcome == "new";
    if created && needs_bundle {
        let sent = upload_bundle(&q, &opts.dir, &tree, &sub.unit_id).await;
        if let Err(why) = sent {
            let _ = q
                .http
                .post(q.url(&format!("/v1/units/{}/cancel", sub.unit_id)))
                .bearer_auth(q.token.expose())
                .send()
                .await;
            return Err(Fallback::Local(why));
        }
    }
    eprintln!(
        "borg run: unit {} {} on the fleet",
        sub.unit_id,
        if created { "queued" } else { "joined" }
    );

    let started = Instant::now();
    let mut offset: u64 = 0;
    let mut running = false;
    let mut unreachable_since: Option<Instant> = None;
    loop {
        let poll = q
            .http
            .get(q.url(&format!(
                "/v1/units/{}/log?from={offset}&wait=10",
                sub.unit_id
            )))
            .bearer_auth(q.token.expose())
            .timeout(Duration::from_secs(25));
        let resp = tokio::select! {
            r = poll.send() => r,
            _ = tokio::signal::ctrl_c() => {
                if created {
                    let _ = q.http.post(q.url(&format!("/v1/units/{}/cancel", sub.unit_id)))
                        .bearer_auth(q.token.expose()).send().await;
                }
                eprintln!("borg run: interrupted");
                return Ok(130);
            }
        };
        let resp = match resp {
            Ok(r) if r.status().is_success() => {
                unreachable_since = None;
                r
            }
            other => {
                let since = *unreachable_since.get_or_insert_with(Instant::now);
                if since.elapsed() > Duration::from_secs(60) {
                    return Err(Fallback::Local(match other {
                        Ok(r) => format!("queen answered {} for 60s", r.status()),
                        Err(e) => format!("lost the queen ({e})"),
                    }));
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        let hdr = |k: &str| {
            resp.headers()
                .get(k)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let state = hdr("x-borg-state").unwrap_or_default();
        let next = hdr("x-borg-next").and_then(|v| v.parse::<u64>().ok());
        let exit = hdr("x-borg-exit").and_then(|v| v.parse::<i32>().ok());
        let failure = hdr("x-borg-failure");
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| Fallback::Local(format!("log read failed ({e})")))?;
        if !bytes.is_empty() {
            let _ = out.write_all(&bytes);
            let _ = out.flush();
        }
        if let Some(n) = next {
            offset = n;
        }
        if state == "running" || state == "done" {
            running = true;
        }
        match state.as_str() {
            "done" => {
                if let Some(code) = exit {
                    return Ok(code);
                }
            }
            "failed" => {
                let why = failure.unwrap_or_else(|| "failed".into());
                return Err(Fallback::Local(format!(
                    "the unit did not run to completion ({why})"
                )));
            }
            "queued" | "leased"
                if !running && started.elapsed() >= Duration::from_secs(opts.wait_secs) =>
            {
                if created {
                    let _ = q
                        .http
                        .post(q.url(&format!("/v1/units/{}/cancel", sub.unit_id)))
                        .bearer_auth(q.token.expose())
                        .send()
                        .await;
                }
                return Err(Fallback::Local(format!(
                    "no drone took it within {}s",
                    opts.wait_secs
                )));
            }
            _ => {}
        }
    }
}
