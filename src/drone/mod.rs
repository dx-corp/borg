//! The drone: runs on every machine that joins the fleet.
//!
//! It probes the host, heartbeats to the queen with its capacity and owner
//! caps, and runs every lease the queen grants as an ephemeral runner in a
//! private directory. Cleanup after each job is unconditional. On shutdown it
//! stops accepting leases and lets in-flight jobs finish, up to a timeout.

pub mod cleanup;
pub mod client;
pub mod config;
pub mod exec;
pub mod install;
pub mod pins;
pub mod runner;

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::task::JoinSet;

use crate::api::{HeartbeatRequest, LeaseGrant, ResultRequest};
use crate::probe::{self, Host};
use cleanup::{Scope, cleanup_docker, remove_lease_dir, sweep_work_root, valid_lease_id};
use client::{ClientError, QueenClient};
use config::DroneConfig;
use exec::Exec;
use runner::{RunSpec, run_lease};

/// Attempts to deliver a lease result before giving up (the lease then
/// expires on the queen's side).
const RESULT_ATTEMPTS: u32 = 5;

type Held = Arc<Mutex<HashSet<String>>>;

/// Removes a lease id from the held set when its task ends, even by panic.
struct HeldGuard {
    held: Held,
    id: String,
}

impl Drop for HeldGuard {
    fn drop(&mut self) {
        if let Ok(mut held) = self.held.lock() {
            held.remove(&self.id);
        }
    }
}

pub struct Drone {
    cfg: Arc<DroneConfig>,
    client: Arc<QueenClient>,
    host: Arc<dyn Host + Send + Sync>,
    exec: Arc<dyn Exec>,
    held: Held,
    abort: tokio::sync::watch::Sender<bool>,
    proc_root: Arc<Path>,
}

impl Drone {
    pub fn new(
        cfg: DroneConfig,
        client: QueenClient,
        host: Arc<dyn Host + Send + Sync>,
        exec: Arc<dyn Exec>,
    ) -> Self {
        let (abort, _) = tokio::sync::watch::channel(false);
        Self {
            cfg: Arc::new(cfg),
            client: Arc::new(client),
            host,
            exec,
            held: Held::default(),
            abort,
            proc_root: Arc::from(Path::new("/proc")),
        }
    }

    fn held_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .held
            .lock()
            .map(|h| h.iter().cloned().collect())
            .unwrap_or_default();
        ids.sort();
        ids
    }

    /// Remove leftovers of a previous run: no lease is held yet, so every
    /// lease directory and borg-labelled docker resource is stale.
    pub async fn startup_sweep(&self) {
        let (work_root, exec, docker) = (
            self.cfg.work_root.clone(),
            Arc::clone(&self.exec),
            self.cfg.docker_bin.clone(),
        );
        let swept = tokio::task::spawn_blocking(move || {
            let dirs = sweep_work_root(&work_root);
            let docker = cleanup_docker(exec.as_ref(), &docker, Scope::AnyLease);
            (dirs, docker)
        })
        .await;
        match swept {
            Ok((dirs, docker)) => {
                tracing::info!(
                    "startup sweep: {:?} lease dirs, {} docker resources",
                    dirs.map(|d| d.len()),
                    docker.removed.len()
                );
            }
            Err(e) => tracing::warn!("startup sweep failed: {e}"),
        }
    }

    /// One heartbeat: report capacity, start any newly granted leases.
    /// Returns how many leases were started.
    pub async fn heartbeat_once(
        &self,
        accepting: bool,
        tasks: &mut JoinSet<()>,
    ) -> Result<usize, ClientError> {
        let host = Arc::clone(&self.host);
        let capacity = tokio::task::spawn_blocking(move || probe::probe(host.as_ref()))
            .await
            .map_err(|e| ClientError::Status {
                path: format!("probe: {e}"),
                status: 0,
            })?;
        let req = HeartbeatRequest {
            capacity,
            caps: self.cfg.host_caps(),
            accepting,
            active_leases: self.held_ids(),
        };
        let resp = self.client.heartbeat(&req).await?;
        let mut started = 0;
        for grant in resp.leases {
            if !accepting || !self.admit(&grant.lease_id) {
                continue;
            }
            let ctx = LeaseContext {
                cfg: Arc::clone(&self.cfg),
                client: Arc::clone(&self.client),
                exec: Arc::clone(&self.exec),
                abort: self.abort.subscribe(),
                proc_root: Arc::clone(&self.proc_root),
                _held: HeldGuard {
                    held: Arc::clone(&self.held),
                    id: grant.lease_id.clone(),
                },
            };
            tasks.spawn(handle_lease(ctx, grant));
            started += 1;
        }
        Ok(started)
    }

    /// Whether a granted lease may start: well-formed, not already running,
    /// and within the local lease cap. Claims the id if so.
    fn admit(&self, lease_id: &str) -> bool {
        if !valid_lease_id(lease_id) {
            tracing::warn!("ignoring lease with unsafe id");
            return false;
        }
        let Ok(mut held) = self.held.lock() else {
            return false;
        };
        if held.contains(lease_id) {
            return false;
        }
        if self
            .cfg
            .max_leases
            .is_some_and(|cap| held.len() >= usize::try_from(cap).unwrap_or(usize::MAX))
        {
            tracing::warn!("at local lease cap; leaving lease {lease_id} unstarted");
            return false;
        }
        held.insert(lease_id.to_owned());
        true
    }

    /// Heartbeat until `shutdown` resolves, then drain.
    pub async fn run(self, shutdown: impl std::future::Future<Output = ()>) {
        self.startup_sweep().await;
        let mut tasks = JoinSet::new();
        let mut every = tokio::time::interval(Duration::from_secs(self.cfg.heartbeat_secs));
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tokio::pin!(shutdown);
        // `interval.tick`, `shutdown` and `JoinSet::join_next` are cancel-safe.
        loop {
            tokio::select! {
                _ = every.tick() => {
                    if let Err(e) = self.heartbeat_once(true, &mut tasks).await {
                        tracing::warn!("heartbeat failed: {e}");
                    }
                }
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
                () = &mut shutdown => break,
            }
        }

        tracing::info!(
            "draining: no new leases; waiting up to {}s for {} job(s)",
            self.cfg.drain_timeout_secs,
            tasks.len()
        );
        let deadline = tokio::time::sleep(Duration::from_secs(self.cfg.drain_timeout_secs));
        tokio::pin!(deadline);
        while !tasks.is_empty() {
            tokio::select! {
                _ = every.tick() => {
                    if let Err(e) = self.heartbeat_once(false, &mut tasks).await {
                        tracing::warn!("heartbeat failed: {e}");
                    }
                }
                _ = tasks.join_next() => {}
                () = &mut deadline => {
                    tracing::warn!("drain timeout: stopping remaining jobs");
                    let _ = self.abort.send(true);
                    while tasks.join_next().await.is_some() {}
                }
            }
        }
    }
}

