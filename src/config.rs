//! Queen configuration, read from a JSON file. Nothing org-specific is baked
//! in: the organization, repositories, label prefix and class sizes all come
//! from here.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::allocator::{WarmFloor, default_warm_floors};
use crate::classes::{Classes, ClassesError};

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
    /// The borg version drones should run. Only reported (`/metrics`,
    /// `/status`): drones update themselves, the queen never forces it.
    #[serde(default)]
    pub rollout_target_version: Option<String>,
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
    fn starvation_can_be_disabled() {
        let c = QueenConfig::from_json(
            &MIN.replace("\"repos\"", "\"starvation_after_secs\":null,\"repos\""),
        )
        .unwrap();
        assert_eq!(c.starvation_after_secs, None);
    }
}
