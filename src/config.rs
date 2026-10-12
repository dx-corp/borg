//! Queen configuration, read from a TOML or JSON file (see
//! [`crate::configfile`]). Nothing org-specific is baked in: the organization,
//! repositories, label prefix and class sizes all come from here.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::allocator::{AdmissionThresholds, WarmFloor, default_warm_floors};
use crate::classes::{Class, Classes, ClassesError};
use crate::envcontract::ContractSettings;
use crate::health::HealthSettings;
use crate::homeostasis::HomeostasisSettings;
use crate::preempt::PreemptSettings;
use crate::profile::ProfileSettings;
use crate::rollout::RolloutSettings;
use crate::schedule::WarmSchedule;
use crate::tailnet::TailnetEnrollSettings;
use crate::units::UnitSettings;
use crate::wake::WakeSettings;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("parse config: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("parse config {0}")]
    File(#[from] crate::configfile::ParseError),
    #[error("invalid classes: {0}")]
    Classes(#[from] ClassesError),
    #[error("{0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubSettings {
    /// Organization that owns the self-hosted runners.
    pub org: String,
    /// GitHub App id (or client id).
    pub app_id: String,
    /// PEM private key file. If absent, `BORG_APP_PRIVATE_KEY` must hold the
    /// PEM text.
    #[serde(default)]
    pub private_key_path: Option<PathBuf>,
    #[serde(default = "default_api_base")]
    pub api_base: String,
    #[serde(default = "default_runner_group")]
    pub runner_group_id: u64,
}

fn default_api_base() -> String {
    "https://api.github.com".to_owned()
}
fn default_runner_group() -> u64 {
    1
}

/// The opt-in supersede janitor (see [`crate::janitor`]). Present means
/// enabled; it starts in dry-run so enabling it cannot cancel anything until
/// the operator has read its log and set `dry_run` to false.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JanitorSettings {
    /// File holding a token with `actions: write` on the listed repositories.
    /// If absent, `BORG_JANITOR_TOKEN` must hold the token.
    #[serde(default)]
    pub token_path: Option<PathBuf>,
    /// Log what would be cancelled without cancelling.
    #[serde(default = "yes")]
    pub dry_run: bool,
    #[serde(default = "default_janitor_interval")]
    pub interval_secs: u64,
    /// Hard cap on cancellations (or dry-run matches) per pass.
    #[serde(default = "default_janitor_max")]
    pub max_cancels_per_pass: u32,
    /// Also cancel superseded runs that are already `in_progress`.
    #[serde(default)]
    pub cancel_running: bool,
    /// Cancel queued runs that duplicate another run of the same workflow,
    /// commit, event and pull request.
    #[serde(default)]
    pub dedupe_same_sha: bool,
}

fn yes() -> bool {
    true
}
fn default_janitor_interval() -> u64 {
    60
}
fn default_janitor_max() -> u32 {
    20
}

/// Standby deploy runners; present means enabled. See `docs/operations.md`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StandbySettings {
    /// Classes each deploy or deploy-only drone keeps standby runners for.
    pub classes: Vec<Class>,
    /// Standby runners per drone and class, 1 to 4.
    pub per_drone: u32,
}

impl StandbySettings {
    fn validate(&self) -> Result<(), String> {
        if self.classes.is_empty() {
            return Err("standby.classes must not be empty".to_owned());
        }
        if !(1..=4).contains(&self.per_drone) {
            return Err("standby.per_drone must be between 1 and 4".to_owned());
        }
        Ok(())
    }
}

/// Admission by CPU pressure; present means enabled. See
/// [`crate::allocator::AdmissionThresholds`].
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionSettings {
    /// From this `cpu some avg10` (permille) a drone takes no new
    /// pull-request grants.
    pub psi_soft_pm: u16,
    /// From this pressure a drone takes no new grants at all.
    pub psi_hard_pm: u16,
    /// A drone's reported pressure older than this counts as unknown.
    pub facts_max_age_secs: i64,
}