struct LeaseContext {
    cfg: Arc<DroneConfig>,
    client: Arc<QueenClient>,
    exec: Arc<dyn Exec>,
    abort: tokio::sync::watch::Receiver<bool>,
    proc_root: Arc<Path>,
    _held: HeldGuard,
}

/// Run one lease, clean up unconditionally, report the result.
async fn handle_lease(ctx: LeaseContext, grant: LeaseGrant) {
    let spec = RunSpec {
        lease_id: &grant.lease_id,
        jit_config: &grant.jit_config,
        runner_dir: &ctx.cfg.runner_dir,
        work_root: &ctx.cfg.work_root,
        cache_root: &ctx.cfg.cache_root,
        max_job: Duration::from_secs(ctx.cfg.max_job_secs),
        proc_root: &ctx.proc_root,
    };
    let outcome = run_lease(&spec, ctx.exec.as_ref(), ctx.abort.clone()).await;

    // Runs whether the job passed, failed, timed out, was aborted or never started.
    let (work_root, id, docker, exec) = (
        ctx.cfg.work_root.clone(),
        grant.lease_id.clone(),
        ctx.cfg.docker_bin.clone(),
        Arc::clone(&ctx.exec),
    );
    let cleaned = tokio::task::spawn_blocking(move || {
        let docker = cleanup_docker(exec.as_ref(), &docker, Scope::Lease(&id));
        (remove_lease_dir(&work_root, &id), docker)
    })
    .await;
    match cleaned {
        Ok((dir, docker)) => {
            if let Err(e) = dir {
                tracing::warn!("lease {} work dir cleanup failed: {e}", grant.lease_id);
            }
            for err in docker.errors {
                tracing::warn!("lease {} docker cleanup: {err}", grant.lease_id);
            }
        }
        Err(e) => tracing::warn!("lease {} cleanup task failed: {e}", grant.lease_id),
    }

    let result = match outcome {
        Ok(o) => ResultRequest {
            exit_code: o.exit_code,
            duration_secs: o.duration_secs,
            peak_rss_mib: o.peak_rss_mib,
        },
        Err(e) => {
            tracing::warn!("lease {} could not run: {e}", grant.lease_id);
            ResultRequest {
                exit_code: -1,
                duration_secs: 0,
                peak_rss_mib: None,
            }
        }
    };
    for attempt in 1..=RESULT_ATTEMPTS {
        match ctx.client.report_result(&grant.lease_id, &result).await {
            Ok(()) => return,
            Err(e) => {
                tracing::warn!(
                    "reporting lease {} failed (attempt {attempt}): {e}",
                    grant.lease_id
                );
                tokio::time::sleep(Duration::from_secs(u64::from(attempt))).await;
            }
        }
    }
}
