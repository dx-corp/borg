//! Drone configuration, a JSON file written by `borg drone install`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::allocator::HostCaps;
use crate::secret::Secret;

#[derive(Debug, thiserror::Error)]
pub enum DroneConfigError {
    #[error("read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("parse config: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("{0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DroneConfig {
    /// Base URL of the queen. Must be `https://` unless `allow_insecure`.
    pub queen_url: String,
    /// File holding the enrollment token. If absent, `BORG_DRONE_TOKEN`.
    #[serde(default)]
    pub token_file: Option<PathBuf>,
    /// PEM CA certificate that signs the queen's certificate. When set it is
    /// the only trust root, which pins the queen's CA.
    #[serde(default)]
    pub ca_cert_path: Option<PathBuf>,
    /// Permit a plain `http://` queen URL (trusted LAN only).
    #[serde(default)]
    pub allow_insecure: bool,
    /// Pinned actions-runner install; every lease gets a private copy of it.
    pub runner_dir: PathBuf,
    /// Parent of the per-lease work directories. Wiped on start and after
    /// every job.
    pub work_root: PathBuf,
    /// Tool caches (sccache, mise, cargo). Never touched by cleanup.
    pub cache_root: PathBuf,
    #[serde(default = "default_docker")]
    pub docker_bin: String,
    #[serde(default = "default_heartbeat")]
    pub heartbeat_secs: u64,
    /// A lease running longer than this is killed.
    #[serde(default = "default_max_job")]
    pub max_job_secs: u64,
    /// On SIGTERM, wait this long for in-flight jobs before killing them.
    #[serde(default = "default_drain")]
    pub drain_timeout_secs: u64,
    /// Owner caps ("this PC gives at most 8 cores / 24 GiB").
    #[serde(default)]
    pub max_vcpu: Option<u32>,
    #[serde(default)]
    pub max_mem_mib: Option<u64>,
    #[serde(default)]
    pub max_leases: Option<u32>,
    #[serde(default = "default_reserve")]
    pub reserve_mem_mib: u64,
}

fn default_docker() -> String {
    "docker".to_owned()
}
fn default_heartbeat() -> u64 {
    10
}
fn default_max_job() -> u64 {
    6 * 3600
}
fn default_drain() -> u64 {
    3600
}
fn default_reserve() -> u64 {
    HostCaps::default().reserve_mem_mib
}

impl DroneConfig {
    pub fn load(path: &Path) -> Result<Self, DroneConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| DroneConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        Self::from_json(&text)
    }

    pub fn from_json(json: &str) -> Result<Self, DroneConfigError> {
        let cfg: Self = serde_json::from_str(json)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), DroneConfigError> {
        let bad = |m: &str| Err(DroneConfigError::Invalid(m.to_owned()));
        let https = self.queen_url.starts_with("https://");
        let http = self.queen_url.starts_with("http://");
        let scheme_ok = https || (http && self.allow_insecure);
        if !scheme_ok {
            return bad(
                "queen_url must start with https:// (set allow_insecure for plain http on a trusted network)",
            );
        }
        if self.heartbeat_secs == 0 || self.max_job_secs == 0 {
            return bad("heartbeat_secs and max_job_secs must be positive");
        }
        if self.max_vcpu == Some(0) || self.max_mem_mib == Some(0) || self.max_leases == Some(0) {
            return bad("owner caps must be positive when set");
        }
        if self.ca_cert_path.is_some() && http {
            return bad("ca_cert_path only applies to an https:// queen_url");
        }
        Ok(())
    }

    /// The caps advertised to the queen with every heartbeat.
    pub fn host_caps(&self) -> HostCaps {
        HostCaps {
            max_vcpu: self.max_vcpu,
            max_mem_mib: self.max_mem_mib,
            max_leases: self.max_leases,
            reserve_mem_mib: self.reserve_mem_mib,
        }
    }

    /// The enrollment token from `token_file` or `BORG_DRONE_TOKEN`.
    pub fn load_token(&self) -> Result<Secret, DroneConfigError> {
        let raw = match &self.token_file {
            Some(path) => {
                std::fs::read_to_string(path).map_err(|source| DroneConfigError::Read {
                    path: path.clone(),
                    source,
                })?
            }
            None => std::env::var("BORG_DRONE_TOKEN").map_err(|_| {
                DroneConfigError::Invalid("set token_file or BORG_DRONE_TOKEN".to_owned())
            })?,
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(DroneConfigError::Invalid(
                "enrollment token is empty".to_owned(),
            ));
        }
        Ok(Secret::new(trimmed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"{"queen_url":"https://queen.example.test:8443","runner_dir":"/r","work_root":"/w","cache_root":"/c"}"#;

    #[test]
    fn defaults_and_caps() {
        let c = DroneConfig::from_json(BASE).unwrap();
        assert_eq!((c.heartbeat_secs, c.docker_bin.as_str()), (10, "docker"));
        let with = BASE.replace('}', r#","max_vcpu":8,"max_mem_mib":24576}"#);
        let caps = DroneConfig::from_json(&with).unwrap().host_caps();
        assert_eq!((caps.max_vcpu, caps.max_mem_mib), (Some(8), Some(24576)));
    }

    #[test]
    fn plain_http_needs_explicit_opt_in() {
        let http = BASE.replace("https://", "http://");
        assert!(DroneConfig::from_json(&http).is_err());
        let allowed = http.replace('}', r#","allow_insecure":true}"#);
        assert!(DroneConfig::from_json(&allowed).is_ok());
        assert!(DroneConfig::from_json(&BASE.replace("https://queen", "ftp://queen")).is_err());
    }

    #[test]
    fn rejects_zero_caps_unknown_fields_and_ca_on_http() {
        for extra in [r#","max_vcpu":0"#, r#","bogus":1"#] {
            assert!(
                DroneConfig::from_json(&BASE.replace('}', &format!("{extra}}}"))).is_err(),
                "{extra}"
            );
        }
        let http_ca = BASE
            .replace("https://", "http://")
            .replace('}', r#","allow_insecure":true,"ca_cert_path":"/ca.pem"}"#);
        assert!(DroneConfig::from_json(&http_ca).is_err());
    }

    #[test]
    fn token_file_is_trimmed_and_must_not_be_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t");
        let mut c = DroneConfig::from_json(BASE).unwrap();
        c.token_file = Some(path.clone());
        std::fs::write(&path, "borg_abc\n").unwrap();
        assert_eq!(c.load_token().unwrap().expose(), "borg_abc");
        std::fs::write(&path, "\n").unwrap();
        assert!(c.load_token().is_err());
    }
}
