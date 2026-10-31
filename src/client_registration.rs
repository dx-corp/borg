//! Operator-approved registration without transmitting the client's secret.
//!
//! Requests confer no authority. Only an administrator with the queen's local
//! database access can approve one. Existing unique client identities prevent
//! replay; expiration is checked by the normal bearer authentication lookup.

use std::path::Path;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::enroll::{hash_token, new_token, valid_drone_name};
use crate::store::Store;

const REQUEST_LIFETIME: i64 = 900;
const MAX_CLIENT_LIFETIME: u32 = 86_400;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub name: String,
    pub token_hash: String,
    pub created_at: i64,
}

impl Request {
    fn validate(&self, now: i64) -> Result<()> {
        if !valid_drone_name(&self.name)
            || self.token_hash.len() != 64
            || !self
                .token_hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            bail!("invalid registration name or SHA-256 token hash");
        }
        let age = now.checked_sub(self.created_at);
        if !matches!(age, Some(0..REQUEST_LIFETIME)) {
            bail!("registration request is expired or from the future");
        }
        Ok(())
    }
}

/// Create, never overwrite or follow an existing symlink. Refuse platforms
/// where this implementation cannot guarantee an owner-only credential file.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (path, bytes);
        bail!("registration requires a Unix host with owner-only credential files")
    }
}

/// Prepare a non-secret request for delivery over an authenticated operator
/// channel. Neither file contents nor the bearer token are printed.
pub fn prepare(name: &str, token_file: &Path, request_file: &Path) -> Result<()> {
    let token = new_token();
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let request = Request {
        name: name.to_owned(),
        token_hash: hash_token(&token),
        created_at: now,
    };
    request.validate(now)?;
    // Persist the only copy of the secret before publishing its request. If
    // the second write fails, retain the secret rather than deleting evidence.
    write_private(token_file, token.as_bytes())?;
    write_private(request_file, &serde_json::to_vec_pretty(&request)?)
}

pub fn read_request(path: &Path) -> Result<Request> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(4097)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 4096 {
        bail!("registration request exceeds 4096 bytes");
    }
    Ok(serde_json::from_slice(&bytes)?)
}

/// Local administrator operation; there is deliberately no remote approval
/// endpoint. The immutable client row is also the durable replay tombstone.
pub async fn approve(store: &Store, request: &Request, lifetime: u32, now: i64) -> Result<i64> {
    request.validate(now)?;
    if !(1..=MAX_CLIENT_LIFETIME).contains(&lifetime) {
        bail!("client lifetime must be 1..=86400 seconds");
    }
    let expires = now
        .checked_add(i64::from(lifetime))
        .ok_or_else(|| anyhow::anyhow!("invalid expiration"))?;
    // sqlx-runtime-query: standalone SQLite client authority; uniqueness makes approval single-use
    sqlx::query("INSERT INTO clients (name, token_hash, created_at, max_scope, expires_at) VALUES (?, ?, ?, 'pr', ?)")
        .bind(&request.name).bind(format!("temporary:{}", request.token_hash)).bind(now).bind(expires)
        .execute(&store.pool).await?;
    Ok(expires)
}

/// Temporary clients cannot inherit legacy clients' shared unit visibility.
pub(crate) async fn is_temporary(
    store: &Store,
    client: &str,
) -> Result<bool, crate::store::StoreError> {
    Ok(
        // sqlx-runtime-query: existing SQLite client authority distinguishes temporary approvals
        sqlx::query_scalar::<_, bool>("SELECT expires_at IS NOT NULL FROM clients WHERE name = ?")
            .bind(client)
            .fetch_optional(&store.pool)
            .await?
            .unwrap_or(false),
    )
}

