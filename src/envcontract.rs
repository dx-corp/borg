//! The build environment contract: facts a drone reports about its build
//! environment, and the queen's pure admission check over them.
//!
//! Before work (a CI runner lease or a work unit) is placed on a drone, six
//! checks are evaluated against what the drone last reported:
//!
//! 1. **compiler_cache**: `RUSTC_WRAPPER` resolves to an allowed wrapper
//!    (sccache, or the single-flight wrapper in front of it), `SCCACHE_DIR` is
//!    set and the sccache server is reachable;
//! 2. **toolchain**: when the work declares a Rust toolchain (the class config,
//!    or `rust-toolchain.toml` at the unit's tree), the drone has it installed;
//! 3. **cache_store**: the sccache stats call works, the error rate is under a
//!    threshold, the cache directory has free space above a floor, and a write
//!    works. A pathological hit rate is *reported* as a warning, never blocks;
//! 4. **resources**: memory available now covers the class's memory, and live
//!    CPU pressure (PSI) is under a ceiling. (Free capacity after active leases
//!    is enforced by the allocator itself.)
//! 5. **tools**: every tool the operator lists as required (globally or for the
//!    class) resolved on the job user's `PATH` when the drone probed it. The
//!    list is empty unless the operator writes one.
//! 6. **capabilities**: actual sandbox, Docker and filesystem smoke tests run
//!    as the job user. Named requirements fail closed even on older drones;
//!    the list is empty unless the operator writes one.
//!
//! The drone is a dumb prober: it reports *facts* ([`EnvFacts`]), every one of
//! which may be unknown (`None`), cached about a minute on its side. Thresholds
//! live in the queen's config, so they change without touching drones. Each
//! check is `required`, `advisory` or `off`, globally and per class. A required
//! check that fails or is unknown makes the drone ineligible for that class;
//! [`evaluate`] is a pure function of the settings, the facts and the clock, so
//! the allocator stays pure (it only sees `DroneState::blocked_classes`).
//!
//! Nothing is enforced unless `env_contract.enabled`; a drone that has never
//! reported is skipped unless `require_report` is set, so enabling the contract
//! does not cut off drones that predate it. Explicit required capability
//! names are the exception: a missing job-user execution proof blocks them.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::classes::{Class, ClassSpec, Classes};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    CompilerCache,
    Toolchain,
    CacheStore,
    Resources,
    Tools,
    Capabilities,
}

/// The tools a drone looks for on the job user's `PATH`, in report order.
pub const PROBE_TOOLS: [&str; 15] = [
    "git", "gh", "node", "npm", "pnpm", "python3", "docker", "cargo", "rustc", "bazel", "go", "uv",
    "mise", "jq", "cosign",
];

/// The value reported for a probed tool that is not on `PATH`.
pub const TOOL_MISSING: &str = "missing";

impl CheckKind {
    pub const ALL: [Self; 6] = [
        Self::CompilerCache,
        Self::Toolchain,
        Self::CacheStore,
        Self::Resources,
        Self::Tools,
        Self::Capabilities,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::CompilerCache => "compiler_cache",
            Self::Toolchain => "toolchain",
            Self::CacheStore => "cache_store",
            Self::Resources => "resources",
            Self::Tools => "tools",
            Self::Capabilities => "capabilities",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// A failing or unknown check makes the drone ineligible.
    Required,
    /// Evaluated and reported, never blocks.
    Advisory,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    Pass,
    Unknown,
    Fail,
}

impl CheckState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Unknown => "unknown",
            Self::Fail => "fail",
        }
    }

    pub const ALL: [Self; 3] = [Self::Pass, Self::Fail, Self::Unknown];
}

/// Execution capabilities proven by a smoke test as the configured job user.
/// Inventory (for example `docker info` as the service user) is not proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Sandbox,
    Docker,
    WorkWrite,
    CacheWrite,
}

impl Capability {
    pub const ALL: [Self; 4] = [
        Self::Sandbox,
        Self::Docker,
        Self::WorkWrite,
        Self::CacheWrite,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sandbox => "sandbox",
            Self::Docker => "docker",
            Self::WorkWrite => "work_write",
            Self::CacheWrite => "cache_write",
        }
    }
}

/// A capability's own observation time survives cached heartbeat reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityFact {
    pub observed_at: i64,
    pub state: CheckState,
}

/// An installed Rust toolchain, as `rustc -vV` reports it for the channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Toolchain {
    /// The channel as `rust-toolchain.toml` names it (`1.95.0`, `stable`,
    /// `nightly-2026-01-01`).
    pub channel: String,
    /// The `release:` line of `rustc -vV` for that channel.
    pub rustc_version: String,
}

/// What a drone reports about its build environment. Every fact may be
/// unknown; `None` means "not measured", never "fine".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvFacts {
    /// Unix time the probes ran (the drone may serve cached probes for ~60 s).
    pub observed_at: i64,
    /// `RUSTC_WRAPPER` as a job on this drone would see it; `None` if unset.
    #[serde(default)]
    pub rustc_wrapper: Option<String>,
    /// `SCCACHE_DIR` as a job would see it; `None` if unset.
    #[serde(default)]
    pub sccache_dir: Option<String>,
    /// Whether the sccache server answered.
    #[serde(default)]
    pub sccache_server_reachable: Option<bool>,
    /// Whether `sccache --show-stats` succeeded.
    #[serde(default)]
    pub sccache_stats_ok: Option<bool>,
    /// Compile requests, hits and errors from the stats.
    #[serde(default)]
    pub cache_requests: Option<u64>,
    #[serde(default)]
    pub cache_hits: Option<u64>,
    #[serde(default)]
    pub cache_errors: Option<u64>,
    /// Free bytes on the filesystem holding the cache directory.
    #[serde(default)]
    pub cache_dir_free_bytes: Option<u64>,
    /// Whether a test write into the cache directory succeeded.
    #[serde(default)]
    pub cache_write_ok: Option<bool>,
    /// Installed toolchains; `None` when the probe could not run.
    #[serde(default)]
    pub toolchains: Option<Vec<Toolchain>>,
    /// `MemAvailable` (Linux) or the platform equivalent, in MiB.
    #[serde(default)]
    pub mem_available_mib: Option<u64>,
    /// Linux PSI `cpu some avg10`, in permille (percent times ten); `None`
    /// where PSI does not exist.
    #[serde(default)]
    pub cpu_psi_some_avg10_pm: Option<u16>,
    /// For each tool in [`PROBE_TOOLS`], the first line of its `--version`
    /// output, or `"missing"`; `None` when the drone did not measure it.
    #[serde(default)]
    pub tools: Option<BTreeMap<String, String>>,
    /// Actual execution proofs as the job user, each with its probe timestamp.
    #[serde(default)]
    pub capabilities: BTreeMap<Capability, CapabilityFact>,
}

