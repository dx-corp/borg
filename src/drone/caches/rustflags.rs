//! Stable path remapping for Rust builds.
//!
//! Every lease runs in `<work_root>/slot-<N>`, so a lease's absolute paths are
//! the same each time its slot is reused. The remap flags below make the paths
//! rustc embeds (debuginfo, panic locations, rlib contents) identical across
//! *all* slots and leases by mapping every slot directory, and the shared cargo
//! directories, onto fixed virtual prefixes. The flag string lists every slot,
//! so it is byte-identical for every lease; a per-slot flag would itself differ
//! between slots and change the compile-cache key.
//!
//! What this cannot do: sccache also hashes the compile's working directory and
//! every `CARGO_*` variable, including `CARGO_MANIFEST_DIR`, and rustc flags
//! cannot change those. Workspace crates therefore hit the compile cache when a
//! slot is reused, not across different slots.

use std::fs;
use std::io;
use std::path::Path;

/// Virtual prefix a slot directory is mapped to.
pub const VIRTUAL_WORK: &str = "/borg/work";
const VIRTUAL_CARGO: &str = "/borg/cargo";
const VIRTUAL_REGISTRY: &str = "/borg/registry";
const VIRTUAL_GIT: &str = "/borg/git";
const MARKER: &str = "# managed by the borg drone; edits are overwritten";

/// The `--remap-path-prefix` flags for `slots` slot directories and the cargo
/// home. Later flags take precedence in rustc, so the more specific registry
/// and git prefixes follow the cargo home.
pub fn remap_flags(work_root: &Path, slots: usize, cargo_home: &Path) -> Vec<String> {
    let mut flags: Vec<String> = (0..slots)
        .map(|n| {
            format!(
                "--remap-path-prefix={}/slot-{n}={VIRTUAL_WORK}",
                work_root.display()
            )
        })
        .collect();
    let home = cargo_home.display();
    flags.push(format!("--remap-path-prefix={home}={VIRTUAL_CARGO}"));
    flags.push(format!(
        "--remap-path-prefix={home}/registry={VIRTUAL_REGISTRY}"
    ));
    flags.push(format!("--remap-path-prefix={home}/git={VIRTUAL_GIT}"));
    flags
}

/// `RUSTFLAGS` splits on whitespace, so paths containing it cannot be passed.
pub fn flags_are_passable(flags: &[String]) -> bool {
    flags.iter().all(|f| !f.contains(char::is_whitespace))
}

/// Content of `$CARGO_HOME/config.toml`: the same flags as `[build] rustflags`.
/// Cargo ignores this when `RUSTFLAGS` is set, so it covers builds that run
/// without the lease environment.
pub fn config_toml(flags: &[String]) -> String {
    let items: Vec<String> = flags
        .iter()
        .map(|f| format!("  \"{}\",", f.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect();
    format!(
        "{MARKER}\n[build]\nrustflags = [\n{}\n]\n",
        items.join("\n")
    )
}

/// Write `config.toml` unless the file exists and is not ours. Returns whether
/// the file was written.
pub fn write_config(cargo_home: &Path, flags: &[String]) -> io::Result<bool> {
    let path = cargo_home.join("config.toml");
    let wanted = config_toml(flags);
    match fs::read_to_string(&path) {
        Ok(existing) if existing == wanted => return Ok(false),
        Ok(existing) if !existing.starts_with(MARKER) => {
            return Err(io::Error::other(format!(
                "{} exists and is not managed by borg; left alone",
                path.display()
            )));
        }
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    fs::create_dir_all(cargo_home)?;
    fs::write(&path, wanted)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_cover_every_slot_and_the_cargo_dirs_identically_for_all_leases() {
        let flags = remap_flags(
            Path::new("/var/lib/borg/work"),
            3,
            Path::new("/cache/cargo"),
        );
        assert_eq!(
            flags,
            [
                "--remap-path-prefix=/var/lib/borg/work/slot-0=/borg/work",
                "--remap-path-prefix=/var/lib/borg/work/slot-1=/borg/work",
                "--remap-path-prefix=/var/lib/borg/work/slot-2=/borg/work",
                "--remap-path-prefix=/cache/cargo=/borg/cargo",
                "--remap-path-prefix=/cache/cargo/registry=/borg/registry",
                "--remap-path-prefix=/cache/cargo/git=/borg/git",
            ]
        );
        assert!(flags_are_passable(&flags));
        assert!(!flags_are_passable(&remap_flags(
            Path::new("/with space"),
            1,
            Path::new("/c")
        )));
    }

    #[test]
    fn config_toml_is_valid_toml_with_the_flags() {
        let flags = remap_flags(Path::new("/w"), 2, Path::new("/c"));
        let text = config_toml(&flags);
        assert!(text.starts_with(MARKER) && text.contains("[build]\nrustflags = ["));
        for f in &flags {
            assert!(text.contains(&format!("\"{f}\"")), "{f}");
        }
    }

    #[test]
    fn write_is_idempotent_and_never_clobbers_a_foreign_config() {
        let dir = tempfile::tempdir().unwrap();
        let flags = remap_flags(Path::new("/w"), 1, dir.path());
        assert!(write_config(dir.path(), &flags).unwrap());
        assert!(!write_config(dir.path(), &flags).unwrap());
        let more = remap_flags(Path::new("/w"), 2, dir.path());
        assert!(
            write_config(dir.path(), &more).unwrap(),
            "our own file is updated"
        );

        fs::write(dir.path().join("config.toml"), "[net]\nretry = 5\n").unwrap();
        assert!(write_config(dir.path(), &flags).is_err());
        assert_eq!(
            fs::read_to_string(dir.path().join("config.toml")).unwrap(),
            "[net]\nretry = 5\n"
        );
    }
}
