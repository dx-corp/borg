//! Drone configuration: the JSON file `borg drone install` writes, or a
//! hand-written TOML file with the same keys (see [`crate::configfile`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::allocator::HostCaps;
use crate::drone::jobuser::JobUser;
use crate::secret::Secret;

#[derive(Debug, thiserror::Error)]
pub enum DroneConfigError {
    #[error("read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("parse config: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("parse config {0}")]
    File(#[from] crate::configfile::ParseError),
    #[error("{0}")]
    Invalid(String),
}

/// One cache directory the drone manages. Present-by-default; set
/// `enabled: false` to skip it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CacheEntry {
    pub enabled: bool,
    /// Size cap such as `512M` or `50G`. Enforced by LRU garbage collection
    /// between leases (sccache enforces its own via `SCCACHE_CACHE_SIZE`).
    /// Each kind has a default; sccache's is 25% of the free disk, at most 100G.
    pub max_size: Option<String>,
}

impl Default for CacheEntry {
    fn default() -> Self {
        Self {
            enabled: true,
            max_size: None,
        }
    }
}

/// A host-level sccache server, so hit rates can be reported.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SccacheServer {
    /// Port the server listens on; exported to jobs as `SCCACHE_SERVER_PORT`.
    pub port: u16,
    /// The sccache binary the drone runs (default `sccache`, from its PATH).
    #[serde(default = "default_sccache_bin")]
    pub bin: String,
}

