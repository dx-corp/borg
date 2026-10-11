//! Zero-touch drone enrollment for hosts on a Tailscale tailnet.
//!
//! `POST /v1/enroll` is disabled unless `tailnet_enroll` is configured. When it
//! is, the queen asks an [`IdentityVerifier`] who owns the caller's network
//! address. The production verifier ([`TailscaledWhois`]) asks the local
//! `tailscaled` over its Unix socket (`/localapi/v0/whois`), which answers from
//! the WireGuard keys of the tailnet, so the identity cannot be forged by the
//! caller. If the node carries an allowed tag, or (for an untagged node) its
//! user is an allowed user, the queen enrolls a drone named after the node and
//! returns a fresh token, exactly like `borg queen enroll`.
//!
//! Requirements and limits:
//! * The caller's address must be the one the queen sees. Bind the queen to its
//!   tailnet address and do not put a proxy in front of `/v1/enroll`: a proxy's
//!   address is the proxy's identity.
//! * The queen's service user must be allowed to use the tailscaled socket
//!   (`tailscale set --operator=<user>`).
//! * A node name that is already enrolled is refused (409). There is no rotate
//!   yet; delete the drone first.
//! * A tagged node is authorized by its tags only; its "user" is the tag owner
//!   pseudo-user and is ignored.

use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use serde::Deserialize;

use crate::enroll::valid_drone_name;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Who a tailnet address belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailnetIdentity {
    /// The node's MagicDNS name, e.g. `host-a.example.invalid.`.
    pub node_name: String,
    /// ACL tags on the node, e.g. `tag:ci`.
    pub tags: Vec<String>,
    /// Login of the owning user; meaningful only for untagged nodes.
    pub user: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum WhoisError {
    #[error("tailscaled socket: {0}")]
    Io(String),
    #[error("tailscaled answered {0}")]
    Status(u16),
    #[error("tailscaled sent an unusable answer: {0}")]
    Decode(String),
}

/// Resolves a caller's address to a tailnet identity. Pluggable so tests can
/// use [`StaticIdentities`].
pub trait IdentityVerifier: Send + Sync {
    /// `Ok(None)`: the address is not a known tailnet node.
    fn whois(&self, addr: SocketAddr)
    -> BoxFuture<'_, Result<Option<TailnetIdentity>, WhoisError>>;
}

/// A fixed table, for tests.
#[derive(Debug, Default, Clone)]
pub struct StaticIdentities(pub HashMap<IpAddr, TailnetIdentity>);

impl IdentityVerifier for StaticIdentities {
    fn whois(
        &self,
        addr: SocketAddr,
    ) -> BoxFuture<'_, Result<Option<TailnetIdentity>, WhoisError>> {
        let found = self.0.get(&addr.ip()).cloned();
        Box::pin(async move { Ok(found) })
    }
}

/// Allowlist from the queen config.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TailnetEnrollSettings {
    #[serde(default = "default_socket")]
    pub socket_path: PathBuf,
    /// Tags whose nodes may enroll, e.g. `tag:ci`.
    #[serde(default)]
    pub allowed_tags: Vec<String>,
    /// Logins of users whose untagged nodes may enroll.
    #[serde(default)]
    pub allowed_users: Vec<String>,
}

fn default_socket() -> PathBuf {
    PathBuf::from("/var/run/tailscale/tailscaled.sock")
}

impl TailnetEnrollSettings {
    pub fn validate(&self) -> Result<(), String> {
        if self.allowed_tags.is_empty() && self.allowed_users.is_empty() {
            return Err("tailnet_enroll needs allowed_tags or allowed_users".to_owned());
        }
        if self.allowed_tags.iter().any(|t| !t.starts_with("tag:")) {
            return Err("tailnet_enroll allowed_tags entries look like tag:name".to_owned());
        }
        Ok(())
    }

    /// Whether `id` is on the allowlist.
    pub fn allows(&self, id: &TailnetIdentity) -> bool {
        if !id.tags.is_empty() {
            return id.tags.iter().any(|t| self.allowed_tags.contains(t));
        }
        id.user
            .as_ref()
            .is_some_and(|u| self.allowed_users.iter().any(|a| a.eq_ignore_ascii_case(u)))
    }
}

