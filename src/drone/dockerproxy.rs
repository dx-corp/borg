//! A Docker endpoint per lease.
//!
//! Without this, a job's containers run in dockerd's cgroup: their memory is
//! outside the lease's `memory.max`, the queen's admission never sees it, and
//! a container that outlives its job keeps eating the host. With
//! `docker_proxy.enabled`, each lease gets its own Unix socket
//! (`DOCKER_HOST=unix://<state_dir>/docker-endpoints/slot-<N>.sock`) served by the
//! drone. The proxy forwards every request to dockerd unchanged except:
//!
//! - **container create** gets `HostConfig.CgroupParent` set to the lease's
//!   cgroup, so the container's memory counts against the job's limit and the
//!   job's OOM handling (dockerd must use the `cgroupfs` cgroup driver: the
//!   `systemd` driver only accepts a systemd slice, and a lease cgroup lives
//!   inside the drone's delegated service cgroup). Its `OomScoreAdj` is raised
//!   to the lease's, so under host pressure the newest job's containers go
//!   first with it.
//! - **container, network and volume create, and `/build`** get the lease's
//!   labels (`borg.lease`, `borg.scope`, `borg.class`), which is what the
//!   between-job cleanup and the startup sweep remove by. The GitHub run and
//!   job are joined to the lease id queen-side (`borg report`).
//! - Requests for host-level access are refused unless the job's scope is in
//!   `docker_proxy.host_access_scopes` (pull-request jobs never): privileged
//!   containers and execs, host network/PID/IPC/UTS/user/cgroup namespaces,
//!   devices, dangerous capabilities, unconfined seccomp/AppArmor, other
//!   runtimes, bind mounts of system paths, bind-type volumes, host and
//!   macvlan networks, plugins and swarm.
//! - A bind mount of the Docker socket is pointed at the lease's own socket,
//!   so Docker-in-Docker (and the runner's `container:` jobs) stays inside the
//!   lease.
//!
//! Only a process inside the lease's cgroup may connect (checked with
//! `SO_PEERCRED` and `/proc/<pid>/cgroup`), so one lease cannot use another's
//! endpoint. With a job user, the drone also drops the user's `docker` group
//! for leases, so the job reaches dockerd only through its endpoint.
//!
//! Images, the registry mirror and the build cache stay shared at the dockerd
//! level; only runtime resources are per lease.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Empty, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{CONTENT_LENGTH, CONTENT_TYPE, HeaderValue, TRANSFER_ENCODING, UPGRADE};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinSet;

use crate::classes::Scope;

/// Every resource created through a lease's endpoint carries this label;
/// the between-job cleanup and the startup sweep remove by it.
pub use crate::drone::cleanup::LEASE_LABEL;

/// Largest request body the proxy reads to rewrite (create calls are small).
const MAX_REWRITE_BODY: usize = 4 * 1024 * 1024;

type Body = UnsyncBoxBody<Bytes, hyper::Error>;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DockerProxySettings {
    /// Give each lease its own Docker endpoint. Off by default; rolled out
    /// one drone at a time.
    #[serde(default)]
    pub enabled: bool,
    /// Job scopes that may ask for host-level access (privileged containers,
    /// host networking, devices, system bind mounts). `pr` is never allowed.
    #[serde(default)]
    pub host_access_scopes: Vec<Scope>,
    /// Where the per-slot sockets go (`slot-<N>.sock`). Default:
    /// `<state_dir>/docker-endpoints`. Every directory on the path must be
    /// searchable by the job user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket_dir: Option<PathBuf>,
}

/// dockerd's socket: the one the drone's own `docker` CLI (and so the
/// between-job cleanup) talks to, so resources made through an endpoint are
/// always the ones cleanup removes.
pub const DOCKERD_SOCKET: &str = "/var/run/docker.sock";

pub fn default_upstream() -> PathBuf {
    PathBuf::from(DOCKERD_SOCKET)
}

impl DockerProxySettings {
    pub fn is_off(&self) -> bool {
        *self == Self::default()
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.host_access_scopes.contains(&Scope::Pr) {
            return Err(
                "docker_proxy.host_access_scopes must not contain pr: pull-request jobs never get host-level Docker access"
                    .to_owned(),
            );
        }
        if self.socket_dir.as_ref().is_some_and(|d| !d.is_absolute()) {
            return Err("docker_proxy.socket_dir must be an absolute path".to_owned());
        }
        Ok(())
    }

    /// Whether a job of `scope` may ask for host-level access.
    pub fn host_access(&self, scope: Scope) -> bool {
        scope != Scope::Pr && self.host_access_scopes.contains(&scope)
    }
}

/// What one lease's endpoint enforces.
#[derive(Debug, Clone)]
pub struct LeasePolicy {
    /// Labels added to every container, network, volume and image build.
    pub labels: BTreeMap<String, String>,
    /// `HostConfig.CgroupParent` for containers (a path from the cgroup
    /// root); `None` when dockerd cannot place containers there.
    pub cgroup_parent: Option<String>,
    /// The lease's `oom_score_adj`; containers never score below it.
    pub oom_score_adj: Option<i32>,
    /// The job's scope may ask for host-level access.
    pub host_access: bool,
    /// This lease's endpoint, substituted for Docker socket bind mounts.
    pub socket: PathBuf,
    /// dockerd's socket.
    pub upstream: PathBuf,
    /// Host paths a container may not bind (nor any of their ancestors).
    pub protected: Vec<PathBuf>,
}

/// System paths no job without host access may bind into a container: each
/// gives root on the host, the drone's secrets, or Docker itself.
pub const PROTECTED_ROOTS: &[&str] = &[
    "/bin",
    "/boot",
    "/dev",
    "/etc",
    "/lib",
    "/lib32",
    "/lib64",
    "/proc",
    "/root",
    "/run",
    "/sbin",
    "/sys",
    "/usr",
    "/var/lib/borg",
    "/var/lib/containerd",
    "/var/lib/docker",
    "/var/run",
];

/// A request the endpoint refuses, with the reason the job sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused(pub String);

fn refuse(why: impl Into<String>) -> Refused {
    Refused(format!("borg: {}", why.into()))
}

/// `/v1.47/containers/create` -> `/containers/create`.
fn unversioned(path: &str) -> &str {
    if let Some(rest) = path.strip_prefix("/v")
        && let Some(slash) = rest.find('/')
        && !rest[..slash].is_empty()
        && rest[..slash]
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'.')
    {
        return &rest[slash..];
    }
    path
}

/// A resource a request addresses by id or name.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Container(String),
    Exec(String),
    Network(String),
    Volume(String),
}

/// What the proxy does with one request.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Route {
    ContainerCreate,
    /// `POST /containers/<id>/exec`: the container must be the lease's.
    ExecCreate(String),
    NetworkCreate,
    /// `POST /networks/<id>/connect`: the network and container must be
    /// usable by the lease.
    NetworkConnect(String),
    VolumeCreate,
    Build,
    /// `POST /<kind>/prune`, narrowed to the lease's resources.
    Prune,
    /// Plugins, swarm and its objects: host access only.
    HostOnly,
    /// Anything else on one container, exec, network or volume.
    Owned(Target),
    Forward,
}

fn route(method: &Method, path: &str) -> Route {
    let path = unversioned(path);
    let mut parts = path.trim_start_matches('/').split('/');
    let first = parts.next().unwrap_or("");
    let (second, third) = (parts.next(), parts.next());
    if matches!(
        first,
        "plugins" | "swarm" | "services" | "nodes" | "secrets" | "configs" | "tasks"
    ) && *method != Method::GET
    {
        return Route::HostOnly;
    }
    let post = *method == Method::POST;
    match (first, second, third) {
        ("containers", Some("create"), None) if post => Route::ContainerCreate,
        ("containers" | "networks" | "volumes", Some("prune"), None) if post => Route::Prune,
        ("containers", Some("json"), None) => Route::Forward,
        ("containers", Some(id), Some("exec")) if post => Route::ExecCreate(id.to_owned()),
        ("containers", Some(id), _) if !id.is_empty() => {
            Route::Owned(Target::Container(id.to_owned()))
        }
        ("exec", Some(id), _) if !id.is_empty() => Route::Owned(Target::Exec(id.to_owned())),
        ("networks", Some("create"), None) if post => Route::NetworkCreate,
        ("networks", Some(id), Some("connect")) if post => Route::NetworkConnect(id.to_owned()),
        ("networks", Some(id), _) if !id.is_empty() => Route::Owned(Target::Network(id.to_owned())),
        ("volumes", Some("create"), None) if post => Route::VolumeCreate,
        ("volumes", Some(name), _) if !name.is_empty() => {
            Route::Owned(Target::Volume(name.to_owned()))
        }
        ("build", None, None) if post => Route::Build,
        _ => Route::Forward,
    }
}

