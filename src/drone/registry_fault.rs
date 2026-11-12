//! Cargo registry faults in a work unit's output, and their repair.
//!
//! A unit whose cargo cannot read or write the registry cache did not fail on
//! its own merits: a crate file the job user cannot open (a host wrapper that
//! pins `CARGO_HOME` to a root-owned toolchain directory), or one left
//! truncated or corrupt by an interrupted download. Reporting cargo's exit code
//! would hand the client a red verdict for an infrastructure fault, so the unit
//! runner asks [`faults`] what cargo complained about and repairs what lies in
//! the drone's own cargo cache (see [`evict`]); anything outside it ends the
//! unit as an infrastructure failure instead.

use std::path::{Component, Path, PathBuf};

use crate::drone::fsops::FsOps;

/// One registry entry cargo failed on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Fault {
    /// A file or directory under some `registry/cache` or `registry/src`.
    Path(PathBuf),
    /// A crate cargo could not unpack, verify or download.
    Crate { name: String, version: String },
}

impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Path(p) => write!(f, "{}", p.display()),
            Self::Crate { name, version } => write!(f, "{name} v{version}"),
        }
    }
}

/// The text between the first pair of backticks after `from` in `line`.
fn backticked(line: &str, from: usize) -> Option<&str> {
    let rest = line.get(from..)?;
    let start = rest.find('`')? + 1;
    let len = rest[start..].find('`')?;
    Some(&rest[start..start + len])
}

/// Crate names and versions are plain: letters, digits, `-`, `_`, `.`, `+`.
fn plain(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'+'))
}

/// The registry faults cargo reported in `text`: its own `error:` lines and
/// the indented `Caused by:` chain, nothing a test merely printed elsewhere.
pub(crate) fn faults(text: &str) -> Vec<Fault> {
    // Every cargo phrasing that names a registry path it could not use:
    // opening, reading or unpacking an entry, creating a cache directory
    // (a root-owned registry), or writing into one.
    const PATH_VERBS: [&str; 5] = [
        "failed to open `",
        "failed to read `",
        "failed to unpack `",
        "failed to create directory `",
        "failed to write `",
    ];
    const CRATE_VERBS: [&str; 3] = [
        "failed to unpack package `",
        "failed to verify the checksum of `",
        "failed to download `",
    ];
    let mut out: Vec<Fault> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim_start();
        let cargo_line =
            raw.starts_with("error: ") || (raw.starts_with("  ") && line.starts_with("failed to "));
        if !cargo_line {
            continue;
        }
        for verb in PATH_VERBS {
            if let Some(at) = line.find(verb)
                && let Some(p) = backticked(line, at)
                && p.starts_with('/')
                && (p.contains("/registry/cache/") || p.contains("/registry/src/"))
            {
                let fault = Fault::Path(PathBuf::from(p));
                if !out.contains(&fault) {
                    out.push(fault);
                }
            }
        }
        for verb in CRATE_VERBS {
            if let Some(at) = line.find(verb)
                && let Some(id) = backticked(line, at)
                && let Some((name, rest)) = id.split_once(" v")
            {
                let version = rest.split_whitespace().next().unwrap_or("");
                if plain(name) && plain(version) {
                    let fault = Fault::Crate {
                        name: name.to_owned(),
                        version: version.to_owned(),
                    };
                    if !out.contains(&fault) {
                        out.push(fault);
                    }
                }
            }
        }
    }
    out
}

/// Whether `path` lies inside `<cargo_home>/registry`, lexically (no `..`).
fn inside_registry(path: &Path, cargo_home: &Path) -> bool {
    path.components().all(|c| c != Component::ParentDir)
        && path.starts_with(cargo_home.join("registry"))
}

/// Faults in the drone's own cargo cache (repairable) and the paths of any
/// outside it (the host's business).
pub(crate) fn split<'a>(
    faults: &'a [Fault],
    cargo_home: &Path,
) -> (Vec<&'a Fault>, Vec<&'a Fault>) {
    faults.iter().partition(|f| match f {
        Fault::Path(p) => inside_registry(p, cargo_home),
        Fault::Crate { .. } => true,
    })
}