/// The drone name for a node: its first DNS label, lowercased, with anything
/// outside the drone-name alphabet turned into `-`. `None` if nothing usable
/// is left.
pub fn drone_name_for(node_name: &str) -> Option<String> {
    let label = node_name.trim_end_matches('.').split('.').next()?;
    let mut name: String = label
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else {
                '-'
            }
        })
        .collect();
    name = name.trim_start_matches('-').to_owned();
    name.truncate(40);
    (valid_drone_name(&name)).then_some(name)
}

/// What the queen needs to serve `/v1/enroll`.
#[derive(Clone)]
pub struct TailnetEnroll {
    pub verifier: Arc<dyn IdentityVerifier>,
    pub settings: TailnetEnrollSettings,
}

// ---------------------------------------------------------------- tailscaled

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct WhoisReply {
    node: Option<WhoisNode>,
    user_profile: Option<WhoisUser>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct WhoisNode {
    #[serde(default)]
    name: String,
    #[serde(default)]
    tags: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct WhoisUser {
    #[serde(default)]
    login_name: String,
}

/// Parse the body of `GET /localapi/v0/whois`.
pub fn parse_whois(body: &[u8]) -> Result<Option<TailnetIdentity>, WhoisError> {
    let reply: WhoisReply =
        serde_json::from_slice(body).map_err(|e| WhoisError::Decode(e.to_string()))?;
    let Some(node) = reply.node else {
        return Ok(None);
    };
    if node.name.is_empty() {
        return Ok(None);
    }
    Ok(Some(TailnetIdentity {
        node_name: node.name,
        tags: node.tags.unwrap_or_default(),
        user: reply
            .user_profile
            .map(|u| u.login_name)
            .filter(|l| !l.is_empty()),
    }))
}

/// Asks the local tailscaled over its Unix socket.
pub struct TailscaledWhois {
    socket: PathBuf,
}

impl TailscaledWhois {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }
}

const MAX_REPLY_BYTES: u64 = 1 << 20;
const WHOIS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

impl IdentityVerifier for TailscaledWhois {
    fn whois(
        &self,
        addr: SocketAddr,
    ) -> BoxFuture<'_, Result<Option<TailnetIdentity>, WhoisError>> {
        Box::pin(async move {
            tokio::time::timeout(WHOIS_TIMEOUT, self.query(addr))
                .await
                .map_err(|_| WhoisError::Io("timed out".to_owned()))?
        })
    }
}

