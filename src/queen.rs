//! The queen: one reconciliation pass ([`Queen::tick`]) turns the GitHub
//! queue plus drone heartbeats into minted leases.
//!
//! A tick:
//! 1. lists the organization's runners and reconciles every active lease with
//!    what GitHub reports (busy, gone, never came online);
//! 2. deletes stale `<prefix>-*` registrations that no lease owns;
//! 3. reads queued jobs and derives demand and queue ages per label;
//! 4. runs the pure allocator and mints a JIT runner for each grant.
//!
//! Any GitHub failure before minting aborts the tick without minting: acting
//! on a partial view of the queue would over- or under-grant.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::RwLock;

use rand::RngCore;
use rand::rngs::OsRng;
use serde::Serialize;
use time::OffsetDateTime;

use crate::allocator::{
    ActiveLease, AllocatorInput, Demand, DroneState, LeaseState, QueueAge, Reservation, Unmet,
    allocate, max_slots,
};
use crate::classes::{Class, Scope};
use crate::config::QueenConfig;
use crate::github::{GithubClient, GithubError, QueuedJob, Runner};
use crate::probe::Capacity;
use crate::store::{LeaseRow, LeaseStatus, Store, StoreError};

#[derive(Debug, thiserror::Error)]
pub enum QueenError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Github(#[from] GithubError),
}

/// What the last tick saw. Served by `/status` and `/metrics`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Snapshot {
    pub last_tick_at: Option<i64>,
    pub last_error: Option<String>,
    pub queue: Vec<QueueEntry>,
    pub reserved_for: Vec<Reservation>,
    pub unmet: Vec<Unmet>,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueueEntry {
    pub label: String,
    pub class: Class,
    pub scope: Scope,
    pub queued: u32,
    pub oldest_age_secs: u64,
}

pub struct Queen {
    pub(crate) cfg: QueenConfig,
    pub(crate) store: Store,
    github: GithubClient,
    snapshot: RwLock<Snapshot>,
    tick_lock: tokio::sync::Mutex<()>,
}

/// The full label set a JIT runner is registered with: the labels GitHub
/// assigns to every self-hosted runner, then the grant's class labels. They
/// are sent explicitly because the API documents only "custom" labels.
pub fn jit_labels(os: &str, arch: &str, class_labels: &[String]) -> Vec<String> {
    let mut labels = vec!["self-hosted".to_owned()];
    match os {
        "linux" => labels.push("Linux".to_owned()),
        "macos" => labels.push("macOS".to_owned()),
        "windows" => labels.push("Windows".to_owned()),
        _ => {}
    }
    match arch {
        "x86_64" | "x86" => labels.push("X64".to_owned()),
        "aarch64" | "arm64" => labels.push("ARM64".to_owned()),
        "arm" => labels.push("ARM".to_owned()),
        _ => {}
    }
    labels.extend(class_labels.iter().cloned());
    labels
}

fn new_lease_id() -> String {
    let mut b = [0u8; 6];
    OsRng.fill_bytes(&mut b);
    hex::encode(b)
}

fn age_secs(now: OffsetDateTime, then: OffsetDateTime) -> u64 {
    u64::try_from((now - then).whole_seconds()).unwrap_or(0)
}

impl Queen {
    pub fn new(cfg: QueenConfig, store: Store, github: GithubClient) -> Self {
        Self {
            cfg,
            store,
            github,
            snapshot: RwLock::new(Snapshot::default()),
            tick_lock: tokio::sync::Mutex::new(()),
        }
    }

    pub fn config(&self) -> &QueenConfig {
        &self.cfg
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.read().map(|s| s.clone()).unwrap_or_default()
    }

    fn update_snapshot(&self, f: impl FnOnce(&mut Snapshot)) {
        if let Ok(mut s) = self.snapshot.write() {
            f(&mut s);
        }
    }

    pub fn record_tick_error(&self, now: OffsetDateTime, err: &QueenError) {
        self.update_snapshot(|s| {
            s.last_tick_at = Some(now.unix_timestamp());
            s.last_error = Some(err.to_string());
        });
    }

    fn runner_prefix(&self) -> String {
        format!("{}-", self.cfg.runner_prefix)
    }

