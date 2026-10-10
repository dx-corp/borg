//! `borg drone install`: idempotently set a host up as a drone and report
//! exactly what changed.
//!
//! Linux (and WSL, which needs systemd enabled) gets a dedicated service user
//! in the docker group and a systemd unit; macOS gets a LaunchDaemon. Both get
//! a copy of this binary, the pinned actions-runner (checksum verified), a
//! config file and the enrollment token. Re-running changes only what differs
//! and restarts the service only if something changed.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::drone::config::DroneConfig;
use crate::drone::exec::Exec;
use crate::drone::pins::RunnerPin;
use crate::secret::Secret;

const SERVICE: &str = "borg-drone.service";
const LAUNCHD_LABEL: &str = "org.borg.drone";
const STOP_GRACE_SECS: u64 = 120;

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("{0}")]
    Preflight(String),
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("`{command}` failed")]
    Command { command: String },
    #[error("checksum mismatch for {file}: expected {expected}, got {actual}")]
    Checksum {
        file: String,
        expected: String,
        actual: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Linux,
    MacOs,
}

impl Platform {
    pub fn current() -> Option<Self> {
        match std::env::consts::OS {
            "linux" => Some(Self::Linux),
            "macos" => Some(Self::MacOs),
            _ => None,
        }
    }
}

pub struct InstallOptions {
    pub queen_url: String,
    pub token: Secret,
    pub ca_cert: Option<PathBuf>,
    pub allow_insecure: bool,
    pub max_vcpu: Option<u32>,
    pub max_mem_mib: Option<u64>,
    pub max_leases: Option<u32>,
    /// Service account the drone and its jobs run as.
    pub run_as: String,
    /// Filesystem root the install is written under (`/` on a real host).
    pub root: PathBuf,
    pub platform: Platform,
    pub pin: RunnerPin,
    /// The borg binary to install.
    pub binary: PathBuf,
    pub dry_run: bool,
    /// Skip the "must be root" check (tests, `--root` staging).
    pub skip_root_check: bool,
    pub drain_timeout_secs: u64,
    /// Repositories (`owner/name`) to mirror on the host.
    pub git_mirrors: Vec<String>,
    /// Remote base for mirrors; the drone default (`https://github.com`) if none.
    pub git_remote_base: Option<String>,
}

/// Where everything lives, as paths on the real host.
struct Layout {
    bin: PathBuf,
    config_dir: PathBuf,
    data: PathBuf,
    service_file: PathBuf,
}

impl Layout {
    fn new(platform: Platform) -> Self {
        match platform {
            Platform::Linux => Self {
                bin: "/usr/local/bin/borg".into(),
                config_dir: "/etc/borg".into(),
                data: "/var/lib/borg".into(),
                service_file: format!("/etc/systemd/system/{SERVICE}").into(),
            },
            Platform::MacOs => Self {
                bin: "/usr/local/bin/borg".into(),
                config_dir: "/usr/local/etc/borg".into(),
                data: "/usr/local/var/borg".into(),
                service_file: format!("/Library/LaunchDaemons/{LAUNCHD_LABEL}.plist").into(),
            },
        }
    }

    fn config(&self) -> PathBuf {
        self.config_dir.join("drone.json")
    }
    fn token_file(&self) -> PathBuf {
        self.config_dir.join("drone.token")
    }
    fn ca_cert(&self) -> PathBuf {
        self.config_dir.join("queen-ca.pem")
    }
    fn runner(&self) -> PathBuf {
        self.data.join("runner")
    }
}

