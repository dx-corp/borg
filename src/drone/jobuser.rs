//! Running work as a separate, unprivileged job user.
//!
//! The drone daemon holds the enrollment token and talks to the queen; a lease
//! runs code from pull requests. If both were one user, a job could read the
//! token and impersonate the drone. With a job user configured, every lease
//! (and every file operation on job-owned directories) runs as that user, which
//! has no access to the drone's config or token.
//!
//! The daemon changes user with `setpriv`, which needs `CAP_SETUID`,
//! `CAP_SETGID` and `CAP_SETPCAP`; the systemd unit grants exactly those as
//! ambient capabilities and `setpriv` drops them again before the job starts,
//! so the job holds no capability.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::drone::exec::{Exec, Output};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobUser {
    /// Account (and primary group) leases run as. Numeric ids are accepted.
    pub name: String,
    /// The job user's home directory, exported to jobs as `HOME`.
    pub home: PathBuf,
    /// The `setpriv` binary (util-linux).
    #[serde(default = "default_priv")]
    pub priv_bin: String,
}

fn default_priv() -> String {
    "setpriv".to_owned()
}

impl JobUser {
    /// `setpriv` arguments that become the job user, load its supplementary
    /// groups (for example `docker`) and drop every capability, ending in `--`.
    pub fn setpriv_args(&self) -> Vec<String> {
        vec![
            "--reuid".to_owned(),
            self.name.clone(),
            "--regid".to_owned(),
            self.name.clone(),
            "--init-groups".to_owned(),
            "--inh-caps=-all".to_owned(),
            "--ambient-caps=-all".to_owned(),
            "--bounding-set=-all".to_owned(),
            "--".to_owned(),
        ]
    }

    pub fn validate(&self) -> Result<(), String> {
        let name_ok = !self.name.is_empty()
            && self.name.len() <= 32
            && self
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
        if !name_ok {
            return Err(format!(
                "job_user.name {:?} must be a plain account name or numeric id",
                self.name
            ));
        }
        if !self.home.is_absolute() {
            return Err("job_user.home must be an absolute path".to_owned());
        }
        if self.priv_bin.is_empty() {
            return Err("job_user.priv_bin must not be empty".to_owned());
        }
        Ok(())
    }
}

/// An [`Exec`] that runs every command as the job user.
pub struct JobExec {
    inner: Arc<dyn Exec>,
    user: JobUser,
}

impl JobExec {
    pub fn new(inner: Arc<dyn Exec>, user: JobUser) -> Self {
        Self { inner, user }
    }
}

impl Exec for JobExec {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<Output> {
        let mut argv = self.user.setpriv_args();
        argv.push(program.to_owned());
        argv.extend(args.iter().map(|a| (*a).to_owned()));
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        self.inner.run(&self.user.priv_bin, &argv)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drone::cleanup::testing::FakeExec;

    fn user() -> JobUser {
        JobUser {
            name: "borg-job".into(),
            home: "/var/lib/borg-job".into(),
            priv_bin: "setpriv".into(),
        }
    }

    #[test]
    fn commands_become_setpriv_invocations_that_drop_capabilities() {
        let fake = Arc::new(FakeExec::default());
        let exec = JobExec::new(fake.clone(), user());
        exec.run("rm", &["-rf", "--", "/w/slot-0"]).unwrap();
        assert_eq!(
            fake.calls(),
            [
                "setpriv --reuid borg-job --regid borg-job --init-groups --inh-caps=-all --ambient-caps=-all --bounding-set=-all -- rm -rf -- /w/slot-0"
            ]
        );
    }

    #[test]
    fn validation_rejects_bad_names_and_relative_homes() {
        assert!(user().validate().is_ok());
        for bad in [
            JobUser {
                name: String::new(),
                ..user()
            },
            JobUser {
                name: "a b".into(),
                ..user()
            },
            JobUser {
                name: "x;rm".into(),
                ..user()
            },
            JobUser {
                home: "relative".into(),
                ..user()
            },
            JobUser {
                priv_bin: String::new(),
                ..user()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
        assert!(
            JobUser {
                name: "65534".into(),
                ..user()
            }
            .validate()
            .is_ok()
        );
    }
}
