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
    /// Supplementary groups jobs get instead of the account's own, set at
    /// drone start (never configured): with the per-lease Docker endpoint the
    /// `docker` group is left out, so a job reaches dockerd only through its
    /// endpoint.
    #[serde(skip)]
    pub groups: Option<Vec<u32>>,
}

fn default_priv() -> String {
    "setpriv".to_owned()
}

impl JobUser {
    /// `setpriv` arguments that become the job user, load its supplementary
    /// groups (for example `docker`) and drop every capability, ending in `--`.
    pub fn setpriv_args(&self) -> Vec<String> {
        let mut groups = match &self.groups {
            None => vec!["--init-groups".to_owned()],
            Some(g) if g.is_empty() => vec!["--clear-groups".to_owned()],
            Some(g) => vec![
                "--groups".to_owned(),
                g.iter().map(u32::to_string).collect::<Vec<_>>().join(","),
            ],
        };
        let mut args = vec![
            "--reuid".to_owned(),
            self.name.clone(),
            "--regid".to_owned(),
            self.name.clone(),
        ];
        args.append(&mut groups);
        args.extend([
            "--inh-caps=-all".to_owned(),
            "--ambient-caps=-all".to_owned(),
            "--bounding-set=-all".to_owned(),
            "--".to_owned(),
        ]);
        args
    }

    /// The account's supplementary groups without `docker`, from `id -G` and
    /// `getent group docker`; `None` when either cannot be read.
    pub fn groups_without_docker(&self, exec: &dyn Exec) -> Option<Vec<u32>> {
        let ids = exec
            .run("id", &["-G", &self.name])
            .ok()
            .filter(|o| o.success)?;
        let docker = exec
            .run("getent", &["group", "docker"])
            .ok()
            .filter(|o| o.success)
            .and_then(|o| {
                o.stdout
                    .split(':')
                    .nth(2)
                    .and_then(|g| g.trim().parse::<u32>().ok())
            });
        let primary = exec
            .run("id", &["-g", &self.name])
            .ok()
            .filter(|o| o.success)
            .and_then(|o| o.stdout.trim().parse::<u32>().ok())?;
        Some(
            ids.stdout
                .split_whitespace()
                .filter_map(|g| g.parse::<u32>().ok())
                .filter(|g| Some(*g) != docker && *g != primary)
                .collect(),
        )
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

impl JobExec {
    /// `setpriv ... -- program args...` arguments.
    fn argv(&self, program: &str, args: &[&str]) -> Vec<String> {
        let mut argv = self.user.setpriv_args();
        argv.push(program.to_owned());
        argv.extend(args.iter().map(|a| (*a).to_owned()));
        argv
    }
}

impl Exec for JobExec {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<Output> {
        let argv = self.argv(program, args);
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        self.inner.run(&self.user.priv_bin, &argv)
    }

    fn run_grouped(&self, program: &str, args: &[&str]) -> io::Result<Output> {
        let argv = self.argv(program, args);
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        self.inner.run_grouped(&self.user.priv_bin, &argv)
    }

    fn run_input(&self, program: &str, args: &[&str], input: &[u8]) -> io::Result<Output> {
        let argv = self.argv(program, args);
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        self.inner.run_input(&self.user.priv_bin, &argv, input)
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
            groups: None,
        }
    }

    #[test]
    fn the_docker_group_can_be_left_out() {
        let fake = FakeExec::default();
        fake.on("id -G borg-job", "1001 998 27\n");
        fake.on("id -g borg-job", "1001\n");
        fake.on("getent group docker", "docker:x:998:borg,borg-job\n");
        let groups = user().groups_without_docker(&fake).unwrap();
        assert_eq!(groups, [27]);
        let args = JobUser {
            groups: Some(groups),
            ..user()
        }
        .setpriv_args();
        assert!(args.windows(2).any(|w| w == ["--groups", "27"]), "{args:?}");
        assert!(!args.contains(&"--init-groups".to_owned()));
        let none = JobUser {
            groups: Some(Vec::new()),
            ..user()
        }
        .setpriv_args();
        assert!(none.contains(&"--clear-groups".to_owned()));
        assert!(user().setpriv_args().contains(&"--init-groups".to_owned()));
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
    fn grouped_commands_preserve_both_isolation_and_the_job_user_boundary() {
        struct GroupOnly;
        impl Exec for GroupOnly {
            fn run(&self, _program: &str, _args: &[&str]) -> io::Result<Output> {
                panic!("JobExec must forward the grouped boundary");
            }
            fn run_grouped(&self, program: &str, args: &[&str]) -> io::Result<Output> {
                assert_eq!(program, "setpriv");
                assert_eq!(
                    args,
                    [
                        "--reuid",
                        "borg-job",
                        "--regid",
                        "borg-job",
                        "--init-groups",
                        "--inh-caps=-all",
                        "--ambient-caps=-all",
                        "--bounding-set=-all",
                        "--",
                        "timeout",
                        "1s",
                        "sh"
                    ]
                );
                Ok(Output {
                    success: true,
                    stdout: String::new(),
                })
            }
        }
        let exec = JobExec::new(Arc::new(GroupOnly), user());
        assert!(exec.run_grouped("timeout", &["1s", "sh"]).unwrap().success);
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