pub fn render_systemd_unit(
    layout_bin: &Path,
    config: &Path,
    data: &Path,
    user: &str,
    drain_timeout_secs: u64,
) -> String {
    format!(
        "[Unit]\n\
         Description=borg CI drone\n\
         After=network-online.target docker.service\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         User={user}\n\
         Group={user}\n\
         WorkingDirectory={data}\n\
         ExecStart={bin} drone run --config {config}\n\
         Restart=always\n\
         RestartSec=5\n\
         KillSignal=SIGTERM\n\
         # SIGTERM goes to the drone only, so in-flight jobs can finish while it\n\
         # drains; leftovers are killed once the drone exits.\n\
         KillMode=mixed\n\
         TimeoutStopSec={stop}\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        bin = layout_bin.display(),
        config = config.display(),
        data = data.display(),
        stop = drain_timeout_secs + STOP_GRACE_SECS,
    )
}

pub fn render_launchd_plist(
    layout_bin: &Path,
    config: &Path,
    data: &Path,
    user: &str,
    drain_timeout_secs: u64,
) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key><string>{LAUNCHD_LABEL}</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n\
         \t\t<string>{bin}</string>\n\
         \t\t<string>drone</string>\n\
         \t\t<string>run</string>\n\
         \t\t<string>--config</string>\n\
         \t\t<string>{config}</string>\n\
         \t</array>\n\
         \t<key>UserName</key><string>{user}</string>\n\
         \t<key>WorkingDirectory</key><string>{data}</string>\n\
         \t<key>RunAtLoad</key><true/>\n\
         \t<key>KeepAlive</key><true/>\n\
         \t<key>ExitTimeOut</key><integer>{stop}</integer>\n\
         \t<key>StandardOutPath</key><string>{data}/drone.log</string>\n\
         \t<key>StandardErrorPath</key><string>{data}/drone.log</string>\n\
         </dict>\n\
         </plist>\n",
        bin = layout_bin.display(),
        config = config.display(),
        data = data.display(),
        stop = drain_timeout_secs + STOP_GRACE_SECS,
    )
}

struct Installer<'a> {
    opts: &'a InstallOptions,
    layout: Layout,
    exec: &'a dyn Exec,
    lines: Vec<String>,
    changed: bool,
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> InstallError + '_ {
    move |source| InstallError::Io {
        path: path.to_owned(),
        source,
    }
}

