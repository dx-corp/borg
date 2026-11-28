//! The drone's single process boundary for short-lived host commands
//! (docker, systemctl, tar, curl, chown ...), behind a trait so cleanup and
//! install logic is tested without touching the machine.

use std::io;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Result of a finished command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub success: bool,
    pub stdout: String,
}

/// A finished command with what explains a failure: its exit code and its
/// standard error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnosed {
    pub output: Output,
    /// The exit code; `None` when a signal ended it or the executor does not
    /// report one.
    pub code: Option<i32>,
    /// Standard error, trimmed; empty when the executor does not capture it.
    pub stderr: String,
}

pub trait Exec: Send + Sync {
    /// Run `program` with `args` to completion. `Err` means it could not be
    /// started; a non-zero exit is `Ok` with `success == false`.
    fn run(&self, program: &str, args: &[&str]) -> io::Result<Output>;

    /// Like [`Exec::run`], also keeping the exit code and standard error, for
    /// a probe whose failure must explain itself. An executor that captures
    /// neither reports them empty.
    fn run_diagnosed(&self, program: &str, args: &[&str]) -> io::Result<Diagnosed> {
        self.run(program, args).map(|output| Diagnosed {
            output,
            code: None,
            stderr: String::new(),
        })
    }

    /// Run in a private process group. Group-signaling supervisors must use
    /// this boundary; executors that cannot guarantee isolation fail closed.
    fn run_grouped(&self, _program: &str, _args: &[&str]) -> io::Result<Output> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "executor cannot guarantee a private process group",
        ))
    }

    /// Like [`Exec::run`], but the command and every process it starts run
    /// in a private process group that gets SIGTERM at `limit` and SIGKILL
    /// [`KILL_GRACE`] later; then `Err` with [`io::ErrorKind::TimedOut`].
    /// Standard output is not kept. Executors without process groups run it
    /// to completion, as [`Exec::run`] does.
    fn run_timed(&self, program: &str, args: &[&str], limit: Duration) -> io::Result<Output> {
        let _ = limit;
        self.run(program, args)
    }

    /// Run to completion with `input` on standard input, so binary data
    /// reaches a command running as another user without a temporary file
    /// anyone else could read. Executors that cannot feed input fail closed.
    fn run_input(&self, _program: &str, _args: &[&str], _input: &[u8]) -> io::Result<Output> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "executor cannot feed standard input",
        ))
    }
}

/// Runs commands on the local machine. Programs and arguments are chosen by
/// this crate (and the operator's config for the docker binary), never taken
/// from the queen.
pub struct SystemExec;

/// How long [`Exec::run_timed`] waits after SIGTERM before SIGKILL.
pub const KILL_GRACE: Duration = Duration::from_secs(10);

