//! Queen configuration, read from a JSON file. Nothing org-specific is baked
//! in: the organization, repositories, label prefix and class sizes all come
//! from here.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::allocator::{WarmFloor, default_warm_floors};
use crate::classes::{Classes, ClassesError};
use crate::envcontract::ContractSettings;
use crate::health::HealthSettings;
use crate::homeostasis::HomeostasisSettings;
use crate::profile::ProfileSettings;
use crate::rollout::RolloutSettings;
use crate::schedule::WarmSchedule;
use crate::tailnet::TailnetEnrollSettings;
use crate::units::UnitSettings;
use crate::wake::WakeSettings;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("parse config: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("invalid classes: {0}")]
    Classes(#[from] ClassesError),
    #[error("{0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubSettings {
    /// Organization that owns the self-hosted runners.
    pub org: String,
    /// GitHub App id (or client id).
    pub app_id: String,
    /// PEM private key file. If absent, `BORG_APP_PRIVATE_KEY` must hold the
    /// PEM text.
    #[serde(default)]
    pub private_key_path: Option<PathBuf>,
    #[serde(default = "default_api_base")]
    pub api_base: String,
    #[serde(default = "default_runner_group")]
    pub runner_group_id: u64,
}

fn default_api_base() -> String {
    "https://api.github.com".to_owned()
}
fn default_runner_group() -> u64 {
    1
}

/// The opt-in supersede janitor (see [`crate::janitor`]). Present means
/// enabled; it starts in dry-run so enabling it cannot cancel anything until
/// the operator has read its log and set `dry_run` to false.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JanitorSettings {
    /// File holding a token with `actions: write` on the listed repositories.
    /// If absent, `BORG_JANITOR_TOKEN` must hold the token.
    #[serde(default)]
    pub token_path: Option<PathBuf>,
    /// Log what would be cancelled without cancelling.
    #[serde(default = "yes")]
    pub dry_run: bool,
    #[serde(default = "default_janitor_interval")]
    pub interval_secs: u64,
    /// Hard cap on cancellations (or dry-run matches) per pass.
    #[serde(default = "default_janitor_max")]
    pub max_cancels_per_pass: u32,
}

