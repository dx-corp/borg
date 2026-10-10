use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use borg::api;
use borg::config::QueenConfig;
use borg::drone::client::QueenClient;
use borg::drone::config::DroneConfig;
use borg::drone::exec::SystemExec;
use borg::drone::install::{self, InstallOptions, Platform};
use borg::drone::{Drone, pins};
use borg::enroll;
use borg::github::GithubClient;
use borg::probe::{self, system::SystemHost};
use borg::queen::Queen;
use borg::store::Store;
use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "borg", version, about = "Self-organizing CI fleet agent")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Probe this host and print its capacity as JSON.
    Probe {
        /// Emit compact single-line JSON instead of pretty-printed JSON.
        #[arg(long)]
        compact: bool,
    },
    /// Run or install the drone (the agent on every machine).
    Drone {
        #[command(subcommand)]
        command: DroneCommand,
    },
    /// Run or administer the queen (the coordinator).
    Queen {
        #[command(subcommand)]
        command: QueenCommand,
    },
}

#[derive(Args)]
struct ConfigArg {
    /// Path to the queen's JSON config file.
    #[arg(long, env = "BORG_QUEEN_CONFIG")]
    config: PathBuf,
}

#[derive(Subcommand)]
enum QueenCommand {
    /// Poll the queue, mint runners and serve the drone API.
    Run(ConfigArg),
    /// Enroll a drone and print its one-time token.
    Enroll {
        #[command(flatten)]
        config: ConfigArg,
        /// Drone name: lowercase letters, digits and dashes.
        name: String,
    },
    /// Stop granting a drone new leases (running jobs finish).
    Quarantine {
        #[command(flatten)]
        config: ConfigArg,
        /// Why the drone is quarantined; shown in `/status`.
        #[arg(long, default_value = "operator")]
        reason: String,
        /// Drone name.
        name: String,
    },
    /// Lift a quarantine.
    Release {
        #[command(flatten)]
        config: ConfigArg,
        /// Drone name.
        name: String,
    },
}

#[derive(Args)]
struct DroneConfigArg {
    /// Path to the drone's JSON config file.
    #[arg(long, env = "BORG_DRONE_CONFIG")]
    config: PathBuf,
}

#[derive(Subcommand)]
enum DroneCommand {
    /// Heartbeat to the queen and run the leases it grants.
    Run(DroneConfigArg),
    /// Install this host as a drone (systemd unit or LaunchDaemon).
    Install(InstallArgs),
}

#[derive(Args)]
struct InstallArgs {
    /// Queen base URL, e.g. https://queen.example.test:8443
    #[arg(long)]
    queen: String,
    /// Enrollment token from `borg queen enroll`.
    #[arg(long, env = "BORG_DRONE_TOKEN", hide_env_values = true)]
    token: String,
    /// PEM CA certificate that signs the queen's certificate (pins the CA).
    #[arg(long)]
    ca_cert: Option<PathBuf>,
    /// Allow a plain http:// queen URL (trusted network only).
    #[arg(long)]
    allow_insecure: bool,
    /// Owner cap: most vCPUs this host lends.
    #[arg(long)]
    max_vcpu: Option<u32>,
    /// Owner cap: most memory (MiB) this host lends.
    #[arg(long)]
    max_mem_mib: Option<u64>,
    /// Owner cap: most concurrent leases.
    #[arg(long)]
    max_leases: Option<u32>,
    /// Service account (Linux: created if missing; macOS: must exist).
    #[arg(long, default_value = "borg")]
    run_as: String,
    /// Install under this root instead of / (staging and tests).
    #[arg(long, default_value = "/")]
    root: PathBuf,
    /// Keep a host-local bare git mirror of this repository (owner/name);
    /// repeatable. Jobs get its path in BORG_GIT_MIRROR_<OWNER>_<NAME>.
    #[arg(long = "git-mirror")]
    git_mirrors: Vec<String>,
    /// Where mirrors fetch from (default https://github.com). Use an ssh base
    /// such as git@host: with a read-only deploy key for private repositories.
    #[arg(long)]
    git_remote_base: Option<String>,
    /// Print what would change without changing anything.
    #[arg(long)]
    dry_run: bool,
}

fn private_key(cfg: &QueenConfig) -> Result<Vec<u8>> {
    match &cfg.github.private_key_path {
        Some(path) => {
            std::fs::read(path).with_context(|| format!("read private key {}", path.display()))
        }
        None => std::env::var("BORG_APP_PRIVATE_KEY")
            .map(String::into_bytes)
            .context("set github.private_key_path or BORG_APP_PRIVATE_KEY"),
    }
}

