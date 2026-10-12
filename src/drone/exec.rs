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

    /// Run in a private process group. Group-signaling supervisors must use
    /// this boundary; executors that cannot guarantee isolation fail closed.
    fn run_grouped(&self, _program: &str, _args: &[&str]) -> io::Result<Output> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "executor cannot guarantee a private process group",
        ))
    }
}

/// Runs commands on the local machine. Programs and arguments are chosen by
/// this crate (and the operator's config for the docker binary), never taken
/// from the queen.
pub struct SystemExec;

impl SystemExec {
    fn output(&self, program: &str, args: &[&str], grouped: bool) -> io::Result<Output> {
        #[cfg(not(unix))]
        if grouped {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "private process groups require Unix",
            ));
        }
        #[allow(
            clippy::disallowed_methods,
            reason = "drone host commands: fixed programs and arguments built by borg, never queen-supplied"
        )]
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        if grouped {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let out = command.output()?;
        Ok(Output {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        })
    }
}

impl Exec for SystemExec {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<Output> {
        self.output(program, args, false)
    }

    fn run_grouped(&self, program: &str, args: &[&str]) -> io::Result<Output> {
        self.output(program, args, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_executor_must_explicitly_opt_in_to_grouped_execution() {
        struct Ungrouped;
        impl Exec for Ungrouped {
            fn run(&self, _program: &str, _args: &[&str]) -> io::Result<Output> {
                panic!("unsupported grouped execution must not run in an ambient group");
            }
        }
        assert_eq!(
            Ungrouped
                .run_grouped("sh", &["-c", "true"])
                .unwrap_err()
                .kind(),
            io::ErrorKind::Unsupported
        );
    }

    #[test]
    #[cfg(unix)]
    fn system_grouped_execution_creates_a_group_led_by_its_child() {
        let output = SystemExec
            .run_grouped("sh", &["-c", "printf '%s ' \"$$\"; ps -o pgid= -p \"$$\""])
            .unwrap();
        assert!(output.success);
        let ids: Vec<u32> = output
            .stdout
            .split_whitespace()
            .map(|id| id.parse().unwrap())
            .collect();
        assert_eq!(ids.len(), 2);
        assert_eq!(
            ids[0], ids[1],
            "child PID must equal its private process group ID"
        );
    }
}