fn default_sccache_bin() -> String {
    "sccache".to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SccacheCache {
    pub enabled: bool,
    /// `SCCACHE_CACHE_SIZE`; default 25% of the free disk, at most 100G.
    pub max_size: Option<String>,
    /// Run one long-lived sccache server per host and report its hit/miss
    /// counts. Off by default: without it each job's own server dies with the
    /// lease and its counters are lost.
    pub server: Option<SccacheServer>,
}

impl Default for SccacheCache {
    fn default() -> Self {
        Self {
            enabled: true,
            max_size: None,
            server: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CargoCache {
    pub enabled: bool,
    pub max_size: Option<String>,
    /// Export `BORG_RUSTFLAGS` and `RUSTFLAGS` (path remaps that make compile
    /// output independent of the slot) and write them to `$CARGO_HOME/config.toml`.
    /// Cargo ignores `build.rustflags` and `target.*.rustflags` from config files
    /// whenever `RUSTFLAGS` is set, so turn this off for repositories that rely on them.
    pub export_rustflags: bool,
}

impl Default for CargoCache {
    fn default() -> Self {
        Self {
            enabled: true,
            max_size: None,
            export_rustflags: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BazelCache {
    pub enabled: bool,
    /// Cap for the disk cache and repository cache together.
    pub max_size: Option<String>,
    /// The rc file the drone writes `--disk_cache` and `--repository_cache`
    /// into (a managed block). Default: `$HOME/.bazelrc`.
    pub bazelrc: Option<PathBuf>,
}

impl Default for BazelCache {
    fn default() -> Self {
        Self {
            enabled: true,
            max_size: None,
            bazelrc: None,
        }
    }
}

/// Bare git mirrors, one per repository. Off unless `mirrors` is non-empty.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GitCaches {
    /// Repositories (`owner/name`). Jobs get each mirror's path in
    /// `BORG_GIT_MIRROR_<OWNER>_<NAME>`.
    pub mirrors: Vec<String>,
    /// Where mirrors fetch from; `<base>/<owner>/<name>.git`. For an `https://`
    /// base the queen's short-lived `mirror_credential` authenticates fetches
    /// of the repositories it names; otherwise credentials are the host's
    /// business (a credential helper, or an ssh base such as `git@host:` with
    /// a read-only deploy key for the service user).
    pub remote_base: String,
    pub bin: String,
    pub interval_secs: u64,
}

impl Default for GitCaches {
    fn default() -> Self {
        Self {
            mirrors: Vec::new(),
            remote_base: "https://github.com".to_owned(),
            bin: "git".to_owned(),
            interval_secs: 600,
        }
    }
}

/// Every cache kind the drone can manage. All kinds except git are on by
/// default; a cache nobody uses costs only the disk it fills, up to its cap.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CachesConfig {
    pub git: GitCaches,
    pub sccache: SccacheCache,
    /// `CARGO_HOME` registry and git data. `bin` is never shared between leases.
    pub cargo: CargoCache,
    pub bazel: BazelCache,
    pub pnpm: CacheEntry,
    pub go: CacheEntry,
    pub uv: CacheEntry,
    pub mise: CacheEntry,
    /// How often sizes are re-measured and garbage collection is attempted
    /// while the drone is idle.
    pub maintenance_interval_secs: u64,
}

impl Default for CachesConfig {
    fn default() -> Self {
        Self {
            git: GitCaches::default(),
            sccache: SccacheCache::default(),
            cargo: CargoCache::default(),
            bazel: BazelCache::default(),
            pnpm: CacheEntry::default(),
            go: CacheEntry::default(),
            uv: CacheEntry::default(),
            mise: CacheEntry::default(),
            maintenance_interval_secs: 600,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DroneConfig {
    /// Base URL of the queen. Must be `https://` unless `allow_insecure`.
    pub queen_url: String,
    /// File holding the enrollment token. If absent, `BORG_DRONE_TOKEN`.
    #[serde(default)]
    pub token_file: Option<PathBuf>,
    /// PEM CA certificate that signs the queen's certificate. When set it is
    /// the only trust root, which pins the queen's CA.
    #[serde(default)]
    pub ca_cert_path: Option<PathBuf>,
    /// Permit a plain `http://` queen URL (trusted LAN only).
    #[serde(default)]
    pub allow_insecure: bool,
    /// Pinned actions-runner install; every lease gets a private copy of it.
    pub runner_dir: PathBuf,
    /// Parent of the per-lease work directories. Wiped on start and after
    /// every job.
    pub work_root: PathBuf,
    /// Tool caches (sccache, mise, cargo). Never touched by between-job
    /// cleanup; reclaim may remove stale cargo target directories under
    /// `<cache_root>/*/slots` (regenerable build output, never while busy).
    pub cache_root: PathBuf,
    #[serde(default = "default_docker")]
    pub docker_bin: String,
    /// Bounded execution smoke tests run through the job-user boundary.
    #[serde(default)]
    pub capability_probe: crate::drone::capabilities::ProbeSettings,
    #[serde(default = "default_heartbeat")]
    pub heartbeat_secs: u64,
    /// A drone holding standby deploy runners starts them all once the queen
    /// has not answered a heartbeat for this long. 0 disables fail-open.
    #[serde(default = "default_fail_open")]
    pub fail_open_after_secs: u64,
    /// A lease running longer than this is killed.
    #[serde(default = "default_max_job")]
    pub max_job_secs: u64,
    /// A preempted lease's runner gets SIGINT, then is killed after this long.
    #[serde(default = "default_preempt_grace")]
    pub preempt_grace_secs: u64,
    /// On SIGTERM, wait this long for in-flight jobs before killing them.
    #[serde(default = "default_drain")]
    pub drain_timeout_secs: u64,
    /// Owner caps ("this PC gives at most 8 cores / 24 GiB").
    #[serde(default)]
    pub max_vcpu: Option<u32>,
    #[serde(default)]
    pub max_mem_mib: Option<u64>,
    #[serde(default)]
    pub max_leases: Option<u32>,
    #[serde(default = "default_reserve")]
    pub reserve_mem_mib: u64,
    /// Number of slot directories (`<work_root>/slot-<N>`) leases run in. A
    /// lease takes the lowest free slot, so its path never contains the lease
    /// id and a reused slot has an identical path. Also the most leases that
    /// run at once. Default: `max_leases`, else 16.
    #[serde(default)]
    pub slots: Option<u32>,
    /// Directory for self-update hand-off and health evidence (`update-request`,
    /// `update-result.json`, `health.json`). Absent: no self-update.
    #[serde(default)]
    pub state_dir: Option<PathBuf>,
    /// Run leases as this unprivileged user instead of the drone's own, so job
    /// code cannot read the drone's token or config. Linux only.
    #[serde(default)]
    pub job_user: Option<JobUser>,
    /// Labels added to every runner this drone registers, for lanes that still
    /// route by label (for example `on-prem`). Labels of the job classes are
    /// ignored by the queen so they cannot change a runner's scope.
    #[serde(default)]
    pub extra_labels: Vec<String>,
    /// `shared` (default), `deploy` or `deploy-only`. A deploy drone runs only
    /// deploy and main jobs, never pull-request jobs or work units; a
    /// deploy-only drone runs only deploy jobs; a shared drone never runs
    /// deploy jobs.
    #[serde(default)]
    pub trust: crate::classes::Trust,
    /// Environment injected into every lease's runner.
    #[serde(default)]
    pub runner_env: BTreeMap<String, String>,
    /// Absolute path of a script (`.sh`) the runner runs when a job starts;
    /// exported as `ACTIONS_RUNNER_HOOK_JOB_STARTED`.
    #[serde(default)]
    pub job_started_hook: Option<PathBuf>,
    /// Same for job completion (`ACTIONS_RUNNER_HOOK_JOB_COMPLETED`).
    #[serde(default)]
    pub job_completed_hook: Option<PathBuf>,
    /// Host-level caches the drone manages under `cache_root`.
    #[serde(default)]
    pub caches: CachesConfig,
    /// Build-box reclaim of abandoned worktrees and cargo directories; off
    /// unless a host opts in, and dry-run by default. See [`crate::reclaim`].
    #[serde(default)]
    pub reclaim: crate::reclaim::ReclaimSettings,
}

/// Slot directories when neither `slots` nor `max_leases` is set.
const DEFAULT_SLOTS: u32 = 16;
const MAX_SLOTS: u32 = 256;

fn default_docker() -> String {
    "docker".to_owned()
}
fn default_heartbeat() -> u64 {
    10
}
fn default_fail_open() -> u64 {
    120
}
fn default_max_job() -> u64 {
    6 * 3600
}
fn default_preempt_grace() -> u64 {
    30
}
fn default_drain() -> u64 {
    3600
}
fn default_reserve() -> u64 {
    HostCaps::default().reserve_mem_mib
}

impl DroneConfig {
    pub fn load(path: &Path) -> Result<Self, DroneConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| DroneConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        let cfg: Self = crate::configfile::parse(path, &text)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn from_json(json: &str) -> Result<Self, DroneConfigError> {
        let cfg: Self = serde_json::from_str(json)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), DroneConfigError> {
        let bad = |m: &str| Err(DroneConfigError::Invalid(m.to_owned()));
        let https = self.queen_url.starts_with("https://");
        let http = self.queen_url.starts_with("http://");
        let scheme_ok = https || (http && self.allow_insecure);
        if !scheme_ok {
            return bad(
                "queen_url must start with https:// (set allow_insecure for plain http on a trusted network)",
            );
        }
        if self.heartbeat_secs == 0 || self.max_job_secs == 0 {
            return bad("heartbeat_secs and max_job_secs must be positive");
        }
        if self.preempt_grace_secs == 0 {
            return bad("preempt_grace_secs must be positive");
        }
        if self.max_vcpu == Some(0) || self.max_mem_mib == Some(0) || self.max_leases == Some(0) {
            return bad("owner caps must be positive when set");
        }
        if self.ca_cert_path.is_some() && http {
            return bad("ca_cert_path only applies to an https:// queen_url");
        }
        if let Some(user) = &self.job_user {
            user.validate().map_err(DroneConfigError::Invalid)?;
        }
        self.capability_probe
            .validate()
            .map_err(DroneConfigError::Invalid)?;
        self.validate_job_settings()?;
        self.reclaim.validate().map_err(DroneConfigError::Invalid)?;
        self.validate_caches()
    }

    fn validate_job_settings(&self) -> Result<(), DroneConfigError> {
        let bad = |m: String| Err(DroneConfigError::Invalid(m));
        for label in &self.extra_labels {
            if !crate::classes::valid_extra_label(label) {
                return bad(format!(
                    "extra_labels entry {label:?} is not a valid runner label"
                ));
            }
        }
        if self.extra_labels.len() > crate::classes::MAX_EXTRA_LABELS {
            return bad(format!(
                "at most {} extra_labels",
                crate::classes::MAX_EXTRA_LABELS
            ));
        }
        for (key, value) in &self.runner_env {
            let name_ok = key
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
            let reserved = key.starts_with("BORG_")
                || key.starts_with("ACTIONS_RUNNER_")
                || key == "COMPOSE_PROJECT_NAME"
                || key == "DISABLE_RUNNER_UPDATE";
            if !name_ok || reserved || value.contains('\0') {
                return bad(format!("runner_env key {key:?} is invalid or reserved"));
            }
        }
        for (name, hook) in [
            ("job_started_hook", &self.job_started_hook),
            ("job_completed_hook", &self.job_completed_hook),
        ] {
            if let Some(path) = hook
                && !(path.is_absolute() && path.extension().is_some_and(|e| e == "sh"))
            {
                return bad(format!("{name} must be an absolute path ending in .sh"));
            }
        }
        Ok(())
    }

    /// The runner hook and `runner_env` variables for a lease.
    pub fn runner_environment(&self) -> Vec<(String, String)> {
        let mut env: Vec<(String, String)> = self
            .runner_env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (var, hook) in [
            ("ACTIONS_RUNNER_HOOK_JOB_STARTED", &self.job_started_hook),
            (
                "ACTIONS_RUNNER_HOOK_JOB_COMPLETED",
                &self.job_completed_hook,
            ),
        ] {
            if let Some(path) = hook {
                env.push((var.to_owned(), path.to_string_lossy().into_owned()));
            }
        }
        env
    }

    fn validate_caches(&self) -> Result<(), DroneConfigError> {
        let bad = |m: String| Err(DroneConfigError::Invalid(m));
        let c = &self.caches;
        if self.slots == Some(0) || self.slots.is_some_and(|s| s > MAX_SLOTS) {
            return bad(format!("slots must be between 1 and {MAX_SLOTS}"));
        }
        if let (Some(leases), Some(slots)) = (self.max_leases, self.slots)
            && leases > slots
        {
            return bad("max_leases must not exceed slots".to_owned());
        }
        if c.git.interval_secs == 0 || c.maintenance_interval_secs == 0 {
            return bad("cache intervals must be positive".to_owned());
        }
        // The work root is wiped on start and after every job; caches must
        // never sit inside it, nor it inside them.
        if self.cache_root.starts_with(&self.work_root)
            || self.work_root.starts_with(&self.cache_root)
        {
            return bad("cache_root and work_root must not contain one another".to_owned());
        }
        let sizes = [
            ("sccache", &c.sccache.max_size),
            ("cargo", &c.cargo.max_size),
            ("bazel", &c.bazel.max_size),
            ("pnpm", &c.pnpm.max_size),
            ("go", &c.go.max_size),
            ("uv", &c.uv.max_size),
            ("mise", &c.mise.max_size),
        ];
        for (name, size) in sizes {
            if let Some(size) = size
                && crate::drone::caches::gc::parse_size(size).is_none()
            {
                return bad(format!(
                    "caches.{name}.max_size {size:?} must look like 512M or 50G"
                ));
            }
        }
        let mut vars = Vec::new();
        for repo in &c.git.mirrors {
            let parts: Vec<&str> = repo.split('/').collect();
            let part_ok = |p: &&str| {
                !p.is_empty()
                    && *p != "."
                    && *p != ".."
                    && p.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            };
            if parts.len() != 2 || !parts.iter().all(part_ok) {
                return bad(format!(
                    "caches.git.mirrors entry {repo:?} must look like owner/name"
                ));
            }
            let var = crate::drone::caches::git::mirror_var(repo);
            if vars.contains(&var) {
                return bad(format!(
                    "caches.git.mirrors entry {repo:?} maps to the same variable {var} as another entry"
                ));
            }
            vars.push(var);
        }
        Ok(())
    }

    /// How many slot directories exist (and so how many leases can run at once).
    pub fn slot_count(&self) -> usize {
        usize::try_from(self.slots.or(self.max_leases).unwrap_or(DEFAULT_SLOTS))
            .unwrap_or(usize::MAX)
    }

    /// The caps advertised to the queen with every heartbeat.
    pub fn host_caps(&self) -> HostCaps {
        HostCaps {
            max_vcpu: self.max_vcpu,
            max_mem_mib: self.max_mem_mib,
            max_leases: self.max_leases,
            reserve_mem_mib: self.reserve_mem_mib,
        }
    }

    /// The enrollment token from `token_file` or `BORG_DRONE_TOKEN`.
    pub fn load_token(&self) -> Result<Secret, DroneConfigError> {
        let raw = match &self.token_file {
            Some(path) => {
                std::fs::read_to_string(path).map_err(|source| DroneConfigError::Read {
                    path: path.clone(),
                    source,
                })?
            }
            None => std::env::var("BORG_DRONE_TOKEN").map_err(|_| {
                DroneConfigError::Invalid("set token_file or BORG_DRONE_TOKEN".to_owned())
            })?,
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(DroneConfigError::Invalid(
                "enrollment token is empty".to_owned(),
            ));
        }
        Ok(Secret::new(trimmed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"{"queen_url":"https://queen.example.test:8443","runner_dir":"/r","work_root":"/w","cache_root":"/c"}"#;

    #[test]
    fn defaults_and_caps() {
        let c = DroneConfig::from_json(BASE).unwrap();
        assert_eq!((c.heartbeat_secs, c.docker_bin.as_str()), (10, "docker"));
        assert_eq!(c.fail_open_after_secs, 120);
        let off = BASE.replace('}', r#","fail_open_after_secs":0}"#);
        assert_eq!(
            DroneConfig::from_json(&off).unwrap().fail_open_after_secs,
            0
        );
        let with = BASE.replace('}', r#","max_vcpu":8,"max_mem_mib":24576}"#);
        let caps = DroneConfig::from_json(&with).unwrap().host_caps();
        assert_eq!((caps.max_vcpu, caps.max_mem_mib), (Some(8), Some(24576)));
    }

    #[test]
    fn toml_drone_config_matches_json() {
        let dir = tempfile::tempdir().unwrap();
        let toml_path = dir.path().join("drone.toml");
        std::fs::write(
            &toml_path,
            r#"
queen_url = "https://queen.example.test:8443"
runner_dir = "/r"
work_root = "/w"
cache_root = "/c"
max_vcpu = 8
extra_labels = ["on-prem"]

[runner_env]
FOO = "bar"
"#,
        )
        .unwrap();
        let json = BASE.replace(
            '}',
            r#","max_vcpu":8,"extra_labels":["on-prem"],"runner_env":{"FOO":"bar"}}"#,
        );
        let t = DroneConfig::load(&toml_path).unwrap();
        let j = DroneConfig::from_json(&json).unwrap();
        assert_eq!(format!("{t:?}"), format!("{j:?}"));
    }

    #[test]
    fn plain_http_needs_explicit_opt_in() {
        let http = BASE.replace("https://", "http://");
        assert!(DroneConfig::from_json(&http).is_err());
        let allowed = http.replace('}', r#","allow_insecure":true}"#);
        assert!(DroneConfig::from_json(&allowed).is_ok());
        assert!(DroneConfig::from_json(&BASE.replace("https://queen", "ftp://queen")).is_err());
    }

    #[test]
    fn rejects_zero_caps_unknown_fields_and_ca_on_http() {
        for extra in [r#","max_vcpu":0"#, r#","bogus":1"#] {
            assert!(
                DroneConfig::from_json(&BASE.replace('}', &format!("{extra}}}"))).is_err(),
                "{extra}"
            );
        }
        let http_ca = BASE
            .replace("https://", "http://")
            .replace('}', r#","allow_insecure":true,"ca_cert_path":"/ca.pem"}"#);
        assert!(DroneConfig::from_json(&http_ca).is_err());
    }

    #[test]
    fn cache_settings_have_defaults_and_are_validated() {
        let with = |extra: &str| BASE.replace('}', &format!(",{extra}}}"));
        let defaults = DroneConfig::from_json(BASE).unwrap().caches;
        assert!(defaults.sccache.enabled && defaults.bazel.enabled && defaults.mise.enabled);
        assert!(defaults.git.mirrors.is_empty());
        assert_eq!(
            (
                defaults.git.remote_base.as_str(),
                defaults.git.interval_secs
            ),
            ("https://github.com", 600)
        );

        let ok = with(
            r#""caches":{"git":{"mirrors":["example-org/app"]},"sccache":{"max_size":"50G"},"uv":{"enabled":false}}"#,
        );
        let cfg = DroneConfig::from_json(&ok).unwrap().caches;
        assert_eq!(cfg.git.mirrors, ["example-org/app"]);
        assert!(!cfg.uv.enabled && cfg.go.enabled);
        for bad in [
            r#""caches":{"git":{"mirrors":["noslash"]}}"#,
            r#""caches":{"git":{"mirrors":["a/b/c"]}}"#,
            r#""caches":{"git":{"mirrors":["../x"]}}"#,
            r#""caches":{"git":{"mirrors":["a-b/c","a/b-c"]}}"#,
            r#""caches":{"sccache":{"max_size":"50"}}"#,
            r#""caches":{"bazel":{"max_size":"G"}}"#,
            r#""caches":{"git":{"interval_secs":0}}"#,
            r#""caches":{"npm":{}}"#,
        ] {
            assert!(DroneConfig::from_json(&with(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn slots_default_to_max_leases_then_sixteen_and_are_validated() {
        let with = |extra: &str| BASE.replace('}', &format!(",{extra}}}"));
        assert_eq!(DroneConfig::from_json(BASE).unwrap().slot_count(), 16);
        assert_eq!(
            DroneConfig::from_json(&with(r#""max_leases":4"#))
                .unwrap()
                .slot_count(),
            4
        );
        assert_eq!(
            DroneConfig::from_json(&with(r#""max_leases":4,"slots":8"#))
                .unwrap()
                .slot_count(),
            8
        );
        for bad in [
            r#""slots":0"#,
            r#""slots":257"#,
            r#""max_leases":9,"slots":8"#,
        ] {
            assert!(DroneConfig::from_json(&with(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn sccache_server_and_cargo_rustflags_are_configurable() {
        let with = |extra: &str| BASE.replace('}', &format!(",{extra}}}"));
        let c = DroneConfig::from_json(&with(
            r#""caches":{"sccache":{"server":{"port":4999}},"cargo":{"export_rustflags":false}}"#,
        ))
        .unwrap()
        .caches;
        assert_eq!(
            c.sccache.server.as_ref().map(|s| (s.port, s.bin.as_str())),
            Some((4999, "sccache"))
        );
        assert!(!c.cargo.export_rustflags);
        let d = DroneConfig::from_json(BASE).unwrap().caches;
        assert!(d.sccache.server.is_none() && d.cargo.export_rustflags);
    }

    #[test]
    fn job_settings_are_validated_and_exported() {
        let with = |extra: &str| BASE.replace('}', &format!(",{extra}}}"));
        let ok = with(
            r#""extra_labels":["on-prem","legacy-lane"],"runner_env":{"HTTP_PROXY":"http://proxy.example.test:3128"},"job_started_hook":"/etc/borg/hooks/start.sh","job_completed_hook":"/etc/borg/hooks/done.sh","job_user":{"name":"borg-job","home":"/var/lib/borg-job"}"#,
        );
        let cfg = DroneConfig::from_json(&ok).unwrap();
        assert_eq!(
            cfg.job_user.as_ref().map(|u| u.priv_bin.as_str()),
            Some("setpriv")
        );
        let env = cfg.runner_environment();
        assert!(env.contains(&(
            "HTTP_PROXY".to_owned(),
            "http://proxy.example.test:3128".to_owned()
        )));
        assert!(env.contains(&(
            "ACTIONS_RUNNER_HOOK_JOB_STARTED".to_owned(),
            "/etc/borg/hooks/start.sh".to_owned()
        )));
        assert!(env.contains(&(
            "ACTIONS_RUNNER_HOOK_JOB_COMPLETED".to_owned(),
            "/etc/borg/hooks/done.sh".to_owned()
        )));
        for bad in [
            r#""job_started_hook":"/etc/hook.py""#,
            r#""job_started_hook":"hook.sh""#,
            r#""job_completed_hook":"/etc/hook""#,
            r#""runner_env":{"BORG_LEASE_ID":"x"}"#,
            r#""runner_env":{"ACTIONS_RUNNER_INPUT_JITCONFIG":"x"}"#,
            r#""runner_env":{"A B":"x"}"#,
            r#""extra_labels":["has space"]"#,
            r#""extra_labels":["self-hosted"]"#,
            r#""job_user":{"name":"x","home":"relative"}"#,
        ] {
            assert!(DroneConfig::from_json(&with(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn caches_may_not_overlap_the_wiped_work_root() {
        let nested = BASE.replace(r#""cache_root":"/c""#, r#""cache_root":"/w/cache""#);
        assert!(DroneConfig::from_json(&nested).is_err());
        let parent = BASE.replace(r#""work_root":"/w""#, r#""work_root":"/c/work""#);
        assert!(DroneConfig::from_json(&parent).is_err());
    }

    #[test]
    fn token_file_is_trimmed_and_must_not_be_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t");
        let mut c = DroneConfig::from_json(BASE).unwrap();
        c.token_file = Some(path.clone());
        std::fs::write(&path, "borg_abc\n").unwrap();
        assert_eq!(c.load_token().unwrap().expose(), "borg_abc");
        std::fs::write(&path, "\n").unwrap();
        assert!(c.load_token().is_err());
    }
}
