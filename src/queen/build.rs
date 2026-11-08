//! What the running queen is (`/status` `queen`), the health record the
//! self-updater waits for after a swap, and the rollout target that follows
//! the queen's own build.

use serde::Serialize;

use super::Queen;
use crate::rollout::RolloutState;

/// A rollout in progress adopts the queen's newer build once that build has
/// run this long; see [`Queen::settled_target`].
const RETARGET_AFTER_SECS: i64 = 3600;

/// What this queen process runs, for `/status` and the updater.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QueenBuild {
    /// `0.1.0+<first 12 hex of the binary's SHA-256>`, the same scheme drones
    /// report, so it matches a release manifest.
    version: String,
    /// The git commit it was built from (`BORG_BUILD_SHA` at build time);
    /// `None` for a build that did not set it.
    build_sha: Option<&'static str>,
    started_at: i64,
}

impl QueenBuild {
    /// Reads the running binary to hash it: call it off the async threads.
    pub fn current(started_at: i64) -> Self {
        Self {
            version: crate::drone::selfupdate::running_build_version()
                .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned()),
            build_sha: option_env!("BORG_BUILD_SHA"),
            started_at,
        }
    }

    /// The build version (`0.1.0+<12 hex>`), the rollout target with
    /// `rollout.follow_queen`.
    pub(super) fn version(&self) -> &str {
        &self.version
    }

    /// When this queen process started on the build.
    fn started_at(&self) -> i64 {
        self.started_at
    }
}

impl Queen {
    /// Record what this process runs; see [`QueenBuild`].
    #[must_use]
    pub fn with_build(mut self, build: QueenBuild) -> Self {
        self.build = Some(build);
        self
    }

    pub(crate) fn build(&self) -> Option<&QueenBuild> {
        self.build.as_ref()
    }

    /// The version drones are rolled out to: the configured target, or with
    /// `rollout.follow_queen` the build this queen runs.
    pub(crate) fn rollout_target(&self) -> Option<String> {
        self.cfg.rollout_target_version.clone().or_else(|| {
            self.cfg
                .rollout
                .follow_queen
                .then(|| self.build.as_ref().map(|b| b.version().to_owned()))
                .flatten()
        })
    }

    /// The target this pass, held while a rollout is in flight when the
    /// target only follows the queen's own build; see
    /// [`crate::rollout::settled_target`]. A configured target applies at once.
    pub(super) fn settled_target(
        &self,
        target: String,
        stored: Option<&RolloutState>,
        now: i64,
    ) -> String {
        if self.cfg.rollout_target_version.is_some() || !self.cfg.rollout.follow_queen {
            return target;
        }
        let build_age = self
            .build
            .as_ref()
            .map(|b| now.saturating_sub(b.started_at()));
        crate::rollout::settled_target(target, stored, build_age, RETARGET_AFTER_SECS)
    }

    /// After a pass that succeeded, tell the updater this binary works: write
    /// `health_file` once any drone has heartbeated to this process (so a
    /// queen that ticks but that no drone reaches is not healthy).
    pub(super) async fn write_health(&self, drones: &[crate::store::DroneRow], now: i64) {
        let (Some(path), Some(build)) = (self.cfg.health_file.clone(), self.build.clone()) else {
            return;
        };
        if !drones
            .iter()
            .any(|d| d.last_heartbeat.is_some_and(|t| t >= build.started_at))
        {
            return;
        }
        let record = crate::update::health::HealthRecord {
            version: build.version,
            at: now,
            pid: std::process::id(),
        };
        match tokio::task::spawn_blocking(move || crate::update::health::write(&path, &record))
            .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!("write queen health file failed: {e}"),
            Err(e) => tracing::warn!("write queen health file task failed: {e}"),
        }
    }
}
