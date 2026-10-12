//! `borg run -- <command>`: run a command where capacity is free, or here.
//!
//! The client resolves the tree it is asked to run against, submits a work unit
//! to the queen (`POST /v1/units`, see [`crate::units`]), streams the unit's log
//! to this process's stdout and exits with the unit's exit code. By default,
//! when the queen is not configured, unreachable, refuses the unit, or no drone
//! picks it up within `wait_secs`, the command runs locally with this process's
//! own stdio. `--remote-only` disables every local fallback and returns
//! infrastructure exit code 125 instead. `--result-file` writes JSON execution
//! provenance separately from command output.
//!
//! * The tree is `git write-tree` of the index (`--index`, for pre-commit
//!   hooks) or `HEAD^{tree}` (default), or an explicit SHA.
//! * An identical unit that is already running is joined (its output is
//!   streamed); one that already succeeded is not run at all: the receipt is
//!   reported and the exit code is 0.
//! * A unit that fails for infrastructure reasons (cancelled, timed out, out of
//!   attempts, or the queen lost track of it) uses local fallback by default
//!   or returns 125 in remote-only mode. A command that ran and exited nonzero
//!   on a drone is a real failure: its code is returned as is.
//! * The drone must be able to get the tree. A tree from a pushed commit is in
//!   its git mirror. Otherwise `--publish` uploads a `git bundle` of just the
//!   missing objects (everything since the merge base with `origin/main`) to the
//!   queen, which hands it to the drone; nothing is pushed to any remote. Without
//!   `--publish`, an unpublished tree uses local fallback by default or returns
//!   125 in remote-only mode.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::units::{Receipt, UnitState};

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
    /// Unit priority scope: `deploy`, `main` or `pr`.
    pub scope: String,
    /// How long to wait for a drone to pick the unit up before running locally.
    pub wait_secs: u64,
    pub publish: bool,
    /// Refuse local fallback and return 125 when infrastructure cannot finish the unit.
    pub remote_only: bool,
    /// Write a JSON execution result separately from command output.
    pub result_file: Option<PathBuf>,
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
    receipt: Option<Receipt>,
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

    async fn cancel_created(&self, id: &str, created: bool) {
        if created {
            let _ = self
                .http
                .post(self.url(&format!("/v1/units/{id}/cancel")))
                .bearer_auth(self.token.expose())
                .timeout(Duration::from_secs(2))
                .send()
                .await;
        }
    }
}

async fn wait_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending::<()>().await,
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

/// Machine-readable execution result, separate from streamed command output.
/// `requested_tree_sha` identifies the requested snapshot, not the mutable
/// working directory used by local fallback. A null host means unavailable.
#[derive(Debug, Serialize)]
pub struct RunResult {
    pub schema_version: u32,
    pub requested_tree_sha: Option<String>,
    pub argv: Vec<String>,
    /// `remote`, `local`, or `none` (no execution was observed).
    pub execution: String,
    /// `new`, `joined`, `reused`, or `fallback`.
    pub provenance: Option<String>,
    pub unit_id: Option<String>,
    pub host: Option<String>,
    pub exit_code: i32,
    pub infrastructure_error: Option<String>,
    pub receipt: Option<Receipt>,
}

#[derive(Deserialize)]
struct UnitStatus {
    id: String,
    repo: String,
    tree_sha: String,
    argv: Vec<String>,
    platform: String,
    toolchain: String,
    state: String,
    exit_code: Option<i32>,
    drone: Option<String>,
    timeout_secs: u64,
    receipt: Option<Receipt>,
}

async fn unit_status(q: &Remote<'_>, id: &str) -> Result<UnitStatus, String> {
    q.http
        .get(q.url(&format!("/v1/units/{id}")))
        .bearer_auth(q.token.expose())
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("cannot read unit metadata ({e})"))?
        .error_for_status()
        .map_err(|e| format!("cannot read unit metadata ({e})"))?
        .json()
        .await
        .map_err(|e| format!("unreadable unit metadata ({e})"))
}

