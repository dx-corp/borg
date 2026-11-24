//! Per-VM tailnet join keys for burst drones, from the Tailscale OAuth client.
//!
//! A burst VM joins the tailnet with a key that the queen puts in its
//! instance metadata and removes once the drone heartbeats. That key used to
//! be one shared, reusable key read from a file. Founder rule: a reusable key
//! is never put in metadata. So for every VM the queen now mints a key that
//! is single-use, pre-authorized, valid for one hour, tagged
//! `gce.tailscale_tag`, and not ephemeral (the iac#439 key shape, made by
//! iac `infrastructure/hetzner/dedicated/mint-tailscale-key.py`). A key read
//! out of metadata is spent by the join or dead within the hour.
//!
//! Non-ephemeral nodes stay in the tailnet after their VM is gone, so when
//! the queen deletes a burst VM it also deletes the tailnet device of that
//! name ([`TailscaleApi::delete_devices_named`]), best effort.
//!
//! Credentials: `gce.tailscale_oauth_path`, a JSON file on the queen,
//! `{"client_id": "...", "client_secret": "..."}`, readable only by borg. It
//! is read on every call, so rotating it needs no restart.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

/// The production API; tests point `gce.tailscale_api_base` at a fake.
pub(crate) const DEFAULT_API_BASE: &str = "https://api.tailscale.com/api/v2";

/// How long a minted key stays valid. A drone joins within minutes of boot.
const KEY_EXPIRY_SECS: u64 = 3600;

#[derive(Debug, thiserror::Error)]
pub enum TsKeyError {
    #[error("read {path}: {source}")]
    Credentials {
        path: String,
        source: std::io::Error,
    },
    #[error("{path}: want {{\"client_id\", \"client_secret\"}}: {0}", path = .1)]
    CredentialsFormat(String, String),
    #[error("tailscale api: {0}")]
    Http(#[from] reqwest::Error),
    #[error("tailscale api {what}: HTTP {status}: {body}")]
    Status {
        what: &'static str,
        status: u16,
        body: String,
    },
    #[error("tailscale api {0}")]
    Unexpected(String),
}

#[derive(Deserialize)]
struct OauthClient {
    client_id: String,
    client_secret: String,
}

/// Mints join keys and removes devices with the tailnet's OAuth client.
#[derive(Clone)]
pub struct TailscaleApi {
    http: reqwest::Client,
    api_base: String,
    oauth_path: PathBuf,
}

/// `application/x-www-form-urlencoded` for one value.
fn form_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

impl TailscaleApi {
    pub fn new(oauth_path: &Path, api_base: &str) -> Result<Self, TsKeyError> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            http,
            api_base: api_base.trim_end_matches('/').to_owned(),
            oauth_path: oauth_path.to_owned(),
        })
    }

    async fn checked(resp: reqwest::Response, what: &'static str) -> Result<Value, TsKeyError> {
        let status = resp.status();
        if !status.is_success() {
            let body: String = resp
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(300)
                .collect();
            return Err(TsKeyError::Status {
                what,
                status: status.as_u16(),
                body,
            });
        }
        let text = resp.text().await?;
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|e| TsKeyError::Unexpected(format!("{what}: {e}")))
    }

    async fn token(&self) -> Result<String, TsKeyError> {
        let raw = tokio::fs::read_to_string(&self.oauth_path)
            .await
            .map_err(|source| TsKeyError::Credentials {
                path: self.oauth_path.display().to_string(),
                source,
            })?;
        let client: OauthClient = serde_json::from_str(&raw).map_err(|e| {
            TsKeyError::CredentialsFormat(e.to_string(), self.oauth_path.display().to_string())
        })?;
        let body = format!(
            "client_id={}&client_secret={}",
            form_escape(client.client_id.trim()),
            form_escape(client.client_secret.trim())
        );
        let resp = self
            .http
            .post(format!("{}/oauth/token", self.api_base))
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body)
            .send()
            .await?;
        let v = Self::checked(resp, "oauth token").await?;
        v.get("access_token")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| TsKeyError::Unexpected("oauth token: no access_token".to_owned()))
    }

    /// A single-use, pre-authorized, one-hour, non-ephemeral key for `tag`.
    /// Refuses a key Tailscale minted with any other capabilities.
    pub async fn mint_join_key(&self, tag: &str, description: &str) -> Result<String, TsKeyError> {
        let token = self.token().await?;
        // Tailscale accepts letters, digits, spaces and dashes in descriptions.
        let description: String = description
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    ' '
                }
            })
            .take(50)
            .collect();
        let body = json!({
            "capabilities": {"devices": {"create": {
                "reusable": false,
                "ephemeral": false,
                "preauthorized": true,
                "tags": [tag],
            }}},
            "expirySeconds": KEY_EXPIRY_SECS,
            "description": description,
        });
        let resp = self
            .http
            .post(format!("{}/tailnet/-/keys", self.api_base))
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await?;
        let key = Self::checked(resp, "create key").await?;
        let create = key.pointer("/capabilities/devices/create");
        let flag = |name: &str| create.and_then(|c| c.get(name)).and_then(Value::as_bool);
        let tagged = create
            .and_then(|c| c.get("tags"))
            .and_then(Value::as_array)
            .is_some_and(|t| t.iter().any(|x| x.as_str() == Some(tag)));
        if flag("reusable") != Some(false) || flag("ephemeral") == Some(true) || !tagged {
            return Err(TsKeyError::Unexpected(format!(
                "create key: minted with unexpected capabilities {}",
                create.map(Value::to_string).unwrap_or_default()
            )));
        }
        key.get("key")
            .and_then(Value::as_str)
            .filter(|k| !k.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| TsKeyError::Unexpected("create key: no key in the answer".to_owned()))
    }

    /// Delete every tailnet device whose hostname is `hostname` and that
    /// carries `tag`. Returns how many were deleted.
    pub async fn delete_devices_named(
        &self,
        hostname: &str,
        tag: &str,
    ) -> Result<usize, TsKeyError> {
        let token = self.token().await?;
        let resp = self
            .http
            .get(format!("{}/tailnet/-/devices", self.api_base))
            .bearer_auth(&token)
            .send()
            .await?;
        let listed = Self::checked(resp, "list devices").await?;
        let ids: Vec<String> = listed
            .get("devices")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|d| d.get("hostname").and_then(Value::as_str) == Some(hostname))
            .filter(|d| {
                d.get("tags")
                    .and_then(Value::as_array)
                    .is_some_and(|t| t.iter().any(|x| x.as_str() == Some(tag)))
            })
            .filter_map(|d| d.get("id").and_then(Value::as_str).map(str::to_owned))
            .collect();
        for id in &ids {
            let resp = self
                .http
                .delete(format!("{}/device/{}", self.api_base, form_escape(id)))
                .bearer_auth(&token)
                .send()
                .await?;
            Self::checked(resp, "delete device").await?;
        }
        Ok(ids.len())
    }
}

