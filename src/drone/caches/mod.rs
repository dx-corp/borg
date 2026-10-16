//! Host-level caches the drone owns so they outlive every lease: one stable
//! path per cache kind under `cache_root`, injected into every job's
//! environment, excluded from per-lease cleanup, size-capped by LRU garbage
//! collection that only runs while no lease is running, and reported in
//! heartbeats.
//!
//! Which kinds exist and how each is wired is in [`kinds`]; git mirrors are in
//! [`git`]; eviction is in [`gc`]; Bazel's rc file is in [`bazel`].

pub mod bazel;
pub mod gc;
pub mod git;
pub mod kinds;
pub mod rustflags;
pub mod sccache;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{OnceCell, OwnedRwLockReadGuard, RwLock};

use crate::api::{CacheStat, Counters, SizeStatus};
use crate::drone::config::DroneConfig;
use crate::drone::exec::Exec;
use crate::drone::fsops::FsOps;
use crate::mirror_auth::MirrorCredential;
use crate::probe::{self, Host};
use git::GitMirrors;
use kinds::Kind;

/// Cache size when free space cannot be measured.
const FALLBACK_CACHE_SIZE: &str = "10G";
const MAX_SCCACHE_GIB: u64 = 100;

/// `SCCACHE_CACHE_SIZE` value: the configured size, else 25% of the free disk
/// capped at 100G (in M below 1G, so tiny disks still get a bounded cache).
pub fn cache_size_for(free_mib: Option<u64>, configured: Option<&str>) -> String {
    if let Some(size) = configured {
        return size.to_owned();
    }
    let Some(free) = free_mib else {
        return FALLBACK_CACHE_SIZE.to_owned();
    };
    let quarter = free / 4;
    if quarter >= 1024 {
        format!("{}G", (quarter / 1024).min(MAX_SCCACHE_GIB))
    } else {
        format!("{}M", quarter.max(64))
    }
}

#[derive(Default, Clone)]
struct Track {
    size_bytes: u64,
    evicted_bytes: u64,
    last_gc_at: Option<i64>,
    /// sccache only, from the host server.
    hits: Option<u64>,
    misses: Option<u64>,
    /// The last measurement missed directories the drone could not list.
    partial: bool,
}

pub struct Caches {
    cfg: Arc<DroneConfig>,
    exec: Arc<dyn Exec>,
    fs_ops: Arc<dyn FsOps>,
    host: Arc<dyn Host + Send + Sync>,
    sccache_size: OnceCell<String>,
    git: GitMirrors,
    /// Leases hold the read side for their whole life; garbage collection only
    /// runs when it can take the write side, so it never overlaps a lease.
    gate: Arc<RwLock<()>>,
    tracks: Mutex<BTreeMap<&'static str, Track>>,
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

impl Caches {
    pub fn new(
        cfg: Arc<DroneConfig>,
        exec: Arc<dyn Exec>,
        fs_ops: Arc<dyn FsOps>,
        host: Arc<dyn Host + Send + Sync>,
    ) -> Self {
        let git = GitMirrors::new(
            &cfg.caches.git,
            &cfg.cache_root,
            Arc::clone(&exec),
            Arc::clone(&fs_ops),
        );
        Self {
            cfg,
            exec,
            fs_ops,
            host,
            sccache_size: OnceCell::new(),
            git,
            gate: Arc::default(),
            tracks: Mutex::new(BTreeMap::new()),
        }
    }

    fn enabled(&self, kind: Kind) -> bool {
        let c = &self.cfg.caches;
        match kind {
            Kind::Sccache => c.sccache.enabled,
            Kind::Cargo => c.cargo.enabled,
            Kind::Bazel => c.bazel.enabled,
            Kind::Pnpm => c.pnpm.enabled,
            Kind::Go => c.go.enabled,
            Kind::Uv => c.uv.enabled,
            Kind::Mise => c.mise.enabled,
        }
    }

