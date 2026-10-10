//! The pinned actions-runner release and its checksums.
//!
//! The values below are the SHA-256 digests published in the
//! `actions/runner` v2.337.0 release notes and were re-computed from the
//! downloaded tarballs. To update, change the version and every digest
//! together.

pub const RUNNER_VERSION: &str = "2.337.0";

/// One downloadable runner tarball.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerPin {
    pub version: String,
    /// Platform part of the asset name, e.g. `linux-x64`.
    pub platform: String,
    pub sha256: String,
}

const TABLE: [(&str, &str, &str, &str); 4] = [
    (
        "linux",
        "x86_64",
        "linux-x64",
        "70920811a4f8ad4328818682bca5c6469c1c942fab52448868071d0063816613",
    ),
    (
        "linux",
        "aarch64",
        "linux-arm64",
        "9b1dc70626422526e3c94767cf024896beb15da5342a3f4819bf2feac13e0393",
    ),
    (
        "macos",
        "x86_64",
        "osx-x64",
        "d383f505d7ed041b1873ab68c35dd766fc093f2252330f95bb427be8f2c6dcfc",
    ),
    (
        "macos",
        "aarch64",
        "osx-arm64",
        "5a2cd92908a93d7276a194e1de6008099f3e7946f3f8e14aa7a1a7b4a31fdec2",
    ),
];

/// The pin for a host, by `std::env::consts` OS and architecture.
pub fn pin_for(os: &str, arch: &str) -> Option<RunnerPin> {
    TABLE
        .iter()
        .find(|(o, a, _, _)| *o == os && *a == arch)
        .map(|(_, _, platform, sha)| RunnerPin {
            version: RUNNER_VERSION.to_owned(),
            platform: (*platform).to_owned(),
            sha256: (*sha).to_owned(),
        })
}

impl RunnerPin {
    pub fn filename(&self) -> String {
        format!("actions-runner-{}-{}.tar.gz", self.platform, self.version)
    }

    pub fn url(&self) -> String {
        format!(
            "https://github.com/actions/runner/releases/download/v{}/{}",
            self.version,
            self.filename()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_supported_host_has_a_well_formed_pin() {
        for (os, arch) in [
            ("linux", "x86_64"),
            ("linux", "aarch64"),
            ("macos", "x86_64"),
            ("macos", "aarch64"),
        ] {
            let pin = pin_for(os, arch).unwrap();
            assert_eq!(pin.sha256.len(), 64);
            assert!(
                pin.sha256
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            );
        }
        assert!(pin_for("windows", "x86_64").is_none());
    }

    #[test]
    fn url_matches_the_release_asset_name() {
        let pin = pin_for("linux", "x86_64").unwrap();
        assert_eq!(
            pin.url(),
            "https://github.com/actions/runner/releases/download/v2.337.0/actions-runner-linux-x64-2.337.0.tar.gz"
        );
    }
}
