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
            builder = builder
                .tls_built_in_root_certs(false)
                .add_root_certificate(cert);
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
