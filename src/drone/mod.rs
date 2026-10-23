//! The drone: runs on every machine that joins the fleet.
//!
//! It probes the host, heartbeats to the queen with its capacity and owner
//! caps, and runs every lease the queen grants as an ephemeral runner in a
//! private directory. Cleanup after each job is unconditional. On shutdown it
//! stops accepting leases and lets in-flight jobs finish, up to a timeout.

pub mod caches;
pub mod capabilities;
pub mod cleanup;
pub mod client;
pub mod config;
pub mod exec;
pub mod fsops;
pub mod install;
pub mod jobuser;
pub mod pins;
pub mod reclaim;
pub mod runner;
pub mod selfupdate;
pub mod slots;
pub mod standby;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::task::JoinSet;

use crate::api::{HeartbeatRequest, LeaseGrant, ResultRequest};
use crate::probe::{self, Host};
use caches::Caches;
use cleanup::{Scope, cleanup_docker, remove_lease_dir, sweep_work_root, valid_lease_id};
use client::{ClientError, QueenClient};
use config::DroneConfig;
use exec::Exec;
use fsops::{ExecFs, FsOps, LocalFs};
use jobuser::JobExec;
use runner::{RunSpec, run_lease};
use selfupdate::{Handled, Plan, SelfUpdate};
use slots::{SlotGuard, Slots, slot_dir, slot_name};
use standby::{Standbys, should_fail_open};

/// Attempts to deliver a lease result before giving up (the lease then
/// expires on the queen's side). The waits grow to a minute, so a queen that
/// is down for a few minutes does not lose results.
const RESULT_ATTEMPTS: u32 = 20;

type Held = Arc<Mutex<HashSet<String>>>;

/// Per running lease, the switch the queen's `cancel_leases` flips.
type Preempts = Arc<Mutex<HashMap<String, tokio::sync::watch::Sender<bool>>>>;

/// Removes a lease id from the held set when its task ends, even by panic.
struct HeldGuard {
    held: Held,
    preempts: Preempts,
    id: String,
}

impl Drop for HeldGuard {
    fn drop(&mut self) {
        if let Ok(mut held) = self.held.lock() {
            held.remove(&self.id);
        }
        if let Ok(mut preempts) = self.preempts.lock() {
            preempts.remove(&self.id);
        }
    }
}

pub struct Drone {
    cfg: Arc<DroneConfig>,
    client: Arc<QueenClient>,
    host: Arc<dyn Host + Send + Sync>,
    exec: Arc<dyn Exec>,
    /// Runs commands as the job user (or as the drone when none is configured).
    job_exec: Arc<dyn Exec>,
    /// File operations on job-owned directories.
    fs_ops: Arc<dyn FsOps>,
    held: Held,
    /// Standby deploy runners held unstarted.
    standbys: Standbys,
    preempts: Preempts,
    abort: tokio::sync::watch::Sender<bool>,
    proc_root: Arc<Path>,
    caches: Arc<Caches>,
    slots: Arc<Slots>,
    /// Self-update hand-off; present when `state_dir` is configured.
    selfupdate: Option<SelfUpdate>,
    /// The queen wants another version: stop taking leases until idle.
    update_wanted: AtomicBool,
    /// The last tools probe, reused for a minute.
    tools_cache: probe::ToolsCache,
    /// Last Queen floor, retained while the Queen is unreachable.
    disk_floor: AtomicU64,
    capability_cache: capabilities::CapabilityCache,
    /// Busy-cores rate of the host, fed by heartbeats and lease samples.
    cpu_meter: Arc<crate::hostdiag::CpuMeter>,
    /// Build-box reclaim; `None` when off or unsupported here.
    reclaimer: Option<Arc<reclaim::Reclaimer>>,
}