fn record_status(
    result: &mut RunResult,
    unit: UnitStatus,
    exit: Option<i32>,
    opts: &RunOptions,
    repo: &str,
) -> Result<u64, String> {
    if result.unit_id.as_deref() != Some(unit.id.as_str())
        || result.requested_tree_sha.as_deref() != Some(unit.tree_sha.as_str())
        || result.argv != unit.argv
        || unit.repo != repo
        || unit.platform != opts.platform
        || unit.toolchain != opts.toolchain
        || exit.is_some_and(|code| {
            unit.state != UnitState::Done.as_str() || unit.exit_code != Some(code)
        })
    {
        return Err("unit metadata does not match the submitted command".into());
    }
    if let Some(receipt) = &unit.receipt
        && (receipt.unit_id != unit.id
            || receipt.tree_sha != unit.tree_sha
            || receipt.argv != unit.argv
            || receipt.repo != unit.repo
            || receipt.platform != unit.platform
            || receipt.toolchain != unit.toolchain
            || unit.drone.as_deref() != Some(receipt.drone.as_str())
            || unit.state != UnitState::Done.as_str()
            || unit.exit_code != Some(0)
            || receipt.exit_code != 0
            || !receipt.verifies())
    {
        return Err("unit receipt does not match the completed command".into());
    }
    if unit.state == UnitState::Running.as_str() || unit.state == UnitState::Done.as_str() {
        result.execution = "remote".into();
    }
    result.host = unit.drone;
    result.receipt = unit.receipt;
    Ok(unit.timeout_secs)
}

/// Run remotely when possible. `remote_only` prevents every local fallback.
/// Returns 125 for infrastructure errors and result-file failures.
pub async fn run(opts: &RunOptions, out: &mut dyn Write) -> i32 {
    // Open before execution so an invalid result destination cannot quietly
    // execute work without the requested result.
    let file = match opts
        .result_file
        .as_ref()
        .map(std::fs::File::create)
        .transpose()
    {
        Ok(file) => file,
        Err(e) => {
            eprintln!("borg run: cannot create result file: {e}");
            return 125;
        }
    };
    let mut result = RunResult {
        schema_version: 1,
        requested_tree_sha: None,
        argv: opts.argv.clone(),
        execution: "none".into(),
        provenance: None,
        unit_id: None,
        host: None,
        exit_code: 125,
        infrastructure_error: None,
        receipt: None,
    };
    result.exit_code = match remote(opts, out, &mut result).await {
        Ok(code) => code,
        Err(Fallback::Local(why)) => {
            result.infrastructure_error = Some(why.clone());
            if opts.remote_only {
                eprintln!("borg run: {why}; remote-only execution failed");
                125
            } else {
                eprintln!("borg run: {why}; running locally");
                result.execution = "local".into();
                result.provenance = Some("fallback".into());
                result.host = None;
                result.receipt = None;
                run_local(&opts.argv, &opts.dir)
            }
        }
    };
    if let Some(mut file) = file
        && let Err(e) = serde_json::to_writer_pretty(&mut file, &result)
            .map_err(std::io::Error::other)
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.flush())
    {
        eprintln!("borg run: cannot write result file: {e}");
        return 125;
    }
    result.exit_code
}