    /// One reconciliation pass.
    pub async fn tick(&self, now: OffsetDateTime) -> Result<(), QueenError> {
        let _serial = self.tick_lock.lock().await;
        let now_ts = now.unix_timestamp();

        let runners = self.github.list_runners().await?;
        let prefix = self.runner_prefix();
        let ours: HashMap<&str, &Runner> = runners
            .iter()
            .filter(|r| r.name.starts_with(&prefix))
            .map(|r| (r.name.as_str(), r))
            .collect();

        let active = self.reconcile(&ours, now_ts).await?;
        self.reap(&ours, &active).await;

        let mut jobs = Vec::new();
        for repo in &self.cfg.repos {
            jobs.extend(self.github.queued_jobs(repo).await?);
        }
        let (queue, demand, queue_ages) = self.summarize(&jobs, now);

        let drone_rows = self.store.drones().await?;
        let mut capacities: HashMap<String, Capacity> = HashMap::new();
        let mut drones = Vec::new();
        for d in &drone_rows {
            let fresh = d
                .last_heartbeat
                .is_some_and(|t| now_ts - t <= self.cfg.drone_stale_secs);
            if let (true, Some(cap)) = (fresh, &d.capacity) {
                let mut state = DroneState::from_capacity(d.name.clone(), cap, &d.caps);
                state.accepting = d.schedulable();
                drones.push(state);
                capacities.insert(d.name.clone(), cap.clone());
            }
        }
        let leases = active
            .iter()
            .map(|l| ActiveLease {
                drone_id: l.drone.clone(),
                class: l.class,
                scope: l.scope,
                state: if l.state == LeaseStatus::Running {
                    LeaseState::Running
                } else {
                    LeaseState::Pending
                },
            })
            .collect();
        let allocation = allocate(&AllocatorInput {
            classes: self.cfg.classes.clone(),
            demand,
            drones,
            leases,
            warm_floors: self.cfg.warm_floors.clone(),
            queue_ages,
            starvation_after_secs: self.cfg.starvation_after_secs,
        });

        let mut mint_error = None;
        for grant in &allocation.grants {
            let Some(cap) = capacities.get(&grant.drone_id) else {
                continue;
            };
            let lease_id = new_lease_id();
            let runner_name = format!("{prefix}{}-{lease_id}", grant.drone_id);
            let labels = jit_labels(&cap.os, &cap.arch, &grant.labels);
            let minted = match self
                .github
                .mint_jit_runner(
                    &runner_name,
                    self.cfg.github.runner_group_id,
                    &labels,
                    &self.cfg.work_folder,
                )
                .await
            {
                Ok(m) => m,
                Err(e) => {
                    // Likely rate limiting or permissions: stop minting this tick.
                    mint_error = Some(e);
                    break;
                }
            };
            let row = LeaseRow {
                id: lease_id,
                drone: grant.drone_id.clone(),
                class: grant.class,
                scope: grant.scope,
                labels,
                runner_name,
                runner_id: minted.runner_id,
                jit_config: Some(minted.jit_config),
                state: LeaseStatus::Pending,
                created_at: now_ts,
                expires_at: now_ts + self.cfg.lease_ttl_secs,
            };
            if let Err(e) = self.store.insert_lease(&row).await {
                // Do not leave an unowned registration behind.
                let _ = self.github.delete_runner(minted.runner_id).await;
                return Err(e.into());
            }
        }

        self.update_snapshot(|s| {
            s.last_tick_at = Some(now_ts);
            s.last_error = mint_error.as_ref().map(|e| format!("mint failed: {e}"));
            s.queue = queue;
            s.reserved_for = allocation.reserved_for.clone();
            s.unmet = allocation.unmet.clone();
        });
        Ok(())
    }

