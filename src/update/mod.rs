//! Self-update for the drone and the queen: `borg update`.
//!
//! The binary comes from a release [`manifest`] whose SHA-256 pins are
//! mandatory. The update drains the service (the service manager's stop is the
//! drain), swaps the binary atomically keeping `borg.prev`, restarts it, waits
//! for health evidence from the new version and rolls back to `borg.prev` if it
//! does not appear. The same machinery installs a newer pinned actions-runner,
//! on its own with `--runner-only` or together with a binary update.

pub mod cli;
pub mod config;
pub mod health;
pub mod manifest;
pub mod runner;
pub mod service;
pub mod swap;

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::drone::exec::Exec;
use health::HealthProbe;
use manifest::Manifest;
use service::ServiceManager;

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error(transparent)]
    Manifest(#[from] manifest::ManifestError),
    #[error(transparent)]
    Swap(#[from] swap::SwapError),
    #[error(transparent)]
    Runner(#[from] runner::RunnerError),
    #[error("the manifest is version {manifest}, not the requested {requested}")]
    VersionMismatch { manifest: String, requested: String },
    #[error("the manifest has no binary for {0}")]
    NoTarget(String),
    #[error("this platform ({0}) has no released borg binary")]
    UnsupportedHost(String),
    #[error("could not drain and stop the service; nothing was changed: {0}")]
    Drain(String),
    #[error("could not restart the service and could not roll back: {0}")]
    Unrecoverable(String),
}

/// What an update run did (or, with `check`, would do).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Outcome {
    UpToDate {
        version: String,
    },
    Available {
        current: String,
        target: String,
        runner: Option<String>,
    },
    Updated {
        from: String,
        to: String,
        runner_updated: bool,
    },
    RunnerUpdated {
        version: String,
    },
    RolledBack {
        from: String,
        to: String,
        reason: String,
    },
}

pub struct Options<'a> {
    pub bin: &'a Path,
    /// The version of the installed binary (the one running this update).
    pub current_version: &'a str,
    pub requested_version: Option<&'a str>,
    pub check: bool,
    pub runner_only: bool,
    pub allow_insecure: bool,
    pub health_timeout: Duration,
    pub poll: Duration,
    pub host_os: &'a str,
    pub host_arch: &'a str,
    /// Where the pinned runner lives and who owns it; `None` skips runner updates.
    pub runner_dir: Option<&'a Path>,
    pub runner_owner: Option<&'a str>,
}