/// Revoke temporary access without deleting the replay tombstone. Existing
/// legacy client identities are outside this operation's authority.
pub async fn revoke(store: &Store, client: &str) -> Result<bool> {
    let result =
        // sqlx-runtime-query: expiry zero is permanently invalid; retain immutable identity and hash
        sqlx::query("UPDATE clients SET expires_at = 0 WHERE name = ? AND expires_at IS NOT NULL")
            .bind(client)
            .execute(&store.pool)
            .await?;
    Ok(result.rows_affected() == 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classes::Scope;

    #[tokio::test]
    async fn approval_is_bounded_single_use_and_does_not_replace_existing_identity() {
        let store = Store::open_in_memory().await.unwrap();
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let r = Request {
            name: "agent".into(),
            token_hash: hash_token(&new_token()),
            created_at: now,
        };
        assert!(
            store
                .client_for_token_hash(&r.token_hash)
                .await
                .unwrap()
                .is_none()
        );
        for ttl in [0, MAX_CLIENT_LIFETIME + 1] {
            assert!(approve(&store, &r, ttl, now).await.is_err());
        }
        approve(&store, &r, 60, now).await.unwrap();
        let old_lookup =
            // sqlx-runtime-query: regression for an old queen's exact authentication lookup
            sqlx::query_scalar::<_, String>("SELECT name FROM clients WHERE token_hash = ?")
                .bind(&r.token_hash)
                .fetch_optional(&store.pool)
                .await
                .unwrap();
        assert!(
            old_lookup.is_none(),
            "old binaries must fail closed for temporary credentials"
        );
        assert_eq!(store.client_max_scope("agent").await.unwrap(), Scope::Pr);
        assert_eq!(
            store
                .client_for_token_hash(&r.token_hash)
                .await
                .unwrap()
                .as_deref(),
            Some("agent")
        );
        assert!(approve(&store, &r, 60, now).await.is_err());
        let other = Request {
            token_hash: hash_token(&new_token()),
            ..r
        };
        assert!(approve(&store, &other, 60, now).await.is_err());
    }

    #[tokio::test]
    async fn expired_credentials_fail_authentication_but_legacy_clients_survive() {
        let store = Store::open_in_memory().await.unwrap();
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let r = Request {
            name: "expired".into(),
            token_hash: hash_token(&new_token()),
            created_at: now - 10,
        };
        approve(&store, &r, 1, now - 10).await.unwrap();
        assert!(
            store
                .client_for_token_hash(&r.token_hash)
                .await
                .unwrap()
                .is_none()
        );
        store
            .enroll_client("legacy", "legacy-hash", 0)
            .await
            .unwrap();
        assert_eq!(
            store
                .client_for_token_hash("legacy-hash")
                .await
                .unwrap()
                .as_deref(),
            Some("legacy")
        );
    }

    #[tokio::test]
    async fn concurrent_approval_has_exactly_one_winner() {
        let store = Store::open_in_memory().await.unwrap();
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let request = Request {
            name: "racing-agent".into(),
            token_hash: hash_token(&new_token()),
            created_at: now,
        };
        let (a, b) = tokio::join!(
            approve(&store, &request, 60, now),
            approve(&store, &request, 60, now)
        );
        assert_ne!(a.is_ok(), b.is_ok());
    }

    #[test]
    fn rejects_stale_future_invalid_or_authority_bearing_requests() {
        let mut r = Request {
            name: "agent".into(),
            token_hash: "a".repeat(64),
            created_at: 1000,
        };
        assert!(r.validate(1000).is_ok());
        for now in [999, 1900, i64::MIN] {
            assert!(r.validate(now).is_err());
        }
        r.token_hash = "z".repeat(64);
        assert!(r.validate(1000).is_err());
        assert!(
            serde_json::from_str::<Request>(
                r#"{"name":"agent","token_hash":"a","created_at":1,"max_scope":"deploy"}"#
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn secret_stays_private_and_existing_paths_are_not_overwritten() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("token");
        let request = dir.path().join("request.json");
        prepare("agent", &secret, &request).unwrap();
        let token = std::fs::read_to_string(&secret).unwrap();
        let parsed = read_request(&request).unwrap();
        assert_eq!(parsed.token_hash, hash_token(&token));
        assert!(!std::fs::read_to_string(&request).unwrap().contains(&token));
        assert_eq!(
            std::fs::metadata(&secret).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(prepare("agent", &secret, &request).is_err());
        assert_eq!(std::fs::read_to_string(secret).unwrap(), token);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlink_without_modifying_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("existing");
        let link = dir.path().join("link");
        std::fs::write(&target, "keep").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(prepare("agent", &link, &dir.path().join("request")).is_err());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "keep");
    }
}
