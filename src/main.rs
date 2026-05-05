//! Use Pod provider-agent entrypoint.
//!
//! Connects an operator's inference backend(s) to the Use Pod coordinator
//! over a long-lived authenticated WebSocket. See `plan/V2_AGENT_SPEC.md`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing::info;
use tracing_subscriber::EnvFilter;

use provider_agent::{config, identity, service as service_mod, setup as setup_mod, ws_client};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser, Debug)]
#[command(
    name = "usepod-agent",
    version = VERSION,
    about = "Use Pod marketplace provider agent",
    propagate_version = true
)]
struct Cli {
    /// Path to agent.toml. If omitted, the agent searches the default locations.
    #[arg(short, long, value_name = "PATH", global = true)]
    config: Option<PathBuf>,

    /// Logging verbosity.
    #[arg(long, default_value = "info", global = true)]
    log_level: String,

    /// Allow ws:// (insecure) coordinator URLs.
    #[arg(long, default_value_t = false, global = true)]
    allow_insecure: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug, Clone)]
enum Command {
    /// Connect to coordinator and serve jobs (default).
    Run,
    /// Pair this agent with a Use Pod account via a one-time browser code.
    Setup {
        /// Override the coordinator base URL (default https://api.usepod.ai).
        #[arg(long, value_name = "URL")]
        coordinator: Option<String>,
    },
    /// Print enrollment status and identity public key.
    Enroll,
    /// Parse config and validate without networking.
    Validate,
    /// Print version and exit.
    Version,
    /// Re-run the official installer to upgrade to the latest release.
    Upgrade {
        /// Pin a specific release tag (default: latest).
        #[arg(long, value_name = "TAG")]
        version: Option<String>,
    },
    /// Manage the agent as a system service (systemd / launchd / SCM).
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[derive(Subcommand, Debug, Clone)]
enum ServiceAction {
    /// Install the agent as a system service and enable on boot.
    /// Requires sudo on Linux/macOS, Administrator on Windows.
    Install,
    /// Stop and remove the system service. Same elevation requirements.
    Uninstall,
    /// Start the installed service.
    Start,
    /// Stop the running service.
    Stop,
    /// Stop then start the service.
    Restart,
    /// One-shot status check (running / stopped / not installed).
    /// Exits 0 for running/stopped, 3 for not installed.
    Status,
    /// Tail the service log.
    Logs {
        /// Follow new log lines (Ctrl-C to stop).
        #[arg(short, long, default_value_t = false)]
        follow: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    init_tracing(&cli.log_level)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;

    runtime.block_on(async move {
        let command = cli.command.clone().unwrap_or(Command::Run);
        match command {
            Command::Version => {
                println!("usepod-agent {VERSION}");
                Ok(())
            }
            Command::Setup { coordinator } => cmd_setup(coordinator).await,
            Command::Validate => cmd_validate(&cli).await,
            Command::Enroll => cmd_enroll(&cli).await,
            Command::Run => cmd_run(&cli).await,
            Command::Upgrade { version } => cmd_upgrade(version).await,
            Command::Service { action } => cmd_service(&cli, action),
        }
    })
}

fn cmd_service(cli: &Cli, action: ServiceAction) -> Result<()> {
    use service_mod::{Action, InstallOptions};
    let resolved = match action {
        ServiceAction::Install => Action::Install(InstallOptions {
            // Propagate `--config` so the installed service points at the
            // same agent.toml the operator validated interactively. If they
            // didn't pass one, the service will search default locations
            // just like an interactive `run`.
            config: cli.config.clone(),
            // Same for log level — if they overrode the default, the service
            // should keep that override. Skip when it's the clap default
            // ("info") so the service unit isn't littered with redundancy.
            log_level: if cli.log_level != "info" {
                Some(cli.log_level.clone())
            } else {
                None
            },
        }),
        ServiceAction::Uninstall => Action::Uninstall,
        ServiceAction::Start => Action::Start,
        ServiceAction::Stop => Action::Stop,
        ServiceAction::Restart => Action::Restart,
        ServiceAction::Status => Action::Status,
        ServiceAction::Logs { follow } => Action::Logs { follow },
    };
    service_mod::run(resolved)
}

fn init_tracing(level: &str) -> Result<()> {
    let filter = EnvFilter::try_new(level)
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("info"));

