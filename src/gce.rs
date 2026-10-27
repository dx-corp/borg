//! The Compute Engine side of burst capacity: the queen creates, scrubs and
//! deletes burst VMs through the REST API with a service-account key.
//!
//! The request shapes are pure functions (`instance_body`,
//! `render_bootstrap`) so tests can check what a VM is asked to be without
//! a network. [`GceClient`] does the HTTP: a signed JWT exchanged for a bearer
//! token (cached until shortly before it expires), then plain JSON calls.

use std::path::Path;
use std::time::Duration;

use jsonwebtoken::{Algorithm, EncodingKey, Header};
use reqwest::{Method, RequestBuilder, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::burst::GceSettings;
use crate::secret::Secret;

/// Instance metadata keys that carry secrets during bootstrap. The queen
/// removes them once the drone heartbeats.
pub(crate) const SECRET_METADATA_KEYS: [&str; 2] = ["borg-ts-authkey", "borg-enroll-token"];
/// Where the drone's startup script lives in instance metadata.
const STARTUP_SCRIPT_KEY: &str = "startup-script";
const TOKEN_SCOPE: &str = "https://www.googleapis.com/auth/compute";
/// Refresh the access token this long before it expires.
const TOKEN_SLACK_SECS: i64 = 60;
/// How long one `operations.wait` call may block (the API caps it at 2 min).
const WAIT_TIMEOUT: Duration = Duration::from_secs(150);
const BOOTSTRAP: &str = include_str!("gce_bootstrap.sh");

#[derive(Debug, thiserror::Error)]
pub enum GceError {
    #[error("request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("{method} {path} returned {status}: {body}")]
    Status {
        method: &'static str,
        path: String,
        status: u16,
        body: String,
    },
    #[error("sign service-account jwt: {0}")]
    Jwt(#[from] jsonwebtoken::errors::Error),
    #[error("service-account key: {0}")]
    Key(String),
    #[error("unexpected response: {0}")]
    Decode(String),
    /// The create operation finished with an error (for example a zone out
    /// of Spot capacity).
    #[error("operation failed: {0}")]
    Operation(String),
}

/// The fields of a service-account key file the queen uses.
#[derive(Debug, Deserialize)]
struct KeyFile {
    client_email: String,
    private_key: String,
    #[serde(default = "default_token_uri")]
    token_uri: String,
}

fn default_token_uri() -> String {
    "https://oauth2.googleapis.com/token".to_owned()
}

#[derive(serde::Serialize)]
struct Claims<'a> {
    iss: &'a str,
    scope: &'static str,
    aud: &'a str,
    iat: i64,
    exp: i64,
}

#[derive(Default)]
struct AuthState {
    token: Option<Secret>,
    expires_at: i64,
}

/// A Compute Engine API client for one project.
pub struct GceClient {
    http: reqwest::Client,
    api_base: String,
    project: String,
    client_email: String,
    token_uri: String,
    key: EncodingKey,
    auth: tokio::sync::Mutex<AuthState>,
}

impl std::fmt::Debug for GceClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GceClient")
            .field("api_base", &self.api_base)
            .field("project", &self.project)
            .field("client_email", &self.client_email)
            .finish_non_exhaustive()
    }
}

fn unix_now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

impl GceClient {
    /// Read a service-account key file (JSON, as the cloud console exports it).
    pub fn from_key_file(path: &Path, project: &str, api_base: &str) -> Result<Self, GceError> {
        let raw = std::fs::read(path)
            .map_err(|e| GceError::Key(format!("read {}: {e}", path.display())))?;
        let key: KeyFile = serde_json::from_slice(&raw)
            .map_err(|e| GceError::Key(format!("parse {}: {e}", path.display())))?;
        Self::new(
            api_base,
            project,
            &key.client_email,
            &key.token_uri,
            key.private_key.as_bytes(),
        )
    }