impl Drone {
    pub fn new(
        cfg: DroneConfig,
        client: QueenClient,
        host: Arc<dyn Host + Send + Sync>,
        exec: Arc<dyn Exec>,
    ) -> Self {
        let (abort, _) = tokio::sync::watch::channel(false);
        let slot_count = cfg.slot_count();
        let cfg = Arc::new(cfg);
        let (job_exec, fs_ops): (Arc<dyn Exec>, Arc<dyn FsOps>) = match &cfg.job_user {
            Some(user) => {
                let job_exec: Arc<dyn Exec> =
                    Arc::new(JobExec::new(Arc::clone(&exec), user.clone()));
                let fs_ops: Arc<dyn FsOps> = Arc::new(ExecFs::new(Arc::clone(&job_exec)));
                (job_exec, fs_ops)
            }
            None => {
                tracing::warn!(
                    "no job_user configured: jobs run as the drone's own user and can read its token"
                );
                (Arc::clone(&exec), Arc::new(LocalFs))
            }
        };
        let caches = Arc::new(Caches::new(
            Arc::clone(&cfg),
            Arc::clone(&job_exec),
            Arc::clone(&fs_ops),
            Arc::clone(&host),
        ));
        let selfupdate = cfg.state_dir.clone().map(SelfUpdate::new);
        let reclaimer = reclaim::Reclaimer::new(&cfg, Arc::clone(&exec));
        let standbys = Standbys::load(cfg.state_dir.clone());
        Self {
            cfg,
            selfupdate,
            update_wanted: AtomicBool::new(false),
            tools_cache: probe::ToolsCache::default(),
            disk_floor: AtomicU64::new(crate::envcontract::DEFAULT_MIN_DISK_FREE_BYTES),
            capability_cache: capabilities::CapabilityCache::default(),
            cpu_meter: Arc::default(),
            reclaimer,
            client: Arc::new(client),
            host,
            exec,
            job_exec,
            fs_ops,
            caches,
            slots: Slots::new(slot_count),
            held: Held::default(),
            standbys,
            preempts: Preempts::default(),
            abort,
            proc_root: Arc::from(Path::new("/proc")),
        }
    }

