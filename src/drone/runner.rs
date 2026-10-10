//! Run one lease as an ephemeral GitHub Actions runner in a private directory.
//!
//! The pinned runner install is cloned into `<work_root>/<lease_id>` (hard
//! links where the filesystem allows, copies otherwise), `run.sh` is started in
//! its own process group with the JIT config in the environment (never argv),
//! and the group is killed on timeout or abort. This module never cleans up;
//! the caller does, unconditionally.
//!
//! Unix only: borg drones run on Linux (including WSL) and macOS.

use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::process::Command;

use crate::drone::cleanup::{docker_prefix, valid_lease_id};
use crate::drone::exec::Exec;
use crate::secret::Secret;

const RSS_SAMPLE_EVERY: Duration = Duration::from_secs(2);

pub struct RunSpec<'a> {
    pub lease_id: &'a str,
    pub jit_config: &'a Secret,
    pub runner_dir: &'a Path,
    pub work_root: &'a Path,
    pub cache_root: &'a Path,
    pub max_job: Duration,
    /// `/proc` (a different root is used by tests).
    pub proc_root: &'a Path,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    pub exit_code: i32,
    pub duration_secs: u64,
    pub peak_rss_mib: Option<u64>,
    pub timed_out: bool,
    pub aborted: bool,
}

/// Exit code reported when a job was killed (128 + SIGKILL).
pub const KILLED_EXIT_CODE: i32 = 137;

