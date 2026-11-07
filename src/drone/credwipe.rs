//! Job-credential wipe: removes the credential files CI jobs write into a
//! home directory (`~/.netrc`, `~/.docker/config.json`, `~/.kube`,
//! `~/.config/gcloud`, ...) once no job is using that home, keeping every
//! cache.
//!
//! Drone leases already run with a private `HOME` inside the lease directory
//! (removed after every job), so the drone's own job user's home should
//! never hold any; it is wiped anyway, between leases, in case a job wrote
//! through an absolute path. The class that did persist is on hosts where
//! static GitHub runners (`actions.runner.*` systemd units) share the
//! machine with a drone: on 2026-11-01 all eight static runner homes on
//! internal-runner-burst-01 (deploy trust) held `.netrc`, `.docker/config.json`,
//! `.kube/config`, `.config/gcloud/{credentials,access_tokens}.db` and
//! `.terraform.d` written by earlier jobs. Those homes are wiped when their
//! runner has no job (`Runner.Worker` not running for that user) and the
//! files have not been touched for `min_idle_secs`.
//!
//! Every removal runs as the owner of the home (never as the owner of the
//! path, which a job could have made a symlink), so it can never delete
//! anything the job user itself could not. A root-owned home is skipped.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::drone::reclaim::Commands;

/// Credential files and directories CI jobs write into `$HOME`, relative to
/// it. Caches (`.cache`, `.cargo/registry`, `.rustup`, `.npm`, the Sigstore
/// TUF root, ...) and the runner's own registration (`actions-runner/`) are
/// not on the list.
const CREDENTIAL_PATHS: &[&str] = &[
    ".netrc",
    ".git-credentials",
    ".docker/config.json",
    ".kube",
    ".config/gcloud",
    ".terraform.d/credentials.tfrc.json",
    ".npmrc",
    ".pypirc",
    ".aws",
    ".azure",
    ".config/gh",
    ".config/helm/registry",
    ".config/containers/auth.json",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".vault-token",
];

/// The `credential_wipe` block of the drone config. On by default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CredentialWipeSettings {
    enabled: bool,
    /// Also wipe the homes of static GitHub runners on this host.
    static_runner_homes: bool,
    /// More home-relative paths to treat as credentials.
    extra_paths: Vec<String>,
    /// Static runner homes: leave files touched more recently than this (a
    /// job may be starting).
    min_idle_secs: u64,
    /// Seconds between sweeps of idle homes.
    interval_secs: u64,
}

impl Default for CredentialWipeSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            static_runner_homes: true,
            extra_paths: Vec::new(),
            min_idle_secs: 600,
            interval_secs: 300,
        }
    }
}

impl CredentialWipeSettings {
    pub(crate) fn validate(&self) -> Result<(), String> {
        for p in &self.extra_paths {
            relative(p).ok_or_else(|| {
                format!("credential_wipe.extra_paths: {p:?} must be a plain relative path")
            })?;
        }
        if self.interval_secs == 0 {
            return Err("credential_wipe.interval_secs must be positive".into());
        }
        Ok(())
    }

    /// Every path to wipe under `home`.
    fn paths(&self, home: &Path) -> Vec<PathBuf> {
        CREDENTIAL_PATHS
            .iter()
            .copied()
            .chain(self.extra_paths.iter().map(String::as_str))
            .filter_map(relative)
            .map(|r| home.join(r))
            .collect()
    }
}

/// `p` as a relative path of normal components only (no `..`, no root).
fn relative(p: &str) -> Option<&Path> {
    let path = Path::new(p);
    (!p.is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)))).then_some(path)
}

