use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use borg::admin;
use borg::api;
use borg::config::{JanitorSettings, QueenConfig};
use borg::drone::client::QueenClient;
use borg::drone::config::DroneConfig;
use borg::drone::exec::SystemExec;
use borg::drone::install::{self, InstallOptions, Platform};
use borg::drone::{Drone, pins};
use borg::enroll;
use borg::gce::GceClient;
use borg::github::{GithubClient, TokenSource};
use borg::janitor::Janitor;
use borg::mirror_auth::MirrorAuth;
use borg::probe::{self, system::SystemHost};
use borg::queen::Queen;
use borg::requeue::Requeuer;
use borg::store::Store;
use borg::tailnet::{TailnetEnroll, TailscaledWhois};
use borg::update;
use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "borg", version, about = "Self-organizing CI fleet agent")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
#[allow(
    clippy::large_enum_variant,
    reason = "parsed once at startup; boxing a clap Args payload only adds noise"
)]
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
    /// Summarize stored job telemetry: time by job and step, queue wait per
    /// class, cancellation share and host-caused failures per drone.
    Report(ReportArgs),
    /// Explain a retained placement decision for a runner label or work unit.
    Explain(ExplainArgs),
    /// Run a command on the fleet when capacity is free, otherwise here.
    /// Everything after `--` is the command.
    Run(RunArgs),
    /// Prepare a hash-only registration request; does not grant fleet access.
    RegisterClient {
        #[arg(long)]
        name: String,
        #[arg(long)]
        token_file: PathBuf,
        #[arg(long)]
        request_file: PathBuf,
    },
    /// Update this host's borg (and, with --runner-only, just its actions-runner).
    /// Downloads and verifies the release, drains the service, swaps the binary
    /// (keeping borg.prev), restarts it, and rolls back if it does not come up.
    Update(UpdateArgs),
    /// Read and change burst capacity on a running queen through its
    /// authenticated control API. Every change needs a reason and is audited.
    QueenCtl(QueenCtlArgs),
    /// Run or administer the queen (the coordinator).
    Queen {
        #[command(subcommand)]
        command: QueenCommand,
    },
    /// Work with configuration files.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Parse and validate a queen (default) or drone config, TOML or JSON, and
    /// print the effective configuration with every default filled in.
    Check {
        /// The config file.
        path: PathBuf,
        /// Read it as a drone config instead of a queen config.
        #[arg(long)]
        drone: bool,
    },
}

#[derive(Args)]
struct UpdateArgs {
    /// Updater configuration (root-owned).
    #[arg(long, default_value = borg::update::config::DEFAULT_DRONE_CONFIG)]
    config: PathBuf,
    /// Install exactly this version (default: the newest release).
    #[arg(long)]
    version: Option<String>,
    /// Only report what would change.
    #[arg(long)]
    check: bool,
    /// Update only the pinned actions-runner; no restart.
    #[arg(long, conflicts_with = "version")]
    runner_only: bool,
    /// Read the target version from the drone's update request and leave a
    /// result for it (how the update path unit runs this).
    #[arg(long)]
    from_request: bool,
    /// Print the outcome as JSON.
    #[arg(long)]
    json: bool,
}

/// A unit scope on the command line.
#[derive(Clone, Copy, clap::ValueEnum)]
enum ScopeArg {
    Deploy,
    Main,
    Pr,
}

impl From<ScopeArg> for borg::classes::Scope {
    fn from(s: ScopeArg) -> Self {
        match s {
            ScopeArg::Deploy => Self::Deploy,
            ScopeArg::Main => Self::Main,
            ScopeArg::Pr => Self::Pr,
        }
    }
}

