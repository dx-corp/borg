//! Update configuration. It is read by the privileged updater, so it must live
//! somewhere only root can change (`/etc/borg-update`, not the drone's own
//! `/etc/borg`): otherwise a compromised drone could point the updater at its
//! own "release" and have it installed as root.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const DEFAULT_DRONE_CONFIG: &str = "/etc/borg-update/update.json";
pub const DEFAULT_QUEEN_CONFIG: &str = "/etc/borg-update/queen.json";
/// The projected public repository's release manifest, used when the installer
/// is not told otherwise.
pub const DEFAULT_MANIFEST_URL: &str =
    "https://github.com/dx-corp/borg/releases/latest/download/borg-manifest.json";
pub const DEFAULT_VERSIONED_MANIFEST_URL: &str =
    "https://github.com/dx-corp/borg/releases/download/v{version}/borg-manifest.json";

#[derive(Debug, thiserror::Error)]
pub enum UpdateConfigError {
    #[error("read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("parse update config: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("{0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateConfig {
    /// Manifest of the newest release.
    pub manifest_url: String,
    /// Manifest of a specific release; `{version}` is replaced. Used when a
    /// version is requested (`--version`, or by the queen's rollout).
    #[serde(default)]
    pub versioned_manifest_url: Option<String>,
    /// Permit plain `http://` URLs (trusted network, tests).
    #[serde(default)]
    pub allow_insecure: bool,
    /// systemd unit (Linux) or launchd label (macOS) of the service.
    pub service: String,
    /// launchd only: the plist path used to start the service again.
    #[serde(default)]
    pub launchd_plist: Option<String>,
    #[serde(default = "default_bin")]
    pub bin: PathBuf,
    /// The record the daemon writes after each successful heartbeat/pass.
    pub health_file: PathBuf,
    /// Directory of the drone's `update-request` and `update-result.json`.
    pub state_dir: PathBuf,
    /// The pinned actions-runner directory and its owner, if this host has one.
    #[serde(default)]
    pub runner_dir: Option<PathBuf>,
    #[serde(default)]
    pub runner_owner: Option<String>,
    #[serde(default = "default_health_timeout")]
    pub health_timeout_secs: u64,
}

fn default_bin() -> PathBuf {
    "/usr/local/bin/borg".into()
}
fn default_health_timeout() -> u64 {
    120
}

impl UpdateConfig {
    pub fn load(path: &Path) -> Result<Self, UpdateConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| UpdateConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        Self::from_json(&text)
    }

    pub fn from_json(json: &str) -> Result<Self, UpdateConfigError> {
        let cfg: Self = serde_json::from_str(json)?;
        if cfg.service.is_empty() || cfg.manifest_url.is_empty() {
            return Err(UpdateConfigError::Invalid(
                "service and manifest_url are required".to_owned(),
            ));
        }
        if let Some(t) = &cfg.versioned_manifest_url
            && !t.contains("{version}")
        {
            return Err(UpdateConfigError::Invalid(
                "versioned_manifest_url must contain {version}".to_owned(),
            ));
        }
        if cfg.health_timeout_secs == 0 {
            return Err(UpdateConfigError::Invalid(
                "health_timeout_secs must be positive".to_owned(),
            ));
        }
        Ok(cfg)
    }

    /// The manifest to read for `version` (the newest release if `None`).
    pub fn manifest_url_for(&self, version: Option<&str>) -> String {
        match (version, &self.versioned_manifest_url) {
            (Some(v), Some(template)) => template.replace("{version}", v),
            _ => self.manifest_url.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"{"manifest_url":"https://m.example.test/latest.json","service":"borg-drone.service","health_file":"/var/lib/borg/health.json","state_dir":"/var/lib/borg"}"#;

    #[test]
    fn defaults_and_url_selection() {
        let c = UpdateConfig::from_json(BASE).unwrap();
        assert_eq!(
            (c.health_timeout_secs, c.bin.as_path()),
            (120, Path::new("/usr/local/bin/borg"))
        );
        assert_eq!(
            c.manifest_url_for(None),
            "https://m.example.test/latest.json"
        );
        assert_eq!(
            c.manifest_url_for(Some("1.2.3")),
            "https://m.example.test/latest.json",
            "no template: newest"
        );

        let t = BASE.replace(
            '}',
            r#","versioned_manifest_url":"https://m.example.test/v{version}.json"}"#,
        );
        let c = UpdateConfig::from_json(&t).unwrap();
        assert_eq!(
            c.manifest_url_for(Some("1.2.3")),
            "https://m.example.test/v1.2.3.json"
        );
        assert_eq!(
            c.manifest_url_for(None),
            "https://m.example.test/latest.json"
        );
    }

    #[test]
    fn rejects_bad_config() {
        for bad in [
            BASE.replace(
                '}',
                r#","versioned_manifest_url":"https://m.example.test/v.json"}"#,
            ),
            BASE.replace('}', r#","health_timeout_secs":0}"#),
            BASE.replace('}', r#","bogus":1}"#),
            BASE.replace("borg-drone.service", ""),
        ] {
            assert!(UpdateConfig::from_json(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn default_urls_point_at_the_public_release_manifest() {
        assert!(
            DEFAULT_MANIFEST_URL.starts_with("https://")
                && DEFAULT_VERSIONED_MANIFEST_URL.contains("{version}")
        );
    }
}