    fn configured_max(&self, kind: Kind) -> Option<&str> {
        let c = &self.cfg.caches;
        match kind {
            Kind::Sccache => c.sccache.max_size.as_deref(),
            Kind::Cargo => c.cargo.max_size.as_deref(),
            Kind::Bazel => c.bazel.max_size.as_deref(),
            Kind::Pnpm => c.pnpm.max_size.as_deref(),
            Kind::Go => c.go.max_size.as_deref(),
            Kind::Uv => c.uv.max_size.as_deref(),
            Kind::Mise => c.mise.max_size.as_deref(),
        }
    }

    /// Create the directories, size the compile cache and write Bazel's rc
    /// file, once. Returns the `SCCACHE_CACHE_SIZE` value.
    async fn init(&self) -> &str {
        self.sccache_size
            .get_or_init(|| async {
                let (cfg, host, exec, fs_ops) = (
                    Arc::clone(&self.cfg),
                    Arc::clone(&self.host),
                    Arc::clone(&self.exec),
                    Arc::clone(&self.fs_ops),
                );
                let flags = self.remap_flags();
                let enabled: Vec<Kind> =
                    Kind::ALL.into_iter().filter(|k| self.enabled(*k)).collect();
                let made = tokio::task::spawn_blocking(move || {
                    for kind in &enabled {
                        for sub in kind.subdirs() {
                            let path = kind.dir(&cfg.cache_root).join(sub);
                            if let Err(e) = fs_ops.mkdir_p(&path) {
                                tracing::warn!("create {}: {e}", path.display());
                            }
                        }
                    }
                    if let Err(e) = fs_ops.mkdir_p(&cfg.cache_root.join("git")) {
                        tracing::warn!("create git cache dir: {e}");
                    }
                    if cfg.caches.bazel.enabled {
                        write_bazelrc(fs_ops.as_ref(), &cfg);
                    }
                    if cfg.caches.cargo.enabled
                        && cfg.caches.cargo.export_rustflags
                        && rustflags::flags_are_passable(&flags)
                        && let Err(e) = rustflags::write_config(
                            fs_ops.as_ref(),
                            &Kind::Cargo.dir(&cfg.cache_root),
                            &flags,
                        )
                    {
                        tracing::warn!("cargo config.toml: {e}");
                    }
                    let free = probe::free_disk_mib_at(host.as_ref(), &cfg.cache_root);
                    let size = cache_size_for(free, cfg.caches.sccache.max_size.as_deref());
                    if let (true, Some(server)) =
                        (cfg.caches.sccache.enabled, &cfg.caches.sccache.server)
                    {
                        let dir = Kind::Sccache
                            .dir(&cfg.cache_root)
                            .to_string_lossy()
                            .into_owned();
                        let env = sccache::server_env(&dir, &size, server.port);
                        if !sccache::start_server(exec.as_ref(), &server.bin, &env) {
                            tracing::warn!(
                                "could not start the sccache server on port {}",
                                server.port
                            );
                        }
                    }
                    size
                })
                .await;
                made.unwrap_or_else(|_| FALLBACK_CACHE_SIZE.to_owned())
            })
            .await
    }

    /// The remap flags for every slot and the cargo home (identical for all leases).
    fn remap_flags(&self) -> Vec<String> {
        rustflags::remap_flags(
            &self.cfg.work_root,
            self.cfg.slot_count(),
            &Kind::Cargo.dir(&self.cfg.cache_root),
        )
    }

    /// Held for the whole life of a lease (refresh, run, cleanup).
    pub async fn begin_lease(&self) -> OwnedRwLockReadGuard<()> {
        Arc::clone(&self.gate).read_owned().await
    }

    /// Refresh stale git mirrors, then return the cache environment for a new
    /// lease running in `lease_dir`.
    pub async fn prepare_lease(&self, lease_dir: &Path) -> Vec<(String, String)> {
        let size = self.init().await.to_owned();
        self.git.refresh_stale().await;
        let mut env = Vec::new();
        for kind in Kind::ALL.into_iter().filter(|k| self.enabled(*k)) {
            env.extend(kind.env(&self.cfg.cache_root, lease_dir, &size));
        }
        env.extend(self.git.env());
        if self.cfg.caches.cargo.enabled && self.cfg.caches.cargo.export_rustflags {
            let flags = self.remap_flags();
            if rustflags::flags_are_passable(&flags) {
                let joined = flags.join(" ");
                env.push(("BORG_RUSTFLAGS".to_owned(), joined.clone()));
                env.push(("RUSTFLAGS".to_owned(), joined));
            } else {
                tracing::warn!(
                    "work_root or cache_root contains whitespace; not exporting RUSTFLAGS"
                );
            }
        }
        if let (true, Some(server)) = (
            self.cfg.caches.sccache.enabled,
            &self.cfg.caches.sccache.server,
        ) {
            env.push(("SCCACHE_SERVER_PORT".to_owned(), server.port.to_string()));
            env.push(("SCCACHE_IDLE_TIMEOUT".to_owned(), "0".to_owned()));
        }
        env
    }