#[derive(Args)]
struct RunArgs {
    /// Queen base URL. Without it the command simply runs here.
    #[arg(long, env = "BORG_QUEEN")]
    queen: Option<String>,
    /// File holding the client token (from `borg queen enroll-client`).
    /// Default: BORG_CLIENT_TOKEN, else ~/.config/borg/client-token.
    #[arg(long, env = "BORG_CLIENT_TOKEN_FILE")]
    token_file: Option<PathBuf>,
    /// Run against the staged content (`git write-tree`), for pre-commit hooks.
    /// Default is HEAD's tree.
    #[arg(long, conflicts_with = "tree")]
    index: bool,
    /// Run against this tree or commit instead.
    #[arg(long)]
    tree: Option<String>,
    /// Repository as owner/name (default: from the origin remote).
    #[arg(long)]
    repo: Option<String>,
    /// Platform the command needs (default: this machine's, e.g. linux-x86_64).
    #[arg(long)]
    platform: Option<String>,
    /// Runner class that sizes the unit.
    #[arg(long, default_value = "medium")]
    class: String,
    /// Digest of the toolchain the result depends on (part of the cache key).
    #[arg(long, env = "BORG_TOOLCHAIN", default_value = "")]
    toolchain: String,
    /// Time limit for the command on the fleet, in seconds.
    #[arg(long)]
    timeout: Option<u64>,
    /// Run even if an identical command already passed for this tree.
    #[arg(long)]
    force: bool,
    /// Priority scope of the unit; the client token's maximum applies.
    #[arg(long, env = "BORG_RUN_SCOPE", value_enum, default_value = "pr")]
    scope: ScopeArg,
    /// Require fleet execution; infrastructure failures exit 125 without running here.
    #[arg(long, env = "BORG_RUN_REMOTE_ONLY")]
    remote_only: bool,
    /// Write a JSON execution result to this file, separately from command stdout.
    #[arg(long)]
    result_file: Option<PathBuf>,
    /// Seconds to wait for a drone before fallback (or failure with --remote-only).
    #[arg(long, env = "BORG_RUN_WAIT_SECS", default_value_t = 60)]
    wait_secs: u64,
    /// Upload a git bundle of the tree (since the merge base with origin/main)
    /// to the queen when no remote has it, so a drone can get it. Nothing is
    /// pushed anywhere.
    #[arg(long, env = "BORG_PUBLISH")]
    publish: bool,
    /// The command and its arguments.
    #[arg(last = true, required = true)]
    argv: Vec<String>,
}

#[derive(Args)]
struct ReportArgs {
    /// Fetch from a running queen, e.g. http://queen.internal:8080
    #[arg(long, conflicts_with = "config", required_unless_present = "config")]
    queen: Option<String>,
    /// Read the queen's SQLite file directly (run on the queen host).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Look back this many hours (capped by the queen's retention).
    #[arg(long, default_value_t = 24)]
    since_hours: u32,
    /// Rows in the by-job and by-step lists.
    #[arg(long, default_value_t = 20)]
    limit: usize,
    /// Print the JSON report instead of text.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ExplainArgs {
    /// Work-unit id or configured runner label.
    key: String,
    /// Fetch from a running queen.
    #[arg(long, conflicts_with = "config", required_unless_present = "config")]
    queen: Option<String>,
    /// Read the queen's SQLite file directly as a local operator.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Client token file for a remote unit query (same defaults as `borg run`).
    #[arg(long, env = "BORG_CLIENT_TOKEN_FILE")]
    token_file: Option<PathBuf>,
    /// Print the typed JSON history instead of text.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct QueenCtlArgs {
    /// Queen base URL. Default: `http://<listen>` from --config.
    #[arg(long, env = "BORG_QUEEN")]
    queen: Option<String>,
    /// The queen's config, for its URL and `control.token_path` when --queen
    /// or --token-file are not given (on the queen host).
    #[arg(
        long,
        env = "BORG_QUEEN_CONFIG",
        default_value = "/etc/borg/queen.json"
    )]
    config: PathBuf,
    /// File holding the control token. Default: BORG_CONTROL_TOKEN, else
    /// `control.token_path` from --config.
    #[arg(long, env = "BORG_CONTROL_TOKEN_FILE")]
    token_file: Option<PathBuf>,
    /// Who is making the change, recorded in the audit log. Default:
    /// BORG_ACTOR, else `$USER@<hostname>`.
    #[arg(long, env = "BORG_ACTOR")]
    actor: Option<String>,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(subcommand)]
    command: QueenCtlCommand,
}

