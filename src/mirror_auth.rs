//! Short-lived read-only GitHub tokens for the drones' git mirrors.
//!
//! A second GitHub App key lives on the queen only. The queen mints
//! installation tokens down-scoped to `contents: read` on the mirrored
//! repositories and hands them to drones in the heartbeat response; the App
//! key never reaches a drone. The token is never logged and `/status` shows
//! only whether mirror auth is configured and when the current token expires.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::github::{GithubClient, GithubError};
use crate::secret::Secret;

/// Mint a new token when the cached one has less than this much life left.
const REMINT_BELOW_SECS: i64 = 10 * 60;

/// What a drone needs to fetch private mirrors with. `repositories` are
/// `owner/name`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorCredential {
    pub token: Secret,
    pub expires_at: i64,
    pub repositories: Vec<String>,
}

/// `/status`: never the token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct MirrorAuthView {
    pub configured: bool,
    pub expires_at: Option<i64>,
}

pub struct MirrorAuth {
    client: GithubClient,
    org: String,
    /// Bare names, as GitHub's `repositories` parameter wants them.
    repositories: Vec<String>,
    cached: tokio::sync::Mutex<Option<(Secret, i64)>>,
    expires_at: std::sync::atomic::AtomicI64,
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

impl MirrorAuth {
    pub fn new(client: GithubClient, org: &str, repositories: &[String]) -> Self {
        Self {
            client,
            org: org.to_owned(),
            repositories: repositories.to_vec(),
            cached: tokio::sync::Mutex::new(None),
            expires_at: std::sync::atomic::AtomicI64::new(0),
        }
    }

    /// The current credential, minting a new token when under ten minutes of
    /// life remain.
    pub async fn credential(&self) -> Result<MirrorCredential, GithubError> {
        let mut cached = self.cached.lock().await;
        let reusable = cached
            .as_ref()
            .filter(|(_, expires_at)| expires_at - REMINT_BELOW_SECS > unix_now());
        let (token, expires_at) = match reusable {
            Some((token, expires_at)) => (token.clone(), *expires_at),
            None => {
                let minted = self
                    .client
                    .installation_token_scoped(
                        &serde_json::json!({"contents": "read"}),
                        &self.repositories,
                    )
                    .await?;
                *cached = Some(minted.clone());
                self.expires_at
                    .store(minted.1, std::sync::atomic::Ordering::Relaxed);
                minted
            }
        };
        Ok(MirrorCredential {
            token,
            expires_at,
            repositories: self
                .repositories
                .iter()
                .map(|r| format!("{}/{r}", self.org))
                .collect(),
        })
    }

    pub fn view(&self) -> MirrorAuthView {
        let expires_at = self.expires_at.load(std::sync::atomic::Ordering::Relaxed);
        MirrorAuthView {
            configured: true,
            expires_at: (expires_at > 0).then_some(expires_at),
        }
    }
}
