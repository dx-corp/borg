//! What the running queen is (`/status` `queen`) and the health record the
//! self-updater waits for after a swap.

use serde::Serialize;

use super::Queen;

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