    // When the agent runs under a service manager that doesn't capture
    // stdout/stderr (Windows SCM in particular), `usepod-agent service
    // install` injects USEPOD_AGENT_LOG_FILE into the service environment.
    // Append-write logs there in addition to the default writer so operators
    // get something to tail. journald + launchd already capture stdout, so
    // on Linux/macOS this is a no-op unless the env var is explicitly set.
    if let Ok(path_str) = std::env::var("USEPOD_AGENT_LOG_FILE") {
        let path = PathBuf::from(&path_str);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Non-rotating file appender; one log file, no rotation. Operators
        // who want rotation can layer logrotate/Get-Eventlog on top — keeping
        // the binary's behaviour simple and predictable.
        let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
        let file_name = path.file_name().unwrap_or_else(|| std::ffi::OsStr::new("usepod-agent.log"));
        let appender = tracing_appender::rolling::never(dir, file_name);
        // We deliberately drop the worker guard; the appender flushes per
        // write under the hood, and we don't want to thread a guard through
        // the runtime startup. Synchronous writes are fine for our log
        // volume.
        let (nb, _guard) = tracing_appender::non_blocking(appender);
        // Leak the guard so it lives for the lifetime of the process. This
        // is a one-time cost at startup.
        Box::leak(Box::new(_guard));
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_writer(nb)
            .with_ansi(false)
            .try_init()
            .ok();
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init()
        .ok(); // tolerate re-init in tests
    Ok(())
}

async fn cmd_setup(coordinator: Option<String>) -> Result<()> {
    let mut args = setup_mod::SetupArgs::defaults()?;
    if let Some(c) = coordinator {
        args.coordinator = c;
    }
    setup_mod::run(args).await
}

async fn cmd_validate(cli: &Cli) -> Result<()> {
    let cfg = config::load(cli.config.as_deref(), cli.allow_insecure)?;
    info!(
        backends = cfg.backends.len(),
        coordinator = %cfg.coordinator.url,
        "config valid"
    );
    println!("ok: parsed {} backend(s)", cfg.backends.len());
    Ok(())
}

async fn cmd_enroll(cli: &Cli) -> Result<()> {
    let cfg = config::load(cli.config.as_deref(), cli.allow_insecure)?;
    let identity = identity::load_or_create(&cfg.identity.expanded_key_path()?)?;
    println!("public_key: {}", identity.public_key_b64());
    match identity.provider_id.as_deref() {
        Some(pid) => println!("provider_id: {pid}"),
        None => println!("provider_id: <not yet enrolled>"),
    }
    if let Some(code) = cfg.coordinator.enrollment_code.as_deref() {
        println!("enrollment_code: {code}");
    }
    Ok(())
}

async fn cmd_run(cli: &Cli) -> Result<()> {
    let cfg = config::load(cli.config.as_deref(), cli.allow_insecure)?;
    let identity = identity::load_or_create(&cfg.identity.expanded_key_path()?)?;
    info!(
        public_key = %identity.public_key_b64(),
        coordinator = %cfg.coordinator.url,
        "starting agent"
    );
    ws_client::run(cfg, identity).await
}

async fn cmd_upgrade(version: Option<String>) -> Result<()> {
    use std::process::Command as ProcCommand;

    let installer_url = std::env::var("USEPOD_INSTALLER_URL")
        .unwrap_or_else(|_| "https://usepod.ai/install.sh".to_string());

    println!("usepod-agent {VERSION} → fetching installer from {installer_url}");
    if let Some(v) = version.as_deref() {
        println!("pinning to {v}");
    }

    let script = ProcCommand::new("curl")
        .args(["-fsSL", &installer_url])
        .output()
        .context("failed to invoke curl; install curl or rerun the installer manually")?;
    if !script.status.success() {
        anyhow::bail!(
            "curl failed ({}): {}",
            script.status,
            String::from_utf8_lossy(&script.stderr)
        );
    }

    let mut sh = ProcCommand::new("sh");
    sh.arg("-s").stdin(std::process::Stdio::piped());
    if let Some(v) = version {
        sh.env("USEPOD_VERSION", v);
    }
    let mut child = sh.spawn().context("failed to spawn sh")?;
    use std::io::Write;
    child
        .stdin
        .as_mut()
        .context("sh stdin unavailable")?
        .write_all(&script.stdout)
        .context("failed to pipe installer to sh")?;
    let status = child.wait().context("installer did not complete")?;
    if !status.success() {
        anyhow::bail!("installer exited with {status}");
    }
    println!("upgrade complete. Restart any running agent (systemctl restart usepod-agent, or relaunch).");
    Ok(())
}