#[derive(Subcommand)]
enum QueenCtlCommand {
    /// Burst state, effective settings with their source, spend against the
    /// budget, capacity per class, and the newest audit entries.
    Status,
    /// Change burst settings (within the ceilings in the queen config).
    Burst {
        #[command(subcommand)]
        command: BurstCtlCommand,
    },
    /// Control writes, newest first.
    Audit {
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
}

#[derive(Subcommand)]
enum BurstCtlCommand {
    /// Let burst create drones.
    On {
        #[arg(long)]
        reason: String,
    },
    /// Stop creating drones; the queen drains and deletes the ones it made
    /// once they hold no lease.
    Off {
        #[arg(long)]
        reason: String,
    },
    /// Set max_drones and/or the drone-hour budgets.
    Set {
        #[arg(long)]
        max_drones: Option<u32>,
        #[arg(long)]
        drone_hours_per_day: Option<f64>,
        #[arg(long)]
        drone_hours_per_month: Option<f64>,
        #[arg(long)]
        reason: String,
    },
    /// Close the enrollment circuit breaker after fixing why burst VMs did
    /// not enroll; the queen creates burst VMs again.
    ResetBreaker {
        #[arg(long)]
        reason: String,
    },
    /// Drop overrides so the configured values apply again.
    Clear {
        /// enabled, max-drones, drone-hours-per-day, drone-hours-per-month.
        #[arg(required = true, value_enum)]
        knobs: Vec<KnobArg>,
        #[arg(long)]
        reason: String,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum KnobArg {
    Enabled,
    MaxDrones,
    DroneHoursPerDay,
    DroneHoursPerMonth,
}

impl From<KnobArg> for borg::control::Knob {
    fn from(k: KnobArg) -> Self {
        match k {
            KnobArg::Enabled => Self::Enabled,
            KnobArg::MaxDrones => Self::MaxDrones,
            KnobArg::DroneHoursPerDay => Self::DroneHoursPerDay,
            KnobArg::DroneHoursPerMonth => Self::DroneHoursPerMonth,
        }
    }
}

/// `borg queen-ctl`: resolve the URL, token and actor, then call the API.
async fn queen_ctl(a: QueenCtlArgs) -> Result<()> {
    let cfg = if a.queen.is_none()
        || (a.token_file.is_none() && std::env::var_os("BORG_CONTROL_TOKEN").is_none())
    {
        Some(QueenConfig::load(&a.config).with_context(|| {
            format!(
                "read {} (or give --queen and --token-file)",
                a.config.display()
            )
        })?)
    } else {
        None
    };
    let base = match (&a.queen, &cfg) {
        (Some(u), _) => u.trim_end_matches('/').to_owned(),
        (None, Some(c)) => format!("http://{}", c.listen),
        (None, None) => bail!("give --queen"),
    };
    let token = if let Some(p) = &a.token_file {
        borg::control::load_token(p).map_err(|e| anyhow::anyhow!(e))?
    } else if let Ok(t) = std::env::var("BORG_CONTROL_TOKEN") {
        t.trim().to_owned()
    } else {
        let path = cfg
            .as_ref()
            .and_then(|c| c.control.as_ref())
            .map(|c| c.token_path.clone())
            .context("the queen config has no control block; give --token-file")?;
        borg::control::load_token(&path).map_err(|e| anyhow::anyhow!(e))?
    };
    let actor = a.actor.clone().unwrap_or_else(|| {
        let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_owned());
        let host = std::env::var("HOSTNAME")
            .ok()
            .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
            .map(|h| h.trim().to_owned())
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "unknown-host".to_owned());
        format!("{user}@{host}")
    });
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let send = |req: reqwest::RequestBuilder| async move {
        let resp = req.bearer_auth(&token).send().await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            bail!("queen answered {status}: {body}");
        }
        serde_json::from_str::<serde_json::Value>(&body).context("decode queen response")
    };
    let write =
        |w: borg::control::BurstWrite| http.post(format!("{base}/v1/control/burst")).json(&w);
    let body = |reason: String| borg::control::BurstWrite {
        actor: actor.clone(),
        reason,
        ..borg::control::BurstWrite::default()
    };
    let (out, audit_only) = match a.command {
        QueenCtlCommand::Status => (
            send(http.get(format!("{base}/v1/control/burst"))).await?,
            false,
        ),
        QueenCtlCommand::Audit { limit } => (
            send(http.get(format!("{base}/v1/control/audit?limit={limit}"))).await?,
            true,
        ),
        QueenCtlCommand::Burst {
            command: BurstCtlCommand::ResetBreaker { reason },
        } => (
            send(
                http.post(format!("{base}/v1/control/burst/reset-breaker"))
                    .json(&borg::control::BreakerReset {
                        actor: actor.clone(),
                        reason,
                    }),
            )
            .await?,
            false,
        ),
        QueenCtlCommand::Burst { command } => {
            let w = match command {
                BurstCtlCommand::On { reason } => borg::control::BurstWrite {
                    enabled: Some(true),
                    ..body(reason)
                },
                BurstCtlCommand::Off { reason } => borg::control::BurstWrite {
                    enabled: Some(false),
                    ..body(reason)
                },
                BurstCtlCommand::Set {
                    max_drones,
                    drone_hours_per_day,
                    drone_hours_per_month,
                    reason,
                } => borg::control::BurstWrite {
                    max_drones,
                    drone_hours_per_day,
                    drone_hours_per_month,
                    ..body(reason)
                },
                BurstCtlCommand::Clear { knobs, reason } => borg::control::BurstWrite {
                    clear: knobs.into_iter().map(Into::into).collect(),
                    ..body(reason)
                },
                BurstCtlCommand::ResetBreaker { .. } => {
                    bail!("reset-breaker is handled above")
                }
            };
            (send(write(w)).await?, false)
        }
    };
    if a.json {
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else if audit_only {
        for line in borg::control::render_audit(&out) {
            println!("{line}");
        }
    } else {
        for line in borg::control::render_status(&out) {
            println!("{line}");
        }
    }
    Ok(())
}

#[derive(Args)]
struct ConfigArg {
    /// Path to the queen's config file (TOML or JSON).
    #[arg(long, env = "BORG_QUEEN_CONFIG")]
    config: PathBuf,
}

#[derive(Subcommand)]
enum QueenCommand {
    /// Poll the queue, mint runners and serve the drone API.
    Run(ConfigArg),
    /// Enroll a drone. Prints only the one-time token on stdout (messages go
    /// to stderr), so `TOKEN=$(borg queen enroll ...)` is safe.
    Enroll {
        #[command(flatten)]
        config: ConfigArg,
        /// Drone name: lowercase letters, digits and dashes.
        name: String,
    },
    /// Revoke a temporary client without removing its replay tombstone.
    RevokeClient {
        #[command(flatten)]
        config: ConfigArg,
        name: String,
    },
    /// Approve a reviewed hash-only request as a temporary PR client.
    ApproveClient {
        #[command(flatten)]
        config: ConfigArg,
        #[arg(long)]
        request_file: PathBuf,
        #[arg(long, default_value_t = 3600)]
        lifetime_secs: u32,
    },
    /// Enroll a client locally and print its one-time token.
    EnrollClient {
        #[command(flatten)]
        config: ConfigArg,
        /// Client name: lowercase letters, digits and dashes.
        name: String,
        /// Highest unit scope this client may submit.
        #[arg(long, value_enum, default_value = "pr")]
        max_scope: ScopeArg,
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
    /// Administer the quorum rollout gate.
    Rollout {
        #[command(subcommand)]
        command: RolloutCommand,
    },
    /// Lift a quarantine.
    Release {
        #[command(flatten)]
        config: ConfigArg,
        /// Drone name.
        name: String,
    },
    /// List or remove enrolled drones.
    Drone {
        #[command(subcommand)]
        command: QueenDroneCommand,
    },
}

#[derive(Subcommand)]
enum QueenDroneCommand {
    /// Show every enrolled drone: state, last heartbeat, host and size.
    List(ConfigArg),
    /// Remove a drone: revokes its token and drops its unfinished leases.
    Remove {
        #[command(flatten)]
        config: ConfigArg,
        name: String,
    },
}

#[derive(Subcommand)]
enum RolloutCommand {
    /// Halt the current rollout now and tell every drone the previous version.
    Halt {
        #[command(flatten)]
        config: ConfigArg,
        #[arg(long, default_value = "operator")]
        reason: String,
    },
    /// Forget the current rollout so the next pass starts it over (use after
    /// fixing the cause of a halt, to retry the same target).
    Reset(ConfigArg),
}

#[derive(Args)]
struct DroneConfigArg {
    /// Path to the drone's config file (TOML or JSON).
    #[arg(long, env = "BORG_DRONE_CONFIG")]
    config: PathBuf,
    /// File holding the enrollment token (overrides `token_file` in the config).
    #[arg(long)]
    token_file: Option<PathBuf>,
}

#[derive(Subcommand)]
#[allow(
    clippy::large_enum_variant,
    reason = "parsed once at startup; boxing a clap Args payload only adds noise"
)]
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
    /// Enrollment token from `borg queen enroll`. Prefer `--token-file`: a
    /// token on the command line is visible in `ps`.
    #[arg(long, env = "BORG_DRONE_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// File holding the enrollment token.
    #[arg(long, conflicts_with = "token")]
    token_file: Option<PathBuf>,
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
    /// Account the drone daemon runs as (Linux: created if missing; macOS: must
    /// exist). It owns the config and the enrollment token.
    #[arg(long, default_value = "borg")]
    run_as: String,
    /// Linux: the separate unprivileged account leases run as, so job code
    /// cannot read the drone's token (created if missing).
    #[arg(long, default_value = "borg-job")]
    job_user: String,
    /// Run leases as the daemon's own account (they can then read its token).
    #[arg(long)]
    no_job_user: bool,
    /// Where host caches live (default: under the job user's home).
    #[arg(long)]
    cache_root: Option<PathBuf>,
    /// Take ownership of an existing --cache-root, keeping its contents (for
    /// example a host's warm compile cache).
    #[arg(long, requires = "cache_root")]
    adopt_cache: bool,
    /// Add this label to every runner the drone registers; repeatable.
    #[arg(long = "extra-label")]
    extra_labels: Vec<String>,
    /// `shared` (default), `deploy` or `deploy-only`: a deploy drone runs only
    /// deploy and main jobs, a deploy-only drone only deploy jobs, a shared
    /// drone never runs deploy jobs.
    #[arg(long, value_parser = parse_trust, default_value = "shared")]
    trust: borg::classes::Trust,
    /// Start standby deploy runners after this many seconds without a queen
    /// answer; 0 turns fail-open off.
    #[arg(long, default_value_t = 120)]
    fail_open_after_secs: u64,
    /// KEY=VALUE injected into every lease's environment; repeatable.
    #[arg(long = "runner-env", value_parser = parse_env_pair)]
    runner_env: Vec<(String, String)>,
    /// Absolute path of a .sh script run when a job starts.
    #[arg(long)]
    job_started_hook: Option<PathBuf>,
    /// Absolute path of a .sh script run when a job completes.
    #[arg(long)]
    job_completed_hook: Option<PathBuf>,
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
    /// Release manifest `borg update` reads (default: the public release).
    #[arg(long)]
    update_manifest_url: Option<String>,
    /// Do not install the self-update units.
    #[arg(long)]
    no_self_update: bool,
    /// Print what would change without changing anything.
    #[arg(long)]
    dry_run: bool,
}

