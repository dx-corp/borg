//! `borg config fetch`: read one file of a private repository at a git ref
//! with the queen's existing contents-read App (`mirror_auth`, else the
//! janitor's `compare_auth`), so the queen config can be delivered without
//! publishing it and without a new identity. Tokens are minted per call,
//! limited to `contents: read` on that one repository, and never logged.

use std::path::Path;

use crate::config::{MirrorAuthSettings, QueenConfig};
use crate::github::GithubClient;
use crate::mirror_auth::MirrorAuth;

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error(
        "the queen config has no contents-read App (mirror_auth or supersede_janitor.compare_auth)"
    )]
    NoApp,
    #[error("{0}")]
    Config(#[from] crate::config::ConfigError),
    #[error("{0}")]
    Github(#[from] crate::github::GithubError),
    #[error("fetch {path}@{git_ref}: {source}")]
    Http {
        path: String,
        git_ref: String,
        source: reqwest::Error,
    },
    #[error("fetch {path}@{git_ref}: GitHub answered {status}")]
    Status {
        path: String,
        git_ref: String,
        status: u16,
    },
}

/// The App to read with: `mirror_auth`, else the janitor's `compare_auth`.
fn app(cfg: &QueenConfig) -> Option<&MirrorAuthSettings> {
    cfg.mirror_auth.as_ref().or_else(|| {
        cfg.supersede_janitor
            .as_ref()
            .and_then(|j| j.compare_auth.as_ref())
    })
}

/// The bytes of `path` in `<org>/<repo>` at `git_ref`.
pub async fn fetch(
    cfg: &QueenConfig,
    repo: &str,
    git_ref: &str,
    path: &str,
) -> Result<Vec<u8>, FetchError> {
    let settings = app(cfg).ok_or(FetchError::NoApp)?;
    let pem = settings.private_key(std::env::var("BORG_MIRROR_APP_PRIVATE_KEY").ok())?;
    let client = GithubClient::new(
        &cfg.github.api_base,
        &cfg.github.org,
        &settings.app_id,
        &pem,
    )?;
    let credential = MirrorAuth::new(client, &cfg.github.org, &[repo.to_owned()])
        .credential()
        .await?;
    let url = format!(
        "{}/repos/{}/{repo}/contents/{path}",
        cfg.github.api_base.trim_end_matches('/'),
        cfg.github.org
    );
    let http_err = |source| FetchError::Http {
        path: path.to_owned(),
        git_ref: git_ref.to_owned(),
        source,
    };
    let response = reqwest::Client::builder()
        .user_agent("borg-queen")
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(http_err)?
        .get(url)
        .query(&[("ref", git_ref)])
        .bearer_auth(credential.token.expose())
        .header("Accept", "application/vnd.github.raw")
        .send()
        .await
        .map_err(http_err)?;
    if !response.status().is_success() {
        return Err(FetchError::Status {
            path: path.to_owned(),
            git_ref: git_ref.to_owned(),
            status: response.status().as_u16(),
        });
    }
    Ok(response.bytes().await.map_err(http_err)?.to_vec())
}

/// Write `bytes` to `out` atomically (temporary file, then rename), 0644.
pub fn write_atomic(out: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let tmp = out.with_extension("fetch-tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644))?;
    std::fs::rename(&tmp, out)
}
