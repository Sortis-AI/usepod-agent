//! Use Pod provider-agent entrypoint.
//!
//! Connects an operator's inference backend(s) to the Use Pod coordinator
//! over a long-lived authenticated WebSocket. See `plan/V2_AGENT_SPEC.md`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing::info;
use tracing_subscriber::EnvFilter;

use provider_agent::{config, identity, setup as setup_mod, ws_client};

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
        }
    })
}

fn init_tracing(level: &str) -> Result<()> {
    let filter = EnvFilter::try_new(level)
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("info"));
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
