//! Unconditional per-lease cleanup: the work directory (with its `_diag`) and
//! the job's docker containers, networks and volumes. Tool caches live outside
//! the work root and are never touched.
//!
//! Docker removal is conservative because other workloads share some hosts: a
//! resource is removed only after `docker inspect` confirms it belongs to the
//! lease (label or exact name prefix). There is no global prune.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use crate::drone::exec::Exec;

/// Label that marks a resource as belonging to a lease.
pub const LEASE_LABEL: &str = "borg.lease";
const COMPOSE_PROJECT_LABEL: &str = "com.docker.compose.project";
const PREFIX: &str = "borg-";

/// Lease ids come from the queen; accept only a filesystem- and docker-safe
/// alphabet before using one in a path or a docker filter.
pub fn valid_lease_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The name prefix jobs should give their docker resources (also exported as
/// `COMPOSE_PROJECT_NAME`).
pub fn docker_prefix(lease_id: &str) -> String {
    format!("{PREFIX}{lease_id}")
}

/// Which resources a cleanup may remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope<'a> {
    /// Resources of one lease.
    Lease(&'a str),
    /// Resources of any borg lease (startup sweep after a crash).
    AnyLease,
}

fn is_hex12(s: &str) -> bool {
    s.len() == 12
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `name` is `prefix` itself or `prefix` followed by `-` or `_`.
fn name_has_prefix(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('-') || rest.starts_with('_'))
}

fn belongs(scope: Scope<'_>, name: &str, labels: &BTreeMap<String, String>) -> bool {
    match scope {
        Scope::Lease(id) => {
            let prefix = docker_prefix(id);
            labels.get(LEASE_LABEL).is_some_and(|v| v == id)
                || labels
                    .get(COMPOSE_PROJECT_LABEL)
                    .is_some_and(|p| *p == prefix)
                || name_has_prefix(name, &prefix)
        }
        Scope::AnyLease => {
            let project_ok = labels
                .get(COMPOSE_PROJECT_LABEL)
                .and_then(|p| p.strip_prefix(PREFIX))
                .is_some_and(is_hex12);
            let name_ok = name.strip_prefix(PREFIX).is_some_and(|rest| {
                rest.get(..12).is_some_and(is_hex12)
                    && rest
                        .get(12..)
                        .is_some_and(|tail| tail.is_empty() || tail.starts_with(['-', '_']))
            });
            labels.contains_key(LEASE_LABEL) || project_ok || name_ok
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Container,
    Network,
    Volume,
}

impl Kind {
    fn list_args(self) -> &'static [&'static str] {
        match self {
            Self::Container => &["ps", "-aq"],
            Self::Network => &["network", "ls", "-q", "--no-trunc"],
            Self::Volume => &["volume", "ls", "-q"],
        }
    }

    fn inspect_args(self) -> (&'static [&'static str], &'static str) {
        match self {
            Self::Container => (&["inspect"], "{{.Name}}\t{{json .Config.Labels}}"),
            Self::Network => (&["network", "inspect"], "{{.Name}}\t{{json .Labels}}"),
            Self::Volume => (&["volume", "inspect"], "{{.Name}}\t{{json .Labels}}"),
        }
    }

    fn remove_args(self) -> &'static [&'static str] {
        match self {
            Self::Container => &["rm", "-f", "-v"],
            Self::Network => &["network", "rm"],
            Self::Volume => &["volume", "rm", "-f"],
        }
    }
}

/// What a docker cleanup did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DockerReport {
    pub removed: Vec<String>,
    /// Listed by a filter but not provably ours, so left alone.
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
}

fn filters(scope: Scope<'_>) -> Vec<String> {
    match scope {
        Scope::Lease(id) => {
            let prefix = docker_prefix(id);
            vec![
                format!("label={LEASE_LABEL}={id}"),
                format!("label={COMPOSE_PROJECT_LABEL}={prefix}"),
                format!("name={prefix}"),
            ]
        }
        Scope::AnyLease => vec![format!("label={LEASE_LABEL}"), format!("name={PREFIX}")],
    }
}