fn yes() -> bool {
    true
}
fn default_janitor_interval() -> u64 {
    60
}
fn default_janitor_max() -> u32 {
    20
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueenConfig {
    pub github: GithubSettings,
    /// Repositories (`owner/name`) whose queued jobs are served.
    pub repos: Vec<String>,
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    pub db_path: PathBuf,
    #[serde(default = "default_runner_prefix")]
    pub runner_prefix: String,
    #[serde(default = "default_work_folder")]
    pub work_folder: String,
    #[serde(default = "ten")]
    pub poll_interval_secs: u64,
    /// A lease whose runner has not come online within this long is expired.
    #[serde(default = "default_lease_ttl")]
    pub lease_ttl_secs: i64,
    /// Hard ceiling on a lease's lifetime.
    #[serde(default = "default_lease_max")]
    pub lease_max_secs: i64,
    /// A drone silent for longer than this gets no new leases.
    #[serde(default = "default_drone_stale")]
    pub drone_stale_secs: i64,
    /// Demand older than this triggers the starvation guard; null disables.
    #[serde(default = "default_starvation")]
    pub starvation_after_secs: Option<u64>,
    #[serde(default)]
    pub classes: Classes,
    #[serde(default = "default_warm_floors")]
    pub warm_floors: Vec<WarmFloor>,
    /// Time-of-day raises to the warm floors; see [`crate::schedule`].
    #[serde(default)]
    pub warm_schedule: WarmSchedule,
    /// Absent (the default) disables `POST /v1/enroll`; see [`crate::tailnet`].
    #[serde(default)]
    pub tailnet_enroll: Option<TailnetEnrollSettings>,
    /// Demand-responsive warm floors; off by default. See
    /// [`crate::homeostasis`].
    #[serde(default)]
    pub homeostasis: HomeostasisSettings,
    /// The build environment contract checked before work is placed on a drone;
    /// off by default. See [`crate::envcontract`].
    #[serde(default)]
    pub env_contract: ContractSettings,
    /// Work units (gate and hook runs) and their API; off by default. See
    /// [`crate::units`].
    #[serde(default)]
    pub units: UnitSettings,
    /// Wake-on-LAN for sleeping hosts when demand goes unmet; off by default.
    /// See [`crate::wake`].
    #[serde(default)]
    pub wake: WakeSettings,
    /// Gating of the rollout of `rollout_target_version`; see
    /// [`crate::rollout`]. Gated only when `previous_version` is set.
    #[serde(default)]
    pub rollout: RolloutSettings,
    /// Learned per-drone profiles that feed placement; see [`crate::profile`].
    #[serde(default)]
    pub profiles: ProfileSettings,
    /// Host-health thresholds, decay and cooldown; see [`crate::health`].
    #[serde(default)]
    pub health: HealthSettings,
    /// Job telemetry older than this is deleted.
    #[serde(default = "default_telemetry_days")]
    pub telemetry_retention_days: u32,
    /// The borg version drones should run. Only reported (`/metrics`,
    /// `/status`): drones update themselves, the queen never forces it.
    #[serde(default)]
    pub rollout_target_version: Option<String>,
    /// Absent (the default) disables the supersede janitor.
    #[serde(default)]
    pub supersede_janitor: Option<JanitorSettings>,
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 8080))
}
fn default_runner_prefix() -> String {
    "borg".to_owned()
}
fn default_work_folder() -> String {
    "_work".to_owned()
}
fn ten() -> u64 {
    10
}
fn default_lease_ttl() -> i64 {
    300
}
fn default_lease_max() -> i64 {
    6 * 3600
}
fn default_drone_stale() -> i64 {
    60
}
#[allow(
    clippy::unnecessary_wraps,
    reason = "serde default for an Option field"
)]
fn default_telemetry_days() -> u32 {
    30
}
fn default_starvation() -> Option<u64> {
    Some(120)
}

