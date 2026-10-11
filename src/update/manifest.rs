//! The release manifest: what version is current and where to get it.
//!
//! A manifest is a small JSON document published next to a release (for
//! example as a GitHub release asset of the public borg repository):
//!
//! ```json
//! {
//!   "schema": 1,
//!   "version": "0.5.0",
//!   "targets": {
//!     "linux-x64-musl":   { "url": "https://.../borg-linux-x64-musl",   "sha256": "<64 hex>" },
//!     "linux-arm64-musl": { "url": "https://.../borg-linux-arm64-musl", "sha256": "<64 hex>" },
//!     "darwin-arm64":     { "url": "https://.../borg-darwin-arm64",     "sha256": "<64 hex>" }
//!   },
//!   "runner": { "version": "2.337.0", "sha256": { "linux-x64": "<64 hex>", "osx-arm64": "<64 hex>" } }
//! }
//! ```
//!
//! sha256 pinning against the manifest is mandatory: an artifact is never
//! installed unless its digest matches. The manifest itself is trusted by the
//! transport (HTTPS to the configured URL); signature verification is not
//! implemented.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::drone::pins::RunnerPin;

pub const SCHEMA: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("fetch manifest {url}: {source}")]
    Fetch { url: String, source: reqwest::Error },
    #[error("manifest {url} returned {status}")]
    Status { url: String, status: u16 },
    #[error("parse manifest: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("invalid manifest: {0}")]
    Invalid(String),
    #[error("manifest URL must be https:// (set allow_insecure for a trusted network): {0}")]
    Insecure(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub url: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerRelease {
    pub version: String,
    /// actions-runner asset platform (`linux-x64`, `linux-arm64`, `osx-x64`,
    /// `osx-arm64`) to the SHA-256 of that tarball.
    pub sha256: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: u32,
    pub version: String,
    pub targets: BTreeMap<String, Artifact>,
    #[serde(default)]
    pub runner: Option<RunnerRelease>,
}

fn is_sha256(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A version is a short, printable string without path or shell characters.
pub fn valid_version(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 64
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+' | b'_'))
}

impl Manifest {
    pub fn parse(json: &str) -> Result<Self, ManifestError> {
        let m: Self = serde_json::from_str(json)?;
        m.validate()?;
        Ok(m)
    }

    pub fn validate(&self) -> Result<(), ManifestError> {
        let bad = |m: String| Err(ManifestError::Invalid(m));
        if self.schema != SCHEMA {
            return bad(format!(
                "unsupported schema {} (expected {SCHEMA})",
                self.schema
            ));
        }
        if !valid_version(&self.version) {
            return bad(format!("bad version {:?}", self.version));
        }
        if self.targets.is_empty() {
            return bad("no targets".to_owned());
        }
        for (name, a) in &self.targets {
            if !is_sha256(&a.sha256) {
                return bad(format!(
                    "target {name}: sha256 must be 64 lowercase hex characters"
                ));
            }
            if !(a.url.starts_with("https://") || a.url.starts_with("http://")) {
                return bad(format!("target {name}: url must be http(s)"));
            }
        }
        if let Some(r) = &self.runner {
            if !valid_version(&r.version) {
                return bad(format!("bad runner version {:?}", r.version));
            }
            if let Some((p, _)) = r.sha256.iter().find(|(_, s)| !is_sha256(s)) {
                return bad(format!(
                    "runner {p}: sha256 must be 64 lowercase hex characters"
                ));
            }
        }
        Ok(())
    }

    pub fn artifact_for(&self, target: &str) -> Option<&Artifact> {
        self.targets.get(target)
    }

    /// The actions-runner to install on a host, if the manifest pins one for it.
    pub fn runner_pin_for(&self, os: &str, arch: &str) -> Option<RunnerPin> {
        let release = self.runner.as_ref()?;
        let platform = crate::drone::pins::pin_for(os, arch)?.platform;
        let sha256 = release.sha256.get(&platform)?.clone();
        Some(RunnerPin {
            version: release.version.clone(),
            platform,
            sha256,
        })
    }
}

/// The manifest key for a host's borg binary. Linux gets the static musl
/// build, which runs on any glibc.
pub fn host_target(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Some("linux-x64-musl"),
        ("linux", "aarch64") => Some("linux-arm64-musl"),
        ("macos", "aarch64") => Some("darwin-arm64"),
        _ => None,
    }
}

/// Fetch and validate a manifest. `https://` is required unless `allow_insecure`.
pub async fn fetch(url: &str, allow_insecure: bool) -> Result<Manifest, ManifestError> {
    let scheme_ok = url.starts_with("https://") || (allow_insecure && url.starts_with("http://"));
    if !scheme_ok {
        return Err(ManifestError::Insecure(url.to_owned()));
    }
    let client = reqwest::Client::builder()
        .user_agent("borg-update")
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|source| ManifestError::Fetch {
            url: url.to_owned(),
            source,
        })?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|source| ManifestError::Fetch {
            url: url.to_owned(),
            source,
        })?;
    if !resp.status().is_success() {
        return Err(ManifestError::Status {
            url: url.to_owned(),
            status: resp.status().as_u16(),
        });
    }
    let text = resp.text().await.map_err(|source| ManifestError::Fetch {
        url: url.to_owned(),
        source,
    })?;
    Manifest::parse(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn json(version: &str, sha: &str) -> String {
        format!(
            r#"{{"schema":1,"version":"{version}","targets":{{"linux-x64-musl":{{"url":"https://example.test/borg","sha256":"{sha}"}}}},
               "runner":{{"version":"2.337.0","sha256":{{"linux-x64":"{SHA}"}}}}}}"#
        )
    }

    #[test]
    fn parses_and_selects_targets_and_the_runner_pin() {
        let m = Manifest::parse(&json("0.5.0", SHA)).unwrap();
        assert_eq!(m.version, "0.5.0");
        assert_eq!(m.artifact_for("linux-x64-musl").unwrap().sha256, SHA);
        assert!(m.artifact_for("darwin-arm64").is_none());
        let pin = m.runner_pin_for("linux", "x86_64").unwrap();
        assert_eq!(
            (
                pin.version.as_str(),
                pin.platform.as_str(),
                pin.sha256.as_str()
            ),
            ("2.337.0", "linux-x64", SHA)
        );
        assert!(
            m.runner_pin_for("linux", "aarch64").is_none(),
            "no digest for that platform"
        );
        assert!(m.runner_pin_for("plan9", "mips").is_none());
    }

    #[test]
    fn rejects_malformed_manifests() {
        for bad in [
            json("0.5.0", "NOTHEX"),
            json("0.5.0", &SHA.to_uppercase()),
            json("../evil", SHA),
            json("", SHA),
            json("0.5.0", SHA).replace("\"schema\":1", "\"schema\":2"),
            json("0.5.0", SHA).replace("https://example.test/borg", "ftp://example.test/borg"),
            json("0.5.0", SHA).replace("\"targets\":{", "\"extra\":1,\"targets\":{"),
            r#"{"schema":1,"version":"1","targets":{}}"#.to_owned(),
        ] {
            assert!(Manifest::parse(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn host_targets() {
        assert_eq!(host_target("linux", "x86_64"), Some("linux-x64-musl"));
        assert_eq!(host_target("linux", "aarch64"), Some("linux-arm64-musl"));
        assert_eq!(host_target("macos", "aarch64"), Some("darwin-arm64"));
        assert_eq!(host_target("macos", "x86_64"), None);
    }

    #[tokio::test]
    async fn plain_http_needs_the_override() {
        assert!(matches!(
            fetch("http://example.test/m.json", false).await,
            Err(ManifestError::Insecure(_))
        ));
    }
}
