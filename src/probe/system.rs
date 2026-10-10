//! The real [`Host`]: reads the local filesystem and runs read-only commands.

use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::Host;

/// Longest any probe command may run. `docker info` can hang on a wedged
/// daemon, and a probe must never block the heartbeat.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// The machine borg is running on.
pub struct SystemHost;

impl Host for SystemHost {
    fn read_to_string(&self, path: &Path) -> Option<String> {
        fs::read_to_string(path).ok()
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn run(&self, program: &str, args: &[&str]) -> Option<String> {
        run_bounded(program, args)
    }

    fn os(&self) -> &'static str {
        std::env::consts::OS
    }

    fn arch(&self) -> &'static str {
        std::env::consts::ARCH
    }

    fn available_parallelism(&self) -> Option<u32> {
        std::thread::available_parallelism()
            .ok()
            .and_then(|n| u32::try_from(n.get()).ok())
    }
}

/// Run a read-only probe command and return trimmed stdout when it exits 0
/// within [`COMMAND_TIMEOUT`]. Stdin is closed and stderr discarded.
fn run_bounded(program: &str, args: &[&str]) -> Option<String> {
    #[allow(
        clippy::disallowed_methods,
        reason = "borg probe runs fixed read-only host-inspection commands (git, docker, mise, df, sysctl, hostname); the program name is chosen by this crate, never by remote input"
    )]
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let deadline = Instant::now() + COMMAND_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL_INTERVAL),
            Ok(None) | Err(_) => {
                // Best effort: the child is ours, so kill and reap it.
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    Some(out.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_program_is_none() {
        assert_eq!(run_bounded("borg-no-such-program-4f1a", &[]), None);
    }

    #[cfg(unix)]
    #[test]
    fn captures_stdout_of_successful_command() {
        assert_eq!(run_bounded("echo", &["hi"]).as_deref(), Some("hi"));
    }

    #[cfg(unix)]
    #[test]
    fn failing_command_is_none() {
        assert_eq!(run_bounded("false", &[]), None);
    }
}