async fn open_store(cfg: &QueenConfig) -> Result<Store> {
    Store::open(&cfg.db_path)
        .await
        .with_context(|| format!("open {}", cfg.db_path.display()))
}

async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = term.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

async fn run_queen(path: &Path) -> Result<()> {
    let cfg = QueenConfig::load(path)?;
    let github = GithubClient::new(
        &cfg.github.api_base,
        &cfg.github.org,
        &cfg.github.app_id,
        &private_key(&cfg)?,
    )?;
    let store = open_store(&cfg).await?;
    let listener = tokio::net::TcpListener::bind(cfg.listen)
        .await
        .with_context(|| format!("bind {}", cfg.listen))?;
    tracing::info!("queen listening on {}", cfg.listen);
    let queen = Arc::new(Queen::new(cfg, store, github));
    api::serve(queen, listener, wait_for_shutdown()).await?;
    Ok(())
}

async fn enroll_drone(path: &Path, name: &str) -> Result<()> {
    if !enroll::valid_drone_name(name) {
        bail!(
            "drone name must be 1-40 lowercase letters, digits or dashes, not starting with a dash"
        );
    }
    let cfg = QueenConfig::load(path)?;
    let store = open_store(&cfg).await?;
    let one_time_code = enroll::new_token();
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    store
        .enroll(name, &enroll::hash_token(&one_time_code), now)
        .await?;
    eprintln!("Enrolled drone {name}. The token is shown once; the queen stores only its hash.");
    println!("{one_time_code}");
    Ok(())
}

async fn set_quarantine(path: &Path, name: &str, reason: Option<&str>) -> Result<()> {
    let cfg = QueenConfig::load(path)?;
    let store = open_store(&cfg).await?;
    if !store.set_quarantine(name, reason).await? {
        bail!("no enrolled drone named {name}");
    }
    match reason {
        Some(r) => eprintln!("Quarantined drone {name} ({r}); it gets no new leases."),
        None => eprintln!("Released drone {name}."),
    }
    Ok(())
}

async fn run_drone(path: &Path) -> Result<()> {
    let cfg = DroneConfig::load(path)?;
    let token = cfg.load_token()?;
    let client = QueenClient::new(&cfg, token)?;
    let drone = Drone::new(cfg, client, Arc::new(SystemHost), Arc::new(SystemExec));
    drone.run(wait_for_shutdown()).await;
    Ok(())
}

fn install_drone(a: InstallArgs) -> Result<()> {
    let platform =
        Platform::current().context("borg drone install supports Linux, WSL and macOS")?;
    let pin = pins::pin_for(std::env::consts::OS, std::env::consts::ARCH).with_context(|| {
        format!(
            "no pinned actions-runner for {} {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let opts = InstallOptions {
        queen_url: a.queen,
        token: borg::secret::Secret::new(a.token),
        ca_cert: a.ca_cert,
        allow_insecure: a.allow_insecure,
        max_vcpu: a.max_vcpu,
        max_mem_mib: a.max_mem_mib,
        max_leases: a.max_leases,
        run_as: a.run_as,
        skip_root_check: a.root != Path::new("/"),
        root: a.root,
        platform,
        pin,
        binary: std::env::current_exe().context("locate this binary")?,
        dry_run: a.dry_run,
        drain_timeout_secs: 3600,
        git_mirrors: a.git_mirrors,
        git_remote_base: a.git_remote_base,
    };
    for line in install::install(&opts, &SystemExec)? {
        println!("{line}");
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Probe { compact } => {
            let capacity = probe::probe(&SystemHost);
            let json = if compact {
                serde_json::to_string(&capacity)
            } else {
                serde_json::to_string_pretty(&capacity)
            }
            .context("serialize capacity")?;
            println!("{json}");
            Ok(())
        }
        Command::Drone { command } => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "info".into()),
                )
                .init();
            match command {
                DroneCommand::Run(c) => run_drone(&c.config).await,
                DroneCommand::Install(a) => install_drone(a),
            }
        }
        Command::Queen { command } => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "info".into()),
                )
                .init();
            match command {
                QueenCommand::Run(c) => run_queen(&c.config).await,
                QueenCommand::Enroll { config, name } => enroll_drone(&config.config, &name).await,
                QueenCommand::Quarantine {
                    config,
                    reason,
                    name,
                } => set_quarantine(&config.config, &name, Some(&reason)).await,
                QueenCommand::Release { config, name } => {
                    set_quarantine(&config.config, &name, None).await
                }
            }
        }
    }
}
