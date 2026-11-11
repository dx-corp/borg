//! Work units on the drone: the drone half of [`crate::units`].
//!
//! A unit reaches the drone in its heartbeat response (`units`). The drone
//! admits it like a lease (a slot, the local lease cap and the held set it
//! shares with leases, so the queen's accounting and the drone's agree), and
//! lists it in `active_units` from then until its result is delivered: the
//! first listing acknowledges it. Then, in the slot:
//!
//! 1. **Tree.** `tree_sha` comes from the host git mirror of the unit's
//!    repository, made on first use when the repository is not configured
//!    ([`GitMirrors::unit_mirror`](crate::drone::caches::git::GitMirrors::unit_mirror)).
//!    When the mirror lacks it and the client published a bundle
//!    (`bundle_bytes > 0`), the drone fetches `GET /v1/units/{id}/bundle` and
//!    unbundles it into the mirror; a stale mirror is refreshed once and both
//!    are tried again.
//! 2. **Checkout** at `<slot>/_work/<name>/<name>`, the path a CI job's
//!    checkout has in the same slot, as a repository borrowing the mirror's
//!    objects whose `HEAD` is a commit of the tree. Compile caches keyed by
//!    paths are therefore shared with CI jobs.
//! 3. **Run** `argv` there as the job user, in its own process group, with a
//!    lease's environment: the shared host caches (sccache, cargo, Bazel and
//!    the rest), the remap `RUSTFLAGS`, a private `HOME` in the slot, and
//!    `CARGO_TARGET_DIR` at the slot's retained target, the directory the
//!    repository's CI uses there (`$BORG_CACHE_ROOT/<name>-ci/slots/<key>/
//!    cargo-target/<ISO week>`, as `platform-ci.yml` computes it). The time
//!    limit is the unit's, capped by `max_job_secs`. The Rust toolchain is the
//!    one the tree's `rust-toolchain.toml` names (rustup reads it); the queen
//!    only places a unit on a drone that has it.
//! 4. **Log.** Output streams to `POST /v1/units/{id}/log?offset=N` about every
//!    second. The drone keeps the whole log (up to [`MAX_LOG_KEEP`]), so a
//!    `409` is answered by learning the queen's length and resending from it.
//! 5. **Result.** `POST /v1/units/{id}/result` with the exit code when the
//!    command ran to completion. When the drone could not get the tree, the
//!    unit timed out or a client cancelled it, the result carries a `failure`
//!    instead: the unit fails and the client falls back, because no exit code
//!    of the drone's making is the command's verdict. A drone that stops a
//!    unit because it is shutting down reports nothing; the queen re-queues it.
//!
//! Cleanup is the lease's: docker resources named for the unit id, the slot
//! directory, and the job user's credentials when nothing else runs.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::watch;

use super::{LeaseContext, RESULT_ATTEMPTS, cleanup_slot};
use crate::drone::caches::git::valid_repo;
use crate::drone::cleanup::docker_prefix;
use crate::drone::client::{LogAppend, QueenClient};
use crate::drone::exec::Exec;
use crate::drone::runner::{LEASE_HOME, Limits, seed_lease_home, spawn_retrying, supervise};
use crate::drone::slots::slot_dir;
use crate::units::UnitGrant;
use crate::units_api::UnitResult;

/// Log bytes a unit keeps in memory for resending; more output is dropped
/// with a note. Above the queen's default `max_log_bytes` (8 MiB).
pub const MAX_LOG_KEEP: usize = 16 << 20;
/// Largest log append.
const LOG_CHUNK: usize = 256 << 10;
/// How often output is sent while a unit runs.
const LOG_EVERY: Duration = Duration::from_secs(1);
/// Attempts to deliver the rest of the log once the command ended.
const FINAL_LOG_ATTEMPTS: u32 = 5;
/// After the command's process group is gone, how long its output pipes may
/// stay open (held by a process that left the group) before reading stops.
const OUTPUT_DRAIN: Duration = Duration::from_secs(5);