fn parse_inspect(line: &str) -> Option<(String, BTreeMap<String, String>)> {
    let (name, labels) = line.split_once('\t')?;
    let labels: Option<BTreeMap<String, String>> = serde_json::from_str(labels.trim()).ok()?;
    Some((
        name.trim_start_matches('/').to_owned(),
        labels.unwrap_or_default(),
    ))
}

/// Remove the docker containers, networks and volumes of `scope`.
pub fn cleanup_docker(exec: &dyn Exec, docker: &str, scope: Scope<'_>) -> DockerReport {
    let mut report = DockerReport::default();
    if let Scope::Lease(id) = scope
        && !valid_lease_id(id)
    {
        report
            .errors
            .push(format!("refusing unsafe lease id {id:?}"));
        return report;
    }
    for kind in [Kind::Container, Kind::Network, Kind::Volume] {
        let mut candidates: Vec<String> = Vec::new();
        for filter in filters(scope) {
            let mut args: Vec<&str> = kind.list_args().to_vec();
            args.extend(["--filter", &filter]);
            match exec.run(docker, &args) {
                Ok(out) if out.success => {
                    for id in out.stdout.lines().map(str::trim).filter(|l| !l.is_empty()) {
                        if !candidates.iter().any(|c| c == id) {
                            candidates.push(id.to_owned());
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    report.errors.push(format!("{docker}: {e}"));
                    return report;
                }
            }
        }
        for id in candidates {
            let (inspect, format) = kind.inspect_args();
            let mut args: Vec<&str> = inspect.to_vec();
            args.extend(["--format", format, &id]);
            let owned = match exec.run(docker, &args) {
                Ok(out) if out.success => {
                    parse_inspect(&out.stdout).is_some_and(|(n, l)| belongs(scope, &n, &l))
                }
                _ => false,
            };
            if !owned {
                report.skipped.push(id);
                continue;
            }
            let mut args: Vec<&str> = kind.remove_args().to_vec();
            args.push(&id);
            match exec.run(docker, &args) {
                Ok(out) if out.success => report.removed.push(id),
                Ok(_) => report.errors.push(format!("failed to remove {id}")),
                Err(e) => report.errors.push(format!("{docker}: {e}")),
            }
        }
    }
    report
}

/// Delete `<work_root>/<lease_id>` (a symlink at that path is unlinked, never
/// followed). Returns whether anything was removed.
pub fn remove_lease_dir(work_root: &Path, lease_id: &str) -> io::Result<bool> {
    if !valid_lease_id(lease_id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsafe lease id",
        ));
    }
    let path = work_root.join(lease_id);
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&path).map(|()| true),
        Ok(_) => std::fs::remove_file(&path).map(|()| true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Delete every lease directory under `work_root` (startup: the drone holds
/// no leases yet). Returns the removed names.
pub fn sweep_work_root(work_root: &Path) -> io::Result<Vec<String>> {
    let mut removed = Vec::new();
    let entries = match std::fs::read_dir(work_root) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(removed),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if remove_lease_dir(work_root, &name)? {
            removed.push(name);
        }
    }
    Ok(removed)
}

#[cfg(test)]
pub(crate) mod testing {
    use std::collections::HashMap;
    use std::io;
    use std::sync::Mutex;

    use crate::drone::exec::{Exec, Output};

    /// Scripted commands: key is the program plus space-joined args; any key
    /// not present is a successful command with empty output.
    #[derive(Default)]
    pub struct FakeExec {
        pub script: Mutex<HashMap<String, Output>>,
        pub calls: Mutex<Vec<String>>,
    }

    impl FakeExec {
        pub fn on(&self, key: &str, stdout: &str) {
            self.script.lock().unwrap().insert(
                key.to_owned(),
                Output {
                    success: true,
                    stdout: stdout.to_owned(),
                },
            );
        }
        pub fn fail(&self, key: &str) {
            self.script.lock().unwrap().insert(
                key.to_owned(),
                Output {
                    success: false,
                    stdout: String::new(),
                },
            );
        }
        pub fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Exec for FakeExec {
        fn run(&self, program: &str, args: &[&str]) -> io::Result<Output> {
            let key = format!("{program} {}", args.join(" "));
            self.calls.lock().unwrap().push(key.clone());
            Ok(self
                .script
                .lock()
                .unwrap()
                .get(&key)
                .cloned()
                .unwrap_or(Output {
                    success: true,
                    stdout: String::new(),
                }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::FakeExec;
    use super::*;

    const ID: &str = "0123456789ab";

    fn inspect_key(prefix: &str, id: &str) -> String {
        format!("docker {prefix}--format {{{{.Name}}}}\t{{{{json .Config.Labels}}}} {id}")
    }

    #[test]
    fn removes_only_containers_that_provably_belong_to_the_lease() {
        let x = FakeExec::default();
        // The name filter is a substring match, so it also lists lookalikes.
        x.on(
            &format!("docker ps -aq --filter name=borg-{ID}"),
            "c-ours\nc-lookalike\nc-longer-id\nc-other-label\nc-broken",
        );
        x.on(
            &inspect_key("inspect ", "c-ours"),
            &format!("/borg-{ID}-db-1\tnull"),
        );
        x.on(
            &inspect_key("inspect ", "c-lookalike"),
            &format!("/xborg-{ID}-db-1\t{{}}"),
        );
        x.on(
            &inspect_key("inspect ", "c-longer-id"),
            &format!("/borg-{ID}9-db-1\t{{}}"),
        );
        x.on(
            &inspect_key("inspect ", "c-other-label"),
            "/dev-db\t{\"borg.lease\":\"fedcba987654\"}",
        );
        x.on(&inspect_key("inspect ", "c-broken"), "not json");
        let r = cleanup_docker(&x, "docker", Scope::Lease(ID));
        assert_eq!(r.removed, ["c-ours"]);
        assert_eq!(
            r.skipped,
            ["c-lookalike", "c-longer-id", "c-other-label", "c-broken"]
        );
        let calls = x.calls();
        assert!(calls.contains(&"docker rm -f -v c-ours".to_owned()));
        assert!(
            !calls
                .iter()
                .any(|c| c.contains("rm -f -v c-lookalike") || c.contains("prune")),
            "{calls:?}"
        );
    }

    #[test]
    fn label_and_compose_project_matches_are_removed_even_with_other_names() {
        let x = FakeExec::default();
        x.on(
            &format!("docker ps -aq --filter label=borg.lease={ID}"),
            "c1",
        );
        x.on(
            &format!("docker ps -aq --filter label=com.docker.compose.project=borg-{ID}"),
            "c2",
        );
        x.on(
            &inspect_key("inspect ", "c1"),
            &format!("/svc\t{{\"borg.lease\":\"{ID}\"}}"),
        );
        x.on(
            &inspect_key("inspect ", "c2"),
            &format!("/svc2\t{{\"com.docker.compose.project\":\"borg-{ID}\"}}"),
        );
        assert_eq!(
            cleanup_docker(&x, "docker", Scope::Lease(ID)).removed,
            ["c1", "c2"]
        );
    }

    #[test]
    fn networks_and_volumes_are_removed_with_their_own_commands() {
        let x = FakeExec::default();
        x.on(
            &format!("docker network ls -q --no-trunc --filter name=borg-{ID}"),
            "n1",
        );
        x.on(
            &format!("docker volume ls -q --filter name=borg-{ID}"),
            "v1",
        );
        let net = "docker network inspect --format {{.Name}}\t{{json .Labels}} n1";
        let vol = "docker volume inspect --format {{.Name}}\t{{json .Labels}} v1";
        x.on(net, &format!("borg-{ID}_default\tnull"));
        x.on(vol, &format!("borg-{ID}_data\t{{}}"));
        let r = cleanup_docker(&x, "docker", Scope::Lease(ID));
        assert_eq!(r.removed, ["n1", "v1"]);
        let calls = x.calls();
        assert!(calls.contains(&"docker network rm n1".to_owned()));
        assert!(calls.contains(&"docker volume rm -f v1".to_owned()));
    }

    #[test]
    fn unsafe_lease_ids_make_no_docker_calls() {
        let x = FakeExec::default();
        let r = cleanup_docker(&x, "docker", Scope::Lease("a b; rm"));
        assert!(x.calls().is_empty());
        assert_eq!(r.errors.len(), 1);
    }

    #[test]
    fn startup_sweep_matches_any_borg_lease_but_not_other_workloads() {
        let x = FakeExec::default();
        x.on("docker ps -aq --filter label=borg.lease", "a\nb");
        x.on("docker ps -aq --filter name=borg-", "b\nc\nd");
        x.on(
            &inspect_key("inspect ", "a"),
            "/job-a\t{\"borg.lease\":\"0123456789ab\"}",
        );
        x.on(
            &inspect_key("inspect ", "b"),
            "/borg-aaaaaaaaaaaa-web-1\t{}",
        );
        x.on(&inspect_key("inspect ", "c"), "/borg-queen-backup\t{}");
        x.on(&inspect_key("inspect ", "d"), "/borg-aaaaaaaaaaaaZ\tnull");
        let r = cleanup_docker(&x, "docker", Scope::AnyLease);
        assert_eq!(r.removed, ["a", "b"]);
        assert_eq!(r.skipped, ["c", "d"]);
    }

    #[test]
    fn docker_errors_are_reported_not_fatal() {
        let x = FakeExec::default();
        x.on(&format!("docker ps -aq --filter name=borg-{ID}"), "c1");
        x.on(&inspect_key("inspect ", "c1"), &format!("/borg-{ID}\tnull"));
        x.fail("docker rm -f -v c1");
        let r = cleanup_docker(&x, "docker", Scope::Lease(ID));
        assert!(r.removed.is_empty());
        assert_eq!(r.errors, ["failed to remove c1"]);
    }

    #[test]
    fn lease_dir_removal_is_contained_and_leaves_caches_alone() {
        let root = tempfile::tempdir().unwrap();
        let work = root.path().join("work");
        let cache = root.path().join("cache/sccache");
        std::fs::create_dir_all(work.join(ID).join("_diag")).unwrap();
        std::fs::write(work.join(ID).join("_diag/log"), "x").unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("entry"), "x").unwrap();
        std::fs::create_dir_all(work.join("keep-me")).unwrap();

        assert!(remove_lease_dir(&work, ID).unwrap());
        assert!(!work.join(ID).exists());
        assert!(
            !remove_lease_dir(&work, ID).unwrap(),
            "second removal is a no-op"
        );
        assert!(cache.join("entry").exists());
        assert!(work.join("keep-me").exists());
        assert!(remove_lease_dir(&work, "../cache").is_err());
        assert!(remove_lease_dir(&work, "").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_lease_dir_is_unlinked_not_followed() {
        let root = tempfile::tempdir().unwrap();
        let work = root.path().join("work");
        let target = root.path().join("precious");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("f"), "x").unwrap();
        std::os::unix::fs::symlink(&target, work.join(ID)).unwrap();
        assert!(remove_lease_dir(&work, ID).unwrap());
        assert!(target.join("f").exists());
    }

    #[test]
    fn sweep_removes_every_lease_dir() {
        let root = tempfile::tempdir().unwrap();
        for id in ["aaa", "bbb"] {
            std::fs::create_dir_all(root.path().join(id).join("x")).unwrap();
        }
        let mut removed = sweep_work_root(root.path()).unwrap();
        removed.sort();
        assert_eq!(removed, ["aaa", "bbb"]);
        assert!(
            sweep_work_root(&root.path().join("missing"))
                .unwrap()
                .is_empty()
        );
    }
}