/// The host as the wipe sees it.
pub trait WipeHost {
    /// `(user, home)` of every static GitHub runner unit on the host.
    fn runner_homes(&self) -> Vec<(String, PathBuf)>;
    /// Whether `user` is running a job now; `None` when it cannot tell.
    fn job_running(&self, user: &str) -> Option<bool>;
    /// Whether `home` is owned by root (or its owner is unknown).
    fn root_or_unknown_owner(&self, home: &Path) -> bool;
    /// Whether `home`'s owner is an administrator (in `sudo`, `wheel` or
    /// `admin`): a person's account, never a dedicated runner or job user.
    /// `None` when it cannot tell.
    fn admin_owner(&self, home: &Path) -> Option<bool>;
    /// Whether `path` exists (as a file, directory or link).
    fn exists(&self, home: &Path, path: &Path) -> Result<bool, String>;
    /// Newest modification time (unix secs) at or below `path`.
    fn newest(&self, home: &Path, path: &Path) -> Option<i64>;
    /// Remove `path` as the owner of `home`.
    fn remove(&self, home: &Path, path: &Path) -> Result<(), String>;
}

/// What one home's wipe did. Paths only, never contents.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct WipeReport {
    removed: Vec<PathBuf>,
    /// Touched within `min_idle_secs`.
    recent: Vec<PathBuf>,
    skipped: Option<String>,
    errors: Vec<String>,
}

/// Wipe one home. `user` is set for a static runner's home: its job state
/// is re-checked before every removal and recently touched files stay.
fn wipe_home(
    host: &dyn WipeHost,
    s: &CredentialWipeSettings,
    home: &Path,
    user: Option<&str>,
    now: i64,
) -> WipeReport {
    let mut r = WipeReport::default();
    if !home.is_absolute() || home == Path::new("/") {
        r.skipped = Some("not a home directory".into());
        return r;
    }
    if host.root_or_unknown_owner(home) {
        r.skipped = Some("owned by root or unknown owner".into());
        return r;
    }
    // A person's credentials are theirs: only dedicated runner and job
    // accounts are wiped.
    if host.admin_owner(home) != Some(false) {
        r.skipped = Some("owner is an administrator account (or unknown)".into());
        return r;
    }
    for path in s.paths(home) {
        match host.exists(home, &path) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => {
                r.errors.push(format!("{}: {e}", path.display()));
                continue;
            }
        }
        if let Some(u) = user {
            match host.job_running(u) {
                Some(false) => {}
                Some(true) => {
                    r.skipped = Some("runner has a job".into());
                    return r;
                }
                None => {
                    r.skipped = Some("cannot tell whether the runner has a job".into());
                    return r;
                }
            }
            let min = i64::try_from(s.min_idle_secs).unwrap_or(i64::MAX);
            match host.newest(home, &path) {
                Some(t) if now.saturating_sub(t) >= min => {}
                _ => {
                    r.recent.push(path);
                    continue;
                }
            }
        }
        match host.remove(home, &path) {
            Ok(()) => r.removed.push(path),
            Err(e) => r.errors.push(format!("{}: {e}", path.display())),
        }
    }
    r
}

/// Parse `systemctl list-units --plain --no-legend` lines into unit names.
fn runner_units(listing: &str) -> Vec<String> {
    listing
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|u| u.starts_with("actions.runner.") && u.ends_with(".service"))
        .map(str::to_owned)
        .collect()
}

/// The `_work` directory of every static GitHub runner unit on the host
/// (`<WorkingDirectory>/_work`).
pub(crate) fn static_runner_work_dirs(cmd: &Commands) -> Vec<PathBuf> {
    let exec = cmd.exec();
    let Ok(out) = exec.run(
        "systemctl",
        &[
            "list-units",
            "--type=service",
            "--all",
            "--plain",
            "--no-legend",
            "--no-pager",
            "actions.runner.*",
        ],
    ) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = runner_units(&out.stdout)
        .iter()
        .filter_map(|unit| {
            let wd = exec
                .run(
                    "systemctl",
                    &["show", "-p", "WorkingDirectory", "--value", unit],
                )
                .ok()?;
            let wd = wd.stdout.trim();
            (wd.starts_with('/') && wd != "/").then(|| Path::new(wd).join("_work"))
        })
        .collect();
    dirs.sort();
    dirs.dedup();
    dirs
}