impl AdmissionSettings {
    pub fn thresholds(&self) -> AdmissionThresholds {
        AdmissionThresholds {
            psi_soft_pm: self.psi_soft_pm,
            psi_hard_pm: self.psi_hard_pm,
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.psi_soft_pm == 0 || self.psi_soft_pm > self.psi_hard_pm || self.psi_hard_pm > 1000 {
            return Err("admission needs 0 < psi_soft_pm <= psi_hard_pm <= 1000".to_owned());
        }
        if self.facts_max_age_secs <= 0 {
            return Err("admission.facts_max_age_secs must be positive".to_owned());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueenConfig {
    pub github: GithubSettings,
    /// Repositories (`owner/name`) whose queued jobs are served.
    pub repos: Vec<String>,
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    pub db_path: PathBuf,
    #[serde(default = "default_runner_prefix")]
    pub runner_prefix: String,
    #[serde(default = "default_work_folder")]
    pub work_folder: String,
    #[serde(default = "ten")]
    pub poll_interval_secs: u64,
    /// A lease whose runner has not come online within this long is expired.
    #[serde(default = "default_lease_ttl")]
    pub lease_ttl_secs: i64,
    /// Hard ceiling on a lease's lifetime.
    #[serde(default = "default_lease_max")]
    pub lease_max_secs: i64,
    /// A drone silent for longer than this gets no new leases.
    #[serde(default = "default_drone_stale")]
    pub drone_stale_secs: i64,
    /// Demand older than this triggers the starvation guard. `false` (or JSON
    /// `null`) disables it; TOML has no null.
    #[serde(default = "default_starvation", deserialize_with = "secs_or_off")]
    pub starvation_after_secs: Option<u64>,
    #[serde(default)]
    pub classes: Classes,
    #[serde(default = "default_warm_floors")]
    pub warm_floors: Vec<WarmFloor>,
    /// Time-of-day raises to the warm floors; see [`crate::schedule`].
    #[serde(default)]
    pub warm_schedule: WarmSchedule,
    /// Absent (the default) disables `POST /v1/enroll`; see [`crate::tailnet`].
    #[serde(default)]
    pub tailnet_enroll: Option<TailnetEnrollSettings>,
    /// Demand-responsive warm floors; off by default. See
    /// [`crate::homeostasis`].
    #[serde(default)]
    pub homeostasis: HomeostasisSettings,
    /// The build environment contract checked before work is placed on a drone;
    /// off by default. See [`crate::envcontract`].
    #[serde(default)]
    pub env_contract: ContractSettings,
    /// Work units (gate and hook runs) and their API; off by default. See
    /// [`crate::units`].
    #[serde(default)]
    pub units: UnitSettings,
    /// Wake-on-LAN for sleeping hosts when demand goes unmet; off by default.
    /// See [`crate::wake`].
    #[serde(default)]
    pub wake: WakeSettings,
    /// Gating of the rollout of `rollout_target_version`; see
    /// [`crate::rollout`]. Gated only when `previous_version` is set.
    #[serde(default)]
    pub rollout: RolloutSettings,
    /// Learned per-drone profiles that feed placement; see [`crate::profile`].
    #[serde(default)]
    pub profiles: ProfileSettings,
    /// Host-health thresholds, decay and cooldown; see [`crate::health`].
    #[serde(default)]
    pub health: HealthSettings,
    /// Job telemetry older than this is deleted.
    #[serde(default = "default_telemetry_days")]
    pub telemetry_retention_days: u32,
    /// The borg version drones should run. Only reported (`/metrics`,
    /// `/status`): drones update themselves, the queen never forces it.
    #[serde(default)]
    pub rollout_target_version: Option<String>,
    /// Absent (the default) disables standby deploy runners.
    #[serde(default)]
    pub standby: Option<StandbySettings>,
    /// Absent (the default) disables admission by CPU pressure.
    #[serde(default)]
    pub admission: Option<AdmissionSettings>,
    /// Absent (the default) disables the supersede janitor.
    #[serde(default)]
    pub supersede_janitor: Option<JanitorSettings>,
    /// Absent (the default) disables preemption; see [`crate::preempt`].
    #[serde(default)]
    pub preemption: Option<PreemptSettings>,
    /// Absent (the default) disables git mirror credentials; see
    /// [`crate::mirror_auth`].
    #[serde(default)]
    pub mirror_auth: Option<MirrorAuthSettings>,
}

/// A second GitHub App, held by the queen only, whose short-lived read-only
/// installation tokens let drones refresh private git mirrors.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MirrorAuthSettings {
    /// The mirror App's id (or client id).
    pub app_id: String,
    /// PEM private key file. If absent, `BORG_MIRROR_APP_PRIVATE_KEY` must
    /// hold the PEM text.
    #[serde(default)]
    pub private_key_path: Option<PathBuf>,
    /// Bare repository names in `github.org` the token may read.
    pub repositories: Vec<String>,
}

impl MirrorAuthSettings {
    fn validate(&self) -> Result<(), ConfigError> {
        let bad = |msg: &str| Err(ConfigError::Invalid(msg.to_owned()));
        if self.app_id.is_empty() {
            return bad("mirror_auth.app_id is required");
        }
        if self.repositories.is_empty() {
            return bad("mirror_auth.repositories must name at least one repository");
        }
        if self.repositories.iter().any(|r| {
            r.is_empty()
                || r.contains('/')
                || r.chars().any(|c| c.is_whitespace() || c.is_control())
        }) {
            return bad("every mirror_auth.repositories entry must be a bare repository name");
        }
        Ok(())
    }

    /// The App private key: `private_key_path`, else `env_pem` (the value of
    /// `BORG_MIRROR_APP_PRIVATE_KEY`).
    pub fn private_key(&self, env_pem: Option<String>) -> Result<Vec<u8>, ConfigError> {
        match (&self.private_key_path, env_pem) {
            (Some(path), _) => std::fs::read(path).map_err(|e| {
                ConfigError::Invalid(format!(
                    "read mirror_auth.private_key_path {}: {e}",
                    path.display()
                ))
            }),
            (None, Some(pem)) if !pem.trim().is_empty() => Ok(pem.into_bytes()),
            (None, _) => Err(ConfigError::Invalid(
                "set mirror_auth.private_key_path or BORG_MIRROR_APP_PRIVATE_KEY".to_owned(),
            )),
        }
    }
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 8080))
}
fn default_runner_prefix() -> String {
    "borg".to_owned()
}
fn default_work_folder() -> String {
    "_work".to_owned()
}
fn ten() -> u64 {
    10
}
fn default_lease_ttl() -> i64 {
    300
}
fn default_lease_max() -> i64 {
    6 * 3600
}
fn default_drone_stale() -> i64 {
    60
}
#[allow(
    clippy::unnecessary_wraps,
    reason = "serde default for an Option field"
)]
fn default_telemetry_days() -> u32 {
    30
}
fn default_starvation() -> Option<u64> {
    Some(120)
}