/// The cache entries to delete for `fault`: the `.crate` file and the source
/// it was unpacked into, in every registry index of `cargo_home`.
fn entries(fault: &Fault, cargo_home: &Path) -> Vec<PathBuf> {
    let registry = cargo_home.join("registry");
    let stem = match fault {
        Fault::Crate { name, version } => format!("{name}-{version}"),
        Fault::Path(p) => {
            // `<registry>/{cache,src}/<index>/<entry>[/...]`
            let Ok(rel) = p.strip_prefix(&registry) else {
                return Vec::new();
            };
            let mut parts = rel.components().skip(2);
            let Some(Component::Normal(entry)) = parts.next() else {
                return Vec::new();
            };
            let entry = entry.to_string_lossy();
            entry.strip_suffix(".crate").unwrap_or(&entry).to_owned()
        }
    };
    if !plain(&stem) {
        return Vec::new();
    }
    let mut out = Vec::new();
    let indexes = std::fs::read_dir(registry.join("cache"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name());
    for index in indexes {
        out.push(
            registry
                .join("cache")
                .join(&index)
                .join(format!("{stem}.crate")),
        );
        out.push(registry.join("src").join(&index).join(&stem));
    }
    out
}

/// Delete the cache entries of `faults` (as the job user, through `fs_ops`) so
/// cargo fetches and unpacks them again. Returns what was removed.
pub(crate) fn evict(fs_ops: &dyn FsOps, cargo_home: &Path, faults: &[&Fault]) -> Vec<PathBuf> {
    let mut removed = Vec::new();
    for fault in faults {
        for path in entries(fault, cargo_home) {
            if inside_registry(&path, cargo_home)
                && path.symlink_metadata().is_ok()
                && fs_ops.remove_all(&path).is_ok()
            {
                removed.push(path);
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drone::fsops::LocalFs;

    const PERMISSION: &str = "    Updating crates.io index
 Downloading crates ...
  Downloaded bit-set v0.8.0
error: failed to open `/opt/runner-toolchain/cargo/registry/cache/index.crates.io-1949cf8c6b5b557f/bit-set-0.8.0.crate`

Caused by:
  Permission denied (os error 13)
";

    #[test]
    fn cargo_registry_errors_are_found_and_test_output_is_not() {
        assert_eq!(
            faults(PERMISSION),
            [Fault::Path(PathBuf::from(
                "/opt/runner-toolchain/cargo/registry/cache/index.crates.io-1949cf8c6b5b557f/bit-set-0.8.0.crate"
            ))]
        );
        let corrupt = "error: failed to unpack package `serde v1.0.228`\n\nCaused by:\n  failed to open `/c/cargo/registry/src/idx/serde-1.0.228/.cargo-ok`\n";
        assert_eq!(
            faults(corrupt),
            [
                Fault::Crate {
                    name: "serde".into(),
                    version: "1.0.228".into()
                },
                Fault::Path(PathBuf::from(
                    "/c/cargo/registry/src/idx/serde-1.0.228/.cargo-ok"
                )),
            ]
        );
        // A root-owned toolchain registry cannot accept a new crate.
        let create = "error: failed to create directory `/opt/runner-toolchain/cargo/registry/cache/index.crates.io-1949cf8c6b5b557f`\n\nCaused by:\n  Permission denied (os error 13)\n";
        assert_eq!(
            faults(create),
            [Fault::Path(PathBuf::from(
                "/opt/runner-toolchain/cargo/registry/cache/index.crates.io-1949cf8c6b5b557f"
            ))]
        );
        assert_eq!(
            split(&faults(create), Path::new("/var/lib/borg-job/cache/cargo"))
                .1
                .len(),
            1,
            "outside the drone's cache: an infrastructure failure"
        );
        let checksum =
            "error: failed to verify the checksum of `bit-set v0.8.0 (registry `crates-io`)`\n";
        assert_eq!(
            faults(checksum),
            [Fault::Crate {
                name: "bit-set".into(),
                version: "0.8.0".into()
            }]
        );
        // A real failure, and a test that prints such text, are not faults.
        assert!(
            faults("error[E0308]: mismatched types\nerror: could not compile `x`\n").is_empty()
        );
        assert!(
            faults("test t ... printed: failed to open `/x/registry/cache/a/b.crate`\n").is_empty()
        );
        assert!(faults("error: failed to download `../../etc v1`\n").is_empty());
    }

    #[test]
    fn only_faults_inside_the_drones_cargo_cache_are_repairable() {
        let home = Path::new("/var/lib/borg-job/cache/cargo");
        let found = faults(PERMISSION);
        let (ours, foreign) = split(&found, home);
        assert!(ours.is_empty());
        assert_eq!(foreign.len(), 1);
        let inside = [
            Fault::Path(home.join("registry/cache/idx/bit-set-0.8.0.crate")),
            Fault::Crate {
                name: "x".into(),
                version: "1.0.0".into(),
            },
        ];
        assert_eq!(split(&inside, home).0.len(), 2);
        let escape = [Fault::Path(home.join("registry/../../../etc/passwd"))];
        assert_eq!(
            split(&escape, home).1.len(),
            1,
            "no way out of the registry"
        );
    }

    #[test]
    fn eviction_removes_the_crate_and_its_source_in_every_index_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("cargo");
        for index in ["idx-a", "idx-b"] {
            let cache = home.join("registry/cache").join(index);
            std::fs::create_dir_all(&cache).unwrap();
            std::fs::write(cache.join("bit-set-0.8.0.crate"), "x").unwrap();
            std::fs::write(cache.join("serde-1.0.0.crate"), "keep").unwrap();
            std::fs::create_dir_all(home.join("registry/src").join(index).join("bit-set-0.8.0"))
                .unwrap();
        }
        let fault = Fault::Path(home.join("registry/cache/idx-a/bit-set-0.8.0.crate"));
        let removed = evict(&LocalFs, &home, &[&fault]);
        assert_eq!(removed.len(), 4, "{removed:?}");
        for index in ["idx-a", "idx-b"] {
            assert!(
                !home
                    .join("registry/cache")
                    .join(index)
                    .join("bit-set-0.8.0.crate")
                    .exists()
            );
            assert!(
                !home
                    .join("registry/src")
                    .join(index)
                    .join("bit-set-0.8.0")
                    .exists()
            );
            assert!(
                home.join("registry/cache")
                    .join(index)
                    .join("serde-1.0.0.crate")
                    .exists()
            );
        }
        // A crate fault names the same entries.
        std::fs::write(home.join("registry/cache/idx-a/bit-set-0.8.0.crate"), "x").unwrap();
        let by_name = Fault::Crate {
            name: "bit-set".into(),
            version: "0.8.0".into(),
        };
        assert_eq!(evict(&LocalFs, &home, &[&by_name]).len(), 1);
    }
}