/// Go's `encoding/json` matches object keys to struct fields without regard
/// to case (folding `ſ` to `s` and the Kelvin sign to `k` as well), so dockerd
/// reads `"privileged"` as `Privileged`. Rename every key of `obj` that folds
/// to one of `names` to its canonical spelling before anything is checked;
/// two spellings of one field are refused.
fn canonicalize(obj: &mut Map<String, Value>, names: &[&str]) -> Result<(), Refused> {
    let fold = |k: &str| -> String {
        k.chars()
            .map(|c| match c {
                '\u{17f}' => 's',
                '\u{212a}' => 'k',
                c => c.to_ascii_lowercase(),
            })
            .collect()
    };
    let keys: Vec<String> = obj.keys().cloned().collect();
    for name in names {
        let want = fold(name);
        let spellings: Vec<&String> = keys.iter().filter(|k| fold(k) == want).collect();
        if spellings.len() > 1 {
            return Err(refuse(format!("{name} is given more than once")));
        }
        if let Some(&k) = spellings.first()
            && k != name
            && let Some(v) = obj.remove(k)
        {
            obj.insert((*name).to_owned(), v);
        }
    }
    Ok(())
}

const CREATE_KEYS: &[&str] = &["Labels", "HostConfig", "NetworkingConfig"];
const HOST_CONFIG_KEYS: &[&str] = &[
    "Binds",
    "CapAdd",
    "CgroupParent",
    "CgroupnsMode",
    "DeviceCgroupRules",
    "DeviceRequests",
    "Devices",
    "IpcMode",
    "Links",
    "Mounts",
    "NetworkMode",
    "OomScoreAdj",
    "PidMode",
    "Privileged",
    "Runtime",
    "SecurityOpt",
    "UTSMode",
    "UsernsMode",
    "VolumesFrom",
];
const MOUNT_KEYS: &[&str] = &["Type", "Source", "VolumeOptions"];

/// What a container create needs besides its rewritten body: the resources
/// it names, which must be the lease's (containers) or usable by it
/// (networks, volumes; a missing named volume is created with the lease's
/// labels first).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CreatePlan {
    pub body: Vec<u8>,
    pub containers: Vec<String>,
    pub networks: Vec<String>,
    pub volumes: Vec<String>,
}

fn object(v: &mut Value) -> Result<&mut Map<String, Value>, Refused> {
    v.as_object_mut()
        .ok_or_else(|| refuse("request body is not a JSON object"))
}

/// `obj[key]` as an object, created when absent or null.
fn child<'a>(
    obj: &'a mut Map<String, Value>,
    key: &str,
) -> Result<&'a mut Map<String, Value>, Refused> {
    let slot = obj
        .entry(key.to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    if slot.is_null() {
        *slot = Value::Object(Map::new());
    }
    slot.as_object_mut()
        .ok_or_else(|| refuse(format!("{key} is not an object")))
}

fn add_labels(
    obj: &mut Map<String, Value>,
    key: &str,
    labels: &BTreeMap<String, String>,
) -> Result<(), Refused> {
    let target = child(obj, key)?;
    for (k, v) in labels {
        target.insert(k.clone(), Value::String(v.clone()));
    }
    Ok(())
}

fn str_field<'a>(obj: &'a Map<String, Value>, key: &str) -> &'a str {
    obj.get(key).and_then(Value::as_str).unwrap_or("")
}

fn non_empty_array(obj: &Map<String, Value>, key: &str) -> bool {
    obj.get(key)
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty())
}

/// Capabilities that amount to host access.
const HOST_CAPS: &[&str] = &[
    "ALL",
    "BPF",
    "DAC_READ_SEARCH",
    "MAC_ADMIN",
    "MAC_OVERRIDE",
    "NET_ADMIN",
    "PERFMON",
    "SYS_ADMIN",
    "SYS_BOOT",
    "SYS_MODULE",
    "SYS_PTRACE",
    "SYS_RAWIO",
    "SYS_TIME",
];

/// Refuse what only host-access jobs may ask for in a `HostConfig`.
fn check_host_config(hc: &Map<String, Value>) -> Result<(), Refused> {
    if hc.get("Privileged").and_then(Value::as_bool) == Some(true) {
        return Err(refuse("privileged containers are not allowed for this job"));
    }
    for key in [
        "NetworkMode",
        "PidMode",
        "IpcMode",
        "UTSMode",
        "UsernsMode",
        "CgroupnsMode",
    ] {
        if str_field(hc, key) == "host" {
            return Err(refuse(format!("{key}=host is not allowed for this job")));
        }
    }
    for key in ["Devices", "DeviceRequests", "DeviceCgroupRules"] {
        if non_empty_array(hc, key) {
            return Err(refuse(format!("{key} is not allowed for this job")));
        }
    }
    for cap in hc
        .get("CapAdd")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        let name = cap.to_ascii_uppercase();
        let name = name.strip_prefix("CAP_").unwrap_or(&name);
        if HOST_CAPS.contains(&name) {
            return Err(refuse(format!(
                "capability {cap} is not allowed for this job"
            )));
        }
    }
    for opt in hc
        .get("SecurityOpt")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        let o = opt.to_ascii_lowercase().replace(':', "=");
        if o.ends_with("=unconfined") || o == "label=disable" {
            return Err(refuse(format!(
                "security option {opt} is not allowed for this job"
            )));
        }
    }
    let runtime = str_field(hc, "Runtime");
    if !(runtime.is_empty() || runtime == "runc") {
        return Err(refuse(format!(
            "runtime {runtime} is not allowed for this job"
        )));
    }
    Ok(())
}

/// `path` with symlinks resolved as far as it exists, then the rest
/// appended lexically (dockerd creates a missing bind source). `..` in the
/// part that does not exist is refused.
fn resolve(path: &Path) -> Result<PathBuf, Refused> {
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    loop {
        if let Ok(real) = std::fs::canonicalize(&existing) {
            let mut out = real;
            for part in rest.iter().rev() {
                out.push(part);
            }
            return Ok(out);
        }
        let Some(name) = existing.file_name().map(std::ffi::OsStr::to_os_string) else {
            return Err(refuse(format!(
                "cannot resolve bind source {}",
                path.display()
            )));
        };
        if path.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(refuse(format!("bind source {} has ..", path.display())));
        }
        rest.push(name);
        existing.pop();
    }
}

impl LeasePolicy {
    fn is_docker_socket(&self, resolved: &Path, raw: &Path) -> bool {
        let real_upstream =
            std::fs::canonicalize(&self.upstream).unwrap_or_else(|_| self.upstream.clone());
        resolved == real_upstream
            || raw == self.upstream
            || raw == Path::new("/var/run/docker.sock")
            || raw == Path::new("/run/docker.sock")
    }

    fn protected_path(&self, resolved: &Path) -> bool {
        resolved == Path::new("/")
            || self
                .protected
                .iter()
                .any(|p| resolved.starts_with(p) || p.starts_with(resolved))
    }

    /// Check one bind source: the path to give dockerd, or `None` for a
    /// named volume.
    fn bind_source(&self, source: &str) -> Result<Option<String>, Refused> {
        if !source.starts_with('/') {
            // A named volume.
            return Ok(None);
        }
        let raw = Path::new(source);
        let resolved = resolve(raw)?;
        if self.is_docker_socket(&resolved, raw) {
            return Ok(Some(self.socket.to_string_lossy().into_owned()));
        }
        if !self.host_access && self.protected_path(&resolved) {
            return Err(refuse(format!(
                "binding {source} into a container is not allowed for this job"
            )));
        }
        // Forward the path that was checked, with every symlink resolved, so
        // dockerd does not resolve a link the job changed meanwhile.
        Ok(Some(resolved.to_string_lossy().into_owned()))
    }

    fn check_volume_options(&self, opts: Option<&Map<String, Value>>) -> Result<(), Refused> {
        if self.host_access {
            return Ok(());
        }
        let Some(opts) = opts else { return Ok(()) };
        let binds = opts.contains_key("device")
            || str_field(opts, "o")
                .split(',')
                .any(|o| o == "bind" || o == "rbind");
        if binds {
            return Err(refuse(
                "volumes that bind host paths are not allowed for this job",
            ));
        }
        Ok(())
    }

