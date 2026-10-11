//! Installing the pinned actions-runner from a manifest pin: download,
//! verify the SHA-256, unpack, swap the directory, hand it to the job user and
//! make it read-only (leases hard-link its files).
//!
//! Leases clone the runner directory when they start, so replacing it while
//! jobs run is safe: running leases keep their own links.

use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::drone::exec::Exec;
use crate::drone::pins::RunnerPin;

#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    #[error("`{0}` failed")]
    Command(String),
    #[error("checksum mismatch for {file}: expected {expected}, got {actual}")]
    Checksum {
        file: String,
        expected: String,
        actual: String,
    },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> RunnerError + '_ {
    move |source| RunnerError::Io {
        path: path.to_owned(),
        source,
    }
}

fn must(exec: &dyn Exec, program: &str, args: &[&str]) -> Result<(), RunnerError> {
    match exec.run(program, args) {
        Ok(o) if o.success => Ok(()),
        _ => Err(RunnerError::Command(format!(
            "{program} {}",
            args.join(" ")
        ))),
    }
}

/// What the pin installs, as recorded in `<runner>/.borg-runner-version`.
pub fn marker(pin: &RunnerPin) -> String {
    format!("{} {}\n", pin.version, pin.sha256)
}

/// Whether `runner_dir` already holds exactly this pin.
pub fn is_installed(runner_dir: &Path, pin: &RunnerPin) -> bool {
    fs::read_to_string(runner_dir.join(".borg-runner-version")).is_ok_and(|m| m == marker(pin))
}

/// Install `pin` into `runner_dir`, owned by `owner` (when given).
pub fn install(
    exec: &dyn Exec,
    pin: &RunnerPin,
    runner_dir: &Path,
    owner: Option<&str>,
) -> Result<(), RunnerError> {
    let parent = runner_dir.parent().unwrap_or(Path::new("."));
    let staging = parent.join("runner.new");
    let tarball = parent.join(pin.filename());
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(io(&staging))?;

    must(
        exec,
        "curl",
        &["-fsSL", "-o", &tarball.to_string_lossy(), &pin.url()],
    )?;
    let actual = hex::encode(Sha256::digest(fs::read(&tarball).map_err(io(&tarball))?));
    if actual != pin.sha256 {
        let _ = fs::remove_file(&tarball);
        let _ = fs::remove_dir_all(&staging);
        return Err(RunnerError::Checksum {
            file: pin.filename(),
            expected: pin.sha256.clone(),
            actual,
        });
    }
    must(
        exec,
        "tar",
        &[
            "-xzf",
            &tarball.to_string_lossy(),
            "-C",
            &staging.to_string_lossy(),
        ],
    )?;
    let _ = fs::remove_file(&tarball);
    fs::write(staging.join(".borg-runner-version"), marker(pin)).map_err(io(&staging))?;

    if runner_dir.exists() {
        // The installed tree is read-only; restore write access to delete it.
        must(exec, "chmod", &["-R", "u+w", &runner_dir.to_string_lossy()])?;
        fs::remove_dir_all(runner_dir).map_err(io(runner_dir))?;
    }
    fs::rename(&staging, runner_dir).map_err(io(runner_dir))?;
    if let Some(owner) = owner {
        must(
            exec,
            "chown",
            &[
                "-R",
                &format!("{owner}:{owner}"),
                &runner_dir.to_string_lossy(),
            ],
        )?;
    }
    must(exec, "chmod", &["-R", "a-w", &runner_dir.to_string_lossy()])
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;
    use crate::drone::cleanup::testing::FakeExec;
    use crate::drone::exec::Output;

    const BODY: &[u8] = b"fake runner tarball";

    struct Hooked(FakeExec);
    impl Exec for Hooked {
        fn run(&self, program: &str, args: &[&str]) -> io::Result<Output> {
            match program {
                "curl" => fs::write(args[2], BODY)?,
                "tar" => fs::write(Path::new(args[3]).join("run.sh"), "#!/bin/sh\n")?,
                _ => {}
            }
            self.0.run(program, args)
        }
    }

    fn pin(sha: &str, version: &str) -> RunnerPin {
        RunnerPin {
            version: version.into(),
            platform: "linux-x64".into(),
            sha256: sha.into(),
        }
    }

    #[test]
    fn installs_verified_runners_and_is_idempotent_by_marker() {
        let dir = tempfile::tempdir().unwrap();
        let runner = dir.path().join("runner");
        let p = pin(&hex::encode(Sha256::digest(BODY)), "2.337.0");
        assert!(!is_installed(&runner, &p));
        let x = Hooked(FakeExec::default());
        install(&x, &p, &runner, Some("borg-job")).unwrap();
        assert!(is_installed(&runner, &p));
        assert!(runner.join("run.sh").exists());
        let calls = x.0.calls();
        assert!(
            calls
                .iter()
                .any(|c| c.starts_with("chown -R borg-job:borg-job")),
            "{calls:?}"
        );
        assert!(calls.last().unwrap().starts_with("chmod -R a-w"));

        // A newer pin replaces it.
        let newer = pin(&p.sha256, "2.338.0");
        install(&x, &newer, &runner, None).unwrap();
        assert!(is_installed(&runner, &newer) && !is_installed(&runner, &p));
    }

    #[test]
    fn a_bad_checksum_installs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let runner = dir.path().join("runner");
        let x = Hooked(FakeExec::default());
        let err = install(&x, &pin(&"0".repeat(64), "2.337.0"), &runner, None).unwrap_err();
        assert!(matches!(err, RunnerError::Checksum { .. }), "{err}");
        assert!(!runner.exists() && !dir.path().join("runner.new").exists());
    }
}
