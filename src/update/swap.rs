//! Downloading a verified binary and swapping it in atomically.
//!
//! Streaming the body into a temporary file next to the destination and
//! renaming it over the target is ported from flotilla's `put_file`
//! (`crates/flotillad/src/server.rs`): a running binary keeps its old inode and
//! readers never see a partial file. The digest is checked before anything is
//! renamed into place.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

#[derive(Debug, thiserror::Error)]
pub enum SwapError {
    #[error("download {url}: {source}")]
    Download { url: String, source: reqwest::Error },
    #[error("download {url} returned {status}")]
    Status { url: String, status: u16 },
    #[error("checksum mismatch for {url}: expected {expected}, got {actual}")]
    Checksum {
        url: String,
        expected: String,
        actual: String,
    },
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("there is no previous binary to roll back to at {0}")]
    NoPrevious(PathBuf),
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> SwapError + '_ {
    move |source| SwapError::Io {
        path: path.to_owned(),
        source,
    }
}

/// `<dir>/borg` -> `<dir>/borg.new`
pub fn new_path(bin: &Path) -> PathBuf {
    sibling(bin, "new")
}

/// `<dir>/borg` -> `<dir>/borg.prev`
pub fn prev_path(bin: &Path) -> PathBuf {
    sibling(bin, "prev")
}

fn sibling(bin: &Path, suffix: &str) -> PathBuf {
    let name = bin
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    bin.with_file_name(format!("{name}.{suffix}"))
}

/// Download `url` to `dest` (mode 0755), verifying its SHA-256 against
/// `expected_sha256`. Nothing is left at `dest` unless the digest matches.
pub async fn download_verified(
    url: &str,
    expected_sha256: &str,
    dest: &Path,
    allow_insecure: bool,
) -> Result<(), SwapError> {
    let scheme_ok = url.starts_with("https://") || (allow_insecure && url.starts_with("http://"));
    if !scheme_ok {
        return Err(SwapError::Status {
            url: url.to_owned(),
            status: 0,
        });
    }
    let client = reqwest::Client::builder()
        .user_agent("borg-update")
        .timeout(Duration::from_secs(900))
        .build()
        .map_err(|source| SwapError::Download {
            url: url.to_owned(),
            source,
        })?;
    let mut resp = client
        .get(url)
        .send()
        .await
        .map_err(|source| SwapError::Download {
            url: url.to_owned(),
            source,
        })?;
    if !resp.status().is_success() {
        return Err(SwapError::Status {
            url: url.to_owned(),
            status: resp.status().as_u16(),
        });
    }
    let tmp = sibling(dest, "download");
    let result: Result<String, SwapError> = async {
        let mut file = tokio::fs::File::create(&tmp).await.map_err(io_err(&tmp))?;
        let mut hasher = Sha256::new();
        while let Some(chunk) = resp.chunk().await.map_err(|source| SwapError::Download {
            url: url.to_owned(),
            source,
        })? {
            hasher.update(&chunk);
            file.write_all(&chunk).await.map_err(io_err(&tmp))?;
        }
        file.flush().await.map_err(io_err(&tmp))?;
        Ok(hex::encode(hasher.finalize()))
    }
    .await;
    let actual = match result {
        Ok(actual) => actual,
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
    };
    if actual != expected_sha256 {
        let _ = fs::remove_file(&tmp);
        return Err(SwapError::Checksum {
            url: url.to_owned(),
            expected: expected_sha256.to_owned(),
            actual,
        });
    }
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).map_err(io_err(&tmp))?;
    fs::rename(&tmp, dest).map_err(io_err(dest))
}

/// Make `new` the installed binary, keeping the current one as `<bin>.prev`.
/// Both steps are renames/links in one directory, so the installed path always
/// names a complete executable.
pub fn install(bin: &Path, new: &Path) -> Result<(), SwapError> {
    let prev = prev_path(bin);
    if bin.exists() {
        let _ = fs::remove_file(&prev);
        fs::hard_link(bin, &prev)
            .or_else(|_| fs::copy(bin, &prev).map(|_| ()))
            .map_err(io_err(&prev))?;
    }
    fs::rename(new, bin).map_err(io_err(bin))
}

/// Put `<bin>.prev` back as the installed binary.
pub fn rollback(bin: &Path) -> Result<(), SwapError> {
    let prev = prev_path(bin);
    if !prev.exists() {
        return Err(SwapError::NoPrevious(prev));
    }
    fs::rename(&prev, bin).map_err(io_err(bin))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_keeps_the_previous_binary_and_rollback_restores_it() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("borg");
        fs::write(&bin, "v1").unwrap();
        let new = new_path(&bin);
        fs::write(&new, "v2").unwrap();

        install(&bin, &new).unwrap();
        assert_eq!(fs::read_to_string(&bin).unwrap(), "v2");
        assert_eq!(fs::read_to_string(prev_path(&bin)).unwrap(), "v1");
        assert!(!new.exists());

        rollback(&bin).unwrap();
        assert_eq!(fs::read_to_string(&bin).unwrap(), "v1");
        assert!(
            matches!(rollback(&bin), Err(SwapError::NoPrevious(_))),
            "nothing further to restore"
        );
    }

    #[test]
    fn a_second_install_replaces_the_old_prev() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("borg");
        fs::write(&bin, "v1").unwrap();
        for (version, prev) in [("v2", "v1"), ("v3", "v2")] {
            fs::write(new_path(&bin), version).unwrap();
            install(&bin, &new_path(&bin)).unwrap();
            assert_eq!(fs::read_to_string(prev_path(&bin)).unwrap(), prev);
        }
    }

    #[test]
    fn installing_over_nothing_works_for_a_first_install() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("borg");
        fs::write(new_path(&bin), "v1").unwrap();
        install(&bin, &new_path(&bin)).unwrap();
        assert_eq!(fs::read_to_string(&bin).unwrap(), "v1");
        assert!(!prev_path(&bin).exists());
    }

    #[test]
    fn sibling_paths() {
        assert_eq!(
            new_path(Path::new("/usr/local/bin/borg")),
            Path::new("/usr/local/bin/borg.new")
        );
        assert_eq!(
            prev_path(Path::new("/usr/local/bin/borg")),
            Path::new("/usr/local/bin/borg.prev")
        );
    }
}
