//! The drone's HTTP client for the queen API.

use std::time::Duration;

use crate::api::{HeartbeatRequest, HeartbeatResponse, ResultRequest};
use crate::drone::config::DroneConfig;
use crate::secret::Secret;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("queen returned {status} for {path}")]
    Status { path: String, status: u16 },
    #[error("read CA certificate {path}: {source}")]
    CaRead {
        path: String,
        source: std::io::Error,
    },
    #[error("invalid CA certificate: {0}")]
    CaParse(reqwest::Error),
}

/// Upper bound for one bundle download.
const BUNDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// What the queen did with a unit log append.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogAppend {
    /// The queen now holds this many bytes (it may cap the log below what was
    /// sent).
    Stored(u64),
    /// The queen holds fewer bytes than the offset sent (409).
    Gap,
    /// The unit is not this drone's (404).
    NotOurs,
}

pub struct QueenClient {
    http: reqwest::Client,
    base: String,
    token: Secret,
}

impl QueenClient {
    /// With `ca_cert_path` set, that CA is the only trust root (system roots
    /// are disabled), which pins the queen's CA.
    pub fn new(cfg: &DroneConfig, token: Secret) -> Result<Self, ClientError> {
        let mut builder = reqwest::Client::builder()
            .user_agent("borg-drone")
            .timeout(Duration::from_secs(30));
        if let Some(path) = &cfg.ca_cert_path {
            let pem = std::fs::read(path).map_err(|source| ClientError::CaRead {
                path: path.display().to_string(),
                source,
            })?;
            let cert = reqwest::Certificate::from_pem(&pem).map_err(ClientError::CaParse)?;
            builder = builder.tls_certs_only([cert]);
        }
        Ok(Self {
            http: builder.build()?,
            base: cfg.queen_url.trim_end_matches('/').to_owned(),
            token,
        })
    }

    pub async fn heartbeat(
        &self,
        req: &HeartbeatRequest,
    ) -> Result<HeartbeatResponse, ClientError> {
        let path = "/v1/heartbeat";
        let resp = self
            .http
            .post(format!("{}{path}", self.base))
            .bearer_auth(self.token.expose())
            .json(req)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(ClientError::Status {
                path: path.to_owned(),
                status: resp.status().as_u16(),
            });
        }
        Ok(resp.json().await?)
    }

    /// `POST /v1/units/{id}/log?offset=N`: append `data` at `offset`.
    pub async fn append_unit_log(
        &self,
        unit_id: &str,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<LogAppend, ClientError> {
        let path = format!("/v1/units/{unit_id}/log");
        let resp = self
            .http
            .post(format!("{}{path}", self.base))
            .bearer_auth(self.token.expose())
            .query(&[("offset", offset)])
            .body(data)
            .send()
            .await?;
        match resp.status().as_u16() {
            200..=299 => {
                let body = resp.text().await?;
                body.trim()
                    .parse()
                    .map(LogAppend::Stored)
                    .map_err(|_| ClientError::Status { path, status: 200 })
            }
            409 => Ok(LogAppend::Gap),
            404 => Ok(LogAppend::NotOurs),
            status => Err(ClientError::Status { path, status }),
        }
    }

    /// `GET /v1/units/{id}/bundle`: the client's git bundle for a tree that
    /// is on no remote. Refused when larger than `max_bytes`.
    pub async fn unit_bundle(&self, unit_id: &str, max_bytes: u64) -> Result<Vec<u8>, ClientError> {
        let path = format!("/v1/units/{unit_id}/bundle");
        let mut resp = self
            .http
            .get(format!("{}{path}", self.base))
            .bearer_auth(self.token.expose())
            // Bundles are bounded by the queen's `max_bundle_bytes` (64 MiB
            // by default), more than the default request timeout allows for
            // on a slow link.
            .timeout(BUNDLE_TIMEOUT)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(ClientError::Status {
                path,
                status: resp.status().as_u16(),
            });
        }
        let too_large = || ClientError::Status {
            path: format!("{path} (over {max_bytes} bytes)"),
            status: 413,
        };
        if resp.content_length().is_some_and(|n| n > max_bytes) {
            return Err(too_large());
        }
        let mut data = Vec::new();
        while let Some(chunk) = resp.chunk().await? {
            data.extend_from_slice(&chunk);
            if u64::try_from(data.len()).unwrap_or(u64::MAX) > max_bytes {
                return Err(too_large());
            }
        }
        Ok(data)
    }

    /// `POST /v1/units/{id}/result`. `Ok(false)`: the unit is no longer this
    /// drone's (re-queued or gone), so retrying is pointless.
    pub async fn report_unit(
        &self,
        unit_id: &str,
        result: &crate::units_api::UnitResult,
    ) -> Result<bool, ClientError> {
        let path = format!("/v1/units/{unit_id}/result");
        let resp = self
            .http
            .post(format!("{}{path}", self.base))
            .bearer_auth(self.token.expose())
            .json(result)
            .send()
            .await?;
        match resp.status().as_u16() {
            200..=299 => Ok(true),
            404 => Ok(false),
            status => Err(ClientError::Status { path, status }),
        }
    }

    pub async fn report_result(
        &self,
        lease_id: &str,
        result: &ResultRequest,
    ) -> Result<(), ClientError> {
        let path = format!("/v1/leases/{lease_id}/result");
        let resp = self
            .http
            .post(format!("{}{path}", self.base))
            .bearer_auth(self.token.expose())
            .json(result)
            .send()
            .await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(ClientError::Status {
                path,
                status: resp.status().as_u16(),
            })
        }
    }
}