#[allow(
    clippy::too_many_lines,
    reason = "one linear submit, wait, stream, finish flow"
)]
async fn remote(
    opts: &RunOptions,
    out: &mut dyn Write,
    result: &mut RunResult,
) -> Result<i32, Fallback> {
    let local = |why: &str| Fallback::Local(why.to_owned());
    let (Some(base), Some(token)) = (&opts.queen, &opts.token) else {
        return Err(local("no queen configured"));
    };
    let tree = resolve_tree(&opts.dir, &opts.tree).map_err(Fallback::Local)?;
    result.requested_tree_sha = Some(tree.clone());
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
        "timeout_secs": opts.timeout_secs, "force": opts.force, "bundle": needs_bundle, "scope": opts.scope,
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
    result.unit_id = Some(sub.unit_id.clone());
    if sub.outcome == "receipt" {
        let receipt = sub
            .receipt
            .ok_or_else(|| local("queen returned no reusable receipt"))?;
        if receipt.unit_id != sub.unit_id
            || receipt.tree_sha != tree
            || receipt.argv != opts.argv
            || receipt.repo != repo
            || receipt.platform != opts.platform
            || receipt.toolchain != opts.toolchain
            || receipt.exit_code != 0
            || !receipt.verifies()
        {
            return Err(local(
                "queen returned a receipt that does not match the command",
            ));
        }
        eprintln!(
            "borg run: tree {} already passed on {} (unit {}); not running again",
            &tree[..tree.len().min(12)],
            receipt.drone,
            sub.unit_id
        );
        result.execution = "remote".into();
        result.provenance = Some("reused".into());
        result.host = Some(receipt.drone.clone());
        result.receipt = Some(receipt);
        return Ok(0);
    }
    let created = match sub.outcome.as_str() {
        "new" => true,
        "joined" => false,
        _ => return Err(local("queen returned an unknown submission outcome")),
    };
    result.provenance = Some(if created { "new" } else { "joined" }.into());
    if created && needs_bundle {
        let sent = upload_bundle(&q, &opts.dir, &tree, &sub.unit_id).await;
        if let Err(why) = sent {
            q.cancel_created(&sub.unit_id, created).await;
            return Err(Fallback::Local(why));
        }
    }
    eprintln!(
        "borg run: unit {} {} on the fleet",
        sub.unit_id,
        if created { "queued" } else { "joined" }
    );

    let timeout_secs = if opts.remote_only {
        // Use the admitted timeout, including the queen's configured default
        // and cap. The client also enforces it if the queen stops making progress.
        let metadata = unit_status(&q, &sub.unit_id)
            .await
            .and_then(|unit| record_status(result, unit, None, opts, &repo));
        match metadata {
            Ok(timeout) => Some(timeout),
            Err(why) => {
                q.cancel_created(&sub.unit_id, created).await;
                return Err(Fallback::Local(why));
            }
        }
    } else {
        None
    };
    let started = Instant::now();
    let queued_deadline = tokio::time::Instant::now() + Duration::from_secs(opts.wait_secs);
    let deadline = timeout_secs.map(|t| queued_deadline + Duration::from_secs(t));
    let mut offset: u64 = 0;
    let mut running = result.execution == "remote";
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
            _ = wait_deadline(deadline.map(|d| if running { d } else { d.min(queued_deadline) })) => {
                q.cancel_created(&sub.unit_id, created).await;
                return Err(local(if running {
                    "remote execution exceeded the command deadline"
                } else {
                    "no drone took the unit within the admission deadline"
                }));
            },
            _ = tokio::signal::ctrl_c() => {
                q.cancel_created(&sub.unit_id, created).await;
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
                let loss_limit = if opts.remote_only {
                    opts.wait_secs.min(60)
                } else {
                    60
                };
                if since.elapsed() >= Duration::from_secs(loss_limit) {
                    q.cancel_created(&sub.unit_id, created && opts.remote_only)
                        .await;
                    return Err(Fallback::Local(match other {
                        Ok(r) => format!("queen answered {} while reading logs", r.status()),
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
        if state == "running" || state == "done" {
            running = true;
            result.execution = "remote".into();
        }
        let bytes = tokio::select! {
            bytes = resp.bytes() => match bytes {
                Ok(bytes) => bytes,
                Err(e) => {
                    q.cancel_created(&sub.unit_id, created && opts.remote_only).await;
                    return Err(Fallback::Local(format!("log read failed ({e})")));
                }
            },
            _ = wait_deadline(deadline.map(|d| if running { d } else { d.min(queued_deadline) })) => {
                q.cancel_created(&sub.unit_id, created).await;
                return Err(local("remote log exceeded the execution deadline"));
            },
        };
        if !bytes.is_empty()
            && let Err(e) = out.write_all(&bytes).and_then(|()| out.flush())
        {
            let why = format!("cannot write remote log ({e})");
            if opts.remote_only {
                q.cancel_created(&sub.unit_id, created).await;
                return Err(Fallback::Local(why));
            }
            // A broken output consumer must never rerun a command that already
            // executed remotely. Preserve the default behavior: drain the log
            // and return the remote exit code even if displaying output fails.
            result.infrastructure_error = Some(why);
        }
        if let Some(n) = next {
            offset = n;
        }
        match state.as_str() {
            "done" => {
                if let Some(code) = exit {
                    if opts.result_file.is_some() {
                        match unit_status(&q, &sub.unit_id)
                            .await
                            .and_then(|unit| record_status(result, unit, Some(code), opts, &repo))
                        {
                            Ok(_) => {}
                            Err(why) => {
                                // The command finished. Metadata loss must never
                                // trigger a second execution through local fallback.
                                result.host = None;
                                result.receipt = None;
                                result.infrastructure_error = Some(why);
                            }
                        }
                    }
                    return Ok(code);
                }
                // The queen labels every chunk of a completed unit "done",
                // but sends the exit code only after its log is fully drained.
                // Keep polling from `next` until that final chunk arrives.
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
                q.cancel_created(&sub.unit_id, created).await;
                return Err(Fallback::Local(format!(
                    "no drone took it within {}s",
                    opts.wait_secs
                )));
            }
            _ => {}
        }
    }
}