fn parse_trust(s: &str) -> Result<borg::classes::Trust, String> {
    serde_json::from_value(serde_json::Value::from(s))
        .map_err(|_| "expected shared, deploy or deploy-only".to_owned())
}

fn parse_env_pair(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .filter(|(k, _)| !k.is_empty())
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .ok_or_else(|| "expected KEY=VALUE".to_owned())
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

/// What the janitor's read credential (`compare_auth`) may do: ancestry
/// compares and pull-request lookups by branch.
static JANITOR_READ_PERMISSIONS: std::sync::LazyLock<serde_json::Value> =
    std::sync::LazyLock::new(|| serde_json::json!({"contents": "read", "pull_requests": "read"}));

/// The janitor's token: `token_path`, else `BORG_JANITOR_TOKEN`, else the
/// queen's own App installation token (the App then needs Actions: write).
fn janitor_token(
    settings: &JanitorSettings,
    cfg: &QueenConfig,
    rate_limit: borg::github::RateLimit,
) -> Result<TokenSource> {
    let raw = match &settings.token_path {
        Some(path) => Some(
            std::fs::read_to_string(path)
                .with_context(|| format!("read janitor token {}", path.display()))?,
        ),
        None => std::env::var("BORG_JANITOR_TOKEN").ok(),
    };
    match raw {
        Some(raw) => {
            let token = raw.trim();
            if token.is_empty() {
                bail!("the janitor token is empty");
            }
            Ok(TokenSource::Static(borg::secret::Secret::new(token)))
        }
        None => Ok(TokenSource::App(Arc::new(
            GithubClient::new(
                &cfg.github.api_base,
                &cfg.github.org,
                &cfg.github.app_id,
                &private_key(cfg)?,
            )?
            .with_rate_limit(rate_limit),
        ))),
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
    let mut gce = None;
    if let Some(b) = cfg.burst.as_ref() {
        if b.provisions() {
            // Validation guarantees the paths are set; they must also be
            // readable now, so a misconfigured queen fails at start and not
            // at the first create.
            let g = b.gce();
            for (name, path) in [
                ("tailscale_authkey_path", g.tailscale_authkey_path.as_ref()),
                ("toolchain_script_path", g.toolchain_script_path.as_ref()),
            ] {
                let path = path.context("burst live path missing")?;
                std::fs::metadata(path)
                    .with_context(|| format!("burst.gce.{name}: {}", path.display()))?;
            }
            let key = g.credentials_path.as_ref().context("credentials_path")?;
            let client = GceClient::from_key_file(key, &g.project, &g.api_base)
                .with_context(|| format!("burst.gce.credentials_path: {}", key.display()))?;
            tracing::info!(
                "burst capacity LIVE (configured enabled={}; control overrides may change it): up to {} {} spot VMs in {} ({}), {} drone-hours/day, {} /month",
                b.enabled,
                b.max_drones,
                g.machine_type,
                g.project,
                g.zones.join(","),
                b.budget.drone_hours_per_day,
                b.budget.drone_hours_per_month,
            );
            gce = Some(client);
        } else {
            tracing::info!(
                "burst capacity enabled (dry-run: decisions are logged as burst_would_* health events; no VM is created), max {} drones, {} drone-hours/day, {} /month",
                b.max_drones,
                b.budget.drone_hours_per_day,
                b.budget.drone_hours_per_month,
            );
        }
    }
    let mut requeuer = None;
    let janitor = match &cfg.supersede_janitor {
        Some(settings) => {
            let token = janitor_token(settings, &cfg, github.rate_limit())?;
            let rate_limit = token.rate_limit();
            let mode = if settings.dry_run {
                "dry-run"
            } else {
                "cancelling"
            };
            let credential_kind = match token {
                TokenSource::Static(_) => "static",
                TokenSource::App(_) => "App installation",
            };
            tracing::info!("supersede janitor enabled ({mode}, {credential_kind} credential)");
            if cfg.preemption.is_some() {
                requeuer = Some(
                    Requeuer::with_token_source(&cfg.github.api_base, token.clone(), settings)?
                        .with_rate_limit(rate_limit.clone()),
                );
            }
            let janitor =
                Janitor::with_token_source(&cfg.github.api_base, token, settings.clone())?
                    .with_rate_limit(rate_limit);
            Some(match &settings.compare_auth {
                Some(c) => {
                    let pem = c.private_key(std::env::var("BORG_MIRROR_APP_PRIVATE_KEY").ok())?;
                    let client =
                        GithubClient::new(&cfg.github.api_base, &cfg.github.org, &c.app_id, &pem)?;
                    tracing::info!(
                        "janitor compares read {} repositories with app {}",
                        c.repositories.len(),
                        c.app_id
                    );
                    janitor.with_compare_auth(
                        MirrorAuth::new(client, &cfg.github.org, &c.repositories)
                            .with_permissions(JANITOR_READ_PERMISSIONS.clone()),
                    )
                }
                None => janitor,
            })
        }
        None => None,
    };
    let tailnet = cfg.tailnet_enroll.clone().map(|settings| {
        tracing::info!(
            "tailnet enrollment enabled via {}",
            settings.socket_path.display()
        );
        TailnetEnroll {
            verifier: Arc::new(TailscaledWhois::new(settings.socket_path.clone())),
            settings,
        }
    });
    let control_token = match &cfg.control {
        Some(c) => Some(
            borg::control::load_token(&c.token_path)
                .map_err(|e| anyhow::anyhow!("control.token_path: {e}"))?,
        ),
        None => None,
    };
    let mut queen = Queen::new(cfg, store, github);
    if let Some(token) = control_token {
        tracing::info!("control API enabled");
        queen = queen.with_control_token(&token);
    }
    if let Some(client) = gce {
        queen = queen.with_gce(client);
    }
    if let Some(t) = tailnet {
        queen = queen.with_tailnet_enroll(t);
    }
    if let Some(j) = janitor {
        queen = queen.with_janitor(j);
    }
    if let Some(r) = requeuer {
        queen = queen.with_requeuer(r);
    }
    if let Some(settings) = &queen.config().mirror_auth {
        let pem = settings.private_key(std::env::var("BORG_MIRROR_APP_PRIVATE_KEY").ok())?;
        let client = GithubClient::new(
            &queen.config().github.api_base,
            &queen.config().github.org,
            &settings.app_id,
            &pem,
        )?;
        let mirror = MirrorAuth::new(client, &queen.config().github.org, &settings.repositories);
        tracing::info!(
            "git mirror credentials enabled for {} repositories",
            settings.repositories.len()
        );
        queen = queen.with_mirror_auth(mirror);
    }
    let queen = Arc::new(queen);
    api::serve(queen, listener, wait_for_shutdown()).await?;
    Ok(())
}

async fn enroll_drone(path: &Path, name: &str) -> Result<()> {
    let cfg = QueenConfig::load(path)?;
    let store = open_store(&cfg).await?;
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let one_time_code = admin::enroll_drone(&store, name, now).await?;
    eprintln!("Enrolled drone {name}. The token is shown once; the queen stores only its hash.");
    // stdout carries the token and nothing else.
    println!("{one_time_code}");
    Ok(())
}

async fn report(a: ReportArgs) -> Result<()> {
    let window = i64::from(a.since_hours.max(1)) * 3600;
    let report = match (&a.queen, &a.config) {
        (Some(url), _) => {
            let url = format!(
                "{}/report?since_secs={window}&limit={}",
                url.trim_end_matches('/'),
                a.limit
            );
            reqwest::get(&url)
                .await
                .and_then(reqwest::Response::error_for_status)
                .with_context(|| format!("GET {url}"))?
                .json::<borg::telemetry::Report>()
                .await
                .context("decode report")?
        }
        (None, Some(path)) => {
            let cfg = QueenConfig::load(path)?;
            let store = open_store(&cfg).await?;
            let now = time::OffsetDateTime::now_utc().unix_timestamp();
            let rows = store
                .telemetry_since(now - window, borg::telemetry::MAX_REPORT_ROWS)
                .await?;
            borg::telemetry::build_report(&rows, now, window, a.limit)
        }
        (None, None) => bail!("give --queen or --config"),
    };
    if a.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", borg::telemetry::render_text(&report));
    }
    Ok(())
}

async fn explain(a: ExplainArgs) -> Result<()> {
    use borg::explain::{Explanation, PlacementKind};
    let report = match (&a.queen, &a.config) {
        (Some(url), _) => {
            let token = client_token(a.token_file.as_deref()).map(borg::secret::Secret::new);
            borg::explain::fetch(url, &a.key, token.as_ref())
                .await
                .context("fetch placement history")?
        }
        (None, Some(path)) => {
            let cfg = QueenConfig::load(path)?;
            let store = open_store(&cfg).await?;
            let (kind, key) = match cfg.classes.parse_label(&a.key) {
                Some((class, scope)) => {
                    (PlacementKind::Runner, cfg.classes.job_label(class, scope))
                }
                None => (PlacementKind::Unit, a.key.clone()),
            };
            Explanation::new(a.key, store.placement_history(kind, &key, None).await?)
        }
        (None, None) => bail!("give --queen or --config"),
    };
    if a.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", borg::explain::render_text(&report));
    }
    Ok(())
}