    /// Cumulative hits and misses of the host sccache server, if configured.
    async fn sccache_counts(&self) -> Option<(u64, u64)> {
        let (true, Some(server)) = (
            self.cfg.caches.sccache.enabled,
            self.cfg.caches.sccache.server.clone(),
        ) else {
            return None;
        };
        let size = self.init().await.to_owned();
        let exec = Arc::clone(&self.exec);
        let dir = Kind::Sccache
            .dir(&self.cfg.cache_root)
            .to_string_lossy()
            .into_owned();
        tokio::task::spawn_blocking(move || {
            let env = sccache::server_env(&dir, &size, server.port);
            sccache::show_stats(exec.as_ref(), &server.bin, &env)
        })
        .await
        .ok()
        .flatten()
    }

    /// Re-measure every cache and, if no lease is running, evict least recently
    /// used units from caches over their cap. Skips silently while a lease is
    /// active.
    pub async fn maintain(&self) {
        let _ = self.init().await;
        // Hit counters are cheap to read and live in the server, so they are
        // refreshed even while leases run.
        if let Some((hits, misses)) = self.sccache_counts().await
            && let Ok(mut tracks) = self.tracks.lock()
        {
            let t = tracks.entry("sccache").or_default();
            t.hits = Some(hits);
            t.misses = Some(misses);
        }
        let Ok(_idle) = self.gate.try_write() else {
            return;
        };
        let plan: Vec<(Kind, Option<u64>)> = Kind::ALL
            .into_iter()
            .filter(|k| self.enabled(*k))
            .map(|k| {
                let cap = self
                    .configured_max(k)
                    .and_then(gc::parse_size)
                    .or_else(|| k.default_max_bytes());
                (k, cap)
            })
            .collect();
        let root = self.cfg.cache_root.clone();
        let fs_ops = Arc::clone(&self.fs_ops);
        let work = tokio::task::spawn_blocking(move || {
            let mut out: Vec<(&'static str, u64, u64, bool)> = Vec::new();
            for (kind, cap) in plan {
                let measured = gc::measure_tree(&kind.dir(&root));
                let mut size = measured.size;
                let mut freed = 0;
                if let Some(cap) = cap
                    && size > cap
                {
                    let units = gc::collect_units(&kind.gc_buckets(&root));
                    let pool: u64 = units.iter().map(|u| u.size).sum();
                    freed = gc::evict_lru(fs_ops.as_ref(), units, pool, cap).freed_bytes;
                    size = size.saturating_sub(freed);
                }
                out.push((kind.name(), size, freed, measured.unreadable_dirs > 0));
            }
            let git = gc::measure_tree(&root.join("git"));
            out.push(("git", git.size, 0, git.unreadable_dirs > 0));
            out
        })
        .await;
        let Ok(results) = work else { return };
        let now = unix_now();
        if let Ok(mut tracks) = self.tracks.lock() {
            for (name, size, freed, partial) in results {
                if partial {
                    tracing::warn!(
                        "cache {name}: some directories are not readable by the drone's user; size {size} is a lower bound"
                    );
                }
                let t = tracks.entry(name).or_default();
                t.size_bytes = size;
                t.partial = partial;
                if freed > 0 {
                    t.evicted_bytes += freed;
                    t.last_gc_at = Some(now);
                }
            }
        }
    }

    /// Per-cache size, cap, eviction and (git) hit statistics for heartbeats.
    pub fn stats(&self) -> Vec<CacheStat> {
        let tracks = self.tracks.lock().map(|t| t.clone()).unwrap_or_default();
        let sccache_cap = self.sccache_size.get().and_then(|s| gc::parse_size(s));
        let (served, missed) = self.git.hit_counts();
        tracks
            .into_iter()
            .map(|(name, t)| {
                let kind = Kind::ALL.into_iter().find(|k| k.name() == name);
                let max_bytes = match kind {
                    Some(Kind::Sccache) => sccache_cap,
                    Some(k) => self
                        .configured_max(k)
                        .and_then(gc::parse_size)
                        .or_else(|| k.default_max_bytes()),
                    None => None,
                };
                let (hits, misses) = if name == "git" {
                    (Some(served), Some(missed))
                } else {
                    (t.hits, t.misses)
                };
                let counters = match (name, hits) {
                    (_, Some(_)) => Counters::Reported,
                    ("sccache", None) => Counters::Unknown,
                    _ => Counters::NotTracked,
                };
                CacheStat {
                    name: name.to_owned(),
                    size_bytes: t.size_bytes,
                    max_bytes,
                    evicted_bytes: t.evicted_bytes,
                    last_gc_at: t.last_gc_at,
                    hits,
                    misses,
                    size_status: if t.partial {
                        SizeStatus::Partial
                    } else {
                        SizeStatus::Measured
                    },
                    counters,
                }
            })
            .collect()
    }

    /// Keep the queen's latest git mirror credential (memory only). The first
    /// usable one refreshes the mirrors now, not at the next timer tick.
    pub fn set_mirror_credential(self: &Arc<Self>, credential: MirrorCredential) {
        if self.git.set_credential(credential) {
            let caches = Arc::clone(self);
            tokio::spawn(async move { caches.git.refresh_stale().await });
        }
    }

    /// Refresh git mirrors and run maintenance on a timer until aborted.
    pub fn spawn_background(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let caches = Arc::clone(self);
        tokio::spawn(async move {
            let _ = caches.init().await;
            let every = Duration::from_secs(
                caches
                    .cfg
                    .caches
                    .maintenance_interval_secs
                    .min(caches.cfg.caches.git.interval_secs),
            );
            loop {
                caches.git.refresh_all().await;
                caches.maintain().await;
                tokio::time::sleep(every).await;
            }
        })
    }
}

/// Point Bazel at the host-level disk and repository caches.
fn write_bazelrc(fs_ops: &dyn FsOps, cfg: &DroneConfig) {
    // Bazel reads the rc of whoever runs it: the job user when there is one.
    let home = match &cfg.job_user {
        Some(user) => Some(user.home.clone()),
        None => std::env::var_os("HOME").map(PathBuf::from),
    };
    let rc: Option<PathBuf> = cfg
        .caches
        .bazel
        .bazelrc
        .clone()
        .or_else(|| home.map(|h| h.join(".bazelrc")));
    let Some(rc) = rc else {
        tracing::warn!(
            "HOME is unset and caches.bazel.bazelrc is not configured; skipping Bazel cache flags"
        );
        return;
    };
    let dir = Kind::Bazel.dir(&cfg.cache_root);
    if let Err(e) = bazel::write_rc(fs_ops, &rc, &dir.join("disk"), &dir.join("repo")) {
        tracing::warn!("write {}: {e}", rc.display());
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;
    use crate::drone::cleanup::testing::FakeExec;

    struct NoHost;
    impl Host for NoHost {
        fn read_to_string(&self, _: &Path) -> Option<String> {
            None
        }
        fn exists(&self, _: &Path) -> bool {
            false
        }
        fn run(&self, program: &str, _: &[&str]) -> Option<String> {
            (program == "df").then(|| {
                "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/x 1 1 419430400 1% /".to_owned()
            })
        }
        fn os(&self) -> &'static str {
            "linux"
        }
        fn arch(&self) -> &'static str {
            "x86_64"
        }
        fn available_parallelism(&self) -> Option<u32> {
            Some(1)
        }
    }