impl TailscaledWhois {
    #[cfg(unix)]
    async fn query(&self, addr: SocketAddr) -> Result<Option<TailnetIdentity>, WhoisError> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let io = |e: std::io::Error| WhoisError::Io(e.to_string());
        let mut stream = tokio::net::UnixStream::connect(&self.socket)
            .await
            .map_err(io)?;
        // HTTP/1.0 so the reply is neither chunked nor kept alive.
        let request = format!(
            "GET /localapi/v0/whois?addr={addr} HTTP/1.0\r\nHost: local-tailscaled.sock\r\nSec-Tailscale: localapi\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await.map_err(io)?;
        let mut raw = Vec::new();
        stream
            .take(MAX_REPLY_BYTES)
            .read_to_end(&mut raw)
            .await
            .map_err(io)?;
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .ok_or_else(|| WhoisError::Decode("no header end".to_owned()))?;
        let head = String::from_utf8_lossy(&raw[..split]);
        let status: u16 = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| WhoisError::Decode("bad status line".to_owned()))?;
        match status {
            200 => parse_whois(&raw[split + 4..]),
            // tailscaled answers 404 for an address that is not a tailnet node.
            404 => Ok(None),
            other => Err(WhoisError::Status(other)),
        }
    }

    #[cfg(not(unix))]
    async fn query(&self, _addr: SocketAddr) -> Result<Option<TailnetIdentity>, WhoisError> {
        let _ = (&self.socket, MAX_REPLY_BYTES);
        Err(WhoisError::Io("unix sockets are not available".to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(tags: &[&str], users: &[&str]) -> TailnetEnrollSettings {
        TailnetEnrollSettings {
            socket_path: default_socket(),
            allowed_tags: tags.iter().map(|s| (*s).to_owned()).collect(),
            allowed_users: users.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn id(tags: &[&str], user: Option<&str>) -> TailnetIdentity {
        TailnetIdentity {
            node_name: "host-a.example.invalid.".to_owned(),
            tags: tags.iter().map(|s| (*s).to_owned()).collect(),
            user: user.map(str::to_owned),
        }
    }

    #[test]
    fn tags_authorize_tagged_nodes_and_users_only_untagged_ones() {
        let s = settings(&["tag:ci"], &["dev@example.test"]);
        assert!(s.allows(&id(&["tag:ci"], None)));
        assert!(s.allows(&id(&["tag:other", "tag:ci"], None)));
        assert!(!s.allows(&id(&["tag:other"], Some("dev@example.test"))));
        assert!(s.allows(&id(&[], Some("Dev@Example.test"))));
        assert!(!s.allows(&id(&[], Some("other@example.test"))));
        assert!(!s.allows(&id(&[], None)));
    }

    #[test]
    fn settings_need_an_allowlist_of_well_formed_tags() {
        assert!(settings(&[], &[]).validate().is_err());
        assert!(settings(&["ci"], &[]).validate().is_err());
        assert!(settings(&["tag:ci"], &[]).validate().is_ok());
        assert!(settings(&[], &["a@b.test"]).validate().is_ok());
    }

    #[test]
    fn drone_names_come_from_the_first_dns_label() {
        assert_eq!(
            drone_name_for("Host_A.tail.example.invalid.").as_deref(),
            Some("host-a")
        );
        assert_eq!(drone_name_for("host-a").as_deref(), Some("host-a"));
        assert_eq!(drone_name_for("_x.example.invalid.").as_deref(), Some("x"));
        assert_eq!(drone_name_for("---.example.invalid."), None);
        assert_eq!(drone_name_for(""), None);
        assert_eq!(
            drone_name_for(&format!("{}.example.invalid", "a".repeat(80))).map(|n| n.len()),
            Some(40)
        );
    }

    #[test]
    fn whois_replies_parse_for_tagged_user_and_unknown_nodes() {
        let tagged = br#"{"Node":{"Name":"host-a.example.invalid.","Tags":["tag:ci"]},
            "UserProfile":{"LoginName":"tagged-devices"}}"#;
        let t = parse_whois(tagged).unwrap().unwrap();
        assert_eq!(
            (t.node_name.as_str(), t.tags.as_slice()),
            ("host-a.example.invalid.", ["tag:ci".to_owned()].as_slice())
        );
        let user = br#"{"Node":{"Name":"laptop.example.invalid.","Tags":null},
            "UserProfile":{"LoginName":"dev@example.test"}}"#;
        let u = parse_whois(user).unwrap().unwrap();
        assert!(u.tags.is_empty());
        assert_eq!(u.user.as_deref(), Some("dev@example.test"));
        assert_eq!(parse_whois(b"{}").unwrap(), None);
        assert!(parse_whois(b"not json").is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_tailscaled_client_speaks_http_over_the_unix_socket() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("ts.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            for answer in ["ok", "unknown", "broken"] {
                let (mut conn, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 4096];
                let n = conn.read(&mut buf).await.unwrap();
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                assert!(
                    req.starts_with("GET /localapi/v0/whois?addr=100.64.0.7:5555 HTTP/1.0\r\n"),
                    "{req}"
                );
                let reply = match answer {
                    "ok" => "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n{\"Node\":{\"Name\":\"n.example.invalid.\",\"Tags\":[\"tag:ci\"]}}".to_owned(),
                    "unknown" => "HTTP/1.0 404 Not Found\r\n\r\nno match".to_owned(),
                    _ => "HTTP/1.0 500 Oops\r\n\r\n".to_owned(),
                };
                conn.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        let v = TailscaledWhois::new(&sock);
        let addr: SocketAddr = "100.64.0.7:5555".parse().unwrap();
        let found = v.whois(addr).await.unwrap().unwrap();
        assert_eq!(found.tags, ["tag:ci"]);
        assert_eq!(v.whois(addr).await.unwrap(), None);
        assert!(matches!(v.whois(addr).await, Err(WhoisError::Status(500))));
        // No daemon listening at all is an error, not "unknown node".
        let gone = TailscaledWhois::new(dir.path().join("missing.sock"));
        assert!(matches!(gone.whois(addr).await, Err(WhoisError::Io(_))));
    }
}