/// How a unit ended on this drone.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ended {
    /// The command ran to completion; its exit code is the verdict.
    Exited { exit_code: i32, duration_secs: u64 },
    /// The drone did not run it to completion; the unit fails with `why`.
    Failed { why: String, duration_secs: u64 },
    /// The drone is shutting down: report nothing, the queen re-queues it.
    Abandoned,
}

impl Ended {
    fn result(&self) -> Option<UnitResult> {
        match self {
            Self::Exited {
                exit_code,
                duration_secs,
            } => Some(UnitResult {
                exit_code: *exit_code,
                duration_secs: *duration_secs,
                failure: None,
            }),
            Self::Failed { why, duration_secs } => Some(UnitResult {
                exit_code: -1,
                duration_secs: *duration_secs,
                failure: Some(why.clone()),
            }),
            Self::Abandoned => None,
        }
    }
}

/// Run one granted unit, clean up unconditionally, report how it ended.
pub(super) async fn handle_unit(ctx: LeaseContext, grant: UnitGrant) {
    // Held for the whole unit: cache garbage collection waits for it.
    let gate = ctx.caches.begin_lease().await;
    let slot = ctx.slot.index();
    if let Some(r) = &ctx.reclaimer {
        r.note_slot_use(slot).await;
    }
    tracing::info!(
        "unit {} (attempt {}) -> slot {slot}: {} {}",
        grant.unit_id,
        grant.attempt,
        grant.repo,
        short(&grant.tree_sha)
    );
    let output = Arc::new(Mutex::new(Output::default()));
    let (stop_log, log_task) = spawn_log(
        Arc::clone(&ctx.client),
        grant.unit_id.clone(),
        Arc::clone(&output),
    );
    let ended = run(&ctx, &grant, &output).await;
    if let Ended::Failed { why, .. } = &ended {
        output_note(&output, &format!("[borg] unit failed on this drone: {why}"));
    }
    // Every byte reaches the queen before the result does, so a client that
    // sees the exit code has the whole log.
    let _ = stop_log.send(true);
    if let Err(e) = log_task.await {
        tracing::warn!("unit {} log task failed: {e}", grant.unit_id);
    }
    if let Some(r) = &ctx.reclaimer {
        r.note_slot_use(slot).await;
    }
    cleanup_slot(&ctx, &grant.unit_id).await;
    drop(gate);
    let caches = Arc::clone(&ctx.caches);
    tokio::spawn(async move { caches.maintain().await });

    match ended.result() {
        Some(result) => report(&ctx.client, &grant.unit_id, &result).await,
        None => tracing::warn!(
            "unit {} stopped by shutdown; the queen re-queues it",
            grant.unit_id
        ),
    }
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

/// Deliver the result, retrying like a lease result; stop early when the
/// unit is no longer this drone's.
async fn report(client: &QueenClient, id: &str, result: &UnitResult) {
    for attempt in 1..=RESULT_ATTEMPTS {
        match client.report_unit(id, result).await {
            Ok(true) => return,
            Ok(false) => {
                tracing::warn!("unit {id} is no longer this drone's; result dropped");
                return;
            }
            Err(e) => {
                tracing::warn!("reporting unit {id} failed (attempt {attempt}): {e}");
                tokio::time::sleep(Duration::from_secs((u64::from(attempt) * 5).min(60))).await;
            }
        }
    }
}

/// Resolves once `rx` reads true; never if its sender is gone.
async fn raised(mut rx: watch::Receiver<bool>) {
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

async fn run(ctx: &LeaseContext, grant: &UnitGrant, output: &Arc<Mutex<Output>>) -> Ended {
    let started = Instant::now();
    let failed = |why: String| Ended::Failed {
        why,
        duration_secs: started.elapsed().as_secs(),
    };
    let slot_dir = slot_dir(&ctx.cfg.work_root, ctx.slot.index());
    // Getting the tree can take minutes (a first mirror clone). Shutdown and
    // cancel are honoured meanwhile; `materialize` only awaits blocking git
    // commands, which finish on their own, so dropping it is safe, and the
    // slot cleanup that follows removes whatever they wrote.
    let checkout = tokio::select! {
        r = materialize(ctx, grant, &slot_dir, output) => match r {
            Ok(dir) => dir,
            Err(why) => return failed(why),
        },
        () = raised(ctx.abort.clone()) => return Ended::Abandoned,
        () = raised(ctx.preempt.clone()) => return failed("cancelled".to_owned()),
    };

    let env = unit_env(ctx, grant, &slot_dir).await;
    let Some((program, args)) = command_line(ctx, &grant.argv) else {
        return failed("empty argv".to_owned());
    };
    #[allow(
        clippy::disallowed_methods,
        reason = "runs the work unit's argv, which the queen accepted from an enrolled client, as the unprivileged job user (through setpriv) in the unit's own checkout; this is what a work unit is"
    )]
    let mut command = Command::new(&program);
    command
        .args(&args)
        .current_dir(&checkout)
        .envs(env.iter().map(|(k, v)| (k, v)))
        // Never hand the drone's own credential to job code.
        .env_remove("BORG_DRONE_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        // With a job user the drone cannot signal the job directly; its group
        // is killed through `job_exec` instead.
        .kill_on_drop(ctx.cfg.job_user.is_none());
    let mut child = match spawn_retrying(&mut command).await {
        Ok(c) => c,
        Err(e) => {
            let shown = grant.argv.first().map_or("", String::as_str);
            return failed(format!("could not start {shown}: {e}"));
        }
    };
    let readers: Vec<_> = [
        child
            .stdout
            .take()
            .map(|r| spawn_reader(r, Arc::clone(output))),
        child
            .stderr
            .take()
            .map(|r| spawn_reader(r, Arc::clone(output))),
    ]
    .into_iter()
    .flatten()
    .collect();
    let max_job = match grant.timeout_secs {
        0 => ctx.cfg.max_job_secs,
        t => t.min(ctx.cfg.max_job_secs),
    };
    let outcome = supervise(
        &mut child,
        ctx.job_exec.as_ref(),
        Limits {
            max_job: Duration::from_secs(max_job),
            preempt_grace: Duration::from_secs(ctx.cfg.preempt_grace_secs),
            proc_root: &ctx.proc_root,
        },
        ctx.abort.clone(),
        ctx.preempt.clone(),
    )
    .await;
    // The group is gone; a process that left it may still hold the pipes.
    let drained = tokio::time::timeout(OUTPUT_DRAIN, async {
        for r in &readers {
            while !r.is_finished() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    })
    .await;
    if drained.is_err() {
        output_note(
            output,
            "[borg] output still open after the command's process group ended; stopped reading",
        );
    }
    for r in readers {
        r.abort();
    }
    match outcome {
        Err(e) => failed(format!("supervising the command failed: {e}")),
        Ok(o) if o.aborted => Ended::Abandoned,
        Ok(o) if o.preempted => failed("cancelled".to_owned()),
        Ok(o) if o.timed_out => failed(format!("timed out after {max_job}s")),
        Ok(o) => Ended::Exited {
            exit_code: o.exit_code,
            duration_secs: started.elapsed().as_secs(),
        },
    }
}

/// `(program, args)`: through `setpriv` as the job user when one is
/// configured, else `argv` itself.
fn command_line(ctx: &LeaseContext, argv: &[String]) -> Option<(String, Vec<String>)> {
    let (first, rest) = argv.split_first()?;
    Some(match &ctx.cfg.job_user {
        Some(user) => {
            let mut args = user.setpriv_args();
            args.extend(argv.iter().cloned());
            (user.priv_bin.clone(), args)
        }
        None => (first.clone(), rest.to_vec()),
    })
}

/// The environment a unit runs with; see the module docs.
async fn unit_env(ctx: &LeaseContext, grant: &UnitGrant, slot_dir: &Path) -> Vec<(String, String)> {
    let path = |p: &Path| p.to_string_lossy().into_owned();
    let mut env: Vec<(String, String)> = ctx
        .cfg
        .runner_env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    env.extend(ctx.caches.prepare_lease(slot_dir).await);
    let prefix = docker_prefix(&grant.unit_id);
    env.extend([
        ("BORG_SLOT".to_owned(), ctx.slot.index().to_string()),
        ("BORG_UNIT_ID".to_owned(), grant.unit_id.clone()),
        ("BORG_CACHE_ROOT".to_owned(), path(&ctx.cfg.cache_root)),
        ("BORG_DOCKER_PREFIX".to_owned(), prefix.clone()),
        ("COMPOSE_PROJECT_NAME".to_owned(), prefix),
    ]);
    let home = slot_dir.join(LEASE_HOME);
    let (fs_ops, user_home, home_dst) = (
        Arc::clone(&ctx.fs_ops),
        ctx.cfg.job_user.as_ref().map(|u| u.home.clone()),
        home.clone(),
    );
    let seeded = tokio::task::spawn_blocking(move || match user_home {
        Some(user_home) => seed_lease_home(fs_ops.as_ref(), &user_home, &home_dst),
        None => fs_ops.mkdir_p(&home_dst),
    })
    .await;
    match seeded {
        Ok(Ok(())) => env.push(("HOME".to_owned(), path(&home))),
        Ok(Err(e)) => tracing::warn!("unit {}: private HOME: {e}", grant.unit_id),
        Err(e) => tracing::warn!("unit {}: private HOME task: {e}", grant.unit_id),
    }
    if let Some(user) = &ctx.cfg.job_user {
        env.push(("USER".to_owned(), user.name.clone()));
        env.push(("LOGNAME".to_owned(), user.name.clone()));
    }
    if ctx.cfg.caches.cargo.enabled
        && let Some(target) = retained_target(ctx, grant).await
    {
        env.push(("CARGO_TARGET_DIR".to_owned(), path(&target)));
    }
    env
}

/// The slot's retained cargo target, created if needed:
/// `<cache_root>/<name>-ci/slots/<CI slot key>/cargo-target/<ISO week>`, where
/// the repository's CI keeps the same slot's build output. `None` when it
/// cannot be created; cargo then builds inside the checkout.
async fn retained_target(ctx: &LeaseContext, grant: &UnitGrant) -> Option<PathBuf> {
    let name = grant.repo.split('/').nth(1)?;
    let date = time::OffsetDateTime::now_utc().date();
    let (year, week, _) = date.to_iso_week_date();
    let dir = ctx
        .cfg
        .cache_root
        .join(format!("{name}-ci"))
        .join("slots")
        .join(crate::drone::reclaim::cache_slot_key(
            &ctx.cfg.work_root,
            ctx.slot.index(),
        ))
        .join("cargo-target")
        .join(format!("{year}-W{week:02}"));
    let (fs_ops, made) = (Arc::clone(&ctx.fs_ops), dir.clone());
    match tokio::task::spawn_blocking(move || fs_ops.mkdir_p(&made)).await {
        Ok(Ok(())) => Some(dir),
        Ok(Err(e)) => {
            tracing::warn!("unit {}: retained cargo target: {e}", grant.unit_id);
            None
        }
        Err(e) => {
            tracing::warn!("unit {}: retained cargo target task: {e}", grant.unit_id);
            None
        }
    }
}

fn valid_object_id(sha: &str) -> bool {
    matches!(sha.len(), 40 | 64) && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Run git as the job user off the async runtime. `Err` carries a reason.
async fn git(
    exec: &Arc<dyn Exec>,
    bin: &str,
    args: Vec<String>,
    input: Option<Vec<u8>>,
) -> Result<String, String> {
    let (exec, bin) = (Arc::clone(exec), bin.to_owned());
    let shown = args.join(" ");
    let ran = tokio::task::spawn_blocking(move || {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match &input {
            Some(data) => exec.run_input(&bin, &args, data),
            None => exec.run(&bin, &args),
        }
    })
    .await
    .map_err(|e| format!("git task: {e}"))?;
    match ran {
        Ok(out) if out.success => Ok(out.stdout),
        Ok(_) => Err(format!("`git {shown}` failed")),
        Err(e) => Err(format!("git: {e}")),
    }
}

/// Get the tree into the mirror and check it out into the slot; see the
/// module docs. Returns the checkout directory.
async fn materialize(
    ctx: &LeaseContext,
    grant: &UnitGrant,
    slot_dir: &Path,
    output: &Arc<Mutex<Output>>,
) -> Result<PathBuf, String> {
    if !valid_repo(&grant.repo) {
        return Err(format!("unsafe repository name {:?}", grant.repo));
    }
    if !valid_object_id(&grant.tree_sha) {
        return Err("tree_sha is not an object id".to_owned());
    }
    let mirrors = ctx.caches.git();
    let bin = mirrors.git_bin().to_owned();
    let exec = &ctx.job_exec;
    let tree = format!("{}^{{tree}}", grant.tree_sha);
    let has_tree = |mirror: PathBuf| {
        let tree = tree.clone();
        let bin = bin.clone();
        async move {
            let m = mirror.to_string_lossy().into_owned();
            git(
                exec,
                &bin,
                vec!["-C".into(), m, "cat-file".into(), "-e".into(), tree],
                None,
            )
            .await
            .is_ok()
        }
    };
    let mut bundle: Option<Vec<u8>> = None;
    let mut last_err = String::new();
    let mut found = None;
    // A just-pushed tree or a bundle's newer prerequisite needs a fresh
    // mirror: the second pass forces a refresh.
    for force in [false, true] {
        let Some(mirror) = mirrors.unit_mirror(&grant.repo, force).await else {
            return Err(format!("unsafe repository name {:?}", grant.repo));
        };
        if has_tree(mirror.clone()).await {
            found = Some(mirror);
            break;
        }
        if grant.bundle_bytes == 0 {
            continue;
        }
        if bundle.is_none() {
            output_note(
                output,
                "[borg] tree not on the remote; fetching the client's bundle",
            );
            match ctx
                .client
                .unit_bundle(&grant.unit_id, grant.bundle_bytes)
                .await
            {
                Ok(data) => bundle = Some(data),
                Err(e) => {
                    last_err = format!("bundle download: {e}");
                    continue;
                }
            }
        }
        let m = mirror.to_string_lossy().into_owned();
        // The bundle goes in on standard input: no temporary file another
        // local user could read.
        let unbundled = git(
            exec,
            &bin,
            vec![
                "-C".into(),
                m,
                "bundle".into(),
                "unbundle".into(),
                "/dev/stdin".into(),
            ],
            bundle.clone(),
        )
        .await;
        if let Err(e) = unbundled {
            last_err = format!("unbundle: {e}");
            continue;
        }
        if has_tree(mirror.clone()).await {
            found = Some(mirror);
            break;
        }
        last_err = "the bundle does not hold the tree".to_owned();
    }
    let Some(mirror) = found else {
        let detail = if last_err.is_empty() {
            String::new()
        } else {
            format!("; {last_err}")
        };
        return Err(format!(
            "could not get tree {} of {} into the git mirror{detail}",
            short(&grant.tree_sha),
            grant.repo
        ));
    };
    checkout(ctx, &bin, &mirror, grant, slot_dir).await
}

/// Check the tree out at `<slot>/_work/<name>/<name>`: a fresh repository
/// that borrows the mirror's objects, with `HEAD` a commit of the tree.
async fn checkout(
    ctx: &LeaseContext,
    bin: &str,
    mirror: &Path,
    grant: &UnitGrant,
    slot_dir: &Path,
) -> Result<PathBuf, String> {
    let name = grant
        .repo
        .split('/')
        .nth(1)
        .ok_or_else(|| format!("unsafe repository name {:?}", grant.repo))?;
    let dir = slot_dir.join("_work").join(name).join(name);
    let (fs_ops, slot, made) = (Arc::clone(&ctx.fs_ops), slot_dir.to_owned(), dir.clone());
    // A reused slot must hold nothing of an earlier job.
    tokio::task::spawn_blocking(move || {
        if slot.symlink_metadata().is_ok() {
            fs_ops.remove_all(&slot)?;
        }
        fs_ops.mkdir_p(&made)
    })
    .await
    .map_err(|e| format!("prepare slot: {e}"))?
    .map_err(|e| format!("prepare slot: {e}"))?;
    let d = dir.to_string_lossy().into_owned();
    let exec = &ctx.job_exec;
    let run = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        git(exec, bin, args, None)
    };
    run(&["init", "--quiet", &d])
        .await
        .map_err(|e| format!("checkout: {e}"))?;
    let alternates = dir.join(".git/objects/info/alternates");
    let objects = format!("{}\n", mirror.join("objects").display());
    let fs_ops = Arc::clone(&ctx.fs_ops);
    tokio::task::spawn_blocking(move || fs_ops.write_file(&alternates, &objects))
        .await
        .map_err(|e| format!("checkout: {e}"))?
        .map_err(|e| format!("checkout: alternates: {e}"))?;
    let message = format!("borg unit {}", grant.unit_id);
    let commit = run(&[
        "-C",
        &d,
        "-c",
        "user.name=borg",
        "-c",
        "user.email=borg@localhost",
        "commit-tree",
        &grant.tree_sha,
        "-m",
        &message,
    ])
    .await
    .map_err(|e| format!("checkout: {e}"))?;
    if !valid_object_id(commit.trim()) {
        return Err("checkout: commit-tree printed no object id".to_owned());
    }
    run(&["-C", &d, "update-ref", "HEAD", commit.trim()])
        .await
        .map_err(|e| format!("checkout: {e}"))?;
    run(&["-C", &d, "read-tree", "--reset", "-u", "HEAD"])
        .await
        .map_err(|e| format!("checkout: {e}"))?;
    Ok(dir)
}

/// Everything the unit printed, as it will be sent.
#[derive(Debug, Default)]
struct Output {
    bytes: Vec<u8>,
    /// Output past [`MAX_LOG_KEEP`] was dropped.
    truncated: bool,
}

fn output_append(output: &Mutex<Output>, data: &[u8]) {
    let Ok(mut o) = output.lock() else { return };
    let room = MAX_LOG_KEEP.saturating_sub(o.bytes.len());
    if data.len() > room {
        if !o.truncated {
            o.truncated = true;
            let note = b"\n[borg] output truncated: the drone keeps at most 16 MiB\n";
            let keep = room.saturating_sub(note.len());
            o.bytes.extend_from_slice(&data[..keep]);
            o.bytes.extend_from_slice(&note[..note.len().min(room)]);
        }
        return;
    }
    o.bytes.extend_from_slice(data);
}

/// A line of the drone's own, on a line of its own.
fn output_note(output: &Mutex<Output>, line: &str) {
    let lead = output
        .lock()
        .is_ok_and(|o| o.bytes.last().is_some_and(|b| *b != b'\n'));
    let text = format!("{}{line}\n", if lead { "\n" } else { "" });
    output_append(output, text.as_bytes());
}

fn spawn_reader(
    mut from: impl AsyncRead + Unpin + Send + 'static,
    output: Arc<Mutex<Output>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf = vec![0u8; 16 << 10];
        loop {
            match from.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => output_append(&output, &buf[..n]),
            }
        }
    })
}