async fn enroll_client(path: &Path, name: &str, max_scope: borg::classes::Scope) -> Result<()> {
    if !enroll::valid_drone_name(name) {
        bail!(
            "client name must be 1-40 lowercase letters, digits or dashes, not starting with a dash"
        );
    }
    let cfg = QueenConfig::load(path)?;
    let store = open_store(&cfg).await?;
    let token = enroll::new_token();
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    store
        .enroll_client_scoped(name, &enroll::hash_token(&token), max_scope, now)
        .await?;
    eprintln!("Enrolled client {name}. The token is shown once; the queen stores only its hash.");
    println!("{token}");
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

async fn rollout_admin(path: &Path, halt: Option<&str>) -> Result<()> {
    let cfg = QueenConfig::load(path)?;
    let store = open_store(&cfg).await?;
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    match halt {
        Some(reason) => {
            let Some(mut st) = store.rollout_state().await? else {
                bail!("no rollout is in progress");
            };
            st.phase = borg::rollout::Phase::Halted;
            st.halted_reason = Some(format!("operator: {reason}"));
            store.save_rollout_state(&st, now).await?;
            store
                .log_rollout(
                    now,
                    &format!("rollout of {} HALTED by an operator: {reason}", st.target),
                )
                .await?;
            eprintln!(
                "Halted the rollout of {}; drones are told {}.",
                st.target, st.previous
            );
        }
        None => {
            store.clear_rollout_state().await?;
            store
                .log_rollout(now, "rollout state reset by an operator")
                .await?;
            eprintln!("Rollout state cleared; the next pass starts over.");
        }
    }
    Ok(())
}

async fn run_update(args: update::cli::Args) -> Result<()> {
    match update::cli::run(args).await? {
        0 => Ok(()),
        _ => bail!("the update was rolled back; see the line above"),
    }
}

async fn run_drone(path: &Path, token_file: Option<PathBuf>) -> Result<()> {
    let mut cfg = DroneConfig::load(path)?;
    if token_file.is_some() {
        cfg.token_file = token_file;
    }
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
    let token = match (&a.token, &a.token_file) {
        (Some(t), _) => t.clone(),
        (None, Some(path)) => std::fs::read_to_string(path)
            .with_context(|| format!("read {}", path.display()))?
            .trim()
            .to_owned(),
        (None, None) => bail!("pass --token-file (preferred) or --token"),
    };
    let job_user = (platform == Platform::Linux && !a.no_job_user).then(|| a.job_user.clone());
    if platform == Platform::MacOs {
        eprintln!("note: macOS runs jobs as the same account as the drone (no separate job user)");
    }
    let opts = InstallOptions {
        queen_url: a.queen,
        token: borg::secret::Secret::new(token),
        ca_cert: a.ca_cert,
        allow_insecure: a.allow_insecure,
        max_vcpu: a.max_vcpu,
        max_mem_mib: a.max_mem_mib,
        max_leases: a.max_leases,
        run_as: a.run_as,
        job_user,
        cache_root: a.cache_root,
        adopt_cache: a.adopt_cache,
        extra_labels: a.extra_labels,
        trust: a.trust,
        fail_open_after_secs: a.fail_open_after_secs,
        runner_env: a.runner_env.into_iter().collect(),
        job_started_hook: a.job_started_hook,
        job_completed_hook: a.job_completed_hook,
        skip_root_check: a.root != Path::new("/"),
        root: a.root,
        platform,
        pin,
        binary: std::env::current_exe().context("locate this binary")?,
        dry_run: a.dry_run,
        drain_timeout_secs: 3600,
        git_mirrors: a.git_mirrors,
        git_remote_base: a.git_remote_base,
        update_manifest_url: a.update_manifest_url,
        no_self_update: a.no_self_update,
    };
    for line in install::install(&opts, &SystemExec)? {
        println!("{line}");
    }
    Ok(())
}

async fn queen_drone_admin(command: QueenDroneCommand) -> Result<()> {
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    match command {
        QueenDroneCommand::List(c) => {
            let cfg = QueenConfig::load(&c.config)?;
            let store = open_store(&cfg).await?;
            for line in admin::list_lines(&store.drones().await?, now, cfg.drone_stale_secs) {
                println!("{line}");
            }
        }
        QueenDroneCommand::Remove { config, name } => {
            let cfg = QueenConfig::load(&config.config)?;
            let store = open_store(&cfg).await?;
            let expired = admin::remove_drone(&store, &name).await?;
            println!(
                "removed drone {name}; its token no longer works; {expired} unfinished lease(s) dropped"
            );
        }
    }
    Ok(())
}

fn client_token(file: Option<&Path>) -> Option<String> {
    if let Ok(t) = std::env::var("BORG_CLIENT_TOKEN") {
        return Some(t.trim().to_owned()).filter(|t| !t.is_empty());
    }
    let path = file.map(Path::to_path_buf).or_else(|| {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/borg/client-token"))
    })?;
    let text = std::fs::read_to_string(path).ok()?;
    Some(text.trim().to_owned()).filter(|t| !t.is_empty())
}

async fn run_command(a: RunArgs) -> u8 {
    let dir = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("borg run: {e}");
            return 2;
        }
    };
    let opts = borg::run::RunOptions {
        token: a
            .queen
            .as_ref()
            .and_then(|_| client_token(a.token_file.as_deref()))
            .map(borg::secret::Secret::new),
        queen: a.queen,
        dir,
        tree: match (a.index, a.tree) {
            (_, Some(t)) => borg::run::TreeSource::Explicit(t),
            (true, None) => borg::run::TreeSource::Index,
            (false, None) => borg::run::TreeSource::Head,
        },
        repo: a.repo,
        platform: a.platform.unwrap_or_else(borg::run::local_platform),
        class: a.class,
        toolchain: a.toolchain,
        timeout_secs: a.timeout,
        force: a.force,
        scope: borg::classes::Scope::from(a.scope).as_str().to_owned(),
        wait_secs: a.wait_secs,
        publish: a.publish,
        remote_only: a.remote_only,
        result_file: a.result_file,
        argv: a.argv,
    };
    let code = borg::run::run(&opts, &mut std::io::stdout()).await;
    u8::try_from(code).unwrap_or(if code < 0 { 1 } else { u8::MAX })
}