impl Installer<'_> {
    fn on_disk(&self, real: &Path) -> PathBuf {
        self.opts.root.join(real.strip_prefix("/").unwrap_or(real))
    }

    fn note(&mut self, verb: &str, what: impl std::fmt::Display) {
        self.lines.push(format!("{verb} {what}"));
    }

    /// Run a host command; a failure is an error unless `tolerate`.
    fn cmd(&self, program: &str, args: &[&str]) -> Result<bool, InstallError> {
        let out = self
            .exec
            .run(program, args)
            .map_err(|source| InstallError::Io {
                path: PathBuf::from(program),
                source,
            })?;
        Ok(out.success)
    }

    fn must(&self, program: &str, args: &[&str]) -> Result<(), InstallError> {
        if self.cmd(program, args)? {
            Ok(())
        } else {
            Err(InstallError::Command {
                command: format!("{program} {}", args.join(" ")),
            })
        }
    }

    fn chown(&self, real: &Path, owner: &str, recursive: bool) -> Result<(), InstallError> {
        let spec = format!("{owner}:{owner}");
        let path = self.on_disk(real);
        let path = path.to_string_lossy();
        if recursive {
            self.must("chown", &["-R", &spec, &path])
        } else {
            self.must("chown", &[&spec, &path])
        }
    }

    fn ensure_dir(
        &mut self,
        real: &Path,
        mode: u32,
        owner: Option<&str>,
    ) -> Result<(), InstallError> {
        let path = self.on_disk(real);
        if path.is_dir() {
            self.note("unchanged", format_args!("dir {}", real.display()));
            return Ok(());
        }
        self.changed = true;
        if self.opts.dry_run {
            self.note("would create", format_args!("dir {}", real.display()));
            return Ok(());
        }
        fs::create_dir_all(&path).map_err(io_err(&path))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).map_err(io_err(&path))?;
        if let Some(owner) = owner {
            self.chown(real, owner, false)?;
        }
        self.note("created", format_args!("dir {}", real.display()));
        Ok(())
    }

    /// Write `content` unless the file already has it. Returns whether it changed.
    fn write_file(
        &mut self,
        real: &Path,
        content: &[u8],
        mode: u32,
        owner: Option<&str>,
    ) -> Result<bool, InstallError> {
        let path = self.on_disk(real);
        let existing = fs::read(&path).ok();
        let same = existing.as_deref() == Some(content)
            && fs::metadata(&path).is_ok_and(|m| m.permissions().mode() & 0o7777 == mode);
        if same {
            self.note("unchanged", real.display());
            return Ok(false);
        }
        self.changed = true;
        let verb = if existing.is_some() {
            "updated"
        } else {
            "created"
        };
        if self.opts.dry_run {
            self.note(
                &format!("would {}", verb.trim_end_matches('d')),
                real.display(),
            );
            return Ok(true);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(io_err(parent))?;
        }
        // Replace via a unique temp file so a running binary is never truncated.
        let tmp = path.with_extension("borg-new");
        fs::write(&tmp, content).map_err(io_err(&tmp))?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode)).map_err(io_err(&tmp))?;
        fs::rename(&tmp, &path).map_err(io_err(&path))?;
        if let Some(owner) = owner {
            self.chown(real, owner, false)?;
        }
        self.note(verb, real.display());
        Ok(true)
    }

    fn preflight(&self) -> Result<(), InstallError> {
        if !self.opts.skip_root_check && !self.opts.dry_run {
            let out = self
                .exec
                .run("id", &["-u"])
                .map_err(|source| InstallError::Io {
                    path: PathBuf::from("id"),
                    source,
                })?;
            if out.stdout != "0" {
                return Err(InstallError::Preflight(
                    "run as root (for example with sudo)".to_owned(),
                ));
            }
        }
        if self.opts.platform == Platform::Linux
            && !self.on_disk(Path::new("/run/systemd/system")).is_dir()
        {
            let wsl = self
                .on_disk(Path::new("/proc/sys/fs/binfmt_misc/WSLInterop"))
                .exists()
                || fs::read_to_string(self.on_disk(Path::new("/proc/version")))
                    .is_ok_and(|v| v.to_ascii_lowercase().contains("microsoft"));
            let hint = if wsl {
                " On WSL, enable systemd: add `[boot]` and `systemd=true` to /etc/wsl.conf, then run `wsl --shutdown` from Windows and start the distribution again."
            } else {
                ""
            };
            return Err(InstallError::Preflight(format!(
                "systemd is not running on this host.{hint}"
            )));
        }
        Ok(())
    }

    fn ensure_user(&mut self) -> Result<(), InstallError> {
        let user = self.opts.run_as.clone();
        let exists = self.cmd("id", &["-u", &user])?;
        match self.opts.platform {
            Platform::MacOs => {
                if !exists {
                    return Err(InstallError::Preflight(format!(
                        "user {user} does not exist; create it first or pass --run-as <existing user>"
                    )));
                }
                self.note("unchanged", format_args!("user {user}"));
            }
            Platform::Linux => {
                if exists {
                    self.note("unchanged", format_args!("user {user}"));
                } else {
                    self.changed = true;
                    if self.opts.dry_run {
                        self.note("would create", format_args!("user {user}"));
                    } else {
                        let home = self.layout.data.to_string_lossy().into_owned();
                        self.must(
                            "useradd",
                            &[
                                "--system",
                                "--user-group",
                                "--home-dir",
                                &home,
                                "--shell",
                                "/usr/sbin/nologin",
                                &user,
                            ],
                        )?;
                        self.note("created", format_args!("user {user}"));
                    }
                }
                self.ensure_docker_group(&user, exists)?;
            }
        }
        Ok(())
    }

    fn ensure_docker_group(&mut self, user: &str, user_exists: bool) -> Result<(), InstallError> {
        if !self.cmd("getent", &["group", "docker"])? {
            self.note("warning", "no docker group found; docker jobs fail until Docker is installed and the user joins its group");
            return Ok(());
        }
        let member = user_exists
            && self
                .exec
                .run("id", &["-nG", user])
                .is_ok_and(|o| o.success && o.stdout.split_whitespace().any(|g| g == "docker"));
        if member {
            self.note("unchanged", format_args!("user {user} in group docker"));
        } else if self.opts.dry_run {
            self.changed = true;
            self.note("would add", format_args!("user {user} to group docker"));
        } else {
            self.changed = true;
            self.must("usermod", &["-aG", "docker", user])?;
            self.note("added", format_args!("user {user} to group docker"));
        }
        Ok(())
    }

    fn install_binary(&mut self) -> Result<(), InstallError> {
        let bytes = fs::read(&self.opts.binary).map_err(io_err(&self.opts.binary))?;
        let bin = self.layout.bin.clone();
        self.write_file(&bin, &bytes, 0o755, None)?;
        Ok(())
    }

    fn install_runner(&mut self) -> Result<(), InstallError> {
        let marker = format!("{} {}\n", self.opts.pin.version, self.opts.pin.sha256);
        let runner = self.layout.runner();
        let marker_path = self.on_disk(&runner.join(".borg-runner-version"));
        if fs::read_to_string(&marker_path).is_ok_and(|m| m == marker) {
            self.note(
                "unchanged",
                format_args!(
                    "actions-runner {} at {}",
                    self.opts.pin.version,
                    runner.display()
                ),
            );
            return Ok(());
        }
        self.changed = true;
        if self.opts.dry_run {
            self.note(
                "would install",
                format_args!(
                    "actions-runner {} at {}",
                    self.opts.pin.version,
                    runner.display()
                ),
            );
            return Ok(());
        }
        let user = self.opts.run_as.clone();
        let data = self.layout.data.clone();
        let staging = data.join("runner.new");
        let tarball = data.join(self.opts.pin.filename());
        let (staging_fs, tarball_fs) = (self.on_disk(&staging), self.on_disk(&tarball));
        let _ = fs::remove_dir_all(&staging_fs);
        fs::create_dir_all(&staging_fs).map_err(io_err(&staging_fs))?;

        let url = self.opts.pin.url();
        self.must(
            "curl",
            &["-fsSL", "-o", &tarball_fs.to_string_lossy(), &url],
        )?;
        let actual = hex::encode(Sha256::digest(
            fs::read(&tarball_fs).map_err(io_err(&tarball_fs))?,
        ));
        if actual != self.opts.pin.sha256 {
            let _ = fs::remove_file(&tarball_fs);
            let _ = fs::remove_dir_all(&staging_fs);
            return Err(InstallError::Checksum {
                file: self.opts.pin.filename(),
                expected: self.opts.pin.sha256.clone(),
                actual,
            });
        }
        self.must(
            "tar",
            &[
                "-xzf",
                &tarball_fs.to_string_lossy(),
                "-C",
                &staging_fs.to_string_lossy(),
            ],
        )?;
        let _ = fs::remove_file(&tarball_fs);
        fs::write(staging_fs.join(".borg-runner-version"), &marker).map_err(io_err(&staging_fs))?;

        let old = self.on_disk(&runner);
        if old.exists() {
            // The installed tree is read-only; restore write access to delete it.
            self.must("chmod", &["-R", "u+w", &old.to_string_lossy()])?;
            fs::remove_dir_all(&old).map_err(io_err(&old))?;
        }
        fs::rename(&staging_fs, &old).map_err(io_err(&old))?;
        self.chown(&runner, &user, true)?;
        // Leases hard-link these files; make them immutable to the runner.
        self.must("chmod", &["-R", "a-w", &old.to_string_lossy()])?;
        self.note(
            "installed",
            format_args!(
                "actions-runner {} at {} (sha256 verified)",
                self.opts.pin.version,
                runner.display()
            ),
        );
        Ok(())
    }

    fn drone_config(&self) -> Result<String, InstallError> {
        let o = self.opts;
        let cfg = DroneConfig {
            queen_url: o.queen_url.clone(),
            token_file: Some(self.layout.token_file()),
            ca_cert_path: o.ca_cert.as_ref().map(|_| self.layout.ca_cert()),
            allow_insecure: o.allow_insecure,
            runner_dir: self.layout.runner(),
            work_root: self.layout.data.join("work"),
            cache_root: self.layout.data.join("cache"),
            docker_bin: "docker".to_owned(),
            heartbeat_secs: 10,
            max_job_secs: 6 * 3600,
            drain_timeout_secs: o.drain_timeout_secs,
            max_vcpu: o.max_vcpu,
            max_mem_mib: o.max_mem_mib,
            max_leases: o.max_leases,
            reserve_mem_mib: crate::allocator::HostCaps::default().reserve_mem_mib,
            caches: crate::drone::config::CachesConfig {
                git: crate::drone::config::GitCaches {
                    mirrors: o.git_mirrors.clone(),
                    remote_base: o
                        .git_remote_base
                        .clone()
                        .unwrap_or_else(|| "https://github.com".to_owned()),
                    ..Default::default()
                },
                ..Default::default()
            },
        };
        cfg.validate()
            .map_err(|e| InstallError::Preflight(e.to_string()))?;
        let mut json = serde_json::to_string_pretty(&cfg)
            .map_err(|e| InstallError::Preflight(e.to_string()))?;
        json.push('\n');
        Ok(json)
    }

    fn install_service(&mut self, config_changed: bool) -> Result<(), InstallError> {
        let l = &self.layout;
        let (bin, cfg, data, user) = (
            l.bin.clone(),
            l.config(),
            l.data.clone(),
            self.opts.run_as.clone(),
        );
        let drain = self.opts.drain_timeout_secs;
        let service_file = l.service_file.clone();
        let before = self.changed;
        match self.opts.platform {
            Platform::Linux => {
                let unit = render_systemd_unit(&bin, &cfg, &data, &user, drain);
                let unit_changed = self.write_file(&service_file, unit.as_bytes(), 0o644, None)?;
                if self.opts.dry_run {
                    return Ok(());
                }
                if unit_changed {
                    self.must("systemctl", &["daemon-reload"])?;
                    self.note("ran", "systemctl daemon-reload");
                }
                if !self.cmd("systemctl", &["is-enabled", "--quiet", SERVICE])? {
                    self.must("systemctl", &["enable", SERVICE])?;
                    self.note("ran", format_args!("systemctl enable {SERVICE}"));
                }
                if before || self.changed || config_changed || unit_changed {
                    self.must("systemctl", &["restart", SERVICE])?;
                    self.note("ran", format_args!("systemctl restart {SERVICE}"));
                } else if !self.cmd("systemctl", &["is-active", "--quiet", SERVICE])? {
                    self.must("systemctl", &["start", SERVICE])?;
                    self.note("ran", format_args!("systemctl start {SERVICE}"));
                }
            }
            Platform::MacOs => {
                let plist = render_launchd_plist(&bin, &cfg, &data, &user, drain);
                let plist_changed =
                    self.write_file(&service_file, plist.as_bytes(), 0o644, None)?;
                if self.opts.dry_run {
                    return Ok(());
                }
                let target = format!("system/{LAUNCHD_LABEL}");
                let path = self.on_disk(&service_file);
                let path = path.to_string_lossy().into_owned();
                let loaded = self.cmd("launchctl", &["print", &target])?;
                if loaded && (self.changed || plist_changed) {
                    self.must("launchctl", &["bootout", &target])?;
                    self.note("ran", format_args!("launchctl bootout {target}"));
                }
                if !loaded || self.changed || plist_changed {
                    self.must("launchctl", &["bootstrap", "system", &path])?;
                    self.note("ran", format_args!("launchctl bootstrap system {path}"));
                }
            }
        }
        Ok(())
    }
}