/// Admission safety, independent of optional cache efficiency checks.
#[derive(Debug, Clone, Serialize)]
pub struct DiskAdmission {
    pub state: CheckState,
    pub minimum_free_bytes: u64,
    pub work_free_bytes: Option<u64>,
    pub cache_free_bytes: Option<u64>,
}

impl DiskAdmission {
    pub fn blocked(&self) -> bool {
        self.state == CheckState::Fail
    }
}

pub const DEFAULT_MIN_DISK_FREE_BYTES: u64 = 10 << 30;
pub fn default_min_disk_free_bytes() -> u64 {
    DEFAULT_MIN_DISK_FREE_BYTES
}

/// A floor guards admission; it does not reserve space for active jobs.
/// Any known low filesystem blocks. Unknown measurements retain compatibility.
pub fn disk_admission(
    minimum_free_bytes: u64,
    work_free_mib: Option<u64>,
    cache_free_bytes: Option<u64>,
) -> DiskAdmission {
    let work_free_bytes = work_free_mib.map(|mib| mib.saturating_mul(1 << 20));
    let low = [work_free_bytes, cache_free_bytes]
        .into_iter()
        .flatten()
        .any(|free| free < minimum_free_bytes);
    let state = if low {
        CheckState::Fail
    } else if work_free_bytes.is_none() || cache_free_bytes.is_none() {
        CheckState::Unknown
    } else {
        CheckState::Pass
    };
    DiskAdmission {
        state,
        minimum_free_bytes,
        work_free_bytes,
        cache_free_bytes,
    }
}

/// Settings of one class (or the defaults for all).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClassContract {
    /// Check modes overriding the global ones.
    pub checks: BTreeMap<CheckKind, Mode>,
    /// The Rust toolchain every build of this class needs.
    pub rust_toolchain: Option<String>,
    /// Tools this class needs on `PATH`; replaces the global list.
    pub required_tools: Option<Vec<String>>,
    /// Execution proofs this class needs; replaces the global list.
    pub required_capabilities: Option<Vec<Capability>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContractSettings {
    /// Off unless the queen opts in.
    pub enabled: bool,
    /// Treat a drone that has never reported facts as failing the required
    /// checks. Off by default so enabling the contract does not cut off drones
    /// that predate it.
    pub require_report: bool,
    /// Facts older than this count as unknown.
    pub facts_max_age_secs: i64,
    pub allowed_wrappers: Vec<String>,
    pub min_cache_dir_free_bytes: u64,
    /// Cache errors per thousand requests above which the store fails.
    pub max_cache_error_pm: u32,
    /// A hit rate at or below this per thousand, after `min_requests_for_hit_rate`
    /// requests, is reported as a warning (never blocks).
    pub hit_rate_warn_pm: u32,
    pub min_requests_for_hit_rate: u64,
    /// CPU pressure (permille) above which resources fail.
    pub max_cpu_psi_pm: u16,
    /// Memory available must be at least this percent of the class's memory.
    pub mem_available_pct: u64,
    /// Tools (from [`PROBE_TOOLS`]) every class needs on `PATH`. Empty means
    /// the `tools` check has nothing to require.
    pub required_tools: Vec<String>,
    /// Execution proofs every class needs. Empty preserves older drones.
    pub required_capabilities: Vec<Capability>,
    pub checks: BTreeMap<CheckKind, Mode>,
    pub classes: BTreeMap<Class, ClassContract>,
}

impl Default for ContractSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            require_report: false,
            facts_max_age_secs: 180,
            allowed_wrappers: vec!["sccache".into()],
            min_cache_dir_free_bytes: DEFAULT_MIN_DISK_FREE_BYTES,
            max_cache_error_pm: 50,
            hit_rate_warn_pm: 10,
            min_requests_for_hit_rate: 200,
            max_cpu_psi_pm: 700,
            mem_available_pct: 100,
            required_tools: Vec::new(),
            required_capabilities: Vec::new(),
            // Conservative: only the checks that protect a build from failing
            // outright block by default; the cache checks report.
            checks: BTreeMap::from([
                (CheckKind::CompilerCache, Mode::Advisory),
                (CheckKind::Toolchain, Mode::Required),
                (CheckKind::CacheStore, Mode::Advisory),
                (CheckKind::Resources, Mode::Required),
                (CheckKind::Tools, Mode::Advisory),
                (CheckKind::Capabilities, Mode::Required),
            ]),
            classes: BTreeMap::new(),
        }
    }
}

impl ContractSettings {
    pub fn validate(&self) -> Result<(), String> {
        if self.facts_max_age_secs <= 0
            || self.allowed_wrappers.is_empty()
            || self.mem_available_pct == 0
        {
            return Err("env_contract needs positive facts_max_age_secs and mem_available_pct and at least one allowed wrapper".into());
        }
        if self.max_cache_error_pm > 1000
            || self.max_cpu_psi_pm > 1000
            || self.hit_rate_warn_pm > 1000
        {
            return Err("env_contract permille values are at most 1000".into());
        }
        let bad_tc = |t: &str| {
            t.is_empty()
                || t.len() > 64
                || !t
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        };
        if self
            .classes
            .values()
            .filter_map(|c| c.rust_toolchain.as_deref())
            .any(bad_tc)
        {
            return Err(
                "env_contract rust_toolchain must look like 1.95.0 or nightly-2026-01-01".into(),
            );
        }
        let unprobed = |t: &String| !PROBE_TOOLS.contains(&t.as_str());
        if self
            .required_tools
            .iter()
            .chain(
                self.classes
                    .values()
                    .flat_map(|c| c.required_tools.iter().flatten()),
            )
            .any(unprobed)
        {
            return Err(format!(
                "env_contract required_tools must be among: {}",
                PROBE_TOOLS.join(", ")
            ));
        }
        Ok(())
    }

    /// The tools a class needs on `PATH`: its own list, else the global one.
    pub fn class_required_tools(&self, class: Class) -> &[String] {
        self.classes
            .get(&class)
            .and_then(|c| c.required_tools.as_deref())
            .unwrap_or(&self.required_tools)
    }

    /// Execution proofs a class needs: its own list, else the global one.
    pub fn class_required_capabilities(&self, class: Class) -> &[Capability] {
        self.classes
            .get(&class)
            .and_then(|c| c.required_capabilities.as_deref())
            .unwrap_or(&self.required_capabilities)
    }

    fn mode(&self, class: Class, kind: CheckKind) -> Mode {
        self.classes
            .get(&class)
            .and_then(|c| c.checks.get(&kind))
            .or_else(|| self.checks.get(&kind))
            .copied()
            .unwrap_or(if kind == CheckKind::Capabilities {
                Mode::Required
            } else {
                Mode::Off
            })
    }

    /// The toolchain a class needs by configuration.
    pub fn class_toolchain(&self, class: Class) -> Option<&str> {
        self.classes
            .get(&class)
            .and_then(|c| c.rust_toolchain.as_deref())
    }
}