#[tokio::main]
async fn main() -> Result<std::process::ExitCode> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let cli = Cli::parse();
    if let Command::Run(a) = cli.command {
        return Ok(std::process::ExitCode::from(run_command(a).await));
    }
    dispatch(cli.command).await?;
    Ok(std::process::ExitCode::SUCCESS)
}

async fn dispatch(command: Command) -> Result<()> {
    match command {
        Command::Explain(a) => explain(a).await,
        Command::Run(_) => Ok(()),
        Command::RegisterClient {
            name,
            token_file,
            request_file,
        } => {
            borg::client_registration::prepare(&name, &token_file, &request_file)?;
            eprintln!(
                "Registration request written; fleet access requires administrator approval."
            );
            Ok(())
        }
        Command::Update(a) => {
            run_update(update::cli::Args {
                config: a.config,
                version: a.version,
                check: a.check,
                runner_only: a.runner_only,
                from_request: a.from_request,
                json: a.json,
            })
            .await
        }
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
        Command::Report(a) => report(a).await,
        Command::QueenCtl(a) => queen_ctl(a).await,
        Command::Config {
            command: ConfigCommand::Check { path, drone },
        } => {
            // Debug output redacts secrets (see `borg::secret::Secret`).
            if drone {
                let cfg = DroneConfig::load(&path)?;
                println!("{cfg:#?}");
            } else {
                let cfg = QueenConfig::load(&path)?;
                println!("{cfg:#?}");
            }
            eprintln!("{}: ok", path.display());
            Ok(())
        }
        Command::Drone { command } => {
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "info".into()),
                )
                .init();
            match command {
                DroneCommand::Run(c) => run_drone(&c.config, c.token_file).await,
                DroneCommand::Install(a) => install_drone(a),
            }
        }
        Command::Queen { command } => {
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
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
                QueenCommand::RevokeClient { config, name } => {
                    let cfg = QueenConfig::load(&config.config)?;
                    let store = open_store(&cfg).await?;
                    if !borg::client_registration::revoke(&store, &name).await? {
                        bail!("temporary client not found; legacy clients are unchanged");
                    }
                    eprintln!("Revoked temporary client {name}; accepted work is not cancelled");
                    Ok(())
                }
                QueenCommand::ApproveClient {
                    config,
                    request_file,
                    lifetime_secs,
                } => {
                    let request = borg::client_registration::read_request(&request_file)?;
                    let cfg = QueenConfig::load(&config.config)?;
                    let store = open_store(&cfg).await?;
                    let now = time::OffsetDateTime::now_utc().unix_timestamp();
                    let expires =
                        borg::client_registration::approve(&store, &request, lifetime_secs, now)
                            .await?;
                    eprintln!(
                        "Approved PR client {} until Unix timestamp {expires}",
                        request.name
                    );
                    Ok(())
                }
                QueenCommand::EnrollClient {
                    config,
                    name,
                    max_scope,
                } => enroll_client(&config.config, &name, max_scope.into()).await,
                QueenCommand::Rollout { command } => match command {
                    RolloutCommand::Halt { config, reason } => {
                        rollout_admin(&config.config, Some(&reason)).await
                    }
                    RolloutCommand::Reset(config) => rollout_admin(&config.config, None).await,
                },
                QueenCommand::Release { config, name } => {
                    set_quarantine(&config.config, &name, None).await
                }
                QueenCommand::Drone { command } => queen_drone_admin(command).await,
            }
        }
    }
}