/// Install (or update) the drone. Returns one line per action taken or skipped.
pub fn install(opts: &InstallOptions, exec: &dyn Exec) -> Result<Vec<String>, InstallError> {
    let mut i = Installer {
        opts,
        layout: Layout::new(opts.platform),
        exec,
        lines: Vec::new(),
        changed: false,
    };
    i.preflight()?;
    let run_as = opts.run_as.clone();
    let owner = Some(run_as.as_str());

    i.ensure_user()?;
    let (config_dir, data) = (i.layout.config_dir.clone(), i.layout.data.clone());
    i.ensure_dir(&config_dir, 0o755, None)?;
    i.ensure_dir(&data, 0o755, owner)?;
    for sub in ["work", "cache"] {
        i.ensure_dir(&data.join(sub), 0o755, owner)?;
    }
    i.install_binary()?;
    i.install_runner()?;

    let mut config_changed = false;
    if let Some(ca) = &opts.ca_cert {
        let pem = fs::read(ca).map_err(io_err(ca))?;
        let target = i.layout.ca_cert();
        config_changed |= i.write_file(&target, &pem, 0o644, None)?;
    }
    let token_file = i.layout.token_file();
    let token = format!("{}\n", opts.token.expose());
    config_changed |= i.write_file(&token_file, token.as_bytes(), 0o600, owner)?;
    let config = i.drone_config()?;
    let config_path = i.layout.config();
    config_changed |= i.write_file(&config_path, config.as_bytes(), 0o644, None)?;

    i.install_service(config_changed)?;

    let mut lines = i.lines;
    let mut summary = String::new();
    let _ = write!(
        summary,
        "{}",
        if i.changed {
            if opts.dry_run {
                "dry run: changes listed above were not applied"
            } else {
                "done: changes applied"
            }
        } else {
            "nothing to change"
        }
    );
    lines.push(summary);
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drone::cleanup::testing::FakeExec;
    use crate::drone::pins::pin_for;

    fn tarball_bytes() -> Vec<u8> {
        b"fake runner tarball".to_vec()
    }

    struct Env {
        _dir: tempfile::TempDir,
        root: PathBuf,
        binary: PathBuf,
        pin: RunnerPin,
    }

    fn env() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir_all(root.join("run/systemd/system")).unwrap();
        let binary = dir.path().join("borg-binary");
        fs::write(&binary, b"binary-v1").unwrap();
        let mut pin = pin_for("linux", "x86_64").unwrap();
        pin.sha256 = hex::encode(Sha256::digest(tarball_bytes()));
        Env {
            _dir: dir,
            root,
            binary,
            pin,
        }
    }

    fn opts(e: &Env) -> InstallOptions {
        InstallOptions {
            queen_url: "https://queen.example.test:8443".into(),
            token: Secret::new("borg_testtoken"),
            ca_cert: None,
            allow_insecure: false,
            max_vcpu: Some(8),
            max_mem_mib: Some(24576),
            max_leases: None,
            run_as: "borg".into(),
            root: e.root.clone(),
            platform: Platform::Linux,
            pin: e.pin.clone(),
            binary: e.binary.clone(),
            dry_run: false,
            skip_root_check: true,
            drain_timeout_secs: 3600,
            git_mirrors: vec!["example-org/app".into()],
            git_remote_base: None,
        }
    }

    /// A fake host where curl/tar produce the fixture files and the user exists.
    fn exec() -> FakeExec {
        let x = FakeExec::default();
        x.on("getent group docker", "docker:x:999:");
        x.on("id -nG borg", "borg docker");
        x
    }

    /// Fake tar/curl side effects on the staging tree.
    fn with_downloads(x: FakeExec) -> impl Exec {
        struct Hooked(FakeExec);
        impl Exec for Hooked {
            fn run(&self, program: &str, args: &[&str]) -> io::Result<crate::drone::exec::Output> {
                match program {
                    "curl" => fs::write(args[2], tarball_bytes())?,
                    "tar" => fs::write(Path::new(args[3]).join("run.sh"), "#!/bin/sh\n")?,
                    _ => {}
                }
                self.0.run(program, args)
            }
        }
        Hooked(x)
    }

    fn calls(x: &FakeExec) -> Vec<String> {
        x.calls()
    }

    #[test]
    fn unit_and_plist_are_rendered_with_drain_safe_settings() {
        let unit = render_systemd_unit(
            Path::new("/usr/local/bin/borg"),
            Path::new("/etc/borg/drone.json"),
            Path::new("/var/lib/borg"),
            "borg",
            3600,
        );
        for line in [
            "User=borg",
            "KillMode=mixed",
            "TimeoutStopSec=3720",
            "ExecStart=/usr/local/bin/borg drone run --config /etc/borg/drone.json",
            "Restart=always",
        ] {
            assert!(unit.contains(line), "{line}\n{unit}");
        }
        let plist = render_launchd_plist(
            Path::new("/usr/local/bin/borg"),
            Path::new("/c.json"),
            Path::new("/d"),
            "builder",
            60,
        );
        for s in [
            "<string>org.borg.drone</string>",
            "<key>UserName</key><string>builder</string>",
            "<integer>180</integer>",
            "<string>/c.json</string>",
        ] {
            assert!(plist.contains(s), "{s}");
        }
    }

    #[test]
    fn first_install_creates_everything_and_second_changes_nothing() {
        let e = env();
        let x = FakeExec::default();
        x.on("getent group docker", "docker:x:999:");
        x.fail("systemctl is-enabled --quiet borg-drone.service");
        x.fail("id -u borg");
        let ex = with_downloads(x);
        let first = install(&opts(&e), &ex).unwrap();
        let text = first.join("\n");
        for needle in [
            "created user borg",
            "added user borg to group docker",
            "created /usr/local/bin/borg",
            "installed actions-runner 2.337.0",
            "created /etc/borg/drone.token",
            "created /etc/borg/drone.json",
            "created /etc/systemd/system/borg-drone.service",
            "ran systemctl restart borg-drone.service",
            "done: changes applied",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in\n{text}");
        }
        assert_eq!(
            fs::read(e.root.join("usr/local/bin/borg")).unwrap(),
            b"binary-v1"
        );
        assert_eq!(
            fs::metadata(e.root.join("etc/borg/drone.token"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let cfg = DroneConfig::from_json(
            &fs::read_to_string(e.root.join("etc/borg/drone.json")).unwrap(),
        )
        .unwrap();
        assert_eq!((cfg.max_vcpu, cfg.max_mem_mib), (Some(8), Some(24576)));
        assert_eq!(cfg.caches.git.mirrors, ["example-org/app"]);
        assert!(
            e.root
                .join("var/lib/borg/runner/.borg-runner-version")
                .exists()
        );

        // Second run on the now-configured host.
        let y = exec();
        let ey = with_downloads(y);
        let second = install(&opts(&e), &ey).unwrap();
        let text = second.join("\n");
        assert!(text.contains("nothing to change"), "{text}");
        assert!(
            !text.contains("created") && !text.contains("updated") && !text.contains("ran "),
            "{text}"
        );
    }

    #[test]
    fn rerun_restarts_only_when_something_changed() {
        let e = env();
        install(&opts(&e), &with_downloads(exec())).unwrap();

        fs::write(&e.binary, b"binary-v2").unwrap();
        let x = exec();
        let hooked = with_downloads(x);
        let out = install(&opts(&e), &hooked).unwrap().join("\n");
        assert!(out.contains("updated /usr/local/bin/borg"), "{out}");
        assert!(
            out.contains("ran systemctl restart borg-drone.service"),
            "{out}"
        );
    }

    #[test]
    fn a_pin_change_reinstalls_the_runner() {
        let e = env();
        install(&opts(&e), &with_downloads(exec())).unwrap();
        let mut o = opts(&e);
        o.pin.version = "2.999.0".into();
        let out = install(&o, &with_downloads(exec())).unwrap().join("\n");
        assert!(out.contains("installed actions-runner 2.999.0"), "{out}");
        let marker =
            fs::read_to_string(e.root.join("var/lib/borg/runner/.borg-runner-version")).unwrap();
        assert!(marker.starts_with("2.999.0 "));
    }

    #[test]
    fn checksum_mismatch_aborts_before_extraction() {
        let e = env();
        let mut o = opts(&e);
        o.pin.sha256 = "0".repeat(64);
        let x = exec();
        let err = install(&o, &with_downloads(x)).unwrap_err();
        assert!(matches!(err, InstallError::Checksum { .. }), "{err}");
        assert!(!e.root.join("var/lib/borg/runner").exists());
        assert!(
            !e.root
                .join("etc/systemd/system/borg-drone.service")
                .exists(),
            "nothing after the failed step"
        );
    }

    #[test]
    fn dry_run_lists_changes_and_writes_nothing() {
        let e = env();
        let mut o = opts(&e);
        o.dry_run = true;
        let x = FakeExec::default();
        x.fail("id -u borg");
        let out = install(&o, &x).unwrap().join("\n");
        assert!(
            out.contains("would create user borg")
                && out.contains("would create /etc/borg/drone.json"),
            "{out}"
        );
        assert!(out.contains("dry run"), "{out}");
        assert!(!e.root.join("etc").exists() && !e.root.join("usr").exists());
        assert!(
            calls(&x)
                .iter()
                .all(|c| !c.starts_with("useradd") && !c.starts_with("systemctl")),
            "{:?}",
            calls(&x)
        );
    }

    #[test]
    fn requires_systemd_and_explains_wsl() {
        let e = env();
        fs::remove_dir_all(e.root.join("run/systemd")).unwrap();
        let err = install(&opts(&e), &exec()).unwrap_err().to_string();
        assert!(
            err.contains("systemd is not running") && !err.contains("wsl.conf"),
            "{err}"
        );
        fs::create_dir_all(e.root.join("proc")).unwrap();
        fs::write(
            e.root.join("proc/version"),
            "Linux version 5.15-microsoft-standard-WSL2",
        )
        .unwrap();
        let err = install(&opts(&e), &exec()).unwrap_err().to_string();
        assert!(
            err.contains("/etc/wsl.conf") && err.contains("systemd=true"),
            "{err}"
        );
    }

    #[test]
    fn non_root_is_refused() {
        let e = env();
        let mut o = opts(&e);
        o.skip_root_check = false;
        let x = FakeExec::default();
        x.on("id -u", "1000");
        assert!(
            install(&o, &x)
                .unwrap_err()
                .to_string()
                .contains("run as root")
        );
    }

    #[test]
    fn macos_uses_launchd_and_requires_an_existing_user() {
        let e = env();
        let mut o = opts(&e);
        o.platform = Platform::MacOs;
        o.run_as = "builder".into();
        let x = FakeExec::default();
        x.fail("launchctl print system/org.borg.drone");
        let out = install(&o, &with_downloads(x)).unwrap().join("\n");
        assert!(
            out.contains("created /Library/LaunchDaemons/org.borg.drone.plist"),
            "{out}"
        );
        assert!(out.contains("ran launchctl bootstrap system"), "{out}");

        let y = FakeExec::default();
        y.fail("id -u builder");
        let err = install(&o, &y).unwrap_err().to_string();
        assert!(err.contains("create it first"), "{err}");
    }
}