    /// `(slot, lease id)` for every lease currently holding a slot.
    pub fn slot_assignments(&self) -> Vec<(usize, String)> {
        self.slots.assignments()
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
        let (work_root, exec, docker, fs_ops) = (
            self.cfg.work_root.clone(),
            Arc::clone(&self.exec),
            self.cfg.docker_bin.clone(),
            Arc::clone(&self.fs_ops),
        );
        let swept = tokio::task::spawn_blocking(move || {
            let dirs = sweep_work_root(fs_ops.as_ref(), &work_root);
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
        // Between jobs the drone obeys `desired_version`: once the queen wants
        // another version it stops accepting and updates when idle.
        let accepting = accepting && !self.update_wanted.load(Ordering::Relaxed);
        let host = Arc::clone(&self.host);
        let cfg = Arc::clone(&self.cfg);
        let (capacity, cache_free) = tokio::task::spawn_blocking(move || {
            let capacity = probe::probe_at(host.as_ref(), &cfg.work_root);
            let cache_free = probe::free_disk_mib_at(host.as_ref(), &cfg.cache_root)
                .map(|mib| mib.saturating_mul(1 << 20));
            (capacity, cache_free)
        })
        .await
        .map_err(|e| ClientError::Status {
            path: format!("probe: {e}"),
            status: 0,
        })?;
        let host = Arc::clone(&self.host);
        let meter = Arc::clone(&self.cpu_meter);
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let mut env_facts = tokio::task::spawn_blocking(move || {
            let now_ms =
                i64::try_from(time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
                    .unwrap_or(i64::MAX);
            let busy = crate::hostdiag::observe_cpu(host.as_ref(), &meter, now_ms);
            let mut facts = probe::env_facts(host.as_ref(), now);
            facts.cpu_busy_pm = busy.zip(host.available_parallelism()).map(|(b, cpus)| {
                (b / f64::from(cpus.max(1)) * 1000.0)
                    .round()
                    .clamp(0.0, 1000.0) as u16
            });
            facts
        })
        .await
        .map_err(|e| ClientError::Status {
            path: format!("probe: {e}"),
            status: 0,
        })?;
        env_facts.cache_dir_free_bytes = cache_free;
        let accepting = accepting
            && !crate::envcontract::disk_admission(
                self.disk_floor.load(Ordering::Relaxed),
                capacity.free_disk_mib,
                cache_free,
            )
            .blocked();
        // The tools probe never fails the heartbeat: no map means not measured.
        let (exec, cache) = (Arc::clone(&self.job_exec), self.tools_cache.clone());
        env_facts.tools = tokio::task::spawn_blocking(move || cache.get(exec.as_ref(), now))
            .await
            .ok()
            .flatten();
        // Slow execution proofs must not make an otherwise healthy drone
        // miss its heartbeat and cause the queen to requeue active work.
        // Refreshes share the existing task set, and drain starts none.
        if accepting {
            self.capability_cache.refresh_if_due(
                Arc::clone(&self.job_exec),
                Arc::clone(&self.cfg),
                now,
                tasks,
            );
        }
        env_facts.capabilities = self.capability_cache.snapshot();
        let req = HeartbeatRequest {
            capacity,
            caps: self.cfg.host_caps(),
            accepting,
            active_leases: self.held_ids(),
            cache_stats: self.caches.stats(),
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            extra_labels: self.cfg.extra_labels.clone(),
            trust: self.cfg.trust,
            held_standby: self.standbys.ids(),
            bids: Vec::new(),
            reclaim: self.reclaimer.as_ref().and_then(|r| r.heartbeat_stats()),
            update_report: self
                .selfupdate
                .as_ref()
                .and_then(SelfUpdate::pending_report),
            active_units: Vec::new(),
            env_facts: Some(env_facts),
        };
        let resp = self.client.heartbeat(&req).await?;
        self.disk_floor
            .store(resp.min_free_disk_bytes, Ordering::Relaxed);
        let accepting = accepting
            && !crate::envcontract::disk_admission(
                resp.min_free_disk_bytes,
                req.capacity.free_disk_mib,
                cache_free,
            )
            .blocked();
        let defer = self.after_heartbeat(&req, resp.desired_version.as_deref());
        if let Some(credential) = resp.mirror_credential {
            self.caches.set_mirror_credential(credential);
        }
        if let Ok(preempts) = self.preempts.lock() {
            for id in &resp.cancel_leases {
                if let Some(tx) = preempts.get(id) {
                    let _ = tx.send(true);
                }
            }
        }
        for id in &resp.drop_standby {
            self.standbys.remove(id);
        }
        for grant in resp.standby {
            self.standbys.insert(grant);
        }
        let mut started = 0;
        for grant in resp.leases {
            if accepting && !defer && self.start_lease(grant, tasks) {
                started += 1;
            }
        }
        if accepting && !defer {
            started += self.start_standbys(Some(&resp.start_standby), tasks);
        }
        Ok(started)
    }

    /// Start held standby runners: the ones in `only`, or all of them. A
    /// standby that cannot start now (no slot) stays held.
    fn start_standbys(&self, only: Option<&[String]>, tasks: &mut JoinSet<()>) -> usize {
        let ids = match only {
            Some(ids) => ids.to_vec(),
            None => self.standbys.ids(),
        };
        let mut started = 0;
        for id in ids {
            let Some(grant) = self.standbys.get(&id) else {
                continue;
            };
            if self.start_lease(grant, tasks) {
                // The lease now lives in `active_leases`; the JIT config is
                // single use, so a restart must not find it again.
                self.standbys.remove(&id);
                started += 1;
            }
        }
        started
    }

    /// Start every held standby because the queen has been unreachable.
    async fn fail_open(&self, tasks: &mut JoinSet<()>) {
        if self.standbys.ids().is_empty() {
            return;
        }
        let host = Arc::clone(&self.host);
        let cfg = Arc::clone(&self.cfg);
        let Ok((work_free, cache_free)) = tokio::task::spawn_blocking(move || {
            probe::job_disk_space(host.as_ref(), &cfg.work_root, &cfg.cache_root)
        })
        .await
        else {
            return;
        };
        if crate::envcontract::disk_admission(
            self.disk_floor.load(Ordering::Relaxed),
            work_free,
            cache_free,
        )
        .blocked()
        {
            tracing::warn!("disk below admission floor: leaving standby runners unstarted");
            return;
        }
        let started = self.start_standbys(None, tasks);
        if started > 0 {
            tracing::warn!("queen unreachable: started {started} standby deploy runner(s)");
        }
    }

    /// Claim a slot for `grant` and run it as a lease. Returns whether it
    /// started.
    fn start_lease(&self, grant: LeaseGrant, tasks: &mut JoinSet<()>) -> bool {
        if !self.admit(&grant.lease_id) {
            return false;
        }
        let held = HeldGuard {
            held: Arc::clone(&self.held),
            preempts: Arc::clone(&self.preempts),
            id: grant.lease_id.clone(),
        };
        let (preempt_tx, preempt) = tokio::sync::watch::channel(false);
        if let Ok(mut preempts) = self.preempts.lock() {
            preempts.insert(grant.lease_id.clone(), preempt_tx);
        }
        let Some(slot) = self.slots.acquire(&grant.lease_id) else {
            tracing::warn!(
                "no free slot for lease {}; leaving it unstarted",
                grant.lease_id
            );
            return false;
        };
        let ctx = LeaseContext {
            cfg: Arc::clone(&self.cfg),
            host: Arc::clone(&self.host),
            cpu_meter: Arc::clone(&self.cpu_meter),
            client: Arc::clone(&self.client),
            exec: Arc::clone(&self.exec),
            job_exec: Arc::clone(&self.job_exec),
            fs_ops: Arc::clone(&self.fs_ops),
            abort: self.abort.subscribe(),
            preempt,
            proc_root: Arc::clone(&self.proc_root),
            caches: Arc::clone(&self.caches),
            slot,
            held,
        };
        tasks.spawn(handle_lease(ctx, grant));
        true
    }

    /// What a successful heartbeat means for self-update: the queen has the
    /// updater's report, the running binary is demonstrably working, and the
    /// queen may want another version (older ones included). Returns true when
    /// leases granted in this response must be left unstarted because an update
    /// is being handed to the updater.
    fn after_heartbeat(&self, req: &HeartbeatRequest, desired: Option<&str>) -> bool {
        let Some(su) = &self.selfupdate else {
            return false;
        };
        let own = env!("CARGO_PKG_VERSION");
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        if let Err(e) = su.record_health(own, now) {
            tracing::warn!("write health record: {e}");
        }
        if req.update_report.is_some() {
            su.acknowledge_report();
        }
        let held = self.held_ids().len();
        let desired = match selfupdate::plan(desired, own, held) {
            Plan::Stay => {
                self.update_wanted.store(false, Ordering::Relaxed);
                return false;
            }
            Plan::FinishJobsFirst => {
                // No new jobs meanwhile.
                self.update_wanted.store(true, Ordering::Relaxed);
                return false;
            }
            Plan::UpdateNow => desired.unwrap_or(own),
        };
        match su.handle(desired, own) {
            Ok(Handled::Requested) => {
                tracing::info!("update to {desired} requested");
                self.update_wanted.store(true, Ordering::Relaxed);
                true
            }
            Ok(Handled::InFlight) => {
                self.update_wanted.store(true, Ordering::Relaxed);
                true
            }
            Ok(Handled::Blocked) => {
                tracing::warn!("not retrying the failed update to {desired}");
                self.update_wanted.store(false, Ordering::Relaxed);
                false
            }
            Ok(Handled::UpToDate) => false,
            Err(e) => {
                tracing::warn!("request update to {desired}: {e}");
                self.update_wanted.store(false, Ordering::Relaxed);
                false
            }
        }
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
        if held.len() >= self.cfg.slot_count() {
            tracing::warn!(
                "all {} slots are busy; leaving lease {lease_id} unstarted",
                self.cfg.slot_count()
            );
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
        let cache_background = self.caches.spawn_background();
        let reclaim_background = self.reclaimer.as_ref().map(reclaim::Reclaimer::spawn);
        let mut every = tokio::time::interval(Duration::from_secs(self.cfg.heartbeat_secs));
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tokio::pin!(shutdown);
        // `interval.tick`, `shutdown` and `JoinSet::join_next` are cancel-safe.
        // While the queen is unreachable, running leases carry on, no new lease
        // arrives, and heartbeats are retried with backoff.
        let mut failures = 0u32;
        let mut retry_at = tokio::time::Instant::now();
        let mut last_ok_at = time::OffsetDateTime::now_utc().unix_timestamp();
        loop {
            tokio::select! {
                _ = every.tick() => {
                    let now = time::OffsetDateTime::now_utc().unix_timestamp();
                    if should_fail_open(last_ok_at, now, self.cfg.fail_open_after_secs) {
                        self.fail_open(&mut tasks).await;
                    }
                    if tokio::time::Instant::now() < retry_at {
                        continue;
                    }
                    match self.heartbeat_once(true, &mut tasks).await {
                        Ok(_) => {
                            failures = 0;
                            last_ok_at = time::OffsetDateTime::now_utc().unix_timestamp();
                        }
                        Err(e) => {
                            failures = failures.saturating_add(1);
                            let wait = selfupdate::backoff(self.cfg.heartbeat_secs, failures);
                            retry_at = tokio::time::Instant::now() + Duration::from_secs(wait);
                            tracing::warn!("heartbeat failed ({failures} in a row), retrying in {wait}s: {e}");
                        }
                    }
                }
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
                () = &mut shutdown => break,
            }
        }

        tracing::info!(
            "draining: no new leases; waiting up to {}s for {} task(s)",
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
        cache_background.abort();
        if let Some(r) = reclaim_background {
            r.abort();
        }
    }
}

struct LeaseContext {
    cfg: Arc<DroneConfig>,
    host: Arc<dyn Host + Send + Sync>,
    cpu_meter: Arc<crate::hostdiag::CpuMeter>,
    client: Arc<QueenClient>,
    exec: Arc<dyn Exec>,
    job_exec: Arc<dyn Exec>,
    fs_ops: Arc<dyn FsOps>,
    abort: tokio::sync::watch::Receiver<bool>,
    preempt: tokio::sync::watch::Receiver<bool>,
    proc_root: Arc<Path>,
    caches: Arc<Caches>,
    /// Dropped before `held`, so the slot is free whenever the id is.
    slot: SlotGuard,
    held: HeldGuard,
}

/// Sample the host for this lease's diagnostics. Never fails the lease.
async fn sample_host(ctx: &LeaseContext) -> Option<crate::hostdiag::HostSample> {
    let (host, cfg, meter) = (
        Arc::clone(&ctx.host),
        Arc::clone(&ctx.cfg),
        Arc::clone(&ctx.cpu_meter),
    );
    let active = ctx
        .held
        .held
        .lock()
        .map(|h| u32::try_from(h.len()).unwrap_or(u32::MAX))
        .unwrap_or(0);
    tokio::task::spawn_blocking(move || {
        let now_ms =
            i64::try_from(time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
                .unwrap_or(i64::MAX);
        crate::hostdiag::sample(
            host.as_ref(),
            &cfg.work_root,
            &cfg.cache_root,
            active,
            &meter,
            now_ms,
        )
    })
    .await
    .ok()
}

/// Run one lease, clean up unconditionally, report the result.
async fn handle_lease(ctx: LeaseContext, grant: LeaseGrant) {
    // Held for the whole lease: cache garbage collection waits for it.
    let lease_guard = ctx.caches.begin_lease().await;
    // Refresh stale git mirrors and learn the shared cache environment. Cache
    // trouble never blocks a job; it just runs without the speedup.
    let slot = ctx.slot.index();
    let lease_dir = slot_dir(&ctx.cfg.work_root, slot);
    let mut extra_env = ctx.cfg.runner_environment();
    extra_env.extend(ctx.caches.prepare_lease(&lease_dir).await);
    extra_env.push(("BORG_SLOT".to_owned(), slot.to_string()));
    let spec = RunSpec {
        lease_id: &grant.lease_id,
        jit_config: &grant.jit_config,
        runner_dir: &ctx.cfg.runner_dir,
        lease_dir: &lease_dir,
        cache_root: &ctx.cfg.cache_root,
        extra_env: &extra_env,
        max_job: Duration::from_secs(ctx.cfg.max_job_secs),
        preempt_grace: Duration::from_secs(ctx.cfg.preempt_grace_secs),
        job_user: ctx.cfg.job_user.as_ref(),
        proc_root: &ctx.proc_root,
    };
    let host_start = sample_host(&ctx).await;
    let outcome = run_lease(
        &spec,
        ctx.job_exec.as_ref(),
        Arc::clone(&ctx.fs_ops),
        ctx.abort.clone(),
        ctx.preempt.clone(),
    )
    .await;
    let host_end = sample_host(&ctx).await;
    let host = Some(crate::hostdiag::LeaseHostReport {
        start: host_start,
        end: host_end,
    });

    // Runs whether the job passed, failed, timed out, was aborted or never started.
    let (work_root, id, slot_name, docker, exec, fs_ops) = (
        ctx.cfg.work_root.clone(),
        grant.lease_id.clone(),
        slot_name(slot),
        ctx.cfg.docker_bin.clone(),
        Arc::clone(&ctx.exec),
        Arc::clone(&ctx.fs_ops),
    );
    let cleaned = tokio::task::spawn_blocking(move || {
        let docker = cleanup_docker(exec.as_ref(), &docker, Scope::Lease(&id));
        (
            remove_lease_dir(fs_ops.as_ref(), &work_root, &slot_name),
            docker,
        )
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

    // Between leases: let the caches re-measure and, if nothing else is
    // running, evict. Skipped by `maintain` itself while another lease holds
    // the gate.
    drop(lease_guard);
    let caches = Arc::clone(&ctx.caches);
    tokio::spawn(async move { caches.maintain().await });

    let result = match outcome {
        Ok(o) => ResultRequest {
            exit_code: o.exit_code,
            duration_secs: o.duration_secs,
            peak_rss_mib: o.peak_rss_mib,
            telemetry: None,
            preempted: o.preempted,
            host,
        },
        Err(e) => {
            tracing::warn!("lease {} could not run: {e}", grant.lease_id);
            ResultRequest {
                exit_code: -1,
                duration_secs: 0,
                peak_rss_mib: None,
                telemetry: None,
                preempted: false,
                host,
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
                tokio::time::sleep(Duration::from_secs((u64::from(attempt) * 5).min(60))).await;
            }
        }
    }
}

#[cfg(test)]
mod disk_tests {
    use super::*;
    use crate::classes::{Class, Scope as JobScope};
    use crate::drone::exec::Output;
    use crate::secret::Secret;

    struct DiskHost;
    impl Host for DiskHost {
        fn read_to_string(&self, _: &Path) -> Option<String> {
            None
        }
        fn exists(&self, _: &Path) -> bool {
            false
        }
        fn run(&self, program: &str, args: &[&str]) -> Option<String> {
            if program != "df" {
                return None;
            }
            let available_kib = if args.last() == Some(&"/work") {
                20 * 1024 * 1024
            } else {
                100 * 1024 * 1024
            };
            Some(format!(
                "fs blocks used avail cap mount\ndisk 999999999 0 {available_kib} 0% /"
            ))
        }
        fn os(&self) -> &'static str {
            "linux"
        }
        fn arch(&self) -> &'static str {
            "x86_64"
        }
        fn available_parallelism(&self) -> Option<u32> {
            Some(1)
        }
    }
    struct NoExec;
    impl Exec for NoExec {
        fn run(&self, _: &str, _: &[&str]) -> std::io::Result<Output> {
            panic!("disk-blocked standby must not run commands")
        }
    }

    #[tokio::test]
    async fn fail_open_retains_standby_and_active_lease_below_learned_queen_floor() {
        let cfg = DroneConfig::from_json(r#"{"queen_url":"http://127.0.0.1:1","allow_insecure":true,"runner_dir":"/runner","work_root":"/work","cache_root":"/cache"}"#).unwrap();
        let client = QueenClient::new(&cfg, Secret::new("test".to_owned())).unwrap();
        let drone = Drone::new(cfg, client, Arc::new(DiskHost), Arc::new(NoExec));
        drone.disk_floor.store(32 << 30, Ordering::Relaxed);
        drone.held.lock().unwrap().insert("active".into());
        drone.standbys.insert(LeaseGrant {
            lease_id: "standby".into(),
            class: Class::Light,
            scope: JobScope::Deploy,
            labels: vec![],
            runner_name: "standby".into(),
            jit_config: Secret::new("jit".to_owned()),
            expires_at: i64::MAX,
        });
        let mut tasks = JoinSet::new();
        drone.fail_open(&mut tasks).await;
        assert!(tasks.is_empty());
        assert_eq!(drone.standbys.ids(), vec!["standby"]);
        assert_eq!(drone.held_ids(), vec!["active"]);
        assert!(drone.slot_assignments().is_empty());
        assert!(!*drone.abort.subscribe().borrow());
    }

    #[test]
    fn older_queen_response_keeps_default_floor_and_new_response_carries_configured_floor() {
        let mut response = serde_json::json!({"leases": []});
        let old: crate::api::HeartbeatResponse = serde_json::from_value(response.clone()).unwrap();
        assert_eq!(old.min_free_disk_bytes, 10 << 30);
        response["min_free_disk_bytes"] = serde_json::json!(32u64 << 30);
        let new: crate::api::HeartbeatResponse = serde_json::from_value(response).unwrap();
        assert_eq!(new.min_free_disk_bytes, 32 << 30);
    }
}
