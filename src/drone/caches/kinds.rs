//! The cache kinds the drone manages: where each lives under `cache_root`, the
//! environment that points jobs at it, and which parts may be garbage
//! collected (and in what units).
//!
//! Concurrency, by kind. Leases on one host run at the same time and share
//! these directories:
//! * sccache, cargo, go, uv, pnpm: documented as safe for concurrent use.
//! * bazel: the disk cache is safe for concurrent Bazel processes; its source
//!   (`DiskCacheClient`) writes entries to a temporary file and renames them
//!   into place, and states that concurrent processes "can safely retrieve and
//!   store entries in a shared disk cache, even when they collide". The
//!   repository cache is content addressed and shared across workspaces.
//! * mise: installs take a per-tool-version lock file (`mise` source,
//!   `tool_version_lock`); read from the source, not stress tested here.

use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Sccache,
    Cargo,
    Bazel,
    Pnpm,
    Go,
    Uv,
    Mise,
}

const GIB: u64 = 1 << 30;

impl Kind {
    pub const ALL: [Kind; 7] = [
        Kind::Sccache,
        Kind::Cargo,
        Kind::Bazel,
        Kind::Pnpm,
        Kind::Go,
        Kind::Uv,
        Kind::Mise,
    ];

    /// Config key, stats name and directory name under `cache_root`.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Sccache => "sccache",
            Kind::Cargo => "cargo",
            Kind::Bazel => "bazel",
            Kind::Pnpm => "pnpm",
            Kind::Go => "go",
            Kind::Uv => "uv",
            Kind::Mise => "mise",
        }
    }

    pub fn dir(self, cache_root: &Path) -> PathBuf {
        cache_root.join(self.name())
    }

    /// Default size cap. sccache is absent: it enforces `SCCACHE_CACHE_SIZE`
    /// itself, so the drone never deletes from its directory.
    pub fn default_max_bytes(self) -> Option<u64> {
        match self {
            Kind::Sccache => None,
            Kind::Bazel => Some(50 * GIB),
            Kind::Uv => Some(10 * GIB),
            Kind::Cargo | Kind::Pnpm | Kind::Go | Kind::Mise => Some(20 * GIB),
        }
    }

    /// Directories to create up front.
    pub fn subdirs(self) -> &'static [&'static str] {
        match self {
            Kind::Bazel => &["disk", "repo", "bazelisk"],
            Kind::Go => &["mod", "build"],
            Kind::Cargo => &["registry", "git"],
            _ => &[""],
        }
    }

    /// The environment that points a lease at this cache. `lease_dir` is the
    /// lease's private directory, for anything that must not be shared.
    pub fn env(
        self,
        cache_root: &Path,
        lease_dir: &Path,
        sccache_size: &str,
    ) -> Vec<(String, String)> {
        let dir = self.dir(cache_root);
        let path = |p: PathBuf| p.to_string_lossy().into_owned();
        let pair = |k: &str, v: String| (k.to_owned(), v);
        match self {
            Kind::Sccache => vec![
                pair("SCCACHE_DIR", path(dir)),
                pair("SCCACHE_CACHE_SIZE", sccache_size.to_owned()),
            ],
            // Registry and git data are shared. `cargo install` writes to
            // `$CARGO_HOME/bin` unless told otherwise, so its root is per
            // lease and no installed binary leaks between leases.
            Kind::Cargo => vec![
                pair("CARGO_HOME", path(dir)),
                pair("CARGO_INSTALL_ROOT", path(lease_dir.join("cargo-install"))),
            ],
            Kind::Bazel => vec![
                pair("BAZELISK_HOME", path(dir.join("bazelisk"))),
                pair("BORG_BAZEL_DISK_CACHE", path(dir.join("disk"))),
                pair("BORG_BAZEL_REPOSITORY_CACHE", path(dir.join("repo"))),
            ],
            Kind::Pnpm => vec![
                pair("PNPM_STORE_DIR", path(dir.clone())),
                pair("npm_config_store_dir", path(dir)),
            ],
            Kind::Go => vec![
                pair("GOMODCACHE", path(dir.join("mod"))),
                pair("GOCACHE", path(dir.join("build"))),
            ],
            Kind::Uv => vec![pair("UV_CACHE_DIR", path(dir))],
            Kind::Mise => vec![pair("MISE_DATA_DIR", path(dir))],
        }
    }

    /// Where garbage collection may delete: `(bucket, depth)` pairs whose
    /// entries `depth` levels down are the eviction units. Everything not
    /// listed (indexes, shims, module caches, sccache) is never deleted.
    pub fn gc_buckets(self, cache_root: &Path) -> Vec<(PathBuf, usize)> {
        let dir = self.dir(cache_root);
        let children = |p: &Path| -> Vec<PathBuf> {
            fs::read_dir(p)
                .map(|rd| rd.flatten().map(|e| e.path()).collect())
                .unwrap_or_default()
        };
        match self {
            Kind::Sccache => Vec::new(),
            // Downloaded crates, unpacked sources, git databases and checkouts.
            Kind::Cargo => {
                let mut b: Vec<(PathBuf, usize)> = Vec::new();
                for sub in ["registry/cache", "registry/src"] {
                    b.extend(children(&dir.join(sub)).into_iter().map(|p| (p, 1)));
                }
                b.push((dir.join("git/db"), 1));
                b.push((dir.join("git/checkouts"), 1));
                b
            }
            // Content-addressed blobs: action results, outputs, downloads.
            Kind::Bazel => vec![
                (dir.join("disk/cas"), 2),
                (dir.join("disk/ac"), 2),
                (dir.join("repo/content_addressable"), 2),
            ],
            // `<store>/<version>/files/<xx>/<hash>`; the index is kept.
            Kind::Pnpm => children(&dir)
                .into_iter()
                .map(|v| (v.join("files"), 2))
                .collect(),
            // The build cache only: module directories are read-only trees
            // that other builds may be reading, so they are left alone.
            Kind::Go => vec![(dir.join("build"), 2)],
            // Entries of each versioned bucket (`wheels-v5`, `archive-v0`...).
            Kind::Uv => children(&dir)
                .into_iter()
                .filter(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().contains("-v"))
                })
                .map(|p| (p, 1))
                .collect(),
            // Installed tool versions and downloads, never shims or config.
            Kind::Mise => {
                let mut b: Vec<(PathBuf, usize)> = children(&dir.join("installs"))
                    .into_iter()
                    .map(|t| (t, 1))
                    .collect();
                b.push((dir.join("downloads"), 2));
                b
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_points_every_kind_at_a_stable_path_under_the_cache_root() {
        let root = Path::new("/cache");
        let lease = Path::new("/work/abc");
        let all: Vec<(String, String)> = Kind::ALL
            .iter()
            .flat_map(|k| k.env(root, lease, "9G"))
            .collect();
        let get = |k: &str| all.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("SCCACHE_DIR"), Some("/cache/sccache"));
        assert_eq!(get("SCCACHE_CACHE_SIZE"), Some("9G"));
        assert_eq!(get("CARGO_HOME"), Some("/cache/cargo"));
        assert_eq!(get("BAZELISK_HOME"), Some("/cache/bazel/bazelisk"));
        assert_eq!(get("PNPM_STORE_DIR"), Some("/cache/pnpm"));
        assert_eq!(get("npm_config_store_dir"), Some("/cache/pnpm"));
        assert_eq!(get("GOMODCACHE"), Some("/cache/go/mod"));
        assert_eq!(get("GOCACHE"), Some("/cache/go/build"));
        assert_eq!(get("UV_CACHE_DIR"), Some("/cache/uv"));
        assert_eq!(get("MISE_DATA_DIR"), Some("/cache/mise"));
    }

    #[test]
    fn cargo_install_root_is_per_lease_so_bin_is_never_shared() {
        let a = Kind::Cargo.env(Path::new("/c"), Path::new("/work/a"), "1G");
        let b = Kind::Cargo.env(Path::new("/c"), Path::new("/work/b"), "1G");
        let root = |env: &[(String, String)]| {
            env.iter()
                .find(|(k, _)| k == "CARGO_INSTALL_ROOT")
                .unwrap()
                .1
                .clone()
        };
        assert_ne!(root(&a), root(&b));
        assert!(root(&a).starts_with("/work/a/"));
        assert_eq!(a[0], b[0], "CARGO_HOME itself is the same for every lease");
    }

    #[test]
    fn only_sccache_has_no_default_cap_and_no_gc_buckets() {
        assert_eq!(Kind::Sccache.default_max_bytes(), None);
        assert!(Kind::Sccache.gc_buckets(Path::new("/c")).is_empty());
        for k in Kind::ALL.into_iter().filter(|k| *k != Kind::Sccache) {
            assert!(k.default_max_bytes().is_some(), "{k:?}");
        }
    }

    #[test]
    fn go_module_cache_and_bazelisk_are_never_gc_buckets() {
        let go = Kind::Go.gc_buckets(Path::new("/c"));
        assert_eq!(go, [(PathBuf::from("/c/go/build"), 2)]);
        let bazel = Kind::Bazel.gc_buckets(Path::new("/c"));
        assert!(
            bazel
                .iter()
                .all(|(p, _)| !p.starts_with("/c/bazel/bazelisk"))
        );
    }
}
