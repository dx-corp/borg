//! File operations on directories the job user owns.
//!
//! Work and cache directories belong to the job user so jobs can write them,
//! which means the drone daemon (a different user) cannot create, clone or
//! delete inside them directly. With a job user configured these operations run
//! as that user through [`JobExec`](crate::drone::jobuser::JobExec); without
//! one they are plain in-process file operations.

use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;

use crate::drone::exec::Exec;

pub trait FsOps: Send + Sync {
    fn mkdir_p(&self, path: &Path) -> io::Result<()>;
    /// Replace `path` with `content` (via a temporary file and rename).
    fn write_file(&self, path: &Path, content: &str) -> io::Result<()>;
    /// Delete a file or directory tree; a missing path is fine.
    fn remove_all(&self, path: &Path) -> io::Result<()>;
    /// Recreate `src` under `dst`, hard-linking files where possible.
    fn clone_tree(&self, src: &Path, dst: &Path) -> io::Result<()>;
}

/// Plain in-process file operations.
pub struct LocalFs;

/// Directory contents may be read-only (module caches); make them removable.
fn make_writable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut stack = vec![path.to_owned()];
        while let Some(p) = stack.pop() {
            let Ok(meta) = fs::symlink_metadata(&p) else {
                continue;
            };
            if meta.is_dir() {
                let _ = fs::set_permissions(&p, fs::Permissions::from_mode(0o755));
                if let Ok(rd) = fs::read_dir(&p) {
                    stack.extend(rd.flatten().map(|e| e.path()));
                }
            }
        }
    }
}

impl FsOps for LocalFs {
    fn mkdir_p(&self, path: &Path) -> io::Result<()> {
        fs::create_dir_all(path)
    }

    fn write_file(&self, path: &Path, content: &str) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("borg-tmp");
        fs::write(&tmp, content)?;
        fs::rename(&tmp, path)
    }

    fn remove_all(&self, path: &Path) -> io::Result<()> {
        let is_dir = match fs::symlink_metadata(path) {
            Ok(m) => m.is_dir(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let attempt = |p: &Path| {
            if is_dir {
                fs::remove_dir_all(p)
            } else {
                fs::remove_file(p)
            }
        };
        match attempt(path) {
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied && is_dir => {
                make_writable(path);
                attempt(path)
            }
            other => other,
        }
    }

    fn clone_tree(&self, src: &Path, dst: &Path) -> io::Result<()> {
        crate::drone::runner::clone_tree(src, dst)
    }
}

/// File operations carried out by commands run through `exec` (the job user).
pub struct ExecFs {
    exec: Arc<dyn Exec>,
}

impl ExecFs {
    pub fn new(exec: Arc<dyn Exec>) -> Self {
        Self { exec }
    }

    fn run(&self, program: &str, args: &[&str]) -> io::Result<()> {
        let out = self.exec.run(program, args)?;
        if out.success {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "`{program} {}` failed",
                args.join(" ")
            )))
        }
    }
}

impl FsOps for ExecFs {
    fn mkdir_p(&self, path: &Path) -> io::Result<()> {
        self.run("mkdir", &["-p", "--", &path.to_string_lossy()])
    }

    fn write_file(&self, path: &Path, content: &str) -> io::Result<()> {
        // $1 = content, $2 = path: nothing is interpolated into the script.
        self.run(
            "sh",
            &[
                "-c",
                "mkdir -p -- \"$(dirname -- \"$2\")\" && printf '%s' \"$1\" > \"$2.borg-tmp\" && mv -f -- \"$2.borg-tmp\" \"$2\"",
                "sh",
                content,
                &path.to_string_lossy(),
            ],
        )
    }

    fn remove_all(&self, path: &Path) -> io::Result<()> {
        self.run("rm", &["-rf", "--", &path.to_string_lossy()])
    }

    fn clone_tree(&self, src: &Path, dst: &Path) -> io::Result<()> {
        if let Some(parent) = dst.parent() {
            self.mkdir_p(parent)?;
        }
        // GNU cp: archive mode, hard links where the filesystem allows.
        self.run(
            "cp",
            &["-al", "--", &src.to_string_lossy(), &dst.to_string_lossy()],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drone::cleanup::testing::FakeExec;

    #[test]
    fn local_fs_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs;
        let file = dir.path().join("a/b/c.txt");
        fs.write_file(&file, "hello").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello");
        fs.write_file(&file, "again").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "again");
        fs.remove_all(&dir.path().join("a")).unwrap();
        assert!(!dir.path().join("a").exists());
        fs.remove_all(&dir.path().join("a")).unwrap();
    }

    #[test]
    fn exec_fs_issues_argument_only_commands() {
        let fake = Arc::new(FakeExec::default());
        let fs = ExecFs::new(fake.clone());
        fs.mkdir_p(Path::new("/c/git")).unwrap();
        fs.remove_all(Path::new("/w/slot-0")).unwrap();
        fs.clone_tree(Path::new("/r"), Path::new("/w/slot-0"))
            .unwrap();
        fs.write_file(Path::new("/h/.bazelrc"), "common --x\n; rm -rf /")
            .unwrap();
        let calls = fake.calls();
        assert_eq!(calls[0], "mkdir -p -- /c/git");
        assert_eq!(calls[1], "rm -rf -- /w/slot-0");
        assert_eq!(calls[2], "mkdir -p -- /w");
        assert_eq!(calls[3], "cp -al -- /r /w/slot-0");
        assert!(calls[4].starts_with("sh -c "), "{}", calls[4]);
        assert!(
            calls[4].ends_with("sh common --x\n; rm -rf / /h/.bazelrc"),
            "content is an argument, not script text: {}",
            calls[4]
        );
    }

    #[test]
    fn a_failing_command_is_an_error() {
        let fake = Arc::new(FakeExec::default());
        fake.fail("rm -rf -- /w");
        assert!(ExecFs::new(fake).remove_all(Path::new("/w")).is_err());
    }
}
