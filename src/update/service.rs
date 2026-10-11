//! The service manager the update restarts the daemon through.
//!
//! Stopping is the drain: the unit sends `SIGTERM` to the daemon, which stops
//! taking leases and lets in-flight jobs finish (up to its drain timeout, which
//! the unit's stop timeout exceeds), and `stop` returns only once it has exited.

use crate::drone::exec::Exec;

pub trait ServiceManager {
    /// Stop the service, returning only after it has fully stopped.
    fn stop(&self) -> Result<(), String>;
    fn start(&self) -> Result<(), String>;
}

fn run(exec: &dyn Exec, program: &str, args: &[&str]) -> Result<(), String> {
    match exec.run(program, args) {
        Ok(out) if out.success => Ok(()),
        Ok(_) => Err(format!("`{program} {}` failed", args.join(" "))),
        Err(e) => Err(format!("{program}: {e}")),
    }
}

pub struct Systemd<'a> {
    pub unit: String,
    pub exec: &'a dyn Exec,
}

impl ServiceManager for Systemd<'_> {
    fn stop(&self) -> Result<(), String> {
        run(self.exec, "systemctl", &["stop", &self.unit])
    }

    fn start(&self) -> Result<(), String> {
        run(self.exec, "systemctl", &["start", &self.unit])
    }
}

pub struct Launchd<'a> {
    pub label: String,
    pub plist: String,
    pub exec: &'a dyn Exec,
}

impl ServiceManager for Launchd<'_> {
    fn stop(&self) -> Result<(), String> {
        run(
            self.exec,
            "launchctl",
            &["bootout", &format!("system/{}", self.label)],
        )
    }

    fn start(&self) -> Result<(), String> {
        run(
            self.exec,
            "launchctl",
            &["bootstrap", "system", &self.plist],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drone::cleanup::testing::FakeExec;

    #[test]
    fn systemd_and_launchd_issue_the_expected_commands() {
        let x = FakeExec::default();
        let s = Systemd {
            unit: "borg-drone.service".into(),
            exec: &x,
        };
        s.stop().unwrap();
        s.start().unwrap();
        let l = Launchd {
            label: "org.borg.drone".into(),
            plist: "/Library/LaunchDaemons/org.borg.drone.plist".into(),
            exec: &x,
        };
        l.stop().unwrap();
        l.start().unwrap();
        assert_eq!(
            x.calls(),
            [
                "systemctl stop borg-drone.service",
                "systemctl start borg-drone.service",
                "launchctl bootout system/org.borg.drone",
                "launchctl bootstrap system /Library/LaunchDaemons/org.borg.drone.plist",
            ]
        );
    }

    #[test]
    fn a_failing_stop_is_an_error() {
        let x = FakeExec::default();
        x.fail("systemctl stop u");
        assert!(
            Systemd {
                unit: "u".into(),
                exec: &x
            }
            .stop()
            .is_err()
        );
    }
}