impl SystemExec {
    #[cfg(all(test, target_os = "linux"))]
    pub(super) fn spawn_reclaim_sleep(
        program: &std::path::Path,
        cwd: &std::path::Path,
    ) -> io::Result<std::process::Child> {
        #[allow(
            clippy::disallowed_methods,
            reason = "reclaim test: run the system sleep executable or its fixture copy with a fixed duration and fixture cwd"
        )]
        let mut command = Command::new(program);
        command.arg("30").current_dir(cwd).spawn()
    }

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

    fn run_diagnosed(&self, program: &str, args: &[&str]) -> io::Result<Diagnosed> {
        #[allow(
            clippy::disallowed_methods,
            reason = "drone host commands: fixed programs and arguments built by borg, never queen-supplied"
        )]
        let mut command = Command::new(program);
        let out = command
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .output()?;
        Ok(Diagnosed {
            output: Output {
                success: out.status.success(),
                stdout: String::from_utf8_lossy(&out.stdout).trim().to_owned(),
            },
            code: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        })
    }

    fn run_grouped(&self, program: &str, args: &[&str]) -> io::Result<Output> {
        self.output(program, args, true)
    }

    #[cfg(unix)]
    fn run_timed(&self, program: &str, args: &[&str], limit: Duration) -> io::Result<Output> {
        use std::os::unix::process::CommandExt;
        #[allow(
            clippy::disallowed_methods,
            reason = "drone host commands: fixed programs and arguments built by borg, never queen-supplied"
        )]
        let mut command = Command::new(program);
        let mut child = command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()?;
        let group = format!("-{}", child.id());
        let wait_until = |child: &mut std::process::Child, until: Instant| loop {
            match child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) if Instant::now() >= until => return None,
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(_) => return None,
            }
        };
        if let Some(status) = wait_until(&mut child, Instant::now() + limit) {
            return Ok(Output {
                success: status.success(),
                stdout: String::new(),
            });
        }
        // The whole group: git fetch runs index-pack and remote helpers as
        // children, and those hold the mirror's pack files.
        let _ = self.run("kill", &["-TERM", "--", &group]);
        if wait_until(&mut child, Instant::now() + KILL_GRACE).is_none() {
            let _ = self.run("kill", &["-KILL", "--", &group]);
            let _ = child.wait();
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("{program} killed after {}s", limit.as_secs()),
        ))
    }

    fn run_input(&self, program: &str, args: &[&str], input: &[u8]) -> io::Result<Output> {
        use std::io::Write;
        #[allow(
            clippy::disallowed_methods,
            reason = "drone host commands: fixed programs and arguments built by borg, never queen-supplied"
        )]
        let mut command = Command::new(program);
        let mut child = command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("child has no standard input"))?;
        // Feed input from another thread: a child that writes its output
        // before reading all of its input must not deadlock against us.
        let out = std::thread::scope(|s| {
            let writer = s.spawn(move || stdin.write_all(input));
            let out = child.wait_with_output();
            // A child that exits without reading everything closes the pipe;
            // its exit status, not the broken pipe, is the verdict.
            let _ = writer.join();
            out
        })?;
        Ok(Output {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_timed_command_and_its_children_are_killed_at_the_limit() {
        // git fetch runs index-pack as a child: the whole group must go.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("child-alive");
        let script = format!("(sleep 3; touch {}) & sleep 30", marker.display());
        let started = std::time::Instant::now();
        let err = SystemExec
            .run_timed("sh", &["-c", &script], Duration::from_millis(300))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < KILL_GRACE,
            "SIGTERM ended it: {:?}",
            started.elapsed()
        );
        std::thread::sleep(Duration::from_secs(4));
        assert!(!marker.exists(), "the child in the group was killed too");
    }

    #[cfg(unix)]
    #[test]
    fn a_timed_command_that_finishes_reports_its_status() {
        assert!(
            SystemExec
                .run_timed("true", &[], Duration::from_secs(5))
                .unwrap()
                .success
        );
        assert!(
            !SystemExec
                .run_timed("false", &[], Duration::from_secs(5))
                .unwrap()
                .success
        );
    }

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
    fn input_reaches_the_command_and_executors_without_it_fail_closed() {
        // Larger than a pipe buffer, with every byte value, so a blocking
        // writer or a lossy conversion would show.
        let input: Vec<u8> = (0..=255u8).cycle().take(1 << 20).collect();
        let out = SystemExec.run_input("wc", &["-c"], &input).unwrap();
        assert!(out.success);
        assert_eq!(out.stdout.trim(), (1usize << 20).to_string());
        // A command that never reads its input still reports its own status.
        assert!(!SystemExec.run_input("false", &[], &input).unwrap().success);

        struct NoInput;
        impl Exec for NoInput {
            fn run(&self, _program: &str, _args: &[&str]) -> io::Result<Output> {
                panic!("input must not be dropped silently");
            }
        }
        assert_eq!(
            NoInput.run_input("cat", &[], b"x").unwrap_err().kind(),
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