/// The home field of a `getent passwd` line.
fn passwd_home(line: &str) -> Option<PathBuf> {
    let home = line.trim().split(':').nth(5)?;
    home.starts_with('/').then(|| PathBuf::from(home))
}

/// Whether an `id -nG` group list marks an administrator.
fn admin_groups(groups: &str) -> bool {
    groups
        .split_whitespace()
        .any(|g| matches!(g, "sudo" | "wheel" | "admin" | "root"))
}

/// A user name safe to hand to `pgrep -u`.
fn plain_user(u: &str) -> bool {
    !u.is_empty()
        && u.len() <= 32
        && u != "root"
        && u.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

/// The live host, through the drone's reclaim command runner.
pub struct LiveWipeHost<'a> {
    cmd: &'a Commands,
}

const PROBE_SECS: u32 = 60;

impl WipeHost for LiveWipeHost<'_> {
    fn runner_homes(&self) -> Vec<(String, PathBuf)> {
        let exec = self.cmd.exec();
        let Ok(out) = exec.run(
            "systemctl",
            &[
                "list-units",
                "--type=service",
                "--all",
                "--plain",
                "--no-legend",
                "--no-pager",
                "actions.runner.*",
            ],
        ) else {
            return Vec::new();
        };
        let mut homes = Vec::new();
        for unit in runner_units(&out.stdout) {
            let Ok(u) = exec.run("systemctl", &["show", "-p", "User", "--value", &unit]) else {
                continue;
            };
            let user = u.stdout.trim().to_owned();
            if !plain_user(&user) {
                continue;
            }
            let Ok(pw) = exec.run("getent", &["passwd", &user]) else {
                continue;
            };
            if let Some(home) = pw.success.then(|| passwd_home(&pw.stdout)).flatten() {
                homes.push((user, home));
            }
        }
        homes.sort();
        homes.dedup();
        homes
    }

    fn job_running(&self, user: &str) -> Option<bool> {
        if !plain_user(user) {
            return None;
        }
        // pgrep exits 0 on a match, 1 on none, more on error.
        let out = self
            .cmd
            .exec()
            .run(
                "sh",
                &[
                    "-c",
                    r#"pgrep -u "$1" -x Runner.Worker >/dev/null; echo $?"#,
                    "credwipe",
                    user,
                ],
            )
            .ok()?;
        match out.stdout.trim() {
            "0" => Some(true),
            "1" => Some(false),
            _ => None,
        }
    }

    fn root_or_unknown_owner(&self, home: &Path) -> bool {
        self.cmd.owner_uid(home).map_or(true, |u| u == "0")
    }

    fn admin_owner(&self, home: &Path) -> Option<bool> {
        let uid = self.cmd.owner_uid(home).ok()?;
        if !uid.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let out = self.cmd.exec().run("id", &["-nG", &uid]).ok()?;
        if !out.success {
            return None;
        }
        Some(admin_groups(&out.stdout))
    }

    fn exists(&self, home: &Path, path: &Path) -> Result<bool, String> {
        let out = self.cmd.sh_as(
            home,
            path,
            PROBE_SECS,
            r#"if [ -e "$1" ] || [ -L "$1" ]; then echo yes; else echo no; fi"#,
        )?;
        Ok(out.trim() == "yes")
    }

    fn newest(&self, home: &Path, path: &Path) -> Option<i64> {
        let out = self
            .cmd
            .sh_as(
                home,
                path,
                PROBE_SECS,
                r#"find -P "$1" -maxdepth 4 -printf '%T@\n' 2>/dev/null | sort -n | tail -1"#,
            )
            .ok()?;
        out.trim().split('.').next()?.parse().ok()
    }

    fn remove(&self, home: &Path, path: &Path) -> Result<(), String> {
        // `rm` never follows a symlink operand, and runs as the home's owner.
        self.cmd
            .sh_as(home, path, PROBE_SECS, r#"rm -rf -- "$1""#)
            .map(drop)
    }
}