impl QueenConfig {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        Self::from_json(&text)
    }

    pub fn from_json(json: &str) -> Result<Self, ConfigError> {
        let cfg: Self = serde_json::from_str(json)?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        self.classes.validate()?;
        let bad = |msg: &str| Err(ConfigError::Invalid(msg.to_owned()));
        if self.github.org.is_empty() || self.github.app_id.is_empty() {
            return bad("github.org and github.app_id are required");
        }
        if self.repos.is_empty() {
            return bad("repos must name at least one owner/name repository");
        }
        if self
            .repos
            .iter()
            .any(|r| r.split('/').filter(|p| !p.is_empty()).count() != 2)
        {
            return bad("every repos entry must look like owner/name");
        }
        let prefix_ok = !self.runner_prefix.is_empty()
            && self
                .runner_prefix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-');
        if !prefix_ok {
            return bad("runner_prefix must be non-empty alphanumerics and dashes");
        }
        if self.poll_interval_secs == 0 || self.lease_ttl_secs <= 0 || self.lease_max_secs <= 0 {
            return bad("poll_interval_secs, lease_ttl_secs and lease_max_secs must be positive");
        }
        if self.telemetry_retention_days == 0 {
            return bad("telemetry_retention_days must be at least 1");
        }
        self.env_contract.validate().map_err(ConfigError::Invalid)?;
        self.units.validate().map_err(ConfigError::Invalid)?;
        self.wake.validate().map_err(ConfigError::Invalid)?;
        self.rollout.validate().map_err(ConfigError::Invalid)?;
        self.profiles.validate().map_err(ConfigError::Invalid)?;
        self.homeostasis.validate().map_err(ConfigError::Invalid)?;
        self.health.validate().map_err(ConfigError::Invalid)?;
        if let Some(t) = &self.tailnet_enroll {
            t.validate().map_err(ConfigError::Invalid)?;
        }
        self.warm_schedule
            .validate()
            .map_err(ConfigError::Invalid)?;
        if let Some(j) = &self.supersede_janitor
            && (j.interval_secs == 0 || j.max_cancels_per_pass == 0)
        {
            return bad(
                "supersede_janitor interval_secs and max_cancels_per_pass must be positive",
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: &str = r#"{"github":{"org":"example-org","app_id":"1"},"repos":["example-org/app"],"db_path":"/tmp/q.db"}"#;

    #[test]
    fn minimal_config_gets_defaults() {
        let c = QueenConfig::from_json(MIN).unwrap();
        assert_eq!(c.poll_interval_secs, 10);
        assert_eq!(c.starvation_after_secs, Some(120));
        assert_eq!(c.github.api_base, "https://api.github.com");
        assert_eq!(c.warm_floors, default_warm_floors());
        assert_eq!(c.listen.ip().to_string(), "127.0.0.1");
    }

    #[test]
    fn rejects_bad_repos_unknown_fields_and_empty_org() {
        for bad in [
            MIN.replace("example-org/app", "app"),
            MIN.replace("\"repos\"", "\"nope\":1,\"repos\""),
            MIN.replace("\"org\":\"example-org\"", "\"org\":\"\""),
            MIN.replace("\"repos\":[\"example-org/app\"]", "\"repos\":[]"),
        ] {
            assert!(QueenConfig::from_json(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn janitor_is_off_by_default_and_dry_run_when_enabled() {
        assert!(
            QueenConfig::from_json(MIN)
                .unwrap()
                .supersede_janitor
                .is_none()
        );
        let c =
            QueenConfig::from_json(&MIN.replace("\"repos\"", "\"supersede_janitor\":{},\"repos\""))
                .unwrap();
        let j = c.supersede_janitor.unwrap();
        assert!(j.dry_run);
        assert_eq!((j.interval_secs, j.max_cancels_per_pass), (60, 20));
        for bad in [
            "{\"interval_secs\":0}",
            "{\"max_cancels_per_pass\":0}",
            "{\"nope\":1}",
        ] {
            let json = MIN.replace(
                "\"repos\"",
                &format!("\"supersede_janitor\":{bad},\"repos\""),
            );
            assert!(QueenConfig::from_json(&json).is_err(), "{bad}");
        }
    }

    #[test]
    fn health_settings_default_and_validate() {
        let c = QueenConfig::from_json(MIN).unwrap();
        assert!(c.health.auto_quarantine);
        let with = |h: &str| {
            QueenConfig::from_json(&MIN.replace("\"repos\"", &format!("\"health\":{h},\"repos\"")))
        };
        assert!(with(r#"{"half_life_secs":0}"#).is_err());
        assert!(with(r#"{"nope":1}"#).is_err());
        assert!(
            !with(r#"{"auto_quarantine":false}"#)
                .unwrap()
                .health
                .auto_quarantine
        );
    }

    #[test]
    fn zero_telemetry_retention_is_rejected() {
        let json = MIN.replace("\"repos\"", "\"telemetry_retention_days\":0,\"repos\"");
        assert!(QueenConfig::from_json(&json).is_err());
    }

    #[test]
    fn tailnet_enroll_is_off_by_default_and_needs_an_allowlist() {
        assert!(
            QueenConfig::from_json(MIN)
                .unwrap()
                .tailnet_enroll
                .is_none()
        );
        let with = |t: &str| {
            QueenConfig::from_json(
                &MIN.replace("\"repos\"", &format!("\"tailnet_enroll\":{t},\"repos\"")),
            )
        };
        assert!(with("{}").is_err());
        assert!(with(r#"{"allowed_tags":["ci"]}"#).is_err());
        let ok = with(r#"{"allowed_tags":["tag:ci"]}"#).unwrap();
        assert_eq!(
            ok.tailnet_enroll.unwrap().socket_path.to_str(),
            Some("/var/run/tailscale/tailscaled.sock")
        );
    }

    #[test]
    fn starvation_can_be_disabled() {
        let c = QueenConfig::from_json(
            &MIN.replace("\"repos\"", "\"starvation_after_secs\":null,\"repos\""),
        )
        .unwrap();
        assert_eq!(c.starvation_after_secs, None);
    }
}