/// Sends a unit's output to the queen in order, without gaps.
struct LogShipper {
    client: Arc<QueenClient>,
    id: String,
    /// Bytes the queen holds.
    sent: u64,
    /// The queen takes no more (its log cap, or the unit is not ours).
    closed: bool,
}

impl LogShipper {
    /// Send everything not yet stored. False on a transport error: try again
    /// later.
    async fn ship(&mut self, output: &Mutex<Output>) -> bool {
        while !self.closed {
            let chunk = {
                let Ok(o) = output.lock() else { return false };
                let start = usize::try_from(self.sent).unwrap_or(usize::MAX);
                if start >= o.bytes.len() {
                    return true;
                }
                o.bytes[start..o.bytes.len().min(start.saturating_add(LOG_CHUNK))].to_vec()
            };
            match self
                .client
                .append_unit_log(&self.id, self.sent, chunk)
                .await
            {
                Ok(LogAppend::Stored(n)) if n > self.sent => self.sent = n,
                // Nothing taken: the queen's log is full.
                Ok(LogAppend::Stored(_)) => {
                    tracing::info!("unit {}: the queen's log cap is reached", self.id);
                    self.closed = true;
                }
                // The queen holds less than we thought: an empty append at
                // offset 0 tells its length, and we resend from there.
                Ok(LogAppend::Gap) => {
                    match self.client.append_unit_log(&self.id, 0, Vec::new()).await {
                        Ok(LogAppend::Stored(n)) => self.sent = n,
                        Ok(LogAppend::NotOurs) => self.closed = true,
                        Ok(LogAppend::Gap) => return false,
                        Err(e) => {
                            tracing::warn!("unit {} log resync: {e}", self.id);
                            return false;
                        }
                    }
                }
                Ok(LogAppend::NotOurs) => self.closed = true,
                Err(e) => {
                    tracing::warn!("unit {} log append: {e}", self.id);
                    return false;
                }
            }
        }
        true
    }
}