fn log(home: &Path, r: &WipeReport) {
    for p in &r.removed {
        tracing::info!("credential wipe: removed {}", p.display());
    }
    if !r.recent.is_empty() {
        tracing::info!(
            "credential wipe {}: left {} recently touched path(s) for the next sweep",
            home.display(),
            r.recent.len()
        );
    }
    for e in &r.errors {
        tracing::warn!("credential wipe {}: {e}", home.display());
    }
}

/// The drone's credential wipe.
pub struct CredWiper {
    settings: CredentialWipeSettings,
    cmd: Commands,
    job_home: Option<PathBuf>,
}

impl CredWiper {
    /// `None` when off or not on Linux.
    pub(super) fn new(
        settings: &CredentialWipeSettings,
        job_home: Option<PathBuf>,
        cmd: Commands,
    ) -> Option<Arc<Self>> {
        if !settings.enabled || !cfg!(target_os = "linux") {
            return None;
        }
        Some(Arc::new(Self {
            settings: settings.clone(),
            cmd,
            job_home,
        }))
    }

    fn now() -> i64 {
        time::OffsetDateTime::now_utc().unix_timestamp()
    }

    /// Wipe the drone job user's home. Call only when no lease is running.
    pub(super) fn wipe_job_home(&self) {
        let Some(home) = &self.job_home else { return };
        let host = LiveWipeHost { cmd: &self.cmd };
        let r = wipe_home(&host, &self.settings, home, None, Self::now());
        log(home, &r);
    }

    /// Wipe every idle static runner home.
    fn wipe_runner_homes(&self) {
        if !self.settings.static_runner_homes {
            return;
        }
        let host = LiveWipeHost { cmd: &self.cmd };
        for (user, home) in host.runner_homes() {
            if self.job_home.as_ref() == Some(&home) {
                continue;
            }
            let r = wipe_home(&host, &self.settings, &home, Some(&user), Self::now());
            log(&home, &r);
        }
    }

