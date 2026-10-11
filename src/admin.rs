//! Queen administration: enrolling, listing and removing drones. The CLI is a
//! thin wrapper so the behaviour is testable without a process.

use crate::enroll;
use crate::store::{DroneRow, Store, StoreError};

#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    #[error(
        "drone name must be 1-40 lowercase letters, digits or dashes, not starting with a dash"
    )]
    BadName,
    #[error("no drone named {0}")]
    NoSuchDrone(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Enroll `name` and return its one-time token (the queen keeps only the hash).
pub async fn enroll_drone(store: &Store, name: &str, now: i64) -> Result<String, AdminError> {
    if !enroll::valid_drone_name(name) {
        return Err(AdminError::BadName);
    }
    let token = enroll::new_token();
    store.enroll(name, &enroll::hash_token(&token), now).await?;
    Ok(token)
}

/// Remove a drone. Its token stops working immediately (the hash is deleted)
/// and its lease records are dropped, which lets the queen deregister any
/// runners still offline. Returns how many unfinished leases it had.
pub async fn remove_drone(store: &Store, name: &str) -> Result<u32, AdminError> {
    store
        .remove_drone(name)
        .await?
        .ok_or_else(|| AdminError::NoSuchDrone(name.to_owned()))
}

/// One line per drone: name, state, heartbeat age, host, size and labels.
pub fn list_lines(rows: &[DroneRow], now: i64, stale_secs: i64) -> Vec<String> {
    let mut lines = vec![format!(
        "{:<24} {:<8} {:>10} {:<24} {:>5} {:>9} {}",
        "NAME", "STATE", "HEARTBEAT", "HOST", "VCPU", "MEM(MiB)", "LABELS"
    )];
    for d in rows {
        let (state, age) = match d.last_heartbeat {
            None => ("never", "-".to_owned()),
            Some(t) if now - t > stale_secs => ("stale", format!("{}s", now - t)),
            Some(t) => (
                if d.accepting { "active" } else { "draining" },
                format!("{}s", now - t),
            ),
        };
        let cap = d.capacity.as_ref();
        lines.push(format!(
            "{:<24} {:<8} {:>10} {:<24} {:>5} {:>9} {}",
            d.name,
            state,
            age,
            cap.map_or("-", |c| c.hostname.as_str()),
            cap.map_or_else(|| "-".to_owned(), |c| c.vcpu.to_string()),
            cap.map_or_else(|| "-".to_owned(), |c| c.mem_mib.to_string()),
            d.extra_labels.join(","),
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allocator::HostCaps;
    use crate::probe::{Capacity, HostKind};

    fn capacity() -> Capacity {
        Capacity {
            hostname: "host-a".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            kind: HostKind::Vm,
            vcpu: 4,
            mem_mib: 8192,
            free_disk_mib: None,
            docker_usable: true,
            mise_version: None,
            git_version: None,
        }
    }

    async fn heartbeat(store: &Store, name: &str, accepting: bool, now: i64) {
        store
            .record_heartbeat(name, &capacity(), &HostCaps::default(), accepting, &[], now)
            .await
            .unwrap();
        store
            .set_extra_labels(name, &["on-prem".to_owned()])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn enroll_returns_only_the_token_and_stores_only_its_hash() {
        let store = Store::open_in_memory().await.unwrap();
        let token = enroll_drone(&store, "host-a", 1).await.unwrap();
        assert!(
            token.starts_with("borg_")
                && token.len() == 5 + 64
                && !token.contains(char::is_whitespace)
        );
        assert_eq!(
            store
                .drone_for_token_hash(&enroll::hash_token(&token))
                .await
                .unwrap()
                .as_deref(),
            Some("host-a")
        );
        assert_eq!(
            store.drone_for_token_hash(&token).await.unwrap(),
            None,
            "the plaintext is not stored"
        );
        assert!(matches!(
            enroll_drone(&store, "Bad Name", 1).await,
            Err(AdminError::BadName)
        ));
        assert!(
            enroll_drone(&store, "host-a", 2).await.is_err(),
            "names are unique"
        );
    }

    #[tokio::test]
    async fn remove_revokes_the_token_and_drops_unfinished_leases() {
        use crate::classes::{Class, Scope};
        use crate::secret::Secret;
        use crate::store::{LeaseRow, LeaseStatus};

        let store = Store::open_in_memory().await.unwrap();
        let token = enroll_drone(&store, "host-a", 1).await.unwrap();
        store
            .insert_lease(&LeaseRow {
                id: "l1".into(),
                drone: "host-a".into(),
                class: Class::Light,
                scope: Scope::Pr,
                labels: vec![],
                runner_name: "borg-host-a-l1".into(),
                runner_id: 1,
                jit_config: Some(Secret::new("jit")),
                state: LeaseStatus::Pending,
                created_at: 1,
                expires_at: 100,
            })
            .await
            .unwrap();

        assert_eq!(remove_drone(&store, "host-a").await.unwrap(), 1);
        assert_eq!(
            store
                .drone_for_token_hash(&enroll::hash_token(&token))
                .await
                .unwrap(),
            None,
            "token revoked"
        );
        assert!(
            store.lease("l1").await.unwrap().is_none(),
            "the lease record, and its JIT config, are gone"
        );
        assert!(store.active_leases().await.unwrap().is_empty());
        assert!(store.drones().await.unwrap().is_empty());
        assert!(matches!(
            remove_drone(&store, "host-a").await,
            Err(AdminError::NoSuchDrone(_))
        ));
    }

    #[tokio::test]
    async fn list_shows_state_from_heartbeat_age_and_acceptance() {
        let store = Store::open_in_memory().await.unwrap();
        for n in ["a-never", "b-active", "c-draining", "d-stale"] {
            enroll_drone(&store, n, 1).await.unwrap();
        }
        heartbeat(&store, "b-active", true, 990).await;
        heartbeat(&store, "c-draining", false, 995).await;
        heartbeat(&store, "d-stale", true, 100).await;
        let lines = list_lines(&store.drones().await.unwrap(), 1000, 60);
        assert!(lines[0].starts_with("NAME"));
        let find = |n: &str| lines.iter().find(|l| l.starts_with(n)).unwrap().clone();
        assert!(find("a-never").contains("never"));
        assert!(
            find("b-active").contains("active")
                && find("b-active").contains("10s")
                && find("b-active").contains("host-a")
        );
        assert!(find("c-draining").contains("draining"));
        assert!(find("d-stale").contains("stale") && find("d-stale").contains("on-prem"));
    }
}