/// Seconds, or `false`/`null` for off.
fn secs_or_off<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum SecsOrOff {
        Secs(u64),
        Off(bool),
    }
    match Option::<SecsOrOff>::deserialize(d)? {
        None | Some(SecsOrOff::Off(false)) => Ok(None),
        Some(SecsOrOff::Secs(s)) => Ok(Some(s)),
        Some(SecsOrOff::Off(true)) => Err(serde::de::Error::custom(
            "expected seconds, or false to disable",
        )),
    }
}

impl QueenConfig {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        let cfg: Self = crate::configfile::parse(path, &text)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        let cfg: Self = crate::configfile::parse(Path::new("config.toml"), text)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn from_json(json: &str) -> Result<Self, ConfigError> {
        let cfg: Self = serde_json::from_str(json)?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        self.classes.validate()?;
        let bad = |msg: &str| Err(ConfigError::Invalid(msg.to_owned()));
        if let Some(m) = &self.mirror_auth {
            m.validate()?;
        }
        if self.github.org.is_empty() || self.github.app_id.is_empty() {
            return bad("github.org and github.app_id are required");
        }
        if self.repos.is_empty() {
            return bad("repos must name at least one owner/name repository");
        }
        if self
            .repos
            .iter()
            .any(|r| r.split('/').filter(|p| !p.is_empty()).count() != 2)
        {
            return bad("every repos entry must look like owner/name");
        }
        let prefix_ok = !self.runner_prefix.is_empty()
            && self
                .runner_prefix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-');
        if !prefix_ok {
            return bad("runner_prefix must be non-empty alphanumerics and dashes");
        }
        if self.poll_interval_secs == 0 || self.lease_ttl_secs <= 0 || self.lease_max_secs <= 0 {
            return bad("poll_interval_secs, lease_ttl_secs and lease_max_secs must be positive");
        }
        if self.telemetry_retention_days == 0 {
            return bad("telemetry_retention_days must be at least 1");
        }
        self.env_contract.validate().map_err(ConfigError::Invalid)?;
        self.units.validate().map_err(ConfigError::Invalid)?;
        self.wake.validate().map_err(ConfigError::Invalid)?;
        self.rollout.validate().map_err(ConfigError::Invalid)?;
        self.profiles.validate().map_err(ConfigError::Invalid)?;
        self.homeostasis.validate().map_err(ConfigError::Invalid)?;
        self.health.validate().map_err(ConfigError::Invalid)?;
        if let Some(a) = &self.admission {
            a.validate().map_err(ConfigError::Invalid)?;
        }
        if let Some(st) = &self.standby {
            st.validate().map_err(ConfigError::Invalid)?;
        }
        if let Some(t) = &self.tailnet_enroll {
            t.validate().map_err(ConfigError::Invalid)?;
        }
        self.warm_schedule
            .validate()
            .map_err(ConfigError::Invalid)?;
        if let Some(j) = &self.supersede_janitor
            && (j.interval_secs == 0 || j.max_cancels_per_pass == 0)
        {
            return bad(
                "supersede_janitor interval_secs and max_cancels_per_pass must be positive",
            );
        }
        if let Some(p) = &self.preemption {
            p.validate().map_err(ConfigError::Invalid)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: &str = r#"{"github":{"org":"example-org","app_id":"1"},"repos":["example-org/app"],"db_path":"/tmp/q.db"}"#;

    #[test]
    fn minimal_config_gets_defaults() {
        let c = QueenConfig::from_json(MIN).unwrap();
        assert_eq!(c.poll_interval_secs, 10);
        assert_eq!(c.starvation_after_secs, Some(120));
        assert_eq!(c.github.api_base, "https://api.github.com");
        assert_eq!(c.warm_floors, default_warm_floors());
        assert_eq!(c.listen.ip().to_string(), "127.0.0.1");
    }

    fn with_mirror(block: &str) -> String {
        MIN.replace("\"repos\"", &format!("\"mirror_auth\":{block},\"repos\""))
    }

    #[test]
    fn mirror_auth_is_off_unless_present_and_validated() {
        assert!(QueenConfig::from_json(MIN).unwrap().mirror_auth.is_none());
        let ok = with_mirror(
            r#"{"app_id":"12345","private_key_path":"/etc/borg/mirror-app.pem","repositories":["app"]}"#,
        );
        let c = QueenConfig::from_json(&ok).unwrap();
        assert_eq!(c.mirror_auth.unwrap().repositories, ["app"]);
        for bad in [
            r#"{"app_id":"12345","repositories":[]}"#,
            r#"{"app_id":"12345","repositories":["example-org/app"]}"#,
            r#"{"app_id":"12345","repositories":[""]}"#,
            r#"{"app_id":"12345","repositories":["a b"]}"#,
            r#"{"app_id":"","repositories":["app"]}"#,
            r#"{"app_id":"12345","repositories":["app"],"nope":1}"#,
            r#"{"repositories":["app"]}"#,
        ] {
            assert!(QueenConfig::from_json(&with_mirror(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn mirror_key_comes_from_the_file_or_the_environment_value() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("mirror-app.pem");
        std::fs::write(&file, "PEM-FROM-FILE").unwrap();
        let settings = |path: Option<PathBuf>| MirrorAuthSettings {
            app_id: "12345".to_owned(),
            private_key_path: path,
            repositories: vec!["app".to_owned()],
        };
        let from_file = settings(Some(file)).private_key(Some("ignored".to_owned()));
        assert_eq!(from_file.unwrap(), b"PEM-FROM-FILE");
        let from_env = settings(None).private_key(Some("PEM-FROM-ENV".to_owned()));
        assert_eq!(from_env.unwrap(), b"PEM-FROM-ENV");
        assert!(settings(None).private_key(None).is_err());
        assert!(settings(None).private_key(Some("  \n".to_owned())).is_err());
        assert!(
            settings(Some(dir.path().join("missing.pem")))
                .private_key(None)
                .is_err()
        );
    }

    #[test]
    fn rejects_bad_repos_unknown_fields_and_empty_org() {
        for bad in [
            MIN.replace("example-org/app", "app"),
            MIN.replace("\"repos\"", "\"nope\":1,\"repos\""),
            MIN.replace("\"org\":\"example-org\"", "\"org\":\"\""),
            MIN.replace("\"repos\":[\"example-org/app\"]", "\"repos\":[]"),
        ] {
            assert!(QueenConfig::from_json(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn janitor_is_off_by_default_and_dry_run_when_enabled() {
        assert!(
            QueenConfig::from_json(MIN)
                .unwrap()
                .supersede_janitor
                .is_none()
        );
        let c =
            QueenConfig::from_json(&MIN.replace("\"repos\"", "\"supersede_janitor\":{},\"repos\""))
                .unwrap();
        let j = c.supersede_janitor.unwrap();
        assert!(j.dry_run);
        assert!(!j.cancel_running && !j.dedupe_same_sha);
        assert_eq!((j.interval_secs, j.max_cancels_per_pass), (60, 20));
        for bad in [
            "{\"interval_secs\":0}",
            "{\"max_cancels_per_pass\":0}",
            "{\"nope\":1}",
        ] {
            let json = MIN.replace(
                "\"repos\"",
                &format!("\"supersede_janitor\":{bad},\"repos\""),
            );
            assert!(QueenConfig::from_json(&json).is_err(), "{bad}");
        }
    }

    #[test]
    fn health_settings_default_and_validate() {
        let c = QueenConfig::from_json(MIN).unwrap();
        assert!(c.health.auto_quarantine);
        let with = |h: &str| {
            QueenConfig::from_json(&MIN.replace("\"repos\"", &format!("\"health\":{h},\"repos\"")))
        };
        assert!(with(r#"{"half_life_secs":0}"#).is_err());
        assert!(with(r#"{"nope":1}"#).is_err());
        assert!(
            !with(r#"{"auto_quarantine":false}"#)
                .unwrap()
                .health
                .auto_quarantine
        );
    }

    #[test]
    fn standby_is_off_by_default_and_validated() {
        assert!(QueenConfig::from_json(MIN).unwrap().standby.is_none());
        let with = |a: &str| {
            QueenConfig::from_json(&MIN.replace("\"repos\"", &format!("\"standby\":{a},\"repos\"")))
        };
        let ok = with(r#"{"classes":["medium"],"per_drone":1}"#).unwrap();
        let st = ok.standby.unwrap();
        assert_eq!((st.classes, st.per_drone), (vec![Class::Medium], 1));
        for bad in [
            r#"{"classes":[],"per_drone":1}"#,
            r#"{"classes":["medium"],"per_drone":0}"#,
            r#"{"classes":["medium"],"per_drone":5}"#,
            r#"{"classes":["huge"],"per_drone":1}"#,
            r#"{"classes":["medium"],"per_drone":1,"nope":1}"#,
            r#"{"per_drone":1}"#,
        ] {
            assert!(with(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn admission_is_off_by_default_and_validated() {
        assert!(QueenConfig::from_json(MIN).unwrap().admission.is_none());
        let with = |a: &str| {
            QueenConfig::from_json(
                &MIN.replace("\"repos\"", &format!("\"admission\":{a},\"repos\"")),
            )
        };
        let ok = with(r#"{"psi_soft_pm":300,"psi_hard_pm":600,"facts_max_age_secs":180}"#).unwrap();
        let t = ok.admission.unwrap().thresholds();
        assert_eq!((t.psi_soft_pm, t.psi_hard_pm), (300, 600));
        for bad in [
            r#"{"psi_soft_pm":700,"psi_hard_pm":600,"facts_max_age_secs":180}"#,
            r#"{"psi_soft_pm":0,"psi_hard_pm":600,"facts_max_age_secs":180}"#,
            r#"{"psi_soft_pm":300,"psi_hard_pm":1001,"facts_max_age_secs":180}"#,
            r#"{"psi_soft_pm":300,"psi_hard_pm":600,"facts_max_age_secs":0}"#,
            r#"{"psi_soft_pm":300,"psi_hard_pm":600,"facts_max_age_secs":180,"nope":1}"#,
            r#"{"psi_soft_pm":300,"psi_hard_pm":600}"#,
        ] {
            assert!(with(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn zero_telemetry_retention_is_rejected() {
        let json = MIN.replace("\"repos\"", "\"telemetry_retention_days\":0,\"repos\"");
        assert!(QueenConfig::from_json(&json).is_err());
    }

    #[test]
    fn tailnet_enroll_is_off_by_default_and_needs_an_allowlist() {
        assert!(
            QueenConfig::from_json(MIN)
                .unwrap()
                .tailnet_enroll
                .is_none()
        );
        let with = |t: &str| {
            QueenConfig::from_json(
                &MIN.replace("\"repos\"", &format!("\"tailnet_enroll\":{t},\"repos\"")),
            )
        };
        assert!(with("{}").is_err());
        assert!(with(r#"{"allowed_tags":["ci"]}"#).is_err());
        let ok = with(r#"{"allowed_tags":["tag:ci"]}"#).unwrap();
        assert_eq!(
            ok.tailnet_enroll.unwrap().socket_path.to_str(),
            Some("/var/run/tailscale/tailscaled.sock")
        );
    }

    #[test]
    fn preemption_is_off_by_default_and_validated() {
        assert!(QueenConfig::from_json(MIN).unwrap().preemption.is_none());
        let with = |p: &str| {
            QueenConfig::from_json(
                &MIN.replace("\"repos\"", &format!("\"preemption\":{p},\"repos\"")),
            )
        };
        let ok = with(r#"{"after_secs":600,"max_per_pass":1,"min_victim_age_secs":60}"#).unwrap();
        assert_eq!(ok.preemption.unwrap().after_secs, 600);
        for bad in [
            r#"{"after_secs":0,"max_per_pass":1,"min_victim_age_secs":60}"#,
            r#"{"after_secs":600,"max_per_pass":0,"min_victim_age_secs":60}"#,
            r#"{"after_secs":600,"max_per_pass":1,"min_victim_age_secs":0}"#,
            r#"{"after_secs":600,"max_per_pass":1}"#,
            r#"{"after_secs":600,"max_per_pass":1,"min_victim_age_secs":60,"nope":1}"#,
        ] {
            assert!(with(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn starvation_can_be_disabled() {
        let c = QueenConfig::from_json(
            &MIN.replace("\"repos\"", "\"starvation_after_secs\":null,\"repos\""),
        )
        .unwrap();
        assert_eq!(c.starvation_after_secs, None);
        let c = QueenConfig::from_json(
            &MIN.replace("\"repos\"", "\"starvation_after_secs\":false,\"repos\""),
        )
        .unwrap();
        assert_eq!(c.starvation_after_secs, None);
        assert!(
            QueenConfig::from_json(
                &MIN.replace("\"repos\"", "\"starvation_after_secs\":true,\"repos\"")
            )
            .is_err()
        );
    }

    const FULL_JSON: &str = r#"{
      "github": {"org": "example-org", "app_id": "7", "private_key_path": "/etc/borg/app.pem", "runner_group_id": 3},
      "repos": ["example-org/app", "example-org/lib"],
      "db_path": "/var/lib/borg/queen.db",
      "listen": "192.0.2.5:8080",
      "classes": {"label_prefix": "example-ci-", "heavy": {"mem_mib": 20480, "vcpu": 6, "docker": true}},
      "warm_floors": [{"class": "medium", "scope": "main", "count": 2}],
      "starvation_after_secs": null,
      "health": {"auto_quarantine": false},
      "units": {"enabled": true},
      "admission": {"psi_soft_pm": 300, "psi_hard_pm": 600, "facts_max_age_secs": 180},
      "supersede_janitor": {"dry_run": false, "cancel_running": true, "dedupe_same_sha": true}
    }"#;

    const FULL_TOML: &str = r#"
repos = ["example-org/app", "example-org/lib"]
db_path = "/var/lib/borg/queen.db"
listen = "192.0.2.5:8080"
starvation_after_secs = false

[github]
org = "example-org"
app_id = "7"
private_key_path = "/etc/borg/app.pem"
runner_group_id = 3

[classes]
label_prefix = "example-ci-"
heavy = { mem_mib = 20480, vcpu = 6, docker = true }

[[warm_floors]]
class = "medium"
scope = "main"
count = 2

[health]
auto_quarantine = false

[units]
enabled = true

[admission]
psi_soft_pm = 300
psi_hard_pm = 600
facts_max_age_secs = 180

[supersede_janitor]
dry_run = false
cancel_running = true
dedupe_same_sha = true
"#;

    #[test]
    fn toml_and_json_give_the_same_config() {
        let j = QueenConfig::from_json(FULL_JSON).unwrap();
        let t = QueenConfig::from_toml(FULL_TOML).unwrap();
        assert_eq!(format!("{j:?}"), format!("{t:?}"));
        assert_eq!(t.repos.len(), 2);
        assert_eq!(t.starvation_after_secs, None);
        assert!(t.units.enabled);
        assert_eq!(t.admission.unwrap().psi_hard_pm, 600);
        let j = t.supersede_janitor.unwrap();
        assert!(!j.dry_run && j.cancel_running && j.dedupe_same_sha);
    }

    #[test]
    fn load_picks_the_format_and_validates() {
        let dir = tempfile::tempdir().unwrap();
        let toml_path = dir.path().join("queen.toml");
        std::fs::write(&toml_path, FULL_TOML).unwrap();
        let json_path = dir.path().join("queen.json");
        std::fs::write(&json_path, FULL_JSON).unwrap();
        let t = QueenConfig::load(&toml_path).unwrap();
        let j = QueenConfig::load(&json_path).unwrap();
        assert_eq!(format!("{j:?}"), format!("{t:?}"));

        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, FULL_TOML.replace("repos = [", "nope = 1\nrepos = [")).unwrap();
        let e = QueenConfig::load(&bad).unwrap_err().to_string();
        assert!(e.contains("bad.toml") && e.contains("nope"), "{e}");

        let empty = dir.path().join("empty.toml");
        std::fs::write(
            &empty,
            FULL_TOML.replace("\"example-org/app\", \"example-org/lib\"", ""),
        )
        .unwrap();
        assert!(matches!(
            QueenConfig::load(&empty),
            Err(ConfigError::Invalid(_))
        ));
    }
}