    fn new(
        api_base: &str,
        project: &str,
        client_email: &str,
        token_uri: &str,
        private_key_pem: &[u8],
    ) -> Result<Self, GceError> {
        let key =
            EncodingKey::from_rsa_pem(private_key_pem).map_err(|e| GceError::Key(e.to_string()))?;
        let http = reqwest::Client::builder()
            .user_agent("borg-queen")
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            http,
            api_base: api_base.trim_end_matches('/').to_owned(),
            project: project.to_owned(),
            client_email: client_email.to_owned(),
            token_uri: token_uri.to_owned(),
            key,
            auth: tokio::sync::Mutex::new(AuthState::default()),
        })
    }

    fn assertion(&self) -> Result<String, GceError> {
        let now = unix_now();
        let claims = Claims {
            iss: &self.client_email,
            scope: TOKEN_SCOPE,
            aud: &self.token_uri,
            iat: now - 10,
            exp: now + 3600,
        };
        Ok(jsonwebtoken::encode(
            &Header::new(Algorithm::RS256),
            &claims,
            &self.key,
        )?)
    }

    async fn bearer(&self) -> Result<Secret, GceError> {
        let mut auth = self.auth.lock().await;
        if let Some(t) = &auth.token
            && unix_now() + TOKEN_SLACK_SECS < auth.expires_at
        {
            return Ok(t.clone());
        }
        #[derive(Deserialize)]
        struct TokenReply {
            access_token: String,
            expires_in: i64,
        }
        // The assertion is base64url, which needs no form encoding.
        let body = format!(
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion={}",
            self.assertion()?
        );
        let resp = self
            .http
            .post(&self.token_uri)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(GceError::Status {
                method: "POST",
                path: "token".to_owned(),
                status: status.as_u16(),
                body: body.chars().take(200).collect(),
            });
        }
        let reply: TokenReply = resp
            .json()
            .await
            .map_err(|e| GceError::Decode(format!("token: {e}")))?;
        let token = Secret::new(reply.access_token);
        auth.token = Some(token.clone());
        auth.expires_at = unix_now() + reply.expires_in;
        Ok(token)
    }

    fn zone_path(&self, zone: &str, rest: &str) -> String {
        format!("/projects/{}/zones/{zone}/{rest}", self.project)
    }

    async fn request(&self, method: Method, path: &str) -> Result<RequestBuilder, GceError> {
        let token = self.bearer().await?;
        Ok(self
            .http
            .request(method, format!("{}{path}", self.api_base))
            .bearer_auth(token.expose()))
    }

    async fn send(
        &self,
        method: &'static str,
        path: &str,
        rb: RequestBuilder,
    ) -> Result<(StatusCode, Value), GceError> {
        let resp = rb.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(GceError::Status {
                method,
                path: path.to_owned(),
                status: status.as_u16(),
                body: body.chars().take(300).collect(),
            });
        }
        let bytes = resp.bytes().await?;
        if bytes.is_empty() {
            return Ok((status, Value::Null));
        }
        let v =
            serde_json::from_slice(&bytes).map_err(|e| GceError::Decode(format!("{path}: {e}")))?;
        Ok((status, v))
    }

    /// Insert an instance from `body`. An error means no VM was created: the
    /// request was refused, or the operation came back already failed. On
    /// success, returns the operation to wait on (if GCE named one); the VM
    /// may exist from here on, so callers record it before waiting.
    pub(crate) async fn insert_instance(
        &self,
        zone: &str,
        body: &Value,
    ) -> Result<Option<String>, GceError> {
        let path = self.zone_path(zone, "instances");
        let rb = self.request(Method::POST, &path).await?.json(body);
        let (_, op) = self.send("POST", &path, rb).await?;
        if let Some(e) = operation_error(&op) {
            return Err(GceError::Operation(e));
        }
        Ok(op.get("name").and_then(Value::as_str).map(str::to_owned))
    }

    /// Wait (bounded) for an insert operation. `Err(Operation)` means the
    /// create failed (stockout, quota) and no VM exists; any other error says
    /// nothing about the VM. A wait that times out is `Ok`: the VM may still
    /// come up, and the provisioning grace covers the rest.
    pub(crate) async fn wait_operation(&self, zone: &str, op: &str) -> Result<(), GceError> {
        let wait = self.zone_path(zone, &format!("operations/{op}/wait"));
        let rb = empty_post(self.request(Method::POST, &wait).await?).timeout(WAIT_TIMEOUT);
        match self.send("POST", &wait, rb).await {
            Ok((_, done)) => match operation_error(&done) {
                Some(e) => Err(GceError::Operation(e)),
                None => Ok(()),
            },
            Err(GceError::Http(e)) if e.is_timeout() => {
                tracing::warn!("gce: create {zone}/{op} still pending after wait");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Every instance in the project labeled `borg-burst=queen`, in any zone,
    /// that is not already being deleted. Needs `compute.instances.list`.
    pub(crate) async fn list_burst_instances(&self) -> Result<Vec<ListedVm>, GceError> {
        let mut out = Vec::new();
        let mut page: Option<String> = None;
        let path = format!("/projects/{}/aggregated/instances", self.project);
        let filter = format!("labels.{BURST_LABEL}={BURST_LABEL_VALUE}");
        // 500 per page; far more pages than max_drones could ever need.
        for _ in 0..20 {
            let mut query = vec![("filter", filter.as_str()), ("maxResults", "500")];
            if let Some(token) = &page {
                query.push(("pageToken", token.as_str()));
            }
            let rb = self.request(Method::GET, &path).await?.query(&query);
            let (_, v) = self.send("GET", &path, rb).await?;
            let scopes = v.get("items").and_then(Value::as_object);
            for instance in scopes
                .into_iter()
                .flat_map(|m| m.values())
                .filter_map(|scope| scope.get("instances").and_then(Value::as_array))
                .flatten()
            {
                if let Some(vm) = ListedVm::from_instance(instance) {
                    out.push(vm);
                }
            }
            page = v
                .get("nextPageToken")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .map(str::to_owned);
            if page.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// Delete an instance. `Ok(false)` when it was already gone.
    pub(crate) async fn delete_instance(&self, zone: &str, vm: &str) -> Result<bool, GceError> {
        let path = self.zone_path(zone, &format!("instances/{vm}"));
        let rb = self.request(Method::DELETE, &path).await?;
        match self.send("DELETE", &path, rb).await {
            Ok(_) => Ok(true),
            Err(GceError::Status { status: 404, .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Remove `keys` from an instance's metadata, keeping everything else
    /// (the startup script included). `Ok(false)` when the VM is gone.
    pub(crate) async fn scrub_metadata(
        &self,
        zone: &str,
        vm: &str,
        keys: &[&str],
    ) -> Result<bool, GceError> {
        let get = self.zone_path(zone, &format!("instances/{vm}"));
        let rb = self.request(Method::GET, &get).await?;
        let instance = match self.send("GET", &get, rb).await {
            Ok((_, v)) => v,
            Err(GceError::Status { status: 404, .. }) => return Ok(false),
            Err(e) => return Err(e),
        };
        let fingerprint = instance
            .pointer("/metadata/fingerprint")
            .and_then(Value::as_str)
            .ok_or_else(|| GceError::Decode(format!("{get}: no metadata fingerprint")))?;
        let items: Vec<Value> = instance
            .pointer("/metadata/items")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter(|i| {
                        i.get("key")
                            .and_then(Value::as_str)
                            .is_none_or(|k| !keys.contains(&k))
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let set = self.zone_path(zone, &format!("instances/{vm}/setMetadata"));
        let body = json!({"fingerprint": fingerprint, "items": items});
        let rb = self.request(Method::POST, &set).await?.json(&body);
        self.send("POST", &set, rb).await?;
        Ok(true)
    }
}

/// The label every burst VM carries, and its value.
const BURST_LABEL: &str = "borg-burst";
const BURST_LABEL_VALUE: &str = "queen";
/// The label naming the drone a burst VM was made for.
const DRONE_LABEL: &str = "borg-drone";

/// A burst VM as `instances.aggregatedList` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedVm {
    pub(crate) vm: String,
    pub(crate) zone: String,
    /// The `borg-drone` label; empty if the VM lacks it.
    pub(crate) drone: String,
    /// `creationTimestamp` in unix seconds, when GCE reported a parseable one.
    pub(crate) created_at: Option<i64>,
}

impl ListedVm {
    fn from_instance(instance: &Value) -> Option<Self> {
        // A VM already being deleted (`STOPPING` on its way out) is not
        // listed again: deleting it twice would only report a failure.
        if instance.get("status").and_then(Value::as_str) == Some("STOPPING") {
            return None;
        }
        let vm = instance.get("name")?.as_str()?.to_owned();
        let zone = instance
            .get("zone")?
            .as_str()?
            .rsplit('/')
            .next()?
            .to_owned();
        let drone = instance
            .pointer(&format!("/labels/{DRONE_LABEL}"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let created_at = instance
            .get("creationTimestamp")
            .and_then(Value::as_str)
            .and_then(|t| {
                time::OffsetDateTime::parse(t, &time::format_description::well_known::Rfc3339).ok()
            })
            .map(time::OffsetDateTime::unix_timestamp);
        Some(Self {
            vm,
            zone,
            drone,
            created_at,
        })
    }
}

/// The message of a finished operation's first error, if it has one.
fn operation_error(op: &Value) -> Option<String> {
    let errors = op.pointer("/error/errors")?.as_array()?;
    let first = errors.first()?;
    let code = first.get("code").and_then(Value::as_str).unwrap_or("error");
    let msg = first.get("message").and_then(Value::as_str).unwrap_or("");
    Some(format!("{code}: {msg}"))
}

/// The region a zone belongs to (`us-west1-b` -> `us-west1`).
fn region_of(zone: &str) -> &str {
    zone.rsplit_once('-').map_or(zone, |(r, _)| r)
}

/// What a burst drone needs at creation.
pub(crate) struct VmSpec<'a> {
    pub(crate) zone: &'a str,
    pub(crate) vm: &'a str,
    pub(crate) drone: &'a str,
    pub(crate) tailscale_authkey: &'a str,
    pub(crate) enroll_token: &'a str,
}

/// The `instances.insert` body for one burst VM: Spot (unless configured
/// otherwise), a hard maximum run time that deletes the VM, an ephemeral
/// external address on the burst network, no service account and no API
/// scopes, and the bootstrap script plus its secrets in metadata.
pub(crate) fn instance_body(gce: &GceSettings, spec: &VmSpec<'_>) -> Value {
    let (provisioning, restart) = if gce.spot {
        ("SPOT", false)
    } else {
        ("STANDARD", true)
    };
    json!({
        "name": spec.vm,
        "machineType": format!("zones/{}/machineTypes/{}", spec.zone, gce.machine_type),
        "scheduling": {
            "provisioningModel": provisioning,
            "instanceTerminationAction": "DELETE",
            "maxRunDuration": {"seconds": gce.max_run_secs.to_string()},
            "automaticRestart": restart,
            "onHostMaintenance": "TERMINATE",
        },
        "disks": [{
            "boot": true,
            "autoDelete": true,
            "initializeParams": {
                "sourceImage": format!(
                    "projects/{}/global/images/family/{}", gce.image_project, gce.image_family
                ),
                "diskSizeGb": gce.boot_disk_gb.to_string(),
                "diskType": format!("zones/{}/diskTypes/{}", spec.zone, gce.boot_disk_type),
            },
        }],
        "networkInterfaces": [{
            "network": format!("global/networks/{}", gce.network),
            "subnetwork": format!(
                "regions/{}/subnetworks/{}", region_of(spec.zone), gce.subnet
            ),
            "accessConfigs": [{"type": "ONE_TO_ONE_NAT", "name": "External NAT"}],
        }],
        "serviceAccounts": [],
        "labels": {BURST_LABEL: BURST_LABEL_VALUE, DRONE_LABEL: spec.drone},
        "metadata": {"items": [
            {"key": STARTUP_SCRIPT_KEY, "value": render_bootstrap(gce, spec.drone)},
            {"key": SECRET_METADATA_KEYS[0], "value": spec.tailscale_authkey},
            {"key": SECRET_METADATA_KEYS[1], "value": spec.enroll_token},
        ]},
    })
}

/// The startup script for drone `drone`: it joins the tailnet, fetches the
/// queen's own binary and the toolchain script from the queen, and installs
/// the drone. Secrets are read from metadata at run time, never inlined.
fn render_bootstrap(gce: &GceSettings, drone: &str) -> String {
    BOOTSTRAP
        .replace(
            "__QUEEN_URL__",
            gce.queen_url.as_deref().unwrap_or_default(),
        )
        .replace("__DRONE__", drone)
        .replace("__TAG__", &gce.tailscale_tag)
        .replace("__MAX_VCPU__", &gce.drone_max_vcpu.to_string())
        .replace("__MAX_MEM_MIB__", &gce.drone_max_mem_mib.to_string())
}

/// A POST with no body still states its length: Google's front end answers a
/// bodyless POST without `Content-Length` with HTTP 411 (seen on
/// `operations/{name}/wait` on 2026-10-24, after the VM had been created).
fn empty_post(rb: RequestBuilder) -> RequestBuilder {
    rb.header(reqwest::header::CONTENT_LENGTH, "0")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gce() -> GceSettings {
        serde_json::from_str(
            r#"{"project": "p", "network": "borg-burst", "subnet": "borg-burst-usw1",
                "zones": ["us-west1-b"], "machine_type": "c2d-highcpu-56",
                "queen_url": "https://queen.example.test", "drone_max_vcpu": 52,
                "drone_max_mem_mib": 104448}"#,
        )
        .unwrap()
    }

    #[test]
    fn bodyless_posts_state_a_zero_content_length() {
        let req = empty_post(
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap()
                .post("http://example.test/wait"),
        )
        .build()
        .unwrap();
        assert_eq!(req.headers()[reqwest::header::CONTENT_LENGTH], "0");
    }

    #[test]
    fn region_is_the_zone_without_its_letter() {
        assert_eq!(region_of("us-west1-b"), "us-west1");
        assert_eq!(region_of("europe-west4-a"), "europe-west4");
        assert_eq!(region_of("nozone"), "nozone");
    }

    #[test]
    fn instance_body_is_spot_self_deleting_credential_free_and_carries_bootstrap() {
        let g = gce();
        let body = instance_body(
            &g,
            &VmSpec {
                zone: "us-west1-b",
                vm: "borg-burst-q1",
                drone: "gcp-burst-q1",
                tailscale_authkey: "tskey-x",
                enroll_token: "tok-y",
            },
        );
        assert_eq!(body["name"], "borg-burst-q1");
        assert_eq!(
            body["machineType"],
            "zones/us-west1-b/machineTypes/c2d-highcpu-56"
        );
        assert_eq!(body["scheduling"]["provisioningModel"], "SPOT");
        assert_eq!(body["scheduling"]["instanceTerminationAction"], "DELETE");
        assert_eq!(body["scheduling"]["maxRunDuration"]["seconds"], "43200");
        assert_eq!(body["scheduling"]["automaticRestart"], false);
        assert_eq!(body["serviceAccounts"], json!([]));
        assert_eq!(
            body["disks"][0]["initializeParams"]["sourceImage"],
            "projects/ubuntu-os-cloud/global/images/family/ubuntu-2204-lts"
        );
        assert_eq!(body["disks"][0]["initializeParams"]["diskSizeGb"], "1000");
        assert_eq!(
            body["networkInterfaces"][0]["subnetwork"],
            "regions/us-west1/subnetworks/borg-burst-usw1"
        );
        assert_eq!(
            body["networkInterfaces"][0]["accessConfigs"][0]["type"],
            "ONE_TO_ONE_NAT"
        );
        let items = body["metadata"]["items"].as_array().unwrap();
        let item = |k: &str| {
            items
                .iter()
                .find(|i| i["key"] == k)
                .map(|i| i["value"].as_str().unwrap().to_owned())
                .unwrap()
        };
        assert_eq!(item("borg-ts-authkey"), "tskey-x");
        assert_eq!(item("borg-enroll-token"), "tok-y");
        let script = item("startup-script");
        assert!(script.starts_with("#!/bin/bash"));
        assert!(script.contains("QUEEN_URL='https://queen.example.test'"));
        assert!(script.contains("DRONE='gcp-burst-q1'"));
        assert!(script.contains("TAG='tag:borg-drone'"));
        assert!(script.contains("--max-vcpu \"$MAX_VCPU\""));
        assert!(script.contains("MAX_MEM_MIB='104448'"));
        assert!(
            !script.contains("tskey-x") && !script.contains("tok-y"),
            "secrets are read from metadata, never inlined"
        );
        assert!(!script.contains("__"), "every placeholder is rendered");
    }

    #[test]
    fn standard_provisioning_restarts_and_still_self_deletes() {
        let mut g = gce();
        g.spot = false;
        let body = instance_body(
            &g,
            &VmSpec {
                zone: "us-west1-b",
                vm: "v",
                drone: "d",
                tailscale_authkey: "k",
                enroll_token: "t",
            },
        );
        assert_eq!(body["scheduling"]["provisioningModel"], "STANDARD");
        assert_eq!(body["scheduling"]["automaticRestart"], true);
        assert_eq!(body["scheduling"]["instanceTerminationAction"], "DELETE");
    }

    #[test]
    fn operation_errors_are_reported_by_code_and_message() {
        let op = json!({"status": "DONE", "error": {"errors": [
            {"code": "ZONE_RESOURCE_POOL_EXHAUSTED", "message": "no capacity"}]}});
        assert_eq!(
            operation_error(&op).as_deref(),
            Some("ZONE_RESOURCE_POOL_EXHAUSTED: no capacity")
        );
        assert_eq!(operation_error(&json!({"status": "DONE"})), None);
    }
}