    fn rewrite_mounts(
        &self,
        hc: &mut Map<String, Value>,
        plan: &mut CreatePlan,
    ) -> Result<(), Refused> {
        if let Some(binds) = hc.get_mut("Binds").and_then(Value::as_array_mut) {
            for bind in binds.iter_mut() {
                let Some(spec) = bind.as_str() else { continue };
                let (source, tail) = spec.split_once(':').unwrap_or((spec, ""));
                match self.bind_source(source)? {
                    Some(new) => {
                        *bind = Value::String(if tail.is_empty() {
                            new
                        } else {
                            format!("{new}:{tail}")
                        });
                    }
                    None => plan.volumes.push(source.to_owned()),
                }
            }
        }
        if let Some(mounts) = hc.get_mut("Mounts").and_then(Value::as_array_mut) {
            for mount in mounts.iter_mut() {
                let Some(m) = mount.as_object_mut() else {
                    continue;
                };
                canonicalize(m, MOUNT_KEYS)?;
                let kind = str_field(m, "Type").to_owned();
                match kind.as_str() {
                    "bind" => {
                        let source = str_field(m, "Source").to_owned();
                        if let Some(new) = self.bind_source(&source)? {
                            m.insert("Source".to_owned(), Value::String(new));
                        }
                    }
                    "volume" => {
                        let source = str_field(m, "Source").to_owned();
                        if !source.is_empty() {
                            plan.volumes.push(source);
                        }
                        let opts = m
                            .get_mut("VolumeOptions")
                            .and_then(Value::as_object_mut)
                            .and_then(|vo| {
                                canonicalize(vo, &["DriverConfig"]).ok()?;
                                vo.get_mut("DriverConfig").and_then(Value::as_object_mut)
                            })
                            .and_then(|dc| {
                                canonicalize(dc, &["Options"]).ok()?;
                                dc.get("Options").and_then(Value::as_object)
                            });
                        self.check_volume_options(opts)?;
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// What dockerd gets for `POST /containers/create`, and what it names.
    pub fn container_create(&self, body: &[u8]) -> Result<CreatePlan, Refused> {
        let mut v: Value = serde_json::from_slice(body)
            .map_err(|e| refuse(format!("bad container create body: {e}")))?;
        let obj = object(&mut v)?;
        canonicalize(obj, CREATE_KEYS)?;
        add_labels(obj, "Labels", &self.labels)?;
        let mut plan = CreatePlan::default();
        if let Some(nc) = obj
            .get_mut("NetworkingConfig")
            .and_then(Value::as_object_mut)
        {
            canonicalize(nc, &["EndpointsConfig"])?;
            if let Some(endpoints) = nc.get("EndpointsConfig").and_then(Value::as_object) {
                plan.networks.extend(endpoints.keys().cloned());
            }
        }
        let hc = child(obj, "HostConfig")?;
        canonicalize(hc, HOST_CONFIG_KEYS)?;
        if !self.host_access {
            check_host_config(hc)?;
        }
        self.rewrite_mounts(hc, &mut plan)?;
        for key in ["NetworkMode", "PidMode", "IpcMode"] {
            let mode = str_field(hc, key);
            if let Some(c) = mode.strip_prefix("container:") {
                plan.containers.push(c.to_owned());
            } else if key == "NetworkMode"
                && !matches!(mode, "" | "default" | "bridge" | "host" | "none")
            {
                plan.networks.push(mode.to_owned());
            }
        }
        for entry in hc
            .get("VolumesFrom")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            plan.containers
                .push(entry.split(':').next().unwrap_or(entry).to_owned());
        }
        for link in hc
            .get("Links")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let name = link.split(':').next().unwrap_or(link);
            plan.containers
                .push(name.trim_start_matches('/').to_owned());
        }
        match &self.cgroup_parent {
            Some(parent) => {
                hc.insert("CgroupParent".to_owned(), Value::String(parent.clone()));
            }
            None if !self.host_access => {
                hc.remove("CgroupParent");
            }
            None => {}
        }
        if let Some(adj) = self.oom_score_adj {
            let asked = hc.get("OomScoreAdj").and_then(Value::as_i64).unwrap_or(0);
            hc.insert(
                "OomScoreAdj".to_owned(),
                Value::from(asked.max(i64::from(adj))),
            );
        }
        plan.body = serde_json::to_vec(&v).map_err(|e| refuse(e.to_string()))?;
        Ok(plan)
    }

    /// The body dockerd gets for `POST /containers/<id>/exec`.
    pub fn exec_create(&self, body: &[u8]) -> Result<Vec<u8>, Refused> {
        let mut v: Value =
            serde_json::from_slice(body).map_err(|e| refuse(format!("bad exec body: {e}")))?;
        let obj = object(&mut v)?;
        canonicalize(obj, &["Privileged"])?;
        if !self.host_access && obj.get("Privileged").and_then(Value::as_bool) == Some(true) {
            return Err(refuse("privileged exec is not allowed for this job"));
        }
        serde_json::to_vec(&v).map_err(|e| refuse(e.to_string()))
    }

    /// The body dockerd gets for `POST /networks/create`.
    pub fn network_create(&self, body: &[u8]) -> Result<Vec<u8>, Refused> {
        let mut v: Value = serde_json::from_slice(body)
            .map_err(|e| refuse(format!("bad network create body: {e}")))?;
        let obj = object(&mut v)?;
        canonicalize(obj, &["Driver", "Labels"])?;
        let driver = str_field(obj, "Driver").to_owned();
        if !self.host_access && matches!(driver.as_str(), "host" | "macvlan" | "ipvlan") {
            return Err(refuse(format!(
                "{driver} networks are not allowed for this job"
            )));
        }
        add_labels(obj, "Labels", &self.labels)?;
        serde_json::to_vec(&v).map_err(|e| refuse(e.to_string()))
    }

    /// The body dockerd gets for `POST /volumes/create`.
    pub fn volume_create(&self, body: &[u8]) -> Result<Vec<u8>, Refused> {
        let mut v: Value = if body.is_empty() {
            Value::Object(Map::new())
        } else {
            serde_json::from_slice(body)
                .map_err(|e| refuse(format!("bad volume create body: {e}")))?
        };
        let obj = object(&mut v)?;
        canonicalize(obj, &["DriverOpts", "Labels"])?;
        self.check_volume_options(obj.get("DriverOpts").and_then(Value::as_object))?;
        add_labels(obj, "Labels", &self.labels)?;
        serde_json::to_vec(&v).map_err(|e| refuse(e.to_string()))
    }

    /// The query string dockerd gets for `POST /build`: the lease's cgroup
    /// for `RUN` steps and its labels on the image.
    pub fn build_query(&self, query: Option<&str>) -> Result<String, Refused> {
        let mut pairs: Vec<(String, String)> = query.map(url_decode_pairs).unwrap_or_default();
        let mut labels: BTreeMap<String, String> = BTreeMap::new();
        pairs.retain(|(k, v)| match k.as_str() {
            "labels" => {
                if let Ok(m) = serde_json::from_str::<BTreeMap<String, String>>(v) {
                    labels.extend(m);
                }
                false
            }
            "cgroupparent" => false,
            _ => true,
        });
        if !self.host_access && pairs.iter().any(|(k, v)| k == "networkmode" && v == "host") {
            return Err(refuse(
                "networkmode=host builds are not allowed for this job",
            ));
        }
        labels.extend(self.labels.clone());
        pairs.push((
            "labels".to_owned(),
            serde_json::to_string(&labels).map_err(|e| refuse(e.to_string()))?,
        ));
        if let Some(parent) = &self.cgroup_parent {
            pairs.push(("cgroupparent".to_owned(), parent.clone()));
        }
        Ok(url_encode_pairs(&pairs))
    }

    /// The lease id this endpoint serves.
    fn lease(&self) -> &str {
        self.labels.get(LEASE_LABEL).map_or("", String::as_str)
    }

    /// The query string dockerd gets for `POST /<kind>/prune`: only the
    /// lease's own resources.
    pub fn prune_query(&self, query: Option<&str>) -> Result<String, Refused> {
        let mut pairs: Vec<(String, String)> = query.map(url_decode_pairs).unwrap_or_default();
        let mut filters: Map<String, Value> = Map::new();
        pairs.retain(|(k, v)| {
            if k == "filters" {
                if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(v) {
                    filters = m;
                }
                false
            } else {
                true
            }
        });
        let ours = format!("{LEASE_LABEL}={}", self.lease());
        match filters.get_mut("label") {
            Some(Value::Array(list)) => list.push(Value::String(ours)),
            Some(Value::Object(set)) => {
                set.insert(ours, Value::Bool(true));
            }
            _ => {
                filters.insert("label".to_owned(), Value::Array(vec![Value::String(ours)]));
            }
        }
        pairs.push((
            "filters".to_owned(),
            serde_json::to_string(&filters).map_err(|e| refuse(e.to_string()))?,
        ));
        Ok(url_encode_pairs(&pairs))
    }
}

fn url_decode_pairs(q: &str) -> Vec<(String, String)> {
    form_urlencoded::parse(q.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn url_encode_pairs(pairs: &[(String, String)]) -> String {
    form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish()
}

/// Who may use a lease's endpoint: processes inside the lease's cgroup
/// (the job, and containers dockerd placed under it).
#[derive(Debug, Clone)]
pub struct PeerCheck {
    /// `/proc`, or a stand-in in tests.
    pub proc_root: PathBuf,
    /// The lease cgroup as `/proc/<pid>/cgroup` names it.
    pub cgroup: String,
}

impl PeerCheck {
    fn admits(&self, pid: Option<i32>) -> bool {
        let Some(pid) = pid.filter(|p| *p > 0) else {
            return false;
        };
        let Ok(text) = std::fs::read_to_string(self.proc_root.join(pid.to_string()).join("cgroup"))
        else {
            return false;
        };
        crate::drone::memlimit::unified_path(&text).is_some_and(|path| {
            path == self.cgroup
                || path
                    .strip_prefix(self.cgroup.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    }
}

fn empty() -> Body {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed_unsync()
}

fn full(bytes: Vec<u8>) -> Body {
    Full::new(Bytes::from(bytes))
        .map_err(|never: Infallible| match never {})
        .boxed_unsync()
}

fn error_response(status: StatusCode, message: &str) -> Response<Body> {
    let body = serde_json::json!({ "message": message })
        .to_string()
        .into_bytes();
    let mut resp = Response::new(full(body));
    *resp.status_mut() = status;
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    resp
}

/// Send `req` to dockerd on its own connection.
async fn forward(req: Request<Body>, upstream: &Path) -> Result<Response<Incoming>, String> {
    let stream = UnixStream::connect(upstream)
        .await
        .map_err(|e| format!("connect {}: {e}", upstream.display()))?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| format!("dockerd handshake: {e}"))?;
    tokio::spawn(async move {
        let _ = conn.with_upgrades().await;
    });
    sender
        .send_request(req)
        .await
        .map_err(|e| format!("dockerd request: {e}"))
}

/// dockerd's cgroup driver (`cgroupfs` or `systemd`), from `GET /info`.
/// How long the drone waits for dockerd's `/info` before a lease goes on
/// without an endpoint.
const INFO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub async fn cgroup_driver(upstream: &Path) -> Option<String> {
    let info = async {
        let (status, body) = get_json(upstream, "/info").await.ok()?;
        (status == StatusCode::OK)
            .then(|| body.get("CgroupDriver")?.as_str().map(str::to_owned))
            .flatten()
    };
    tokio::time::timeout(INFO_TIMEOUT, info)
        .await
        .ok()
        .flatten()
}

/// `GET path` on dockerd: the status and the JSON body (`null` when it is
/// not JSON).
async fn get_json(upstream: &Path, path: &str) -> Result<(StatusCode, Value), String> {
    let req = Request::get(path)
        .header(hyper::header::HOST, "docker")
        .body(empty())
        .map_err(|e| e.to_string())?;
    let resp = forward(req, upstream).await?;
    let status = resp.status();
    let bytes = Limited::new(resp.into_body(), MAX_REWRITE_BODY)
        .collect()
        .await
        .map_err(|e| e.to_string())?
        .to_bytes();
    Ok((
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    ))
}

/// The `borg.lease` label of what `path` inspects (labels at `pointer`):
/// `Ok(None)` when dockerd does not know it (it will say so itself), and
/// `Ok(Some(None))` when it carries no lease label.
async fn lease_of(
    upstream: &Path,
    path: &str,
    pointer: &str,
) -> Result<Option<Option<String>>, Refused> {
    let (status, body) = get_json(upstream, path)
        .await
        .map_err(|e| refuse(format!("dockerd: {e}")))?;
    if status == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(refuse(format!("dockerd answered {status} for {path}")));
    }
    Ok(Some(
        body.pointer(pointer)
            .and_then(|labels| labels.get(LEASE_LABEL))
            .and_then(Value::as_str)
            .map(str::to_owned),
    ))
}

fn path_segment(s: &str) -> String {
    form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

impl LeasePolicy {
    /// Containers and execs must be the lease's own; networks and volumes the
    /// lease's or nobody's (shared ones like `bridge`).
    async fn check_target(&self, target: &Target) -> Result<(), Refused> {
        let ours = Some(self.lease().to_owned());
        match target {
            Target::Container(id) => {
                let path = format!("/containers/{}/json", path_segment(id));
                match lease_of(&self.upstream, &path, "/Config/Labels").await? {
                    None => Ok(()),
                    Some(owner) if owner == ours => Ok(()),
                    Some(_) => Err(refuse(format!("container {id} belongs to another job"))),
                }
            }
            Target::Exec(id) => {
                let path = format!("/exec/{}/json", path_segment(id));
                let (status, body) = get_json(&self.upstream, &path)
                    .await
                    .map_err(|e| refuse(format!("dockerd: {e}")))?;
                if status == StatusCode::NOT_FOUND {
                    return Ok(());
                }
                let container = body
                    .get("ContainerID")
                    .and_then(Value::as_str)
                    .ok_or_else(|| refuse(format!("exec {id} names no container")))?
                    .to_owned();
                Box::pin(self.check_target(&Target::Container(container))).await
            }
            Target::Network(id) | Target::Volume(id) => {
                let (path, kind) = match target {
                    Target::Network(_) => (format!("/networks/{}", path_segment(id)), "network"),
                    _ => (format!("/volumes/{}", path_segment(id)), "volume"),
                };
                match lease_of(&self.upstream, &path, "/Labels").await? {
                    None | Some(None) => Ok(()),
                    Some(owner) if owner == ours => Ok(()),
                    Some(_) => Err(refuse(format!("{kind} {id} belongs to another job"))),
                }
            }
        }
    }

    /// Check what a container create names, and create its missing named
    /// volumes with the lease's labels so cleanup removes them.
    async fn prepare_create(&self, plan: &CreatePlan) -> Result<(), Refused> {
        for c in &plan.containers {
            self.check_target(&Target::Container(c.clone())).await?;
        }
        for n in &plan.networks {
            self.check_target(&Target::Network(n.clone())).await?;
        }
        for v in &plan.volumes {
            let path = format!("/volumes/{}", path_segment(v));
            match lease_of(&self.upstream, &path, "/Labels").await? {
                Some(None) => {}
                Some(owner) if owner.as_deref() == Some(self.lease()) => {}
                Some(_) => return Err(refuse(format!("volume {v} belongs to another job"))),
                None => {
                    let body = serde_json::json!({ "Name": v, "Labels": self.labels });
                    let req = Request::post("/volumes/create")
                        .header(hyper::header::HOST, "docker")
                        .header(CONTENT_TYPE, "application/json")
                        .body(full(body.to_string().into_bytes()))
                        .map_err(|e| refuse(e.to_string()))?;
                    let resp = forward(req, &self.upstream)
                        .await
                        .map_err(|e| refuse(format!("dockerd: {e}")))?;
                    if !resp.status().is_success() {
                        return Err(refuse(format!(
                            "creating volume {v} failed: {}",
                            resp.status()
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}

async fn rewrite_body(
    req: Request<Incoming>,
    rewrite: impl FnOnce(&[u8]) -> Result<Vec<u8>, Refused>,
) -> Result<Request<Body>, Refused> {
    let (mut parts, body) = req.into_parts();
    let bytes = Limited::new(body, MAX_REWRITE_BODY)
        .collect()
        .await
        .map_err(|e| refuse(format!("reading the request: {e}")))?
        .to_bytes();
    let new = rewrite(&bytes)?;
    parts.headers.remove(TRANSFER_ENCODING);
    parts
        .headers
        .insert(CONTENT_LENGTH, HeaderValue::from(new.len()));
    parts
        .headers
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(Request::from_parts(parts, full(new)))
}

/// The request dockerd gets, or why it gets none.
async fn read_body(
    req: Request<Incoming>,
) -> Result<(hyper::http::request::Parts, Bytes), Refused> {
    let (parts, body) = req.into_parts();
    let bytes = Limited::new(body, MAX_REWRITE_BODY)
        .collect()
        .await
        .map_err(|e| refuse(format!("reading the request: {e}")))?
        .to_bytes();
    Ok((parts, bytes))
}

fn with_body(mut parts: hyper::http::request::Parts, body: Vec<u8>) -> Request<Body> {
    parts.headers.remove(TRANSFER_ENCODING);
    parts
        .headers
        .insert(CONTENT_LENGTH, HeaderValue::from(body.len()));
    parts
        .headers
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Request::from_parts(parts, full(body))
}

fn with_query(req: Request<Incoming>, query: &str) -> Result<Request<Body>, Refused> {
    let uri = format!("{}?{query}", req.uri().path())
        .parse::<hyper::Uri>()
        .map_err(|e| refuse(format!("query: {e}")))?;
    let (mut parts, body) = req.into_parts();
    parts.uri = uri;
    Ok(Request::from_parts(parts, body.boxed_unsync()))
}

/// The request dockerd gets, or why it gets none.
async fn rewrite(req: Request<Incoming>, policy: &LeasePolicy) -> Result<Request<Body>, Refused> {
    match route(req.method(), req.uri().path()) {
        Route::HostOnly if !policy.host_access => {
            Err(refuse("plugins and swarm are not available to this job"))
        }
        Route::HostOnly | Route::Forward => Ok(req.map(BodyExt::boxed_unsync)),
        Route::Owned(target) => {
            policy.check_target(&target).await?;
            Ok(req.map(BodyExt::boxed_unsync))
        }
        Route::ContainerCreate => {
            let (parts, bytes) = read_body(req).await?;
            let plan = policy.container_create(&bytes)?;
            policy.prepare_create(&plan).await?;
            Ok(with_body(parts, plan.body))
        }
        Route::ExecCreate(id) => {
            policy.check_target(&Target::Container(id)).await?;
            rewrite_body(req, |b| policy.exec_create(b)).await
        }
        Route::NetworkConnect(id) => {
            policy.check_target(&Target::Network(id)).await?;
            let (parts, bytes) = read_body(req).await?;
            let mut v: Value = serde_json::from_slice(&bytes)
                .map_err(|e| refuse(format!("bad network connect body: {e}")))?;
            let obj = object(&mut v)?;
            canonicalize(obj, &["Container"])?;
            let container = str_field(obj, "Container").to_owned();
            policy.check_target(&Target::Container(container)).await?;
            Ok(with_body(
                parts,
                serde_json::to_vec(&v).map_err(|e| refuse(e.to_string()))?,
            ))
        }
        Route::NetworkCreate => rewrite_body(req, |b| policy.network_create(b)).await,
        Route::VolumeCreate => rewrite_body(req, |b| policy.volume_create(b)).await,
        Route::Prune => {
            let query = policy.prune_query(req.uri().query())?;
            with_query(req, &query)
        }
        Route::Build => {
            let query = policy.build_query(req.uri().query())?;
            with_query(req, &query)
        }
    }
}

/// Copy bytes both ways between an upgraded client and dockerd connection
/// (`docker exec`, `attach`, BuildKit sessions) until either side closes or
/// the endpoint stops.
async fn splice(
    client: hyper::upgrade::OnUpgrade,
    daemon: hyper::upgrade::OnUpgrade,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let (Ok(client), Ok(daemon)) = tokio::join!(client, daemon) else {
        return;
    };
    let (mut client, mut daemon) = (TokioIo::new(client), TokioIo::new(daemon));
    tokio::select! {
        _ = tokio::io::copy_bidirectional(&mut client, &mut daemon) => {}
        () = async { let _ = stop.wait_for(|s| *s).await; } => {}
    }
}

async fn handle(
    mut req: Request<Incoming>,
    policy: &LeasePolicy,
    stop: tokio::sync::watch::Receiver<bool>,
) -> Response<Body> {
    let client_upgrade = req
        .headers()
        .contains_key(UPGRADE)
        .then(|| hyper::upgrade::on(&mut req));
    let (method, path) = (req.method().clone(), req.uri().path().to_owned());
    let req = match rewrite(req, policy).await {
        Ok(req) => req,
        Err(Refused(why)) => {
            tracing::warn!("docker endpoint refused {method} {path}: {why}");
            return error_response(StatusCode::FORBIDDEN, &why);
        }
    };
    let mut resp = match forward(req, &policy.upstream).await {
        Ok(resp) => resp,
        Err(e) => return error_response(StatusCode::BAD_GATEWAY, &format!("borg: {e}")),
    };
    if resp.status() == StatusCode::SWITCHING_PROTOCOLS {
        if let Some(client) = client_upgrade {
            let daemon = hyper::upgrade::on(&mut resp);
            tokio::spawn(splice(client, daemon, stop));
        }
        let (parts, _) = resp.into_parts();
        return Response::from_parts(parts, empty());
    }
    resp.map(BodyExt::boxed_unsync)
}

async fn serve(
    stream: UnixStream,
    policy: Arc<LeasePolicy>,
    stop: tokio::sync::watch::Receiver<bool>,
) {
    let service = hyper::service::service_fn(move |req| {
        let (policy, stop) = (Arc::clone(&policy), stop.clone());
        async move { Ok::<_, Infallible>(handle(req, &policy, stop).await) }
    });
    if let Err(e) = hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .with_upgrades()
        .await
    {
        tracing::debug!("docker endpoint connection: {e}");
    }
}

/// One lease's running endpoint. Stop it before the lease's Docker
/// resources are removed, so the job cannot create more meanwhile.
#[derive(Debug)]
pub struct LeaseEndpoint {
    socket: PathBuf,
    stop: tokio::sync::watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl LeaseEndpoint {
    /// Listen on `policy.socket` (replacing a leftover) and serve until
    /// stopped. The socket is world-connectable; [`PeerCheck`] decides.
    pub fn start(policy: LeasePolicy, peers: PeerCheck) -> io::Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let socket = policy.socket.clone();
        if let Some(dir) = socket.parent() {
            std::fs::create_dir_all(dir)?;
        }
        match std::fs::remove_file(&socket) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let listener = UnixListener::bind(&socket)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o666))?;
        let (stop, stop_rx) = tokio::sync::watch::channel(false);
        let policy = Arc::new(policy);
        let task = tokio::spawn(async move {
            let mut conns = JoinSet::new();
            let mut stopped = stop_rx.clone();
            loop {
                tokio::select! {
                    () = async { let _ = stopped.wait_for(|s| *s).await; } => break,
                    accepted = listener.accept() => match accepted {
                        Ok((stream, _)) => {
                            let pid = stream.peer_cred().ok().and_then(|c| c.pid());
                            if !peers.admits(pid) {
                                tracing::warn!("docker endpoint: refused a connection from pid {pid:?} outside {}", peers.cgroup);
                                continue;
                            }
                            conns.spawn(serve(stream, Arc::clone(&policy), stop_rx.clone()));
                        }
                        Err(e) => {
                            tracing::warn!("docker endpoint accept: {e}");
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                    },
                    Some(_) = conns.join_next(), if !conns.is_empty() => {}
                }
            }
            conns.shutdown().await;
        });
        Ok(Self {
            socket,
            stop,
            task: Some(task),
        })
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Stop serving, close every connection and remove the socket.
    pub async fn stop(mut self) {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

impl Drop for LeaseEndpoint {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::json;

    use super::*;

    const LEASE: &str = "lease1";

    fn policy(dir: &Path, host_access: bool) -> LeasePolicy {
        LeasePolicy {
            labels: BTreeMap::from([
                (LEASE_LABEL.to_owned(), LEASE.to_owned()),
                ("borg.scope".to_owned(), "pr".to_owned()),
            ]),
            cgroup_parent: Some("/svc/lease-lease1".to_owned()),
            oom_score_adj: Some(125),
            host_access,
            socket: dir.join("slot-0.sock"),
            upstream: dir.join("dockerd.sock"),
            protected: PROTECTED_ROOTS.iter().map(PathBuf::from).collect(),
        }
    }

    fn create(p: &LeasePolicy, body: Value) -> Result<Value, Refused> {
        p.container_create(body.to_string().as_bytes())
            .map(|plan| serde_json::from_slice(&plan.body).unwrap())
    }

    #[test]
    fn versioned_paths_route_like_unversioned_ones() {
        assert_eq!(
            unversioned("/v1.47/containers/create"),
            "/containers/create"
        );
        assert_eq!(unversioned("/containers/create"), "/containers/create");
        assert_eq!(unversioned("/volumes/v1/x"), "/volumes/v1/x");
        assert_eq!(
            route(&Method::POST, "/v1.47/containers/create"),
            Route::ContainerCreate
        );
        assert_eq!(
            route(&Method::POST, "/containers/abc/exec"),
            Route::ExecCreate("abc".to_owned())
        );
        assert_eq!(
            route(&Method::POST, "/containers/abc/start"),
            Route::Owned(Target::Container("abc".to_owned()))
        );
        assert_eq!(
            route(&Method::GET, "/v1.47/exec/e1/json"),
            Route::Owned(Target::Exec("e1".to_owned()))
        );
        assert_eq!(route(&Method::POST, "/volumes/prune"), Route::Prune);
        assert_eq!(
            route(&Method::DELETE, "/networks/n1"),
            Route::Owned(Target::Network("n1".to_owned()))
        );
        assert_eq!(
            route(&Method::POST, "/networks/n1/connect"),
            Route::NetworkConnect("n1".to_owned())
        );
        assert_eq!(route(&Method::POST, "/v1.41/build"), Route::Build);
        assert_eq!(route(&Method::POST, "/plugins/pull"), Route::HostOnly);
        assert_eq!(route(&Method::GET, "/plugins"), Route::Forward);
        assert_eq!(route(&Method::GET, "/containers/json"), Route::Forward);
    }

    #[test]
    fn a_container_lands_in_the_lease_cgroup_with_its_labels_and_score() {
        let dir = tempfile::tempdir().unwrap();
        let p = policy(dir.path(), false);
        let out = create(
            &p,
            json!({"Image": "postgres:16", "Labels": {"app": "db"}, "HostConfig": {"CgroupParent": "/", "OomScoreAdj": -500, "Memory": 1024}}),
        )
        .unwrap();
        assert_eq!(out["HostConfig"]["CgroupParent"], "/svc/lease-lease1");
        assert_eq!(out["HostConfig"]["OomScoreAdj"], 125);
        assert_eq!(out["HostConfig"]["Memory"], 1024);
        assert_eq!(out["Labels"]["app"], "db");
        assert_eq!(out["Labels"][LEASE_LABEL], LEASE);
        // A higher score the job asked for stays.
        let out = create(
            &p,
            json!({"HostConfig": {"OomScoreAdj": 900}, "Labels": null}),
        )
        .unwrap();
        assert_eq!(out["HostConfig"]["OomScoreAdj"], 900);
        assert_eq!(out["Labels"][LEASE_LABEL], LEASE);
    }

    #[test]
    fn host_level_requests_are_refused_without_host_access() {
        let dir = tempfile::tempdir().unwrap();
        let p = policy(dir.path(), false);
        for hc in [
            json!({"Privileged": true}),
            json!({"NetworkMode": "host"}),
            json!({"PidMode": "host"}),
            json!({"IpcMode": "host"}),
            json!({"UsernsMode": "host"}),
            json!({"CgroupnsMode": "host"}),
            json!({"Devices": [{"PathOnHost": "/dev/kvm"}]}),
            json!({"DeviceRequests": [{"Driver": "nvidia"}]}),
            json!({"CapAdd": ["cap_sys_admin"]}),
            json!({"CapAdd": ["ALL"]}),
            json!({"SecurityOpt": ["seccomp:unconfined"]}),
            json!({"SecurityOpt": ["apparmor=unconfined"]}),
            json!({"Runtime": "sysbox-runc"}),
            json!({"Binds": ["/:/host"]}),
            json!({"Binds": ["/etc:/x:ro"]}),
            json!({"Binds": ["/var:/x"]}),
            json!({"Binds": ["/var/lib/docker/volumes:/x"]}),
            json!({"Mounts": [{"Type": "bind", "Source": "/root", "Target": "/x"}]}),
            json!({"Mounts": [{"Type": "volume", "Source": "v", "Target": "/x", "VolumeOptions": {"DriverConfig": {"Options": {"type": "none", "o": "bind", "device": "/etc"}}}}]}),
        ] {
            let err = create(&p, json!({"Image": "x", "HostConfig": hc.clone()})).unwrap_err();
            assert!(err.0.starts_with("borg: "), "{hc}: {err:?}");
        }
        // Allowed: ordinary limits, capabilities and workspace binds.
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let ok = create(
            &p,
            json!({"HostConfig": {"CapAdd": ["NET_BIND_SERVICE"], "NetworkMode": "borg-lease1_default", "Binds": [format!("{}:/w", work.display()), "named:/data"]}}),
        );
        assert!(ok.is_ok(), "{ok:?}");
    }

    #[test]
    fn host_access_jobs_may_ask_for_host_level_settings() {
        let dir = tempfile::tempdir().unwrap();
        let p = policy(dir.path(), true);
        let out = create(&p, json!({"HostConfig": {"Privileged": true, "NetworkMode": "host", "Binds": ["/etc:/etc"]}})).unwrap();
        // Still inside the lease.
        assert_eq!(out["HostConfig"]["CgroupParent"], "/svc/lease-lease1");
    }

    #[test]
    fn docker_socket_binds_point_at_the_lease_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let p = policy(dir.path(), false);
        let sock = p.socket.to_string_lossy().into_owned();
        let out = create(
            &p,
            json!({"HostConfig": {
                "Binds": ["/var/run/docker.sock:/var/run/docker.sock", "/run/docker.sock:/d.sock:ro"],
                "Mounts": [{"Type": "bind", "Source": "/var/run/docker.sock", "Target": "/s"}]
            }}),
        )
        .unwrap();
        assert_eq!(
            out["HostConfig"]["Binds"][0],
            format!("{sock}:/var/run/docker.sock")
        );
        assert_eq!(out["HostConfig"]["Binds"][1], format!("{sock}:/d.sock:ro"));
        assert_eq!(out["HostConfig"]["Mounts"][0]["Source"], sock);
    }

    #[test]
    fn a_symlink_to_a_system_path_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let p = policy(dir.path(), false);
        let link = dir.path().join("innocent");
        std::os::unix::fs::symlink("/etc", &link).unwrap();
        let err = create(
            &p,
            json!({"HostConfig": {"Binds": [format!("{}/ssl:/x", link.display())]}}),
        )
        .unwrap_err();
        assert!(err.0.contains("not allowed"), "{err:?}");
        // A missing source under the workspace is fine; `..` in it is not.
        let missing = dir.path().join("new/dir");
        assert!(
            create(
                &p,
                json!({"HostConfig": {"Binds": [format!("{}:/x", missing.display())]}})
            )
            .is_ok()
        );
        let sneaky = format!("{}/nope/../../../../etc:/x", dir.path().display());
        assert!(create(&p, json!({"HostConfig": {"Binds": [sneaky]}})).is_err());
    }

    #[test]
    fn exec_networks_volumes_and_builds_follow_the_policy() {
        let dir = tempfile::tempdir().unwrap();
        let p = policy(dir.path(), false);
        assert!(
            p.exec_create(br#"{"Privileged":true,"Cmd":["sh"]}"#)
                .is_err()
        );
        assert!(p.exec_create(br#"{"Cmd":["sh"]}"#).is_ok());

        let net: Value =
            serde_json::from_slice(&p.network_create(br#"{"Name":"n"}"#).unwrap()).unwrap();
        assert_eq!(net["Labels"][LEASE_LABEL], LEASE);
        assert!(
            p.network_create(br#"{"Name":"n","Driver":"macvlan"}"#)
                .is_err()
        );

        let vol: Value = serde_json::from_slice(&p.volume_create(b"").unwrap()).unwrap();
        assert_eq!(vol["Labels"][LEASE_LABEL], LEASE);
        assert!(
            p.volume_create(br#"{"DriverOpts":{"o":"bind","device":"/"}}"#)
                .is_err()
        );

        let q = p
            .build_query(Some(
                "t=img%3A1&labels=%7B%22a%22%3A%22b%22%7D&cgroupparent=%2F",
            ))
            .unwrap();
        let pairs = url_decode_pairs(&q);
        let get = |k: &str| {
            pairs
                .iter()
                .filter(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(get("t"), ["img:1"]);
        assert_eq!(get("cgroupparent"), ["/svc/lease-lease1"]);
        let labels: BTreeMap<String, String> = serde_json::from_str(&get("labels")[0]).unwrap();
        assert_eq!(labels.get("a").map(String::as_str), Some("b"));
        assert_eq!(labels.get(LEASE_LABEL).map(String::as_str), Some(LEASE));
        assert!(p.build_query(Some("networkmode=host")).is_err());
    }

    #[test]
    fn field_names_in_any_case_are_checked_as_dockerd_reads_them() {
        let dir = tempfile::tempdir().unwrap();
        let p = policy(dir.path(), false);
        for body in [
            json!({"hostconfig": {"privileged": true}}),
            json!({"HostConfig": {"PRIVILEGED": true}}),
            json!({"HostConfig": {"networkmode": "host"}}),
            json!({"HostConfig": {"capadd": ["SYS_ADMIN"]}}),
            json!({"HostConfig": {"mounts": [{"type": "bind", "source": "/etc", "target": "/x"}]}}),
            // Two spellings of one field.
            json!({"HostConfig": {"Privileged": false, "privileged": true}}),
        ] {
            assert!(create(&p, body.clone()).is_err(), "{body}");
        }
        // Go folds U+017F to s: this is SecurityOpt to dockerd.
        let long_s_key = "{\"HostConfig\": {\"\u{17f}ecurityOpt\": [\"seccomp=unconfined\"]}}";
        assert!(p.container_create(long_s_key.as_bytes()).is_err());
        let long_s = "{\"HostConfig\": {\"Privileged\u{17f}\": true}}";
        // A key that only folds to a longer name is left alone (dockerd
        // ignores it too).
        assert!(p.container_create(long_s.as_bytes()).is_ok());
        let out = create(&p, json!({"hostconfig": {"cgroupparent": "/"}})).unwrap();
        assert_eq!(out["HostConfig"]["CgroupParent"], "/svc/lease-lease1");
        assert!(p.exec_create(br#"{"privileged":true}"#).is_err());
    }

    #[test]
    fn binds_are_forwarded_with_symlinks_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let p = policy(dir.path(), false);
        let real = dir.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let out = create(
            &p,
            json!({"HostConfig": {"Binds": [format!("{}:/w:ro", link.display())]}}),
        )
        .unwrap();
        let canonical = std::fs::canonicalize(&real).unwrap();
        assert_eq!(
            out["HostConfig"]["Binds"][0],
            format!("{}:/w:ro", canonical.display())
        );
        let plan = p
            .container_create(json!({"HostConfig": {"Binds": ["cache:/c"], "Mounts": [{"Type": "volume", "Source": "data", "Target": "/d"}], "NetworkMode": "borg-lease1_default", "VolumesFrom": ["sib:ro"], "Links": ["/db:db"]}, "NetworkingConfig": {"EndpointsConfig": {"net2": {}}}}).to_string().as_bytes())
            .unwrap();
        assert_eq!(plan.volumes, ["cache", "data"]);
        assert_eq!(plan.networks, ["net2", "borg-lease1_default"]);
        assert_eq!(plan.containers, ["sib", "db"]);
    }

    #[test]
    fn prune_reaches_only_the_lease() {
        let dir = tempfile::tempdir().unwrap();
        let p = policy(dir.path(), false);
        for (query, want) in [
            (None, json!({"label": ["borg.lease=lease1"]})),
            (
                Some("filters=%7B%22until%22%3A%5B%2224h%22%5D%7D"),
                json!({"until": ["24h"], "label": ["borg.lease=lease1"]}),
            ),
            (
                Some("filters=%7B%22label%22%3A%7B%22a%3Db%22%3Atrue%7D%7D"),
                json!({"label": {"a=b": true, "borg.lease=lease1": true}}),
            ),
        ] {
            let q = p.prune_query(query).unwrap();
            let pairs = url_decode_pairs(&q);
            let filters: Value =
                serde_json::from_str(&pairs.iter().find(|(k, _)| k == "filters").unwrap().1)
                    .unwrap();
            assert_eq!(filters, want, "{query:?}");
        }
    }

    #[test]
    fn pull_request_jobs_never_get_host_access() {
        let s = DockerProxySettings {
            host_access_scopes: vec![Scope::Main, Scope::Pr],
            ..DockerProxySettings::default()
        };
        assert!(s.validate().is_err());
        let s = DockerProxySettings {
            host_access_scopes: vec![Scope::Main],
            ..DockerProxySettings::default()
        };
        assert!(s.validate().is_ok());
        assert!(s.host_access(Scope::Main));
        assert!(!s.host_access(Scope::Pr));
        assert!(!s.host_access(Scope::Deploy));
    }

    /// What the fake dockerd saw: method, path with query, body.
    type Seen = Arc<Mutex<Vec<(String, String, String)>>>;

    /// A dockerd stand-in: answers `/info`, upgrades any request that asks
    /// to and echoes on the upgraded stream, and answers 201 otherwise.
    async fn fake_dockerd(socket: &Path) -> Seen {
        let seen: Seen = Arc::default();
        let listener = UnixListener::bind(socket).unwrap();
        let log = Arc::clone(&seen);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let log = Arc::clone(&log);
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(move |mut req: Request<Incoming>| {
                        let log = Arc::clone(&log);
                        async move {
                            let upgrade = req
                                .headers()
                                .contains_key(UPGRADE)
                                .then(|| hyper::upgrade::on(&mut req));
                            let (method, uri) = (req.method().to_string(), req.uri().to_string());
                            let method_copy = method.clone();
                            let body = req.into_body().collect().await.unwrap().to_bytes();
                            let path = uri.clone();
                            log.lock().unwrap().push((
                                method,
                                uri,
                                String::from_utf8_lossy(&body).into_owned(),
                            ));
                            if let Some(up) = upgrade {
                                tokio::spawn(async move {
                                    let mut io = TokioIo::new(up.await.unwrap());
                                    let (mut r, mut w) = tokio::io::split(&mut io);
                                    let _ = tokio::io::copy(&mut r, &mut w).await;
                                });
                                let mut resp = Response::new(empty());
                                *resp.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
                                resp.headers_mut()
                                    .insert(UPGRADE, HeaderValue::from_static("tcp"));
                                resp.headers_mut().insert(
                                    hyper::header::CONNECTION,
                                    HeaderValue::from_static("Upgrade"),
                                );
                                return Ok::<_, Infallible>(resp);
                            }
                            let get = method_copy == "GET";
                            let (status, body): (StatusCode, &[u8]) = if path.ends_with("/info") {
                                (StatusCode::OK, br#"{"CgroupDriver":"cgroupfs"}"#)
                            } else if get && path.starts_with("/exec/") {
                                (StatusCode::OK, br#"{"ContainerID":"mine"}"#)
                            } else if get && path.starts_with("/containers/mine/") {
                                (
                                    StatusCode::OK,
                                    br#"{"Config":{"Labels":{"borg.lease":"lease1"}}}"#,
                                )
                            } else if get && path.starts_with("/containers/theirs/") {
                                (
                                    StatusCode::OK,
                                    br#"{"Config":{"Labels":{"borg.lease":"other"}}}"#,
                                )
                            } else if get && path.starts_with("/volumes/") {
                                (StatusCode::NOT_FOUND, br#"{"message":"no such volume"}"#)
                            } else if get && path.starts_with("/containers/json") {
                                (StatusCode::OK, b"[]")
                            } else {
                                (StatusCode::CREATED, br#"{"Id":"c1"}"#)
                            };
                            let mut resp = Response::new(full(body.to_vec()));
                            *resp.status_mut() = status;
                            Ok(resp)
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .with_upgrades()
                        .await;
                });
            }
        });
        seen
    }

    /// A proc root in which this test process sits in `cgroup`.
    fn proc_with_self_in(dir: &Path, cgroup: &str) -> PathBuf {
        let root = dir.join("proc");
        let me = root.join(std::process::id().to_string());
        std::fs::create_dir_all(&me).unwrap();
        std::fs::write(me.join("cgroup"), format!("0::{cgroup}\n")).unwrap();
        root
    }

    async fn send(socket: &Path, req: Request<Body>) -> Result<Response<Incoming>, String> {
        forward(req, socket).await
    }

    fn post(path: &str, body: Value) -> Request<Body> {
        Request::post(path)
            .header(hyper::header::HOST, "docker")
            .header(CONTENT_TYPE, "application/json")
            .body(full(body.to_string().into_bytes()))
            .unwrap()
    }

    #[tokio::test]
    async fn the_endpoint_rewrites_creates_refuses_and_forwards() {
        let dir = tempfile::tempdir().unwrap();
        let p = policy(dir.path(), false);
        let seen = fake_dockerd(&p.upstream).await;
        assert_eq!(
            cgroup_driver(&p.upstream).await.as_deref(),
            Some("cgroupfs")
        );
        let peers = PeerCheck {
            proc_root: proc_with_self_in(dir.path(), "/svc/lease-lease1/job"),
            cgroup: "/svc/lease-lease1".to_owned(),
        };
        let endpoint = LeaseEndpoint::start(p.clone(), peers).unwrap();

        let resp = send(
            endpoint.socket(),
            post(
                "/v1.47/containers/create?name=db",
                json!({"Image": "postgres"}),
            ),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let resp = send(
            endpoint.socket(),
            post(
                "/containers/create",
                json!({"HostConfig": {"Privileged": true}}),
            ),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("privileged"));
        let resp = send(
            endpoint.socket(),
            Request::get("/containers/json")
                .header(hyper::header::HOST, "docker")
                .body(empty())
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Another lease's container is out of reach; this lease's is not.
        let theirs = Request::get("/containers/theirs/json")
            .header(hyper::header::HOST, "docker")
            .body(empty())
            .unwrap();
        assert_eq!(
            send(endpoint.socket(), theirs).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        let stop = Request::post("/containers/mine/stop")
            .header(hyper::header::HOST, "docker")
            .body(empty())
            .unwrap();
        assert_eq!(
            send(endpoint.socket(), stop).await.unwrap().status(),
            StatusCode::CREATED
        );
        let resp = send(
            endpoint.socket(),
            post(
                "/containers/create",
                json!({"HostConfig": {"VolumesFrom": ["theirs:ro"]}}),
            ),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        // A named volume that does not exist yet is created with the labels.
        let resp = send(
            endpoint.socket(),
            post(
                "/containers/create",
                json!({"HostConfig": {"Binds": ["cache:/c"]}}),
            ),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        // Prune only reaches the lease's own resources.
        let prune = Request::post("/volumes/prune")
            .header(hyper::header::HOST, "docker")
            .body(empty())
            .unwrap();
        assert_eq!(
            send(endpoint.socket(), prune).await.unwrap().status(),
            StatusCode::CREATED
        );

        let seen = seen.lock().unwrap().clone();
        let vol = seen
            .iter()
            .find(|(m, u, _)| m == "POST" && u == "/volumes/create")
            .expect("the named volume was created first");
        let vol: Value = serde_json::from_str(&vol.2).unwrap();
        assert_eq!(
            (vol["Name"].as_str(), vol["Labels"][LEASE_LABEL].as_str()),
            (Some("cache"), Some(LEASE))
        );
        let pruned = seen
            .iter()
            .find(|(_, u, _)| u.starts_with("/volumes/prune"))
            .unwrap();
        assert!(pruned.1.contains("borg.lease%3Dlease1"), "{}", pruned.1);
        let creates: Vec<_> = seen
            .iter()
            .filter(|(_, u, _)| u.contains("/containers/create"))
            .collect();
        assert_eq!(
            creates.len(),
            2,
            "the refused create never reached dockerd: {seen:?}"
        );
        assert_eq!(creates[0].1, "/v1.47/containers/create?name=db");
        let sent: Value = serde_json::from_str(&creates[0].2).unwrap();
        assert_eq!(sent["HostConfig"]["CgroupParent"], "/svc/lease-lease1");
        assert_eq!(sent["Labels"][LEASE_LABEL], LEASE);
        assert!(
            seen.iter()
                .any(|(m, u, _)| m == "GET" && u == "/containers/json")
        );
        endpoint.stop().await;
        assert!(!p.socket.exists(), "stop removes the socket");
    }

    #[tokio::test]
    async fn upgraded_streams_are_spliced_through() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = tempfile::tempdir().unwrap();
        let p = policy(dir.path(), false);
        let _seen = fake_dockerd(&p.upstream).await;
        let peers = PeerCheck {
            proc_root: proc_with_self_in(dir.path(), "/svc/lease-lease1/job"),
            cgroup: "/svc/lease-lease1".to_owned(),
        };
        let endpoint = LeaseEndpoint::start(p, peers).unwrap();
        let req = Request::post("/exec/e1/start")
            .header(hyper::header::HOST, "docker")
            .header(hyper::header::CONNECTION, "Upgrade")
            .header(UPGRADE, "tcp")
            .body(empty())
            .unwrap();
        let mut resp = send(endpoint.socket(), req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SWITCHING_PROTOCOLS);
        let mut io = TokioIo::new(hyper::upgrade::on(&mut resp).await.unwrap());
        io.write_all(b"hello").await.unwrap();
        let mut buf = [0u8; 5];
        io.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");
        endpoint.stop().await;
    }

    #[tokio::test]
    async fn a_process_outside_the_lease_cannot_connect() {
        let dir = tempfile::tempdir().unwrap();
        let p = policy(dir.path(), false);
        let seen = fake_dockerd(&p.upstream).await;
        // This process sits in another lease's cgroup (or one whose name
        // merely starts with this lease's).
        for elsewhere in ["/svc/lease-other/job", "/svc/lease-lease10/job"] {
            let peers = PeerCheck {
                proc_root: proc_with_self_in(dir.path(), elsewhere),
                cgroup: "/svc/lease-lease1".to_owned(),
            };
            let endpoint = LeaseEndpoint::start(p.clone(), peers).unwrap();
            let got = send(endpoint.socket(), post("/containers/create", json!({}))).await;
            assert!(got.is_err(), "{elsewhere}: {got:?}");
            endpoint.stop().await;
        }
        assert!(seen.lock().unwrap().is_empty());
    }

    /// Against a real dockerd, opt-in: `BORG_DOCKER_IT=1` (and optionally
    /// `BORG_DOCKER_IT_IMAGE`, default `busybox:1.37`) on a host where this
    /// user can reach `/var/run/docker.sock`, such as a drone. Checks that a
    /// container created through the endpoint carries the lease label, that a
    /// privileged one never reaches dockerd, and removes what it made.
    #[tokio::test]
    async fn against_a_real_dockerd() {
        if std::env::var("BORG_DOCKER_IT").as_deref() != Ok("1") {
            return;
        }
        let image =
            std::env::var("BORG_DOCKER_IT_IMAGE").unwrap_or_else(|_| "busybox:1.37".to_owned());
        let dir = tempfile::tempdir().unwrap();
        let mut p = policy(dir.path(), false);
        p.upstream = default_upstream();
        p.cgroup_parent = None;
        p.labels
            .insert(LEASE_LABEL.to_owned(), "it-lease".to_owned());
        let peers = PeerCheck {
            proc_root: proc_with_self_in(dir.path(), "/svc/lease-lease1/job"),
            cgroup: "/svc/lease-lease1".to_owned(),
        };
        let endpoint = LeaseEndpoint::start(p, peers).unwrap();
        let sock = endpoint.socket().to_path_buf();
        let (repo, tag) = image.rsplit_once(':').unwrap_or((image.as_str(), "latest"));
        let pull = Request::post(format!(
            "/images/create?fromImage={}&tag={}",
            path_segment(repo),
            path_segment(tag)
        ))
        .header(hyper::header::HOST, "docker")
        .body(empty())
        .unwrap();
        let resp = send(&sock, pull).await.unwrap();
        let _ = resp.into_body().collect().await;

        let resp = send(
            &sock,
            post(
                "/containers/create?name=borg-it-lease-ok",
                json!({"Image": image, "Cmd": ["true"]}),
            ),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let resp = send(
            &sock,
            post(
                "/containers/create?name=borg-it-lease-priv",
                json!({"Image": image, "HostConfig": {"Privileged": true}}),
            ),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        let inspect = Request::get("/containers/borg-it-lease-ok/json")
            .header(hyper::header::HOST, "docker")
            .body(empty())
            .unwrap();
        let resp = send(&sock, inspect).await.unwrap();
        let body: Value =
            serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(body["Config"]["Labels"][LEASE_LABEL], "it-lease");
        let gone = Request::get("/containers/borg-it-lease-priv/json")
            .header(hyper::header::HOST, "docker")
            .body(empty())
            .unwrap();
        assert_eq!(
            send(&sock, gone).await.unwrap().status(),
            StatusCode::NOT_FOUND
        );

        let rm = Request::delete("/containers/borg-it-lease-ok?force=true&v=true")
            .header(hyper::header::HOST, "docker")
            .body(empty())
            .unwrap();
        assert!(send(&sock, rm).await.unwrap().status().is_success());
        endpoint.stop().await;
    }
}
