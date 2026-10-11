//! The `borg update` command line: configuration, manifest fetch, the real
//! service manager and health file, and the request/result hand-off with a
//! running drone.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::drone::exec::SystemExec;
use crate::update::config::UpdateConfig;
use crate::update::health::FileHealth;
use crate::update::manifest;
use crate::update::service::{Launchd, ServiceManager, Systemd};
use crate::update::{Options, Outcome, Request, UpdateResult, request_path, result_path};

pub struct Args {
    pub config: PathBuf,
    pub version: Option<String>,
    pub check: bool,
    pub runner_only: bool,
    /// Read the target version from the drone's request file and leave a
    /// result for it (how the systemd path unit invokes the updater).
    pub from_request: bool,
    pub json: bool,
}

fn unix_now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

/// Run an update; returns the process exit code (0 ok, 3 rolled back).
pub async fn run(args: Args) -> Result<i32> {
    let cfg = UpdateConfig::load(&args.config)?;
    let mut version = args.version.clone();
    if args.from_request {
        let path = request_path(&cfg.state_dir);
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let request: Request = serde_json::from_str(&text).context("parse update request")?;
        if !manifest::valid_version(&request.version) {
            bail!("the update request names an invalid version");
        }
        version = Some(request.version);
    }

    let outcome = attempt(&cfg, &args, version.as_deref()).await;
    if args.from_request {
        let result =
            UpdateResult::from_outcome(version.as_deref().unwrap_or(""), &outcome, unix_now());
        std::fs::write(result_path(&cfg.state_dir), serde_json::to_vec(&result)?)
            .context("write update result")?;
        let _ = std::fs::remove_file(request_path(&cfg.state_dir));
    }
    let outcome = outcome?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&outcome)?);
    } else {
        println!("{}", describe(&outcome));
    }
    Ok(i32::from(matches!(outcome, Outcome::RolledBack { .. })) * 3)
}

async fn attempt(
    cfg: &UpdateConfig,
    args: &Args,
    version: Option<&str>,
) -> Result<Outcome, crate::update::UpdateError> {
    let manifest = manifest::fetch(&cfg.manifest_url_for(version), cfg.allow_insecure).await?;
    let exec = SystemExec;
    let service: Box<dyn ServiceManager + '_> = if cfg!(target_os = "macos") {
        Box::new(Launchd {
            label: cfg.service.clone(),
            plist: cfg.launchd_plist.clone().unwrap_or_default(),
            exec: &exec,
        })
    } else {
        Box::new(Systemd {
            unit: cfg.service.clone(),
            exec: &exec,
        })
    };
    let opts = Options {
        bin: &cfg.bin,
        current_version: env!("CARGO_PKG_VERSION"),
        requested_version: version,
        check: args.check,
        runner_only: args.runner_only,
        allow_insecure: cfg.allow_insecure,
        health_timeout: Duration::from_secs(cfg.health_timeout_secs),
        poll: Duration::from_secs(2),
        host_os: std::env::consts::OS,
        host_arch: std::env::consts::ARCH,
        runner_dir: cfg.runner_dir.as_deref(),
        runner_owner: cfg.runner_owner.as_deref(),
    };
    crate::update::run(
        &opts,
        &manifest,
        service.as_ref(),
        &FileHealth(cfg.health_file.clone()),
        &exec,
    )
    .await
}

pub fn describe(outcome: &Outcome) -> String {
    match outcome {
        Outcome::UpToDate { version } => format!("up to date ({version})"),
        Outcome::Available {
            current,
            target,
            runner,
        } => match runner {
            Some(r) => format!("update available: {current} -> {target}, runner {r}"),
            None => format!("update available: {current} -> {target}"),
        },
        Outcome::Updated {
            from,
            to,
            runner_updated,
        } => {
            format!(
                "updated {from} -> {to}{}",
                if *runner_updated {
                    " (runner updated)"
                } else {
                    ""
                }
            )
        }
        Outcome::RunnerUpdated { version } => format!("runner updated to {version}"),
        Outcome::RolledBack { from, to, reason } => {
            format!("rolled back to {from}: {to} failed: {reason}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcomes_read_clearly() {
        assert_eq!(
            describe(&Outcome::UpToDate {
                version: "1".into()
            }),
            "up to date (1)"
        );
        assert!(
            describe(&Outcome::Available {
                current: "1".into(),
                target: "2".into(),
                runner: Some("2.3".into())
            })
            .contains("runner 2.3")
        );
        assert!(
            describe(&Outcome::RolledBack {
                from: "1".into(),
                to: "2".into(),
                reason: "x".into()
            })
            .starts_with("rolled back to 1")
        );
    }
}
