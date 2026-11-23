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
    /// of capacity).
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
        template: &str,
        body: &Value,
    ) -> Result<Option<String>, GceError> {
        let path = self.zone_path(zone, "instances");
        let rb = self
            .request(Method::POST, &path)
            .await?
            .query(&[("sourceInstanceTemplate", template)])
            .json(body);
        let (_, op) = self.send("POST", &path, rb).await?;
        if let Some(e) = operation_error(&op) {
            return Err(GceError::Operation(e));
        }
        Ok(op.get("name").and_then(Value::as_str).map(str::to_owned))
    }

    /// The instance template every burst VM is created from, as the
    /// reference to pass to `insert_instance` and the template itself for
    /// `instance_body`. Every burst VM comes from it: with none configured,
    /// or one that cannot be read, this is an error and the caller creates
    /// nothing; the queen never builds a host from its own settings. Needs
    /// `compute.instanceTemplates.get`.
    pub(crate) async fn burst_template(
        &self,
        gce: &GceSettings,
    ) -> Result<(String, Value), String> {
        let name = gce
            .instance_template
            .as_deref()
            .ok_or("gce.instance_template is not set; burst VMs are created only from it")?;
        let path = instance_template_path(name)
            .ok_or("gce.instance_template is not an instance template name")?;
        let template = async {
            let rb = self.request(Method::GET, &path).await?;
            self.send("GET", &path, rb).await
        }
        .await
        .map_err(|e: GceError| {
            let why: String = e.to_string().chars().take(300).collect();
            format!("instance template {name}: {why}")
        })?
        .1;
        template_matches_plan(&template, gce)
            .map_err(|why| format!("instance template {name}: {why}; no VM was created"))?;
        Ok((path.trim_start_matches('/').to_owned(), template))
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

/// What a burst drone needs at creation.
pub(crate) struct VmSpec<'a> {
    pub(crate) vm: &'a str,
    pub(crate) drone: &'a str,
    pub(crate) tailscale_authkey: &'a str,
    pub(crate) enroll_token: &'a str,
}

/// The API path of an instance template named as
/// `projects/<project>/global/instanceTemplates/<name>`, with or without the
/// `https://www.googleapis.com/compute/v1/` prefix, or as the regional form
/// `projects/<project>/regions/<region>/instanceTemplates/<name>`. `None`
/// when `template` is neither.
pub(crate) fn instance_template_path(template: &str) -> Option<String> {
    let rest = template
        .strip_prefix("https://www.googleapis.com/compute/v1/")
        .unwrap_or(template);
    let parts: Vec<&str> = rest.split('/').collect();
    let label = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    let ok = match parts.as_slice() {
        ["projects", project, "global", "instanceTemplates", name] => label(project) && label(name),
        [
            "projects",
            project,
            "regions",
            region,
            "instanceTemplates",
            name,
        ] => label(project) && label(region) && label(name),
        _ => false,
    };
    ok.then(|| format!("/{rest}"))
}

/// Whether the template gives a VM the lifetime the queen plans with. The
/// template owns scheduling, but the planner retires drones, counts drone
/// hours and stops tracking a VM by `gce.max_run_secs`: a template that
/// deletes sooner would cut running jobs off, and one that deletes later, or
/// never, would leave a VM running that the queen no longer counts or
/// deletes. So the template must be STANDARD, delete on termination, and run
/// for exactly `max_run_secs`. Checked before a drone is enrolled.
pub(crate) fn template_matches_plan(template: &Value, gce: &GceSettings) -> Result<(), String> {
    let scheduling = template.pointer("/properties/scheduling");
    let field = |name: &str| scheduling.and_then(|s| s.get(name)).and_then(Value::as_str);
    if let Some(model) = field("provisioningModel")
        && model != "STANDARD"
    {
        return Err(format!(
            "provisioningModel is {model}, and burst VMs are STANDARD only"
        ));
    }
    if scheduling.and_then(|s| s.get("preemptible")) == Some(&Value::Bool(true)) {
        return Err("it is preemptible, and burst VMs are STANDARD only".to_owned());
    }
    match field("instanceTerminationAction") {
        Some("DELETE") => {}
        other => {
            return Err(format!(
                "instanceTerminationAction is {}, and a burst VM must be deleted at the end of its run",
                other.unwrap_or("not set")
            ));
        }
    }
    // The API writes int64 as a string; accept a number too.
    let seconds = scheduling
        .and_then(|s| s.pointer("/maxRunDuration/seconds"))
        .and_then(|v| match v {
            Value::String(s) => s.parse::<i64>().ok(),
            other => other.as_i64(),
        });
    match seconds {
        Some(s) if s == gce.max_run_secs => Ok(()),
        Some(s) => Err(format!(
            "maxRunDuration is {s}s but gce.max_run_secs is {}s; they must be equal, because the queen retires and counts drones by max_run_secs",
            gce.max_run_secs
        )),
        None => Err(format!(
            "it has no maxRunDuration; it must be {}s (gce.max_run_secs) so no VM outlives the queen's record of it",
            gce.max_run_secs
        )),
    }
}

