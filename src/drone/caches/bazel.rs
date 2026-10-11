//! The drone's managed block in a Bazel user rc file.
//!
//! Bazel has no environment variable for the disk or repository cache, so the
//! drone points them at host-level directories through the service user's
//! `~/.bazelrc`. The user rc is read after the workspace rc, so it wins for
//! these two flags. Only the block between the markers is ever rewritten.

use std::fs;
use std::io;
use std::path::Path;

use crate::drone::fsops::FsOps;

const BEGIN: &str = "# >>> borg managed cache flags (do not edit) >>>";
const END: &str = "# <<< borg managed cache flags <<<";

/// The managed block for the given cache directories.
pub fn managed_block(disk_cache: &Path, repository_cache: &Path) -> String {
    format!(
        "{BEGIN}\ncommon --disk_cache={}\ncommon --repository_cache={}\n{END}\n",
        disk_cache.display(),
        repository_cache.display()
    )
}

/// Replace (or append) the managed block in `existing`, leaving all other
/// content untouched.
pub fn with_block(existing: &str, block: &str) -> String {
    match (existing.find(BEGIN), existing.find(END)) {
        (Some(start), Some(end)) if end > start => {
            let end = end + END.len();
            let tail = existing[end..]
                .strip_prefix('\n')
                .unwrap_or(&existing[end..]);
            format!("{}{block}{tail}", &existing[..start])
        }
        _ if existing.is_empty() => block.to_owned(),
        _ if existing.ends_with('\n') => format!("{existing}\n{block}"),
        _ => format!("{existing}\n\n{block}"),
    }
}

/// Write the block into `rcfile`. Returns whether the file changed.
pub fn write_rc(
    fs_ops: &dyn FsOps,
    rcfile: &Path,
    disk_cache: &Path,
    repository_cache: &Path,
) -> io::Result<bool> {
    let existing = match fs::read_to_string(rcfile) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let updated = with_block(&existing, &managed_block(disk_cache, repository_cache));
    if updated == existing {
        return Ok(false);
    }
    fs_ops.write_file(rcfile, &updated)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drone::fsops::LocalFs;

    fn block() -> String {
        managed_block(Path::new("/c/bazel/disk"), Path::new("/c/bazel/repo"))
    }

    #[test]
    fn block_lists_both_caches() {
        let b = block();
        assert!(b.contains("common --disk_cache=/c/bazel/disk\n"));
        assert!(b.contains("common --repository_cache=/c/bazel/repo\n"));
    }

    #[test]
    fn appends_to_existing_content_and_is_idempotent() {
        let once = with_block("build --jobs=4\n", &block());
        assert!(once.starts_with("build --jobs=4\n\n# >>> borg"));
        assert_eq!(with_block(&once, &block()), once);
    }

    #[test]
    fn replaces_only_the_managed_block() {
        let old = format!(
            "before\n\n{}after\n",
            managed_block(Path::new("/old/d"), Path::new("/old/r"))
        );
        let updated = with_block(&old, &block());
        assert!(updated.starts_with("before\n\n"));
        assert!(updated.ends_with("after\n"));
        assert!(updated.contains("/c/bazel/disk") && !updated.contains("/old/"));
        assert_eq!(updated.matches(BEGIN).count(), 1);
    }

    #[test]
    fn writes_the_file_and_reports_changes() {
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join("home/.bazelrc");
        assert!(
            write_rc(
                &LocalFs,
                &rc,
                Path::new("/c/bazel/disk"),
                Path::new("/c/bazel/repo")
            )
            .unwrap()
        );
        assert!(
            !write_rc(
                &LocalFs,
                &rc,
                Path::new("/c/bazel/disk"),
                Path::new("/c/bazel/repo")
            )
            .unwrap()
        );
        assert!(
            write_rc(
                &LocalFs,
                &rc,
                Path::new("/c/other/disk"),
                Path::new("/c/bazel/repo")
            )
            .unwrap()
        );
        assert!(fs::read_to_string(&rc).unwrap().contains("/c/other/disk"));
    }
}
