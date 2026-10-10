//! The drone's single process boundary for short-lived host commands
//! (docker, systemctl, tar, curl, chown ...), behind a trait so cleanup and
//! install logic is tested without touching the machine.

use std::io;
use std::process::{Command, Stdio};

/// Result of a finished command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub success: bool,
    pub stdout: String,
}

pub trait Exec: Send + Sync {
    /// Run `program` with `args` to completion. `Err` means it could not be
    /// started; a non-zero exit is `Ok` with `success == false`.
    fn run(&self, program: &str, args: &[&str]) -> io::Result<Output>;
}

/// Runs commands on the local machine. Programs and arguments are chosen by
/// this crate (and the operator's config for the docker binary), never taken
/// from the queen.
pub struct SystemExec;

impl Exec for SystemExec {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<Output> {
        #[allow(
            clippy::disallowed_methods,
            reason = "drone host commands: fixed programs and arguments built by borg, never queen-supplied"
        )]
        let out = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()?;
        Ok(Output {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        })
    }
}