/// The `instances.insert` body for one burst VM made from an instance
/// template (`sourceInstanceTemplate`). The template decides everything about
/// the host: provisioning model, machine type, disk, image, network, address
/// and identity. The body carries only what differs per VM: its name, and the
/// template's labels and metadata with the queen's own added. Labels and
/// metadata are sent whole because a field given in the body replaces the
/// template's field; `template` is the template as `instanceTemplates.get`
/// returned it. A queen key wins over a template key of the same name.
pub(crate) fn instance_body(gce: &GceSettings, spec: &VmSpec<'_>, template: &Value) -> Value {
    let mut labels = template
        .pointer("/properties/labels")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    labels.insert(BURST_LABEL.to_owned(), json!(BURST_LABEL_VALUE));
    labels.insert(DRONE_LABEL.to_owned(), json!(spec.drone));

    let ours = [
        (STARTUP_SCRIPT_KEY, render_bootstrap(gce, spec.drone)),
        (SECRET_METADATA_KEYS[0], spec.tailscale_authkey.to_owned()),
        (SECRET_METADATA_KEYS[1], spec.enroll_token.to_owned()),
    ];
    let mut items: Vec<Value> = template
        .pointer("/properties/metadata/items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| {
            let key = item.get("key").and_then(Value::as_str);
            !ours.iter().any(|(k, _)| Some(*k) == key)
        })
        .cloned()
        .collect();
    items.extend(
        ours.iter()
            .map(|(key, value)| json!({"key": key, "value": value})),
    );

    json!({
        "name": spec.vm,
        "labels": labels,
        "metadata": {"items": items},
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
            r#"{"project": "p", "network": "borg-burst", "subnet": "borg-burst-usc1",
                "zones": ["us-central1-b"], "machine_type": "c2d-highcpu-56",
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

    fn template() -> Value {
        json!({
            "name": "borg-burst-v1",
            "properties": {
                "machineType": "c2d-highcpu-56",
                "labels": {"owner": "platform", "purpose": "borg-burst-drone", "lane": "burst"},
                "metadata": {"items": [
                    {"key": "enable-oslogin", "value": "TRUE"},
                    {"key": "block-project-ssh-keys", "value": "TRUE"},
                    {"key": "startup-script", "value": "echo from the template"},
                ]},
                "scheduling": {"provisioningModel": "STANDARD"},
            }
        })
    }

    fn spec() -> VmSpec<'static> {
        VmSpec {
            vm: "borg-burst-q1",
            drone: "gcp-burst-q1",
            tailscale_authkey: "tskey-x",
            enroll_token: "tok-y",
        }
    }

    #[test]
    fn instance_body_names_the_vm_and_leaves_the_host_to_the_template() {
        let body = instance_body(&gce(), &spec(), &template());
        let mut keys: Vec<&str> = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["labels", "metadata", "name"],
            "nothing that shapes the host (machine type, disk, image, network, scheduling, identity) is sent"
        );
        assert_eq!(body["name"], "borg-burst-q1");
    }

    #[test]
    fn instance_body_keeps_template_labels_and_metadata_and_adds_the_queens() {
        let body = instance_body(&gce(), &spec(), &template());
        assert_eq!(
            body["labels"],
            json!({"owner": "platform", "purpose": "borg-burst-drone", "lane": "burst",
                   "borg-burst": "queen", "borg-drone": "gcp-burst-q1"})
        );
        let items = body["metadata"]["items"].as_array().unwrap();
        let item = |k: &str| {
            let found: Vec<&Value> = items.iter().filter(|i| i["key"] == k).collect();
            assert_eq!(found.len(), 1, "{k} appears once");
            found[0]["value"].as_str().unwrap().to_owned()
        };
        assert_eq!(item("enable-oslogin"), "TRUE");
        assert_eq!(item("block-project-ssh-keys"), "TRUE");
        assert_eq!(item("borg-ts-authkey"), "tskey-x");
        assert_eq!(item("borg-enroll-token"), "tok-y");
        let script = item("startup-script");
        assert!(
            script.starts_with("#!/bin/bash"),
            "the queen's bootstrap replaces a template startup script"
        );
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
    fn instance_body_tolerates_a_template_without_labels_or_metadata() {
        let body = instance_body(&gce(), &spec(), &json!({"properties": {}}));
        assert_eq!(
            body["labels"],
            json!({"borg-burst": "queen", "borg-drone": "gcp-burst-q1"})
        );
        assert_eq!(body["metadata"]["items"].as_array().unwrap().len(), 3);
    }

    fn template_with_scheduling(scheduling: Value) -> Value {
        json!({"properties": {"scheduling": scheduling}})
    }

    #[test]
    fn a_template_with_the_planned_lifetime_is_accepted() {
        let g = gce();
        assert_eq!(g.max_run_secs, 43_200);
        for seconds in [json!("43200"), json!(43200)] {
            let t = template_with_scheduling(json!({
                "provisioningModel": "STANDARD", "instanceTerminationAction": "DELETE",
                "maxRunDuration": {"seconds": seconds}}));
            assert_eq!(template_matches_plan(&t, &g), Ok(()));
        }
    }

    #[test]
    fn a_template_whose_lifetime_differs_from_the_plan_is_refused() {
        let g = gce();
        let with = |seconds: &str| {
            template_with_scheduling(json!({
                "provisioningModel": "STANDARD", "instanceTerminationAction": "DELETE",
                "maxRunDuration": {"seconds": seconds}}))
        };
        // Shorter: GCE would delete the VM under a running job before the
        // queen retires it.
        let shorter = template_matches_plan(&with("21600"), &g).unwrap_err();
        assert!(
            shorter.contains("21600s") && shorter.contains("43200s"),
            "{shorter}"
        );
        // Longer: the VM would outlive the queen's record of it.
        let longer = template_matches_plan(&with("86400"), &g).unwrap_err();
        assert!(longer.contains("86400s"), "{longer}");
        // Absent: nothing would ever delete it.
        let absent = template_matches_plan(
            &template_with_scheduling(
                json!({"provisioningModel": "STANDARD", "instanceTerminationAction": "DELETE"}),
            ),
            &g,
        )
        .unwrap_err();
        assert!(absent.contains("no maxRunDuration"), "{absent}");
        let none = template_matches_plan(&json!({"properties": {}}), &g).unwrap_err();
        assert!(
            none.contains("instanceTerminationAction is not set"),
            "{none}"
        );
    }

    #[test]
    fn a_template_that_stops_instead_of_deleting_or_is_not_standard_is_refused() {
        let g = gce();
        let stop = template_matches_plan(
            &template_with_scheduling(json!({
                "provisioningModel": "STANDARD", "instanceTerminationAction": "STOP",
                "maxRunDuration": {"seconds": "43200"}})),
            &g,
        )
        .unwrap_err();
        assert!(stop.contains("instanceTerminationAction is STOP"), "{stop}");
        for scheduling in [
            json!({"provisioningModel": "SPOT", "instanceTerminationAction": "DELETE",
                   "maxRunDuration": {"seconds": "43200"}}),
            json!({"preemptible": true, "instanceTerminationAction": "DELETE",
                   "maxRunDuration": {"seconds": "43200"}}),
        ] {
            let e = template_matches_plan(&template_with_scheduling(scheduling), &g).unwrap_err();
            assert!(e.contains("STANDARD only"), "{e}");
        }
    }

    #[test]
    fn instance_template_path_accepts_only_template_names() {
        let p = "projects/example-project/global/instanceTemplates/borg-burst-v1";
        assert_eq!(
            instance_template_path(p).as_deref(),
            Some(&*format!("/{p}"))
        );
        assert_eq!(
            instance_template_path(&format!("https://www.googleapis.com/compute/v1/{p}"))
                .as_deref(),
            Some(&*format!("/{p}"))
        );
        assert_eq!(
            instance_template_path("projects/p/regions/us-central1/instanceTemplates/t").as_deref(),
            Some("/projects/p/regions/us-central1/instanceTemplates/t")
        );
        for bad in [
            "",
            "borg-burst-v1",
            "global/instanceTemplates/borg-burst-v1",
            "projects/p/global/instanceTemplates/",
            "projects/p/global/instanceTemplates/a/b",
            "projects/p/global/instanceTemplates/a?x=1",
            "projects/p/zones/us-central1-a/instances/vm",
            "https://example.test/projects/p/global/instanceTemplates/t",
        ] {
            assert_eq!(instance_template_path(bad), None, "{bad}");
        }
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