/// Ship output every [`LOG_EVERY`] until `stop`, then deliver the rest.
fn spawn_log(
    client: Arc<QueenClient>,
    id: String,
    output: Arc<Mutex<Output>>,
) -> (watch::Sender<bool>, tokio::task::JoinHandle<()>) {
    let (stop, mut stopped) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut shipper = LogShipper {
            client,
            id,
            sent: 0,
            closed: false,
        };
        let mut every = tokio::time::interval(LOG_EVERY);
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Both arms are cancel-safe: `Interval::tick` and
        // `watch::Receiver::changed` keep no partially consumed state.
        loop {
            tokio::select! {
                _ = every.tick() => {
                    shipper.ship(&output).await;
                }
                changed = stopped.changed() => {
                    if changed.is_err() || *stopped.borrow() {
                        break;
                    }
                }
            }
        }
        for attempt in 1..=FINAL_LOG_ATTEMPTS {
            if shipper.ship(&output).await {
                return;
            }
            tokio::time::sleep(Duration::from_secs(u64::from(attempt))).await;
        }
        tracing::warn!("unit {}: could not deliver the end of its log", shipper.id);
    });
    (stop, task)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_keeps_order_and_truncates_once_with_a_note() {
        let o = Mutex::new(Output::default());
        output_append(&o, b"hello");
        output_note(&o, "[borg] note");
        assert_eq!(o.lock().unwrap().bytes, b"hello\n[borg] note\n");

        let o = Mutex::new(Output::default());
        output_append(&o, &vec![b'a'; MAX_LOG_KEEP - 10]);
        output_append(&o, &[b'b'; 100]);
        output_append(&o, b"more");
        let o = o.lock().unwrap();
        assert!(o.truncated);
        assert!(o.bytes.len() <= MAX_LOG_KEEP);
        assert!(!o.bytes.ends_with(b"more"), "nothing after the cap");
    }

    #[test]
    fn results_say_whether_the_command_gave_the_verdict() {
        let exited = Ended::Exited {
            exit_code: 3,
            duration_secs: 7,
        }
        .result()
        .unwrap();
        assert_eq!((exited.exit_code, exited.failure), (3, None));
        let failed = Ended::Failed {
            why: "timed out after 5s".into(),
            duration_secs: 5,
        }
        .result()
        .unwrap();
        assert_eq!(failed.failure.as_deref(), Some("timed out after 5s"));
        assert!(
            Ended::Abandoned.result().is_none(),
            "shutdown reports nothing"
        );
    }

    #[test]
    fn object_ids_are_hex_of_sha1_or_sha256_length() {
        assert!(valid_object_id(&"a".repeat(40)));
        assert!(valid_object_id(&"0".repeat(64)));
        assert!(!valid_object_id(&"g".repeat(40)));
        assert!(!valid_object_id("-c"));
        assert!(!valid_object_id(&"a".repeat(41)));
    }
}