    fn caches(root: &Path, caches_json: &str) -> Arc<Caches> {
        let json = format!(
            r#"{{"queen_url":"https://q.example.test","runner_dir":"/r","work_root":"/w","cache_root":"{}",
               "caches":{caches_json}}}"#,
            root.display()
        );
        let cfg = Arc::new(DroneConfig::from_json(&json).unwrap());
        Arc::new(Caches::new(
            cfg,
            Arc::new(FakeExec::default()),
            Arc::new(crate::drone::fsops::LocalFs),
            Arc::new(NoHost),
        ))
    }

    fn rc(root: &Path) -> String {
        format!(
            r#""bazel":{{"bazelrc":"{}/home/.bazelrc"}}"#,
            root.display()
        )
    }

    fn blob(path: &Path, bytes: usize, age_secs: u64) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![1u8; bytes]).unwrap();
        let when = SystemTime::now() - Duration::from_secs(age_secs);
        let times = std::fs::FileTimes::new()
            .set_modified(when)
            .set_accessed(when);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(times)
            .unwrap();
    }

    #[test]
    fn cache_size_is_a_quarter_of_free_disk_capped_at_100g() {
        let gib = |n: u64| Some(n * 1024);
        assert_eq!(cache_size_for(gib(400), None), "100G");
        assert_eq!(cache_size_for(gib(2000), None), "100G");
        assert_eq!(cache_size_for(gib(200), None), "50G");
        assert_eq!(cache_size_for(gib(100), None), "25G");
        assert_eq!(cache_size_for(Some(2048), None), "512M");
        assert_eq!(cache_size_for(Some(100), None), "64M");
        assert_eq!(cache_size_for(None, None), "10G");
        assert_eq!(
            cache_size_for(gib(400), Some("7G")),
            "7G",
            "configured size wins"
        );
    }

    #[tokio::test]
    async fn every_lease_gets_the_same_cache_paths_and_bazel_flags_are_written() {
        let root = tempfile::tempdir().unwrap();
        let c = caches(root.path(), &format!("{{{}}}", rc(root.path())));
        let a = c.prepare_lease(Path::new("/work/a")).await;
        let b = c.prepare_lease(Path::new("/work/b")).await;
        let strip = |env: &[(String, String)]| -> Vec<(String, String)> {
            env.iter()
                .filter(|(k, _)| k != "CARGO_INSTALL_ROOT")
                .cloned()
                .collect()
        };
        assert_eq!(
            strip(&a),
            strip(&b),
            "shared paths are identical across leases"
        );
        let get = |k: &str| a.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(
            get("SCCACHE_DIR"),
            Some(format!("{}/sccache", root.path().display()))
        );
        assert_eq!(get("SCCACHE_CACHE_SIZE").as_deref(), Some("100G"));
        assert_eq!(
            get("CARGO_INSTALL_ROOT").as_deref(),
            Some("/work/a/cargo-install")
        );
        for dir in [
            "sccache",
            "cargo/registry",
            "bazel/disk",
            "bazel/repo",
            "go/mod",
            "go/build",
            "uv",
            "mise",
            "pnpm",
            "git",
        ] {
            assert!(root.path().join(dir).is_dir(), "{dir} created");
        }
        let bazelrc = std::fs::read_to_string(root.path().join("home/.bazelrc")).unwrap();
        assert!(bazelrc.contains(&format!(
            "--disk_cache={}/bazel/disk",
            root.path().display()
        )));
        assert!(bazelrc.contains(&format!(
            "--repository_cache={}/bazel/repo",
            root.path().display()
        )));
    }

    #[tokio::test]
    async fn disabled_kinds_get_no_env_no_dir_and_no_bazel_flags() {
        let root = tempfile::tempdir().unwrap();
        let c = caches(
            root.path(),
            &format!(
                r#"{{"sccache":{{"enabled":false}},"uv":{{"enabled":false}},"bazel":{{"enabled":false,"bazelrc":"{}/rc"}}}}"#,
                root.path().display()
            ),
        );
        let env = c.prepare_lease(Path::new("/work/a")).await;
        for var in ["SCCACHE_DIR", "UV_CACHE_DIR", "BAZELISK_HOME"] {
            assert!(!env.iter().any(|(k, _)| k == var), "{var}");
        }
        assert!(env.iter().any(|(k, _)| k == "CARGO_HOME"));
        assert!(!root.path().join("sccache").exists() && !root.path().join("rc").exists());
    }

    #[tokio::test]
    async fn git_mirror_variables_appear_only_for_mirrors_that_exist() {
        let root = tempfile::tempdir().unwrap();
        let c = caches(
            root.path(),
            r#"{"git":{"mirrors":["example-org/app"],"remote_base":"https://git.example.test"}}"#,
        );
        let present = git::mirror_path(root.path(), "example-org/app");
        std::fs::create_dir_all(&present).unwrap();
        std::fs::write(present.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let env = c.prepare_lease(Path::new("/work/a")).await;
        assert!(env.contains(&(
            "BORG_GIT_MIRROR_EXAMPLE_ORG_APP".to_owned(),
            present.display().to_string()
        )));
        c.maintain().await;
        let git = c.stats().into_iter().find(|s| s.name == "git").unwrap();
        assert_eq!((git.hits, git.misses), (Some(1), Some(0)));
        assert_eq!(git.counters, Counters::Reported);
    }

    #[tokio::test]
    async fn gc_evicts_least_recently_used_between_leases_and_never_during_one() {
        let root = tempfile::tempdir().unwrap();
        let c = caches(root.path(), r#"{"cargo":{"max_size":"1M"}}"#);
        let reg = root.path().join("cargo/registry/cache/index-abc");
        for (name, age) in [("old.crate", 900), ("mid.crate", 500), ("new.crate", 100)] {
            blob(&reg.join(name), 600 * 1024, age);
        }
        // Not collectable: the sccache dir and Go's module cache, however big.
        blob(&root.path().join("sccache/big"), 3 * 1024 * 1024, 1000);
        blob(
            &root.path().join("go/mod/example.test/m@v1/f.go"),
            3 * 1024 * 1024,
            1000,
        );

        let lease = c.begin_lease().await;
        c.maintain().await;
        assert!(
            reg.join("old.crate").exists(),
            "no collection while a lease is active"
        );
        drop(lease);

        c.maintain().await;
        assert!(!reg.join("old.crate").exists());
        assert!(reg.join("new.crate").exists(), "recently used entries stay");
        assert!(
            root.path().join("sccache/big").exists()
                && root.path().join("go/mod/example.test/m@v1/f.go").exists()
        );

        let stats = c.stats();
        let cargo = stats.iter().find(|s| s.name == "cargo").unwrap();
        assert!(cargo.evicted_bytes >= 600 * 1024 && cargo.last_gc_at.is_some());
        assert_eq!(cargo.max_bytes, Some(1 << 20));
        assert!(cargo.size_bytes <= (1 << 20), "{}", cargo.size_bytes);
        let sccache = stats.iter().find(|s| s.name == "sccache").unwrap();
        assert!(sccache.size_bytes >= 3 * 1024 * 1024 && sccache.evicted_bytes == 0);
        assert_eq!(
            sccache.max_bytes,
            Some(100 << 30),
            "sccache reports its own cap"
        );
        // No host server: the counts live in each job's own server.
        assert_eq!((sccache.hits, sccache.counters), (None, Counters::Unknown));
        assert_eq!(sccache.size_status, SizeStatus::Measured);
        assert_eq!(cargo.counters, Counters::NotTracked);
    }

    #[tokio::test]
    async fn bazel_collection_leaves_the_bazelisk_download_cache_alone() {
        let root = tempfile::tempdir().unwrap();
        let c = caches(
            root.path(),
            &format!(
                r#"{{"bazel":{{"max_size":"1M","bazelrc":"{}/rc"}}}}"#,
                root.path().display()
            ),
        );
        let cas = root.path().join("bazel/disk/cas");
        for (name, age) in [("aa/h1", 900), ("bb/h2", 100), ("cc/h3", 50)] {
            blob(&cas.join(name), 700 * 1024, age);
        }
        blob(
            &root.path().join("bazel/bazelisk/downloads/bazel-8"),
            3 * 1024 * 1024,
            1000,
        );
        c.maintain().await;
        assert!(!cas.join("aa/h1").exists());
        assert!(cas.join("cc/h3").exists());
        assert!(
            root.path()
                .join("bazel/bazelisk/downloads/bazel-8")
                .exists()
        );
    }
}