    /// Sweep idle homes every `interval_secs`; `idle` says whether the
    /// drone holds no lease (then its job home is wiped too).
    pub(super) fn spawn(
        self: &Arc<Self>,
        idle: impl Fn() -> bool + Send + Sync + 'static,
    ) -> tokio::task::JoinHandle<()> {
        let me = Arc::clone(self);
        let idle = Arc::new(idle);
        tokio::spawn(async move {
            let mut every =
                tokio::time::interval(Duration::from_secs(me.settings.interval_secs.max(30)));
            every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                every.tick().await;
                let (run, idle) = (Arc::clone(&me), Arc::clone(&idle));
                let swept = tokio::task::spawn_blocking(move || {
                    run.wipe_runner_homes();
                    if idle() {
                        run.wipe_job_home();
                    }
                })
                .await;
                if let Err(e) = swept {
                    tracing::warn!("credential wipe panicked: {e}");
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;

    #[derive(Default)]
    struct Fake {
        files: RefCell<BTreeMap<PathBuf, i64>>,
        job: Option<bool>,
        root_owned: bool,
        admin: Option<bool>,
        jobs_checked: RefCell<u32>,
    }

    impl Fake {
        fn with(paths: &[(&str, i64)]) -> Self {
            Self {
                files: RefCell::new(paths.iter().map(|(p, t)| (PathBuf::from(p), *t)).collect()),
                job: Some(false),
                admin: Some(false),
                ..Self::default()
            }
        }
        fn left(&self) -> BTreeSet<String> {
            self.files
                .borrow()
                .keys()
                .map(|p| p.display().to_string())
                .collect()
        }
    }

    impl WipeHost for Fake {
        fn runner_homes(&self) -> Vec<(String, PathBuf)> {
            Vec::new()
        }
        fn job_running(&self, _: &str) -> Option<bool> {
            *self.jobs_checked.borrow_mut() += 1;
            self.job
        }
        fn root_or_unknown_owner(&self, _: &Path) -> bool {
            self.root_owned
        }
        fn admin_owner(&self, _: &Path) -> Option<bool> {
            self.admin
        }
        fn exists(&self, _: &Path, p: &Path) -> Result<bool, String> {
            Ok(self.files.borrow().keys().any(|f| f.starts_with(p)))
        }
        fn newest(&self, _: &Path, p: &Path) -> Option<i64> {
            self.files
                .borrow()
                .iter()
                .filter(|(f, _)| f.starts_with(p))
                .map(|(_, t)| *t)
                .max()
        }
        fn remove(&self, _: &Path, p: &Path) -> Result<(), String> {
            self.files.borrow_mut().retain(|f, _| !f.starts_with(p));
            Ok(())
        }
    }

    const NOW: i64 = 100_000;

    /// What earlier jobs left in every static runner home on
    /// internal-runner-burst-01 on 2026-11-01 (names only), plus the caches
    /// and runner registration that must survive.
    fn burst01_home() -> Fake {
        Fake::with(&[
            ("/home/runner1/.netrc", 1000),
            ("/home/runner1/.docker/config.json", 1000),
            ("/home/runner1/.kube/config", 1000),
            ("/home/runner1/.kube/gke_gcloud_auth_plugin_cache", 1000),
            ("/home/runner1/.config/gcloud/credentials.db", 1000),
            ("/home/runner1/.config/gcloud/access_tokens.db", 1000),
            (
                "/home/runner1/.config/gcloud/configurations/config_default",
                1000,
            ),
            ("/home/runner1/.terraform.d/credentials.tfrc.json", 1000),
            ("/home/runner1/.terraform.d/plugin-cache/x", 1000),
            ("/home/runner1/.sigstore/root/root.json", 1000),
            ("/home/runner1/.cache/go-build/x", 1000),
            ("/home/runner1/.rustup/toolchains/x", 1000),
            ("/home/runner1/.config/go/env", 1000),
            ("/home/runner1/actions-runner/.credentials", 1000),
            ("/home/runner1/actions-runner/.runner", 1000),
        ])
    }

    #[test]
    fn an_idle_static_runner_home_loses_every_credential_and_keeps_caches() {
        let f = burst01_home();
        let r = wipe_home(
            &f,
            &CredentialWipeSettings::default(),
            Path::new("/home/runner1"),
            Some("runner1"),
            NOW,
        );
        assert!(r.errors.is_empty() && r.skipped.is_none(), "{r:?}");
        assert_eq!(
            f.left(),
            [
                "/home/runner1/.cache/go-build/x",
                "/home/runner1/.config/go/env",
                "/home/runner1/.rustup/toolchains/x",
                "/home/runner1/.sigstore/root/root.json",
                "/home/runner1/.terraform.d/plugin-cache/x",
                "/home/runner1/actions-runner/.credentials",
                "/home/runner1/actions-runner/.runner",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect()
        );
        assert_eq!(r.removed.len(), 5, "{:?}", r.removed);
    }

    #[test]
    fn a_runner_with_a_job_or_an_unknown_state_is_left_alone() {
        for job in [Some(true), None] {
            let mut f = burst01_home();
            f.job = job;
            let before = f.left();
            let r = wipe_home(
                &f,
                &CredentialWipeSettings::default(),
                Path::new("/home/runner1"),
                Some("runner1"),
                NOW,
            );
            assert!(r.skipped.is_some());
            assert_eq!(f.left(), before);
        }
    }

    #[test]
    fn recently_touched_credentials_wait_for_the_next_sweep() {
        let f = Fake::with(&[
            ("/home/r/.netrc", NOW - 60),
            ("/home/r/.kube/config", NOW - 3600),
        ]);
        let r = wipe_home(
            &f,
            &CredentialWipeSettings::default(),
            Path::new("/home/r"),
            Some("r"),
            NOW,
        );
        assert_eq!(r.recent, [PathBuf::from("/home/r/.netrc")]);
        assert_eq!(f.left(), BTreeSet::from(["/home/r/.netrc".to_owned()]));
        // Re-checked before every removal, not once per home.
        assert_eq!(*f.jobs_checked.borrow(), 2);
    }

    #[test]
    fn the_drone_job_home_is_wiped_without_the_idle_wait() {
        let f = Fake::with(&[
            ("/var/lib/borg-job/.netrc", NOW),
            ("/var/lib/borg-job/.bazelrc", NOW),
        ]);
        let r = wipe_home(
            &f,
            &CredentialWipeSettings::default(),
            Path::new("/var/lib/borg-job"),
            None,
            NOW,
        );
        assert_eq!(r.removed, [PathBuf::from("/var/lib/borg-job/.netrc")]);
        assert_eq!(*f.jobs_checked.borrow(), 0);
        assert_eq!(
            f.left(),
            BTreeSet::from(["/var/lib/borg-job/.bazelrc".to_owned()])
        );
    }

    #[test]
    fn root_owned_and_non_homes_are_refused() {
        let mut f = Fake::with(&[("/root/.netrc", 0)]);
        f.root_owned = true;
        let s = CredentialWipeSettings::default();
        assert!(
            wipe_home(&f, &s, Path::new("/root"), None, NOW)
                .skipped
                .is_some()
        );
        let f = Fake::with(&[("/.netrc", 0)]);
        assert!(
            wipe_home(&f, &s, Path::new("/"), None, NOW)
                .skipped
                .is_some()
        );
        assert_eq!(f.left().len(), 1);
    }

    #[test]
    fn a_persons_account_is_never_wiped() {
        // dev-desktop's `developer` is in sudo; an unknown answer counts too.
        for admin in [Some(true), None] {
            let mut f = Fake::with(&[("/home/developer/.kube/config", 0)]);
            f.admin = admin;
            let r = wipe_home(
                &f,
                &CredentialWipeSettings::default(),
                Path::new("/home/developer"),
                Some("developer"),
                NOW,
            );
            assert!(r.skipped.is_some() && r.removed.is_empty());
            assert_eq!(f.left().len(), 1);
        }
        assert!(admin_groups("developer sudo docker"));
        assert!(!admin_groups("runner1 docker"));
    }

    #[test]
    fn extra_paths_must_stay_inside_the_home() {
        let mut s = CredentialWipeSettings {
            extra_paths: vec![".config/acme/token".into()],
            ..CredentialWipeSettings::default()
        };
        assert!(s.validate().is_ok());
        assert!(
            s.paths(Path::new("/h"))
                .contains(&PathBuf::from("/h/.config/acme/token"))
        );
        for bad in ["../etc", "/etc/passwd", "", "a/../../b"] {
            s.extra_paths = vec![bad.into()];
            assert!(s.validate().is_err(), "{bad}");
        }
        assert!(CREDENTIAL_PATHS.iter().all(|p| relative(p).is_some()));
    }

    #[test]
    fn runner_units_users_and_homes_parse() {
        let listing = "actions.runner.dx-corp.gce-internal-01-1.service loaded active running GitHub Actions Runner\n\
                       borg-drone.service loaded active running borg\n\
                       actions.runner.dx-corp.gce-internal-01-2.service loaded inactive dead x\n";
        assert_eq!(
            runner_units(listing),
            [
                "actions.runner.dx-corp.gce-internal-01-1.service",
                "actions.runner.dx-corp.gce-internal-01-2.service"
            ]
        );
        assert_eq!(
            passwd_home("runner1:x:1001:1001::/home/runner1:/bin/bash"),
            Some(PathBuf::from("/home/runner1"))
        );
        assert_eq!(passwd_home("bad"), None);
        assert!(
            plain_user("runner1") && !plain_user("root") && !plain_user("a b") && !plain_user("")
        );
    }
}