/// The join key for burst VM `vm`, minted with `gce.tailscale_oauth_path`.
/// No reusable key is ever put in metadata.
pub(crate) async fn mint_for_vm(g: &crate::burst::GceSettings, vm: &str) -> Result<String, String> {
    let oauth = g
        .tailscale_oauth_path
        .as_ref()
        .ok_or("gce.tailscale_oauth_path is not set")?;
    TailscaleApi::new(oauth, &g.tailscale_api_base)
        .map_err(|e| e.to_string())?
        .mint_join_key(&g.tailscale_tag, &format!("borg burst {vm}"))
        .await
        .map_err(|e| format!("mint tailnet key for {vm}: {e}"))
}

/// Remove the tailnet device of a deleted burst drone: it joined as a
/// non-ephemeral node. Best effort; a leftover record is offline and holds
/// no access.
pub(crate) async fn remove_device(g: &crate::burst::GceSettings, drone: &str) {
    let Some(oauth) = g.tailscale_oauth_path.as_ref() else {
        // Keep the OAuth client configured until burst drones drain to have
        // their records removed; without it the record stays, offline.
        tracing::warn!(
            "burst: tailnet device {drone} left in the tailnet: gce.tailscale_oauth_path is not configured"
        );
        return;
    };
    let removed = match TailscaleApi::new(oauth, &g.tailscale_api_base) {
        Ok(api) => api.delete_devices_named(drone, &g.tailscale_tag).await,
        Err(e) => Err(e),
    };
    if let Err(e) = removed {
        tracing::warn!("burst: remove tailnet device {drone}: {e}");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::extract::{Path as AxPath, State};
    use axum::routing::{delete, get, post};

    use super::*;

    #[derive(Default)]
    struct Fake {
        minted: Vec<Value>,
        deleted: Vec<String>,
        reusable: bool,
    }

    type Shared = Arc<Mutex<Fake>>;

    async fn serve(fake: Shared) -> String {
        let app = Router::new()
            .route(
                "/oauth/token",
                post(|body: String| async move {
                    assert!(body.contains("client_id=cid") && body.contains("client_secret=s%2Fecret"));
                    axum::Json(json!({"access_token": "tok"}))
                }),
            )
            .route(
                "/tailnet/-/keys",
                post(|State(f): State<Shared>, axum::Json(body): axum::Json<Value>| async move {
                    let mut f = f.lock().unwrap();
                    let mut create = body["capabilities"]["devices"]["create"].clone();
                    if f.reusable {
                        create["reusable"] = json!(true);
                    }
                    f.minted.push(body);
                    axum::Json(json!({"key": "tskey-auth-1", "capabilities": {"devices": {"create": create}}}))
                }),
            )
            .route(
                "/tailnet/-/devices",
                get(|| async {
                    axum::Json(json!({"devices": [
                        {"id": "1", "hostname": "borg-burst-a", "tags": ["tag:borg-drone"]},
                        {"id": "2", "hostname": "borg-burst-a", "tags": ["tag:other"]},
                        {"id": "3", "hostname": "borg-burst-b", "tags": ["tag:borg-drone"]},
                    ]}))
                }),
            )
            .route(
                "/device/{id}",
                delete(|State(f): State<Shared>, AxPath(id): AxPath<String>| async move {
                    f.lock().unwrap().deleted.push(id);
                    ""
                }),
            )
            .with_state(fake);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn creds(dir: &tempfile::TempDir) -> PathBuf {
        let p = dir.path().join("ts-oauth.json");
        std::fs::write(&p, r#"{"client_id": "cid", "client_secret": "s/ecret"}"#).unwrap();
        p
    }

    #[tokio::test]
    async fn mints_a_single_use_non_ephemeral_tagged_key() {
        let fake = Shared::default();
        let base = serve(fake.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let api = TailscaleApi::new(&creds(&dir), &base).unwrap();
        let key = api
            .mint_join_key("tag:borg-drone", "borg burst borg-burst-a (x)")
            .await
            .unwrap();
        assert_eq!(key, "tskey-auth-1");
        let f = fake.lock().unwrap();
        let body = &f.minted[0];
        let create = &body["capabilities"]["devices"]["create"];
        assert_eq!(create["reusable"], json!(false));
        assert_eq!(create["ephemeral"], json!(false));
        assert_eq!(create["preauthorized"], json!(true));
        assert_eq!(create["tags"], json!(["tag:borg-drone"]));
        assert_eq!(body["expirySeconds"], json!(3600));
        assert_eq!(body["description"], json!("borg burst borg-burst-a  x "));
    }

    #[tokio::test]
    async fn refuses_a_key_minted_reusable() {
        let fake = Shared::default();
        fake.lock().unwrap().reusable = true;
        let base = serve(fake).await;
        let dir = tempfile::tempdir().unwrap();
        let api = TailscaleApi::new(&creds(&dir), &base).unwrap();
        let err = api.mint_join_key("tag:borg-drone", "x").await.unwrap_err();
        assert!(err.to_string().contains("unexpected capabilities"), "{err}");
    }

    #[tokio::test]
    async fn deletes_only_the_named_tagged_devices() {
        let fake = Shared::default();
        let base = serve(fake.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let api = TailscaleApi::new(&creds(&dir), &base).unwrap();
        let n = api
            .delete_devices_named("borg-burst-a", "tag:borg-drone")
            .await
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(fake.lock().unwrap().deleted, vec!["1".to_owned()]);
    }

    #[tokio::test]
    async fn missing_credentials_are_a_clear_error() {
        let api =
            TailscaleApi::new(Path::new("/nonexistent/ts.json"), "http://127.0.0.1:9").unwrap();
        let err = api.mint_join_key("tag:borg-drone", "x").await.unwrap_err();
        assert!(err.to_string().contains("/nonexistent/ts.json"), "{err}");
    }
}