    /// Bring lease state in line with GitHub; returns the leases still active.
    async fn reconcile(
        &self,
        ours: &HashMap<&str, &Runner>,
        now: i64,
    ) -> Result<Vec<LeaseRow>, QueenError> {
        let mut still_active = Vec::new();
        for lease in self.store.active_leases().await? {
            let runner = ours.get(lease.runner_name.as_str());
            let overdue = now - lease.created_at > self.cfg.lease_max_secs;
            let never_came_online =
                now > lease.expires_at && runner.is_none_or(|r| r.status != "online");
            match runner {
                _ if overdue => {
                    self.end(&lease, runner.copied(), LeaseStatus::Expired, now)
                        .await?
                }
                Some(r) if r.busy => {
                    if lease.state == LeaseStatus::Pending {
                        self.store
                            .set_state(&lease.id, LeaseStatus::Running, now)
                            .await?;
                    }
                    still_active.push(lease);
                }
                // Ephemeral runners deregister after their one job.
                None if lease.state == LeaseStatus::Running => {
                    self.store
                        .set_state(&lease.id, LeaseStatus::Done, now)
                        .await?;
                }
                _ if never_came_online => {
                    self.end(&lease, runner.copied(), LeaseStatus::Expired, now)
                        .await?
                }
                _ => still_active.push(lease),
            }
        }
        Ok(still_active)
    }

    async fn end(
        &self,
        lease: &LeaseRow,
        runner: Option<&Runner>,
        to: LeaseStatus,
        now: i64,
    ) -> Result<(), QueenError> {
        self.store.set_state(&lease.id, to, now).await?;
        if let Some(r) = runner {
            // Best effort: the reaper retries next tick.
            if let Err(e) = self.github.delete_runner(r.id).await {
                tracing::warn!("delete runner {} failed: {e}", r.name);
            }
        }
        Ok(())
    }

    /// Delete offline `<prefix>-*` registrations that no active lease owns.
    async fn reap(&self, ours: &HashMap<&str, &Runner>, active: &[LeaseRow]) {
        let owned: HashSet<&str> = active.iter().map(|l| l.runner_name.as_str()).collect();
        for (name, r) in ours {
            if !owned.contains(name)
                && r.status == "offline"
                && !r.busy
                && let Err(e) = self.github.delete_runner(r.id).await
            {
                tracing::warn!("reap runner {name} failed: {e}");
            }
        }
    }

    fn summarize(
        &self,
        jobs: &[QueuedJob],
        now: OffsetDateTime,
    ) -> (Vec<QueueEntry>, Vec<Demand>, Vec<QueueAge>) {
        let mut by: BTreeMap<(Class, Scope), (u32, u64)> = BTreeMap::new();
        for job in jobs {
            let Some((class, scope)) = job
                .labels
                .iter()
                .find_map(|l| self.cfg.classes.parse_label(l))
            else {
                continue;
            };
            let e = by.entry((class, scope)).or_default();
            e.0 += 1;
            e.1 = e.1.max(age_secs(now, job.created_at));
        }
        let queue = by
            .iter()
            .map(|(&(class, scope), &(queued, oldest_age_secs))| QueueEntry {
                label: self.cfg.classes.job_label(class, scope),
                class,
                scope,
                queued,
                oldest_age_secs,
            })
            .collect();
        let demand = by
            .iter()
            .map(|(&(class, scope), &(count, _))| Demand {
                class,
                scope,
                count,
            })
            .collect();
        let ages = by
            .iter()
            .map(|(&(class, scope), &(_, oldest_age_secs))| QueueAge {
                class,
                scope,
                oldest_age_secs,
            })
            .collect();
        (queue, demand, ages)
    }

    /// Per-class totals for status and metrics: slots the fleet could hold.
    pub(crate) fn class_slots(&self, drones: &[DroneState]) -> BTreeMap<Class, u32> {
        Class::ALL
            .into_iter()
            .map(|c| {
                let spec = self.cfg.classes.spec(c);
                (
                    c,
                    drones
                        .iter()
                        .filter(|d| d.accepting)
                        .map(|d| max_slots(d, spec))
                        .sum(),
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jit_labels_list_platform_labels_first_then_class_labels() {
        let class = vec!["borg-ci-medium".to_owned(), "borg-ci-medium-pr".to_owned()];
        assert_eq!(
            jit_labels("linux", "x86_64", &class),
            [
                "self-hosted",
                "Linux",
                "X64",
                "borg-ci-medium",
                "borg-ci-medium-pr"
            ]
        );
        assert_eq!(
            jit_labels("macos", "aarch64", &class[..1]),
            ["self-hosted", "macOS", "ARM64", "borg-ci-medium"]
        );
        assert_eq!(jit_labels("plan9", "mips", &[]), ["self-hosted"]);
    }
}