/// What the work being admitted needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Need<'a> {
    pub class: Class,
    pub mem_mib: u64,
    pub rust_toolchain: Option<&'a str>,
}

impl<'a> Need<'a> {
    /// The need of a class; `unit_toolchain` (from the unit's tree) wins over
    /// the class configuration.
    pub fn for_class(
        s: &'a ContractSettings,
        class: Class,
        spec: &ClassSpec,
        unit_toolchain: Option<&'a str>,
    ) -> Self {
        Self {
            class,
            mem_mib: spec.mem_mib,
            rust_toolchain: unit_toolchain.or_else(|| s.class_toolchain(class)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckOutcome {
    pub kind: CheckKind,
    pub mode: Mode,
    pub state: CheckState,
    pub reason: String,
    /// Values the verdict was based on.
    pub observed: BTreeMap<String, String>,
    /// Things worth knowing that never block (e.g. a pathological hit rate).
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Verdict {
    /// Whether any facts have been reported.
    pub reported: bool,
    pub age_secs: Option<i64>,
    pub checks: Vec<CheckOutcome>,
    pub admit: bool,
    /// Why the work is not admitted.
    pub why: Option<String>,
}

fn worst(a: CheckState, b: CheckState) -> CheckState {
    a.max(b)
}

fn basename(p: &str) -> &str {
    p.rsplit(['/', '\\']).next().unwrap_or(p)
}

struct Acc {
    state: CheckState,
    reasons: Vec<String>,
    observed: BTreeMap<String, String>,
    warnings: Vec<String>,
}

impl Acc {
    fn new() -> Self {
        Self {
            state: CheckState::Pass,
            reasons: Vec::new(),
            observed: BTreeMap::new(),
            warnings: Vec::new(),
        }
    }

    fn note(&mut self, state: CheckState, why: impl Into<String>) {
        if state != CheckState::Pass {
            self.state = worst(self.state, state);
            self.reasons.push(why.into());
        }
    }

    fn see(&mut self, k: &str, v: impl ToString) {
        self.observed.insert(k.to_owned(), v.to_string());
    }

    fn finish(self, kind: CheckKind, mode: Mode) -> CheckOutcome {
        let reason = if self.reasons.is_empty() {
            "ok".to_owned()
        } else {
            self.reasons.join("; ")
        };
        CheckOutcome {
            kind,
            mode,
            state: self.state,
            reason,
            observed: self.observed,
            warnings: self.warnings,
        }
    }
}

fn compiler_cache(s: &ContractSettings, f: &EnvFacts) -> Acc {
    let mut a = Acc::new();
    match &f.rustc_wrapper {
        None => a.note(CheckState::Fail, "RUSTC_WRAPPER is not set"),
        Some(w) => {
            a.see("rustc_wrapper", w);
            if !s.allowed_wrappers.iter().any(|ok| ok == basename(w)) {
                a.note(
                    CheckState::Fail,
                    format!("RUSTC_WRAPPER={w} is not an allowed wrapper"),
                );
            }
        }
    }
    match &f.sccache_dir {
        None => a.note(CheckState::Fail, "SCCACHE_DIR is not set"),
        Some(d) => a.see("sccache_dir", d),
    }
    match f.sccache_server_reachable {
        Some(true) => a.see("sccache_server_reachable", true),
        Some(false) => a.note(CheckState::Fail, "the sccache server is not reachable"),
        None => a.note(
            CheckState::Unknown,
            "sccache server reachability is unknown",
        ),
    }
    a
}

fn toolchain(f: &EnvFacts, need: &Need<'_>) -> Acc {
    let mut a = Acc::new();
    let Some(want) = need.rust_toolchain else {
        a.see("required", "none");
        return a;
    };
    a.see("required", want);
    match &f.toolchains {
        None => a.note(CheckState::Unknown, "installed toolchains are unknown"),
        Some(have) => {
            let names: Vec<String> = have.iter().map(|t| t.channel.clone()).collect();
            a.see("installed", names.join(","));
            if !have
                .iter()
                .any(|t| t.channel == want || t.rustc_version == want)
            {
                a.note(
                    CheckState::Fail,
                    format!(
                        "toolchain {want} is not installed (have {})",
                        names.join(", ")
                    ),
                );
            }
        }
    }
    a
}

fn cache_store(s: &ContractSettings, f: &EnvFacts) -> Acc {
    let mut a = Acc::new();
    match f.sccache_stats_ok {
        Some(true) => {}
        Some(false) => a.note(CheckState::Fail, "sccache --show-stats failed"),
        None => a.note(CheckState::Unknown, "cache stats were not measured"),
    }
    if let (Some(req), Some(err)) = (f.cache_requests, f.cache_errors) {
        a.see("cache_requests", req);
        a.see("cache_errors", err);
        if req > 0 && err.saturating_mul(1000) / req > u64::from(s.max_cache_error_pm) {
            a.note(
                CheckState::Fail,
                format!(
                    "cache error rate {}‰ is over {}‰",
                    err.saturating_mul(1000) / req,
                    s.max_cache_error_pm
                ),
            );
        }
    }
    match f.cache_dir_free_bytes {
        Some(free) => {
            a.see("cache_dir_free_bytes", free);
            if free < s.min_cache_dir_free_bytes {
                a.note(
                    CheckState::Fail,
                    format!(
                        "cache directory has {free} bytes free, under the floor of {}",
                        s.min_cache_dir_free_bytes
                    ),
                );
            }
        }
        None => a.note(CheckState::Unknown, "cache directory free space is unknown"),
    }
    match f.cache_write_ok {
        Some(true) => {}
        Some(false) => a.note(CheckState::Fail, "writing to the cache directory failed"),
        None => a.note(CheckState::Unknown, "cache writability was not measured"),
    }
    if let (Some(req), Some(hits)) = (f.cache_requests, f.cache_hits)
        && req >= s.min_requests_for_hit_rate
    {
        let pm = hits.saturating_mul(1000) / req.max(1);
        a.see("hit_rate_pm", pm);
        if pm <= u64::from(s.hit_rate_warn_pm) {
            a.warnings
                .push(format!("hit rate is {pm}‰ after {req} requests"));
        }
    }
    a
}

fn resources(s: &ContractSettings, f: &EnvFacts, need: &Need<'_>) -> Acc {
    let mut a = Acc::new();
    let mut known = 0;
    if let Some(avail) = f.mem_available_mib {
        known += 1;
        a.see("mem_available_mib", avail);
        a.see("mem_needed_mib", need.mem_mib);
        if avail.saturating_mul(100) < need.mem_mib.saturating_mul(s.mem_available_pct) {
            a.note(
                CheckState::Fail,
                format!(
                    "{avail} MiB available, {} class needs {} MiB",
                    need.class, need.mem_mib
                ),
            );
        }
    }
    if let Some(psi) = f.cpu_psi_some_avg10_pm {
        known += 1;
        a.see("cpu_psi_some_avg10_pm", psi);
        if psi > s.max_cpu_psi_pm {
            a.note(
                CheckState::Fail,
                format!("cpu pressure {psi}‰ is over {}‰", s.max_cpu_psi_pm),
            );
        }
    }
    if known == 0 {
        a.note(
            CheckState::Unknown,
            "neither available memory nor cpu pressure was measured",
        );
    }
    a
}

fn tools(s: &ContractSettings, f: &EnvFacts, need: &Need<'_>) -> Acc {
    let mut a = Acc::new();
    let wanted = s.class_required_tools(need.class);
    a.see("required", wanted.join(","));
    let Some(have) = &f.tools else {
        if wanted.is_empty() {
            return a;
        }
        if s.require_report {
            a.note(CheckState::Fail, "the drone did not report its tools");
        } else {
            a.warnings
                .push("the drone did not report its tools".to_owned());
        }
        return a;
    };
    a.see(
        "versions",
        have.iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; "),
    );
    a.see(
        "missing",
        have.iter()
            .filter(|(_, v)| *v == TOOL_MISSING)
            .map(|(k, _)| k.as_str())
            .collect::<Vec<_>>()
            .join(","),
    );
    let missing: Vec<&str> = wanted
        .iter()
        .filter(|t| have.get(*t).is_none_or(|v| v == TOOL_MISSING))
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        a.note(
            CheckState::Fail,
            format!("required tools missing on PATH: {}", missing.join(", ")),
        );
    }
    a
}

fn capability_fresh(s: &ContractSettings, fact: &CapabilityFact, now: i64) -> bool {
    fact.observed_at <= now && now.saturating_sub(fact.observed_at) <= s.facts_max_age_secs
}

fn capability_state(s: &ContractSettings, fact: Option<&CapabilityFact>, now: i64) -> CheckState {
    match fact {
        Some(fact) if capability_fresh(s, fact, now) => fact.state,
        _ => CheckState::Unknown,
    }
}

/// Effective states of the named proofs this class needs, using the same
/// per-proof freshness rules as admission. Check mode is deliberately left
/// to the caller: diagnostics may also show advisory requirements.
pub fn capability_states(
    s: &ContractSettings,
    facts: Option<&EnvFacts>,
    class: Class,
    now: i64,
) -> BTreeMap<Capability, CheckState> {
    s.class_required_capabilities(class)
        .iter()
        .map(|&kind| {
            let fact = facts.and_then(|f| f.capabilities.get(&kind));
            (kind, capability_state(s, fact, now))
        })
        .collect()
}

fn capabilities(s: &ContractSettings, f: &EnvFacts, need: &Need<'_>, now: i64) -> Acc {
    let mut a = Acc::new();
    let required = s.class_required_capabilities(need.class);
    a.see(
        "required",
        required
            .iter()
            .map(|c| c.as_str())
            .collect::<Vec<_>>()
            .join(","),
    );
    for &capability in required {
        let name = capability.as_str();
        match f.capabilities.get(&capability) {
            None => a.note(
                CheckState::Unknown,
                format!("{name}: job-user proof was not reported"),
            ),
            Some(fact) => {
                let age = now.saturating_sub(fact.observed_at);
                a.see(&format!("{name}_age_secs"), age);
                a.see(name, fact.state.as_str());
                if !capability_fresh(s, fact, now) {
                    a.note(
                        CheckState::Unknown,
                        format!("{name}: proof is stale or future-dated ({age}s old)"),
                    );
                } else {
                    a.note(
                        capability_state(s, Some(fact), now),
                        format!("{name}: job-user smoke test {}", fact.state.as_str()),
                    );
                }
            }
        }
    }
    if s.mode(need.class, CheckKind::Capabilities) == Mode::Advisory {
        a.warnings.extend(a.reasons.iter().cloned());
    }
    // Unrequired failures remain diagnostic and never exclude a class.
    for (capability, fact) in &f.capabilities {
        if !required.contains(capability) && fact.state != CheckState::Pass {
            a.warnings.push(format!(
                "{}: unrequired job-user smoke test {}",
                capability.as_str(),
                fact.state.as_str()
            ));
        }
    }
    a
}

/// Evaluate the contract for `need` against `facts` at `now`. Pure.
pub fn evaluate(
    s: &ContractSettings,
    facts: Option<&EnvFacts>,
    need: &Need<'_>,
    now: i64,
) -> Verdict {
    if !s.enabled {
        return Verdict {
            reported: facts.is_some(),
            age_secs: facts.map(|f| now - f.observed_at),
            checks: Vec::new(),
            admit: true,
            why: None,
        };
    }
    let modes: Vec<(CheckKind, Mode)> = CheckKind::ALL
        .into_iter()
        .map(|k| (k, s.mode(need.class, k)))
        .filter(|(_, m)| *m != Mode::Off)
        .collect();
    let Some(f) = facts else {
        let blocked = modes.iter().any(|&(kind, mode)| {
            mode == Mode::Required
                && if kind == CheckKind::Capabilities {
                    !s.class_required_capabilities(need.class).is_empty()
                } else {
                    s.require_report
                }
        });
        return Verdict {
            reported: false,
            age_secs: None,
            checks: modes
                .iter()
                .map(|&(kind, mode)| {
                    if kind == CheckKind::Capabilities {
                        capabilities(s, &EnvFacts::default(), need, now).finish(kind, mode)
                    } else {
                        CheckOutcome {
                            kind,
                            mode,
                            state: CheckState::Unknown,
                            reason: "the drone has not reported its environment".into(),
                            observed: BTreeMap::new(),
                            warnings: Vec::new(),
                        }
                    }
                })
                .collect(),
            admit: !blocked,
            why: blocked.then(|| "the drone has not reported its environment".to_owned()),
        };
    };
    let age = now - f.observed_at;
    let stale = age > s.facts_max_age_secs;
    let mut checks = Vec::new();
    for (kind, mode) in modes {
        let acc = if kind == CheckKind::Capabilities {
            // Each execution proof has its own clock. An empty requirement
            // must not become blocking just because unrelated facts aged.
            capabilities(s, f, need, now)
        } else if stale {
            let mut a = Acc::new();
            a.note(
                CheckState::Unknown,
                format!("facts are {age}s old (limit {}s)", s.facts_max_age_secs),
            );
            a
        } else {
            match kind {
                CheckKind::CompilerCache => compiler_cache(s, f),
                CheckKind::Toolchain => toolchain(f, need),
                CheckKind::CacheStore => cache_store(s, f),
                CheckKind::Resources => resources(s, f, need),
                CheckKind::Tools => tools(s, f, need),
                CheckKind::Capabilities => capabilities(s, f, need, now),
            }
        };
        checks.push(acc.finish(kind, mode));
    }
    let blockers: Vec<String> = checks
        .iter()
        .filter(|c| c.mode == Mode::Required && c.state != CheckState::Pass)
        .map(|c| format!("{} {}: {}", c.kind.as_str(), c.state.as_str(), c.reason))
        .collect();
    Verdict {
        reported: true,
        age_secs: Some(age),
        checks,
        admit: blockers.is_empty(),
        why: (!blockers.is_empty()).then(|| blockers.join("; ")),
    }
}

/// The classes `facts` do not admit, for `DroneState::blocked_classes`.
pub fn blocked_classes(
    s: &ContractSettings,
    facts: Option<&EnvFacts>,
    classes: &Classes,
    unit_toolchain: Option<&str>,
    now: i64,
) -> Vec<Class> {
    if !s.enabled {
        return Vec::new();
    }
    Class::ALL
        .into_iter()
        .filter(|&c| {
            let need = Need::for_class(s, c, classes.spec(c), unit_toolchain);
            !evaluate(s, facts, &need, now).admit
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_admission_blocks_either_known_low_mount_and_keeps_unknown_explicit() {
        let floor = 32 << 30;
        assert!(disk_admission(floor, Some(8 * 1024), Some(100 << 30)).blocked());
        assert!(disk_admission(floor, Some(100 * 1024), Some(8 << 30)).blocked());
        assert!(disk_admission(floor, None, Some(0)).blocked());
        assert_eq!(
            disk_admission(floor, Some(32 * 1024), Some(floor)).state,
            CheckState::Pass
        );
        assert_eq!(disk_admission(floor, None, None).state, CheckState::Unknown);
        assert_eq!(
            disk_admission(floor, Some(100 * 1024), None).state,
            CheckState::Unknown
        );
        assert!(!disk_admission(floor, None, None).blocked());
    }

    const NOW: i64 = 10_000;

    fn on() -> ContractSettings {
        ContractSettings {
            enabled: true,
            ..ContractSettings::default()
        }
    }

    fn good() -> EnvFacts {
        EnvFacts {
            observed_at: NOW - 10,
            rustc_wrapper: Some("/usr/local/bin/sccache".into()),
            sccache_dir: Some("/var/lib/borg/cache/sccache".into()),
            sccache_server_reachable: Some(true),
            sccache_stats_ok: Some(true),
            cache_requests: Some(1000),
            cache_hits: Some(600),
            cache_errors: Some(2),
            cache_dir_free_bytes: Some(50 << 30),
            cache_write_ok: Some(true),
            toolchains: Some(vec![Toolchain {
                channel: "1.95.0".into(),
                rustc_version: "1.95.0".into(),
            }]),
            mem_available_mib: Some(20_000),
            cpu_psi_some_avg10_pm: Some(100),
            tools: Some(tool_map(&[
                ("git", "git version 2.45.0"),
                ("gh", "gh version 2.50.0"),
            ])),
            capabilities: BTreeMap::new(),
        }
    }

    fn tool_map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn tools_on(mode: Mode, required: &[&str]) -> ContractSettings {
        ContractSettings {
            checks: BTreeMap::from([(CheckKind::Tools, mode)]),
            required_tools: required.iter().map(|t| (*t).to_owned()).collect(),
            ..on()
        }
    }

    fn need(class: Class, tc: Option<&str>) -> Need<'_> {
        Need {
            class,
            mem_mib: Classes::default().spec(class).mem_mib,
            rust_toolchain: tc,
        }
    }

    fn check(v: &Verdict, kind: CheckKind) -> &CheckOutcome {
        v.checks.iter().find(|c| c.kind == kind).unwrap()
    }

    fn all_required() -> ContractSettings {
        ContractSettings {
            checks: CheckKind::ALL
                .into_iter()
                .map(|k| (k, Mode::Required))
                .collect(),
            ..on()
        }
    }

    #[test]
    fn a_healthy_drone_passes_every_check_and_is_admitted() {
        let v = evaluate(
            &all_required(),
            Some(&good()),
            &need(Class::Medium, Some("1.95.0")),
            NOW,
        );
        assert!(v.admit, "{v:?}");
        assert!(
            v.checks.iter().all(|c| c.state == CheckState::Pass),
            "{v:?}"
        );
        assert_eq!(v.age_secs, Some(10));
        assert_eq!(
            check(&v, CheckKind::Toolchain).observed["installed"],
            "1.95.0"
        );
    }

    #[test]
    fn compiler_cache_requires_an_allowed_wrapper_a_dir_and_a_reachable_server() {
        let s = all_required();
        let n = need(Class::Light, None);
        let run = |f: EnvFacts| evaluate(&s, Some(&f), &n, NOW);
        let v = run(EnvFacts {
            rustc_wrapper: None,
            ..good()
        });
        assert!(!v.admit);
        assert!(
            check(&v, CheckKind::CompilerCache)
                .reason
                .contains("RUSTC_WRAPPER is not set")
        );
        let v = run(EnvFacts {
            rustc_wrapper: Some("/usr/bin/ccache".into()),
            ..good()
        });
        assert!(
            check(&v, CheckKind::CompilerCache)
                .reason
                .contains("not an allowed wrapper")
        );
        // The single-flight wrapper is allowed when configured.
        let s2 = ContractSettings {
            allowed_wrappers: vec!["sccache".into(), "borg-sccache".into()],
            ..all_required()
        };
        let f = EnvFacts {
            rustc_wrapper: Some("/opt/borg/bin/borg-sccache".into()),
            ..good()
        };
        assert!(evaluate(&s2, Some(&f), &n, NOW).admit);
        let v = run(EnvFacts {
            sccache_dir: None,
            ..good()
        });
        assert!(
            check(&v, CheckKind::CompilerCache)
                .reason
                .contains("SCCACHE_DIR")
        );
        let v = run(EnvFacts {
            sccache_server_reachable: Some(false),
            ..good()
        });
        assert_eq!(check(&v, CheckKind::CompilerCache).state, CheckState::Fail);
        let v = run(EnvFacts {
            sccache_server_reachable: None,
            ..good()
        });
        assert_eq!(
            check(&v, CheckKind::CompilerCache).state,
            CheckState::Unknown
        );
        assert!(!v.admit, "unknown on a required check blocks");
    }

    #[test]
    fn toolchain_matches_by_channel_or_version_and_only_when_declared() {
        let s = all_required();
        let f = good();
        assert!(evaluate(&s, Some(&f), &need(Class::Light, None), NOW).admit);
        let v = evaluate(&s, Some(&f), &need(Class::Light, Some("1.96.0")), NOW);
        assert!(!v.admit);
        assert!(
            check(&v, CheckKind::Toolchain)
                .reason
                .contains("1.96.0 is not installed (have 1.95.0)")
        );
        // A named channel resolved to a version matches by version too.
        let g = EnvFacts {
            toolchains: Some(vec![Toolchain {
                channel: "stable".into(),
                rustc_version: "1.95.0".into(),
            }]),
            ..good()
        };
        assert!(evaluate(&s, Some(&g), &need(Class::Light, Some("1.95.0")), NOW).admit);
        let u = EnvFacts {
            toolchains: None,
            ..good()
        };
        let v = evaluate(&s, Some(&u), &need(Class::Light, Some("1.95.0")), NOW);
        assert_eq!(check(&v, CheckKind::Toolchain).state, CheckState::Unknown);
        // The class config supplies a requirement; a unit's own wins.
        let mut cs = all_required();
        cs.classes.insert(
            Class::Heavy,
            ClassContract {
                rust_toolchain: Some("1.90.0".into()),
                ..ClassContract::default()
            },
        );
        let n = Need::for_class(
            &cs,
            Class::Heavy,
            Classes::default().spec(Class::Heavy),
            None,
        );
        assert_eq!(n.rust_toolchain, Some("1.90.0"));
        let n = Need::for_class(
            &cs,
            Class::Heavy,
            Classes::default().spec(Class::Heavy),
            Some("1.95.0"),
        );
        assert_eq!(n.rust_toolchain, Some("1.95.0"));
    }

    #[test]
    fn cache_store_checks_errors_free_space_writes_and_only_warns_about_hit_rate() {
        let s = all_required();
        let n = need(Class::Light, None);
        let run = |f: EnvFacts| evaluate(&s, Some(&f), &n, NOW);
        let v = run(EnvFacts {
            cache_errors: Some(100),
            ..good()
        });
        assert!(
            check(&v, CheckKind::CacheStore)
                .reason
                .contains("error rate 100‰")
        );
        let v = run(EnvFacts {
            cache_dir_free_bytes: Some(1 << 30),
            ..good()
        });
        assert!(
            check(&v, CheckKind::CacheStore)
                .reason
                .contains("under the floor")
        );
        let v = run(EnvFacts {
            cache_write_ok: Some(false),
            ..good()
        });
        assert_eq!(check(&v, CheckKind::CacheStore).state, CheckState::Fail);
        let v = run(EnvFacts {
            sccache_stats_ok: Some(false),
            ..good()
        });
        assert_eq!(check(&v, CheckKind::CacheStore).state, CheckState::Fail);
        // A 0% hit rate after enough requests warns and does not block.
        let v = run(EnvFacts {
            cache_hits: Some(0),
            ..good()
        });
        assert!(v.admit, "{v:?}");
        assert!(check(&v, CheckKind::CacheStore).warnings[0].contains("hit rate is 0"));
        // Too few requests to judge: no warning.
        let v = run(EnvFacts {
            cache_requests: Some(10),
            cache_hits: Some(0),
            cache_errors: Some(0),
            ..good()
        });
        assert!(check(&v, CheckKind::CacheStore).warnings.is_empty());
    }

    #[test]
    fn resources_compare_available_memory_with_the_class_and_cpu_pressure_with_the_ceiling() {
        let s = all_required();
        // Heavy needs 24 GiB.
        let heavy = need(Class::Heavy, None);
        let v = evaluate(
            &s,
            Some(&EnvFacts {
                mem_available_mib: Some(10_000),
                ..good()
            }),
            &heavy,
            NOW,
        );
        assert!(!v.admit);
        assert!(
            check(&v, CheckKind::Resources)
                .reason
                .contains("10000 MiB available")
        );
        let v = evaluate(
            &s,
            Some(&EnvFacts {
                mem_available_mib: Some(30_000),
                ..good()
            }),
            &heavy,
            NOW,
        );
        assert!(v.admit);
        let v = evaluate(
            &s,
            Some(&EnvFacts {
                cpu_psi_some_avg10_pm: Some(900),
                ..good()
            }),
            &need(Class::Light, None),
            NOW,
        );
        assert!(
            check(&v, CheckKind::Resources)
                .reason
                .contains("cpu pressure 900")
        );
        // No PSI (macOS) but memory known: still decidable.
        let v = evaluate(
            &s,
            Some(&EnvFacts {
                cpu_psi_some_avg10_pm: None,
                ..good()
            }),
            &need(Class::Light, None),
            NOW,
        );
        assert!(v.admit);
        // Nothing measured: unknown, which a required check treats as blocking.
        let v = evaluate(
            &s,
            Some(&EnvFacts {
                cpu_psi_some_avg10_pm: None,
                mem_available_mib: None,
                ..good()
            }),
            &need(Class::Light, None),
            NOW,
        );
        assert_eq!(check(&v, CheckKind::Resources).state, CheckState::Unknown);
        assert!(!v.admit);
    }

    #[test]
    fn advisory_and_off_checks_never_block_and_class_overrides_apply() {
        let bad = EnvFacts {
            rustc_wrapper: None,
            ..good()
        };
        let n = need(Class::Light, None);
        // Default: compiler_cache is advisory, so the failure is reported only.
        let v = evaluate(&on(), Some(&bad), &n, NOW);
        assert!(v.admit);
        assert_eq!(check(&v, CheckKind::CompilerCache).state, CheckState::Fail);
        // Required for one class only.
        let mut s = on();
        s.classes.insert(
            Class::Heavy,
            ClassContract {
                checks: BTreeMap::from([(CheckKind::CompilerCache, Mode::Required)]),
                ..ClassContract::default()
            },
        );
        assert!(evaluate(&s, Some(&bad), &need(Class::Light, None), NOW).admit);
        assert!(!evaluate(&s, Some(&bad), &need(Class::Heavy, None), NOW).admit);
        // Off removes the check from the report.
        s.checks.insert(CheckKind::CompilerCache, Mode::Off);
        let v = evaluate(&s, Some(&bad), &need(Class::Light, None), NOW);
        assert!(v.checks.iter().all(|c| c.kind != CheckKind::CompilerCache));
    }

    #[test]
    fn stale_or_missing_facts_are_unknown_and_the_report_requirement_is_opt_in() {
        let s = all_required();
        let old = EnvFacts {
            observed_at: NOW - 1000,
            ..good()
        };
        let v = evaluate(&s, Some(&old), &need(Class::Light, None), NOW);
        assert!(!v.admit);
        assert!(
            v.checks
                .iter()
                .filter(|c| c.kind != CheckKind::Capabilities)
                .all(|c| c.state == CheckState::Unknown)
        );
        assert_eq!(check(&v, CheckKind::Capabilities).state, CheckState::Pass);
        assert!(v.checks[0].reason.contains("1000s old"));
        // A drone that never reported: admitted by default, blocked on request.
        let v = evaluate(&s, None, &need(Class::Light, None), NOW);
        assert!(v.admit && !v.reported);
        let strict = ContractSettings {
            require_report: true,
            ..all_required()
        };
        let v = evaluate(&strict, None, &need(Class::Light, None), NOW);
        assert!(!v.admit);
        assert!(v.why.unwrap().contains("not reported"));
    }

    #[test]
    fn a_disabled_contract_admits_everything_and_blocked_classes_are_per_class() {
        let off = ContractSettings::default();
        assert!(
            evaluate(
                &off,
                Some(&EnvFacts::default()),
                &need(Class::Heavy, None),
                NOW
            )
            .admit
        );
        assert!(blocked_classes(&off, None, &Classes::default(), None, NOW).is_empty());
        // 10 GiB available: light and medium fit (2.5 and 12 GiB: medium does not).
        let s = all_required();
        let f = EnvFacts {
            mem_available_mib: Some(10_000),
            ..good()
        };
        let blocked = blocked_classes(&s, Some(&f), &Classes::default(), None, NOW);
        assert_eq!(blocked, [Class::Medium, Class::Heavy]);
        // A unit's toolchain requirement blocks every class on a drone without it.
        let blocked = blocked_classes(&s, Some(&good()), &Classes::default(), Some("9.9.9"), NOW);
        assert_eq!(blocked, Class::ALL);
    }

    #[test]
    fn settings_validate_and_evaluation_is_deterministic() {
        assert!(ContractSettings::default().validate().is_ok());
        for bad in [
            ContractSettings {
                facts_max_age_secs: 0,
                ..ContractSettings::default()
            },
            ContractSettings {
                allowed_wrappers: vec![],
                ..ContractSettings::default()
            },
            ContractSettings {
                max_cpu_psi_pm: 2000,
                ..ContractSettings::default()
            },
        ] {
            assert!(bad.validate().is_err());
        }
        let mut s = on();
        s.classes.insert(
            Class::Light,
            ClassContract {
                rust_toolchain: Some("bad toolchain!".into()),
                ..ClassContract::default()
            },
        );
        assert!(s.validate().is_err());
        let n = need(Class::Medium, Some("1.95.0"));
        assert_eq!(
            evaluate(&all_required(), Some(&good()), &n, NOW),
            evaluate(&all_required(), Some(&good()), &n, NOW)
        );
    }

    #[test]
    fn a_missing_required_tool_is_not_admitted_and_named() {
        let s = tools_on(Mode::Required, &["git", "gh", "node"]);
        let f = EnvFacts {
            tools: Some(tool_map(&[
                ("git", "git version 2.45.0"),
                ("gh", "missing"),
                ("node", "missing"),
                ("jq", "missing"),
            ])),
            ..good()
        };
        let v = evaluate(&s, Some(&f), &need(Class::Light, None), NOW);
        assert!(!v.admit);
        let c = check(&v, CheckKind::Tools);
        assert_eq!(c.state, CheckState::Fail);
        assert!(c.reason.contains("gh, node"), "{}", c.reason);
        assert!(!c.reason.contains("git"), "{}", c.reason);
        assert_eq!(c.observed["missing"], "gh,jq,node");
        assert!(c.observed["versions"].contains("git=git version 2.45.0"));
        // A tool that is not in the map at all counts as missing.
        let f = EnvFacts {
            tools: Some(tool_map(&[("git", "2")])),
            ..good()
        };
        let v = evaluate(&s, Some(&f), &need(Class::Light, None), NOW);
        assert!(check(&v, CheckKind::Tools).reason.contains("gh, node"));
        // Everything present: admitted.
        let f = EnvFacts {
            tools: Some(tool_map(&[("git", "2"), ("gh", "2"), ("node", "22")])),
            ..good()
        };
        assert!(evaluate(&s, Some(&f), &need(Class::Light, None), NOW).admit);
    }

    #[test]
    fn advisory_tools_warn_in_the_verdict_but_admit() {
        let s = tools_on(Mode::Advisory, &["gh"]);
        let f = EnvFacts {
            tools: Some(tool_map(&[("gh", "missing")])),
            ..good()
        };
        let v = evaluate(&s, Some(&f), &need(Class::Light, None), NOW);
        assert!(v.admit, "{v:?}");
        let c = check(&v, CheckKind::Tools);
        assert_eq!((c.state, c.mode), (CheckState::Fail, Mode::Advisory));
        assert!(c.reason.contains("gh"));
    }

    #[test]
    fn the_default_requires_nothing_and_a_class_list_replaces_the_global_one() {
        // Enabled with defaults: an empty list passes even with tools missing.
        let f = EnvFacts {
            tools: Some(tool_map(&[("gh", "missing")])),
            ..good()
        };
        let v = evaluate(&on(), Some(&f), &need(Class::Light, None), NOW);
        assert_eq!(check(&v, CheckKind::Tools).mode, Mode::Advisory);
        assert_eq!(check(&v, CheckKind::Tools).state, CheckState::Pass);

        let mut s = tools_on(Mode::Required, &["gh"]);
        s.classes.insert(
            Class::Heavy,
            ClassContract {
                required_tools: Some(vec!["docker".into()]),
                ..ClassContract::default()
            },
        );
        s.classes.insert(
            Class::Medium,
            ClassContract {
                required_tools: Some(Vec::new()),
                ..ClassContract::default()
            },
        );
        let f = EnvFacts {
            tools: Some(tool_map(&[("gh", "missing"), ("docker", "27.0")])),
            ..good()
        };
        let admit = |c| evaluate(&s, Some(&f), &need(c, None), NOW).admit;
        assert!(!admit(Class::Light)); // global gh
        assert!(admit(Class::Heavy)); // docker only
        assert!(admit(Class::Medium)); // explicitly nothing
        let off = ContractSettings {
            classes: BTreeMap::from([(
                Class::Light,
                ClassContract {
                    checks: BTreeMap::from([(CheckKind::Tools, Mode::Off)]),
                    ..ClassContract::default()
                },
            )]),
            ..s.clone()
        };
        let v = evaluate(&off, Some(&f), &need(Class::Light, None), NOW);
        assert!(v.admit && v.checks.iter().all(|c| c.kind != CheckKind::Tools));
    }

    #[test]
    fn an_absent_tools_map_fails_only_with_require_report() {
        let f = EnvFacts {
            tools: None,
            ..good()
        };
        let s = tools_on(Mode::Required, &["gh"]);
        let v = evaluate(&s, Some(&f), &need(Class::Light, None), NOW);
        assert!(v.admit, "{v:?}");
        let c = check(&v, CheckKind::Tools);
        assert_eq!(c.state, CheckState::Pass);
        assert_eq!(c.warnings.len(), 1);

        let strict = ContractSettings {
            require_report: true,
            ..s.clone()
        };
        let v = evaluate(&strict, Some(&f), &need(Class::Light, None), NOW);
        assert!(!v.admit);
        assert!(
            check(&v, CheckKind::Tools)
                .reason
                .contains("did not report its tools")
        );
        // Nothing required: an absent map is fine even with require_report.
        let none = ContractSettings {
            required_tools: Vec::new(),
            ..strict
        };
        assert!(evaluate(&none, Some(&f), &need(Class::Light, None), NOW).admit);
    }

    #[test]
    fn required_tools_must_be_tools_the_drone_probes() {
        assert!(tools_on(Mode::Required, &["git", "jq"]).validate().is_ok());
        assert!(tools_on(Mode::Required, &["ffmpeg"]).validate().is_err());
        let mut s = on();
        s.classes.insert(
            Class::Light,
            ClassContract {
                required_tools: Some(vec!["nope".into()]),
                ..ClassContract::default()
            },
        );
        assert!(s.validate().is_err());
    }

    fn capability_settings(mode: Mode) -> ContractSettings {
        ContractSettings {
            checks: BTreeMap::from([(CheckKind::Capabilities, mode)]),
            required_capabilities: vec![Capability::Sandbox],
            ..on()
        }
    }

    fn capability_facts(state: CheckState, observed_at: i64) -> EnvFacts {
        EnvFacts {
            capabilities: BTreeMap::from([(
                Capability::Sandbox,
                CapabilityFact { observed_at, state },
            )]),
            ..good()
        }
    }

    #[test]
    fn required_job_user_capabilities_fail_closed_including_old_drones() {
        let settings = capability_settings(Mode::Required);
        let need = need(Class::Light, None);
        assert!(
            evaluate(
                &settings,
                Some(&capability_facts(CheckState::Pass, NOW)),
                &need,
                NOW
            )
            .admit
        );
        for state in [CheckState::Fail, CheckState::Unknown] {
            let v = evaluate(&settings, Some(&capability_facts(state, NOW)), &need, NOW);
            assert!(!v.admit);
            assert_eq!(check(&v, CheckKind::Capabilities).state, state);
            assert!(v.why.unwrap().contains("sandbox"));
        }
        assert!(!evaluate(&settings, Some(&good()), &need, NOW).admit);
        // require_report=false cannot skip an explicitly required named proof.
        assert!(!evaluate(&settings, None, &need, NOW).admit);
        let old: EnvFacts = serde_json::from_str(r#"{"observed_at":10000}"#).unwrap();
        assert!(old.capabilities.is_empty());
        assert!(!evaluate(&settings, Some(&old), &need, NOW).admit);
    }

    #[test]
    fn a_fresh_heartbeat_cannot_refresh_an_old_or_future_capability_proof() {
        let settings = capability_settings(Mode::Required);
        for at in [NOW - settings.facts_max_age_secs - 1, NOW + 1] {
            let v = evaluate(
                &settings,
                Some(&capability_facts(CheckState::Pass, at)),
                &need(Class::Light, None),
                NOW,
            );
            assert_eq!(v.age_secs, Some(10));
            assert!(!v.admit);
            assert_eq!(
                check(&v, CheckKind::Capabilities).state,
                CheckState::Unknown
            );
        }
    }

    #[test]
    fn capability_requirements_and_advisory_modes_are_class_specific() {
        let mut settings = capability_settings(Mode::Required);
        settings.required_capabilities.clear();
        settings.classes.insert(
            Class::Heavy,
            ClassContract {
                required_capabilities: Some(vec![Capability::Sandbox]),
                ..ClassContract::default()
            },
        );
        let facts = capability_facts(CheckState::Fail, NOW);
        assert!(evaluate(&settings, Some(&facts), &need(Class::Light, None), NOW).admit);
        assert!(!evaluate(&settings, Some(&facts), &need(Class::Heavy, None), NOW).admit);
        let v = evaluate(&settings, Some(&facts), &need(Class::Light, None), NOW);
        assert!(check(&v, CheckKind::Capabilities).warnings[0].contains("sandbox"));
        settings
            .classes
            .get_mut(&Class::Heavy)
            .unwrap()
            .checks
            .insert(CheckKind::Capabilities, Mode::Advisory);
        let v = evaluate(&settings, Some(&facts), &need(Class::Heavy, None), NOW);
        assert!(v.admit);
        assert_eq!(check(&v, CheckKind::Capabilities).state, CheckState::Fail);
        assert!(check(&v, CheckKind::Capabilities).warnings[0].contains("sandbox"));
        assert!(evaluate(&settings, None, &need(Class::Light, None), NOW).admit);
    }

    #[test]
    fn capability_names_are_closed_and_the_class_list_replaces_global_requirements() {
        let json = r#"{"enabled":true,"required_capabilities":["sandbox"],"classes":{"heavy":{"required_capabilities":["docker"]},"medium":{"required_capabilities":[]}}}"#;
        let settings: ContractSettings = serde_json::from_str(json).unwrap();
        assert!(settings.validate().is_ok());
        assert_eq!(
            settings.class_required_capabilities(Class::Light),
            [Capability::Sandbox]
        );
        assert_eq!(
            settings.class_required_capabilities(Class::Heavy),
            [Capability::Docker]
        );
        assert!(
            settings
                .class_required_capabilities(Class::Medium)
                .is_empty()
        );
        assert!(
            serde_json::from_str::<ContractSettings>(&json.replace("sandbox", "arbitrary_command"))
                .is_err()
        );
        // No requirements is backward compatible, even with the new check required.
        assert!(evaluate(&on(), Some(&good()), &need(Class::Light, None), NOW).admit);
        let empty = ContractSettings {
            checks: BTreeMap::from([(CheckKind::Capabilities, Mode::Required)]),
            required_capabilities: Vec::new(),
            require_report: true,
            ..on()
        };
        assert!(evaluate(&empty, None, &need(Class::Light, None), NOW).admit);
        let stale = EnvFacts {
            observed_at: NOW - 3600,
            ..good()
        };
        assert!(evaluate(&empty, Some(&stale), &need(Class::Light, None), NOW).admit);
    }

    #[test]
    fn capability_diagnostics_use_admissions_effective_proof_states() {
        let settings = capability_settings(Mode::Required);
        for (state, at, expected) in [
            (CheckState::Pass, NOW, CheckState::Pass),
            (CheckState::Fail, NOW, CheckState::Fail),
            (CheckState::Unknown, NOW, CheckState::Unknown),
            (CheckState::Pass, NOW - 3600, CheckState::Unknown),
            (CheckState::Fail, NOW + 1, CheckState::Unknown),
        ] {
            let facts = capability_facts(state, at);
            let states = capability_states(&settings, Some(&facts), Class::Light, NOW);
            let verdict = evaluate(&settings, Some(&facts), &need(Class::Light, None), NOW);
            assert_eq!(states[&Capability::Sandbox], expected);
            assert_eq!(
                states[&Capability::Sandbox],
                check(&verdict, CheckKind::Capabilities).state
            );
        }
        assert_eq!(
            capability_states(&settings, None, Class::Light, NOW)[&Capability::Sandbox],
            CheckState::Unknown
        );
        assert_eq!(
            capability_states(&settings, Some(&good()), Class::Light, NOW)[&Capability::Sandbox],
            CheckState::Unknown
        );
        let mut settings = settings;
        settings.classes.insert(
            Class::Heavy,
            ClassContract {
                required_capabilities: Some(Vec::new()),
                ..ClassContract::default()
            },
        );
        assert!(capability_states(&settings, None, Class::Heavy, NOW).is_empty());
    }
}