/// Recreate `src` under `dst`: directories are created, files are hard-linked
/// (falling back to a copy across filesystems or when links are refused),
/// symlinks are recreated.
pub fn clone_tree(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        let kind = entry.file_type()?;
        if kind.is_dir() {
            clone_tree(&from, &to)?;
        } else if kind.is_symlink() {
            symlink(fs::read_link(&from)?, &to)?;
        } else if fs::hard_link(&from, &to).is_err() {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Sum of resident memory (KiB) of every process in process group `pgid`.
pub fn group_rss_kib(proc_root: &Path, pgid: u32) -> Option<u64> {
    let mut total = 0;
    let mut seen = false;
    for entry in fs::read_dir(proc_root).ok()?.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // Fields after the parenthesised command: state ppid pgrp ...
        let Some(after) = stat.rsplit_once(')').map(|(_, rest)| rest) else {
            continue;
        };
        if after
            .split_whitespace()
            .nth(2)
            .and_then(|f| f.parse::<u32>().ok())
            != Some(pgid)
        {
            continue;
        }
        let Ok(status) = fs::read_to_string(entry.path().join("status")) else {
            continue;
        };
        let rss = status
            .lines()
            .find_map(|l| l.strip_prefix("VmRSS:"))
            .and_then(|v| v.split_whitespace().next())
            .and_then(|v| v.parse::<u64>().ok());
        if let Some(kib) = rss {
            total += kib;
            seen = true;
        }
    }
    seen.then_some(total)
}

/// `spawn`, retrying briefly on ETXTBSY: the script was hard-linked from an
/// install another thread may still hold open for writing.
async fn spawn_retrying(command: &mut Command) -> io::Result<tokio::process::Child> {
    const ETXTBSY: i32 = 26;
    let mut attempts = 0;
    loop {
        match command.spawn() {
            Err(e) if e.raw_os_error() == Some(ETXTBSY) && attempts < 10 => {
                attempts += 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            other => return other,
        }
    }
}

fn kill_group(exec: &dyn Exec, pgid: u32) {
    // Negative pid addresses the whole process group.
    let target = format!("-{pgid}");
    let _ = exec.run("kill", &["-KILL", "--", &target]);
}

/// Run the lease to completion, timeout or abort.
pub async fn run_lease(
    spec: &RunSpec<'_>,
    exec: &dyn Exec,
    mut abort: tokio::sync::watch::Receiver<bool>,
) -> io::Result<RunOutcome> {
    if !valid_lease_id(spec.lease_id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsafe lease id",
        ));
    }
    let lease_dir: PathBuf = spec.work_root.join(spec.lease_id);
    let (src, dst) = (spec.runner_dir.to_owned(), lease_dir.clone());
    tokio::task::spawn_blocking(move || clone_tree(&src, &dst))
        .await
        .map_err(io::Error::other)??;

    let prefix = docker_prefix(spec.lease_id);
    #[allow(
        clippy::disallowed_methods,
        reason = "starts the pinned actions-runner run.sh from the drone-owned per-lease directory; its only argument is the queen-issued JIT config"
    )]
    let mut command = Command::new(lease_dir.join("run.sh"));
    command
        // The runner reads `ACTIONS_RUNNER_INPUT_<ARG>` as a fallback for each
        // command-line argument (Runner.Listener `CommandSettings`, v2.337.0),
        // masks `jitconfig` as a secret and removes the variable from its own
        // environment. Passing it here keeps it out of argv, which any local
        // user can read from `ps` or `/proc/<pid>/cmdline`.
        .env("ACTIONS_RUNNER_INPUT_JITCONFIG", spec.jit_config.expose())
        .current_dir(&lease_dir)
        .env("BORG_LEASE_ID", spec.lease_id)
        .env("BORG_DOCKER_PREFIX", &prefix)
        .env("COMPOSE_PROJECT_NAME", &prefix)
        .env("BORG_CACHE_ROOT", spec.cache_root)
        // The shared install is hard-linked into every lease; never let a
        // runner rewrite it in place.
        .env("DISABLE_RUNNER_UPDATE", "1")
        .stdin(Stdio::null())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = spawn_retrying(&mut command).await?;
    let pgid = child.id();

    let started = Instant::now();
    let deadline = tokio::time::sleep(spec.max_job);
    tokio::pin!(deadline);
    let mut sample = tokio::time::interval(RSS_SAMPLE_EVERY);
    let (mut peak_kib, mut timed_out, mut aborted) = (0u64, false, false);

    // Every arm is cancel-safe: `Child::wait`, `Sleep`, `Interval::tick` and
    // `watch::Receiver::changed` keep no partially consumed state.
    let status = loop {
        tokio::select! {
            status = child.wait() => break status?,
            () = &mut deadline => {
                timed_out = true;
                if let Some(pgid) = pgid { kill_group(exec, pgid); }
                break child.wait().await?;
            }
            changed = abort.changed() => {
                if changed.is_ok() && *abort.borrow() {
                    aborted = true;
                    if let Some(pgid) = pgid { kill_group(exec, pgid); }
                    break child.wait().await?;
                }
            }
            _ = sample.tick() => {
                if let Some(pgid) = pgid {
                    peak_kib = peak_kib.max(group_rss_kib(spec.proc_root, pgid).unwrap_or(0));
                }
            }
        }
    };
    // Reap anything the runner left in its group.
    if let Some(pgid) = pgid {
        kill_group(exec, pgid);
    }
    Ok(RunOutcome {
        exit_code: if timed_out || aborted {
            KILLED_EXIT_CODE
        } else {
            status.code().unwrap_or(KILLED_EXIT_CODE)
        },
        duration_secs: started.elapsed().as_secs(),
        peak_rss_mib: (peak_kib > 0).then_some(peak_kib / 1024),
        timed_out,
        aborted,
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::drone::exec::SystemExec;

    fn runner_with_script(dir: &Path, body: &str) -> PathBuf {
        let runner = dir.join("runner");
        fs::create_dir_all(runner.join("bin")).unwrap();
        fs::write(runner.join("bin/marker"), "pinned").unwrap();
        let script = runner.join("run.sh");
        fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        runner
    }

    async fn run(dir: &Path, body: &str, max_job: Duration) -> (RunOutcome, PathBuf) {
        let runner = runner_with_script(dir, body);
        let work = dir.join("work");
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let secret = Secret::new("jit-secret-value");
        let spec = RunSpec {
            lease_id: "lease1",
            jit_config: &secret,
            runner_dir: &runner,
            work_root: &work,
            cache_root: &dir.join("cache"),
            max_job,
            proc_root: Path::new("/proc"),
        };
        let out = run_lease(&spec, &SystemExec, rx).await.unwrap();
        (out, work.join("lease1"))
    }

    #[tokio::test]
    async fn runs_run_sh_in_a_private_clone_with_the_jit_config_and_lease_env() {
        let dir = tempfile::tempdir().unwrap();
        let (out, lease_dir) = run(
            dir.path(),
            "echo \"$@\" > args.txt; printf '%s' \"$ACTIONS_RUNNER_INPUT_JITCONFIG\" > jit.txt; echo \"$BORG_LEASE_ID $BORG_DOCKER_PREFIX $COMPOSE_PROJECT_NAME $DISABLE_RUNNER_UPDATE\" > env.txt; pwd > cwd.txt; tr '\\0' ' ' < /proc/$$/cmdline > cmdline.txt 2>/dev/null || true",
            Duration::from_secs(30),
        )
        .await;
        assert_eq!(
            (out.exit_code, out.timed_out, out.aborted),
            (0, false, false)
        );
        // The JIT config travels in the environment and never in argv.
        assert_eq!(
            fs::read_to_string(lease_dir.join("jit.txt")).unwrap(),
            "jit-secret-value"
        );
        assert_eq!(
            fs::read_to_string(lease_dir.join("args.txt"))
                .unwrap()
                .trim(),
            "",
            "run.sh gets no arguments"
        );
        let cmdline = fs::read_to_string(lease_dir.join("cmdline.txt")).unwrap_or_default();
        assert!(
            !cmdline.contains("jit-secret-value"),
            "secret visible in the process command line: {cmdline}"
        );
        assert_eq!(
            fs::read_to_string(lease_dir.join("env.txt"))
                .unwrap()
                .trim(),
            "lease1 borg-lease1 borg-lease1 1"
        );
        assert!(
            fs::read_to_string(lease_dir.join("cwd.txt"))
                .unwrap()
                .trim()
                .ends_with("work/lease1")
        );
        assert_eq!(
            fs::read_to_string(lease_dir.join("bin/marker")).unwrap(),
            "pinned",
            "install was cloned"
        );
        assert!(
            !dir.path().join("runner/args.txt").exists(),
            "the shared install is not written to"
        );
    }

    #[tokio::test]
    async fn reports_the_runner_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let (out, _) = run(dir.path(), "exit 7", Duration::from_secs(30)).await;
        assert_eq!(out.exit_code, 7);
    }

    #[tokio::test]
    async fn kills_the_whole_process_group_on_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let (out, lease_dir) = run(
            dir.path(),
            "sleep 300 &\necho $! > child.pid\nwait",
            Duration::from_millis(500),
        )
        .await;
        assert!(out.timed_out);
        assert_eq!(out.exit_code, KILLED_EXIT_CODE);
        let pid = fs::read_to_string(lease_dir.join("child.pid"))
            .unwrap()
            .trim()
            .to_owned();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let alive = fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(')')
                .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
        });
        assert!(
            !alive || cfg!(not(target_os = "linux")),
            "background child {pid} survived"
        );
    }

    #[tokio::test]
    async fn abort_signal_stops_a_running_job() {
        let dir = tempfile::tempdir().unwrap();
        let runner = runner_with_script(dir.path(), "sleep 300");
        let work = dir.path().join("work");
        let (tx, rx) = tokio::sync::watch::channel(false);
        let secret = Secret::new("x");
        let cache = dir.path().join("cache");
        let spec = RunSpec {
            lease_id: "lease2",
            jit_config: &secret,
            runner_dir: &runner,
            work_root: &work,
            cache_root: &cache,
            max_job: Duration::from_secs(60),
            proc_root: Path::new("/proc"),
        };
        let abort = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let _ = tx.send(true);
            tx
        });
        let out = run_lease(&spec, &SystemExec, rx).await.unwrap();
        let _tx = abort.await.unwrap();
        assert!(out.aborted && !out.timed_out);
    }

    #[tokio::test]
    async fn unsafe_lease_ids_never_touch_the_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let runner = runner_with_script(dir.path(), "true");
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let secret = Secret::new("x");
        let spec = RunSpec {
            lease_id: "../escape",
            jit_config: &secret,
            runner_dir: &runner,
            work_root: &dir.path().join("work"),
            cache_root: dir.path(),
            max_job: Duration::from_secs(1),
            proc_root: Path::new("/proc"),
        };
        assert!(run_lease(&spec, &SystemExec, rx).await.is_err());
        assert!(!dir.path().join("work").exists());
    }

    #[test]
    fn clone_hard_links_files_and_recreates_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(src.join("a/b")).unwrap();
        fs::write(src.join("a/b/f"), "data").unwrap();
        symlink("a/b/f", src.join("link")).unwrap();
        let dst = dir.path().join("dst");
        clone_tree(&src, &dst).unwrap();
        assert_eq!(fs::read_to_string(dst.join("a/b/f")).unwrap(), "data");
        assert_eq!(fs::read_link(dst.join("link")).unwrap(), Path::new("a/b/f"));
        // Deleting the clone leaves the source intact.
        fs::remove_dir_all(&dst).unwrap();
        assert_eq!(fs::read_to_string(src.join("a/b/f")).unwrap(), "data");
    }

    #[test]
    fn group_rss_sums_members_of_the_group_only() {
        let dir = tempfile::tempdir().unwrap();
        let add = |pid: &str, comm: &str, pgrp: u32, rss: &str| {
            let p = dir.path().join(pid);
            fs::create_dir_all(&p).unwrap();
            fs::write(
                p.join("stat"),
                format!("{pid} ({comm}) S 1 {pgrp} {pgrp} 0 -1\n"),
            )
            .unwrap();
            fs::write(
                p.join("status"),
                format!("Name:\t{comm}\nVmRSS:\t{rss} kB\n"),
            )
            .unwrap();
        };
        add("100", "run.sh", 100, "1000");
        add("101", "weird) name", 100, "3000");
        add("200", "other", 200, "9999");
        fs::create_dir_all(dir.path().join("self")).unwrap();
        assert_eq!(group_rss_kib(dir.path(), 100), Some(4000));
        assert_eq!(group_rss_kib(dir.path(), 999), None);
    }
}