/// Run an update against an already fetched manifest.
pub async fn run(
    opts: &Options<'_>,
    manifest: &Manifest,
    service: &dyn ServiceManager,
    health: &dyn HealthProbe,
    exec: &dyn Exec,
) -> Result<Outcome, UpdateError> {
    if let Some(requested) = opts.requested_version
        && requested != manifest.version
    {
        return Err(UpdateError::VersionMismatch {
            manifest: manifest.version.clone(),
            requested: requested.to_owned(),
        });
    }
    let runner_pin = match opts.runner_dir {
        Some(dir) => manifest
            .runner_pin_for(opts.host_os, opts.host_arch)
            .filter(|pin| !runner::is_installed(dir, pin)),
        None => None,
    };
    let binary_needed = !opts.runner_only && manifest.version != opts.current_version;

    if opts.check {
        return Ok(if binary_needed || runner_pin.is_some() {
            Outcome::Available {
                current: opts.current_version.to_owned(),
                target: manifest.version.clone(),
                runner: runner_pin.map(|p| p.version),
            }
        } else {
            Outcome::UpToDate {
                version: opts.current_version.to_owned(),
            }
        });
    }
    if !binary_needed && runner_pin.is_none() {
        return Ok(Outcome::UpToDate {
            version: opts.current_version.to_owned(),
        });
    }

    // Everything that can fail without touching the running service comes first.
    let new_bin = swap::new_path(opts.bin);
    if binary_needed {
        let target = manifest::host_target(opts.host_os, opts.host_arch).ok_or_else(|| {
            UpdateError::UnsupportedHost(format!("{}-{}", opts.host_os, opts.host_arch))
        })?;
        let artifact = manifest
            .artifact_for(target)
            .ok_or_else(|| UpdateError::NoTarget(target.to_owned()))?;
        swap::download_verified(
            &artifact.url,
            &artifact.sha256,
            &new_bin,
            opts.allow_insecure,
        )
        .await?;
    }
    let mut runner_updated = false;
    if let (Some(pin), Some(dir)) = (&runner_pin, opts.runner_dir) {
        if let Err(e) = runner::install(exec, pin, dir, opts.runner_owner) {
            let _ = std::fs::remove_file(&new_bin);
            return Err(e.into());
        }
        runner_updated = true;
    }
    if !binary_needed {
        return Ok(Outcome::RunnerUpdated {
            version: runner_pin.map(|p| p.version).unwrap_or_default(),
        });
    }

    // Drain: no new leases, in-flight jobs finish, then the service exits.
    if let Err(e) = service.stop() {
        let _ = std::fs::remove_file(&new_bin);
        return Err(UpdateError::Drain(e));
    }
    swap::install(opts.bin, &new_bin)?;
    let started = service.start();
    let healthy = started.is_ok()
        && wait_healthy(health, &manifest.version, opts.health_timeout, opts.poll).await;
    if healthy {
        return Ok(Outcome::Updated {
            from: opts.current_version.to_owned(),
            to: manifest.version.clone(),
            runner_updated,
        });
    }

    let reason = match started {
        Err(e) => format!("the new version did not start: {e}"),
        Ok(()) => format!(
            "no health evidence from {} within {}s",
            manifest.version,
            opts.health_timeout.as_secs()
        ),
    };
    // Roll back: stop the failed version, restore the previous binary, restart.
    let _ = service.stop();
    swap::rollback(opts.bin).map_err(|e| UpdateError::Unrecoverable(e.to_string()))?;
    service.start().map_err(UpdateError::Unrecoverable)?;
    Ok(Outcome::RolledBack {
        from: opts.current_version.to_owned(),
        to: manifest.version.clone(),
        reason,
    })
}

/// Wait until the daemon reports `version` as healthy, or the timeout passes.
async fn wait_healthy(
    health: &dyn HealthProbe,
    version: &str,
    timeout: Duration,
    poll: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if health.current().is_some_and(|r| r.version == version) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(poll).await;
    }
}

/// The path the drone writes an update request to and the updater reads.
pub fn request_path(state_dir: &Path) -> PathBuf {
    state_dir.join("update-request")
}

/// Where the updater leaves its result for the drone to report.
pub fn result_path(state_dir: &Path) -> PathBuf {
    state_dir.join("update-result.json")
}

/// A request for the privileged updater, written by the drone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub version: String,
}

/// What the updater leaves for the drone to report to the queen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateResult {
    pub target: String,
    /// `installed`, `runner_updated`, `up_to_date`, `rolled_back` or `failed`.
    pub state: String,
    pub detail: String,
    pub at: i64,
}

impl UpdateResult {
    pub fn from_outcome(target: &str, outcome: &Result<Outcome, UpdateError>, at: i64) -> Self {
        let (state, detail) = match outcome {
            Ok(Outcome::Updated { to, .. }) => ("installed", format!("updated to {to}")),
            Ok(Outcome::RunnerUpdated { version }) => {
                ("runner_updated", format!("runner {version}"))
            }
            Ok(Outcome::UpToDate { version }) => ("up_to_date", version.clone()),
            Ok(Outcome::Available { target, .. }) => ("up_to_date", format!("available {target}")),
            Ok(Outcome::RolledBack { reason, .. }) => ("rolled_back", reason.clone()),
            Err(e) => ("failed", e.to_string()),
        };
        Self {
            target: target.to_owned(),
            state: state.to_owned(),
            detail,
            at,
        }
    }
}
