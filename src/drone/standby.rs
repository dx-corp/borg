//! Standby deploy runners the drone holds unstarted.
//!
//! The queen mints a standby runner for a deploy drone and sends its JIT
//! config. The drone keeps it in memory and, when `state_dir` is set, in a
//! `0600` file so a restart while the queen is down still has it. The drone
//! starts it when the queen says so, or when the queen has been unreachable
//! for `fail_open_after_secs` ([`should_fail_open`]).

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::api::LeaseGrant;
use crate::drone::cleanup::valid_lease_id;

/// Directory under `state_dir` holding one `<lease id>.json` per standby.
const DIR: &str = "standby";

/// Whether the queen has been silent long enough to start the held standby
/// runners. `after_secs == 0` turns fail-open off.
pub fn should_fail_open(last_ok_at: i64, now: i64, after_secs: u64) -> bool {
    after_secs > 0
        && now.saturating_sub(last_ok_at) >= i64::try_from(after_secs).unwrap_or(i64::MAX)
}

#[derive(Default)]
pub struct Standbys {
    dir: Option<PathBuf>,
    held: Mutex<BTreeMap<String, LeaseGrant>>,
}

impl Standbys {
    /// Load the standbys persisted under `state_dir`; unreadable files are
    /// skipped (the queen resends what the drone does not report).
    pub fn load(state_dir: Option<PathBuf>) -> Self {
        let dir = state_dir.map(|d| d.join(DIR));
        let mut held = BTreeMap::new();
        if let Some(dir) = &dir
            && let Ok(entries) = std::fs::read_dir(dir)
        {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|e| e != "json") {
                    continue;
                }
                let grant = std::fs::read(&path)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<LeaseGrant>(&b).ok())
                    .filter(|g| valid_lease_id(&g.lease_id));
                match grant {
                    Some(g) => {
                        held.insert(g.lease_id.clone(), g);
                    }
                    None => {
                        tracing::warn!("discarding unreadable standby file {}", path.display());
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
        }
        Self {
            dir,
            held: Mutex::new(held),
        }
    }

    pub fn ids(&self) -> Vec<String> {
        self.held
            .lock()
            .map(|h| h.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub fn get(&self, id: &str) -> Option<LeaseGrant> {
        self.held.lock().ok()?.get(id).cloned()
    }

    /// Hold `grant`. Returns false (holding nothing) for an unsafe id.
    pub fn insert(&self, grant: LeaseGrant) -> bool {
        if !valid_lease_id(&grant.lease_id) {
            return false;
        }
        if let Err(e) = self.persist(&grant) {
            tracing::warn!("persist standby {}: {e}", grant.lease_id);
        }
        if let Ok(mut h) = self.held.lock() {
            h.insert(grant.lease_id.clone(), grant);
        }
        true
    }

    /// Forget a standby (started, or ended on the queen's side) and delete its
    /// file.
    pub fn remove(&self, id: &str) {
        if let Ok(mut h) = self.held.lock() {
            h.remove(id);
        }
        if let Some(dir) = &self.dir
            && valid_lease_id(id)
        {
            let _ = std::fs::remove_file(dir.join(format!("{id}.json")));
        }
    }

    fn persist(&self, grant: &LeaseGrant) -> std::io::Result<()> {
        let Some(dir) = &self.dir else {
            return Ok(());
        };
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!("{}.tmp", grant.lease_id));
        let bytes = serde_json::to_vec(grant).map_err(std::io::Error::other)?;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, dir.join(format!("{}.json", grant.lease_id)))
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::classes::{Class, Scope};
    use crate::secret::Secret;

    fn grant(id: &str) -> LeaseGrant {
        LeaseGrant {
            lease_id: id.into(),
            class: Class::Medium,
            scope: Scope::Deploy,
            labels: vec!["self-hosted".into()],
            runner_name: format!("borg-host-a-{id}"),
            jit_config: Secret::new("jit"),
            expires_at: i64::MAX,
        }
    }

    #[test]
    fn fail_open_waits_for_the_configured_silence() {
        assert!(!should_fail_open(1000, 1119, 120));
        assert!(should_fail_open(1000, 1120, 120));
        assert!(should_fail_open(1000, 5000, 120));
        // 0 disables fail-open however long the queen is silent.
        assert!(!should_fail_open(0, i64::MAX, 0));
        // A clock that went backwards does not trigger it.
        assert!(!should_fail_open(2000, 1000, 120));
    }

    #[test]
    fn standbys_survive_a_restart_in_a_private_file_until_removed() {
        let dir = tempfile::tempdir().unwrap();
        let a = Standbys::load(Some(dir.path().to_owned()));
        assert!(a.insert(grant("abc123")));
        assert!(!a.insert(grant("../escape")));
        let file = dir.path().join("standby/abc123.json");
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let b = Standbys::load(Some(dir.path().to_owned()));
        assert_eq!(b.ids(), vec!["abc123".to_owned()]);
        assert_eq!(b.get("abc123").unwrap().runner_name, "borg-host-a-abc123");

        b.remove("abc123");
        assert!(b.ids().is_empty() && !file.exists());
        assert!(Standbys::load(Some(dir.path().to_owned())).ids().is_empty());
    }

    #[test]
    fn without_a_state_dir_standbys_live_in_memory_only() {
        let s = Standbys::load(None);
        assert!(s.insert(grant("m1")));
        assert_eq!(s.ids(), vec!["m1".to_owned()]);
        s.remove("m1");
        assert!(s.ids().is_empty());
    }
}
