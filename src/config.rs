//! `agent.toml` parsing and validation. See `plan/V2_AGENT_SPEC.md` §3.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    /// Operator block is metadata only. Populated by `enroll` in v0.1.x; in
    /// the v0.2.x pairing-code flow the dashboard owns operator identity
    /// and the agent only needs coordinator URL + backends + identity. The
    /// `setup` subcommand writes a placeholder so the field stays present
    /// for back-compat with v0.1.x configs.
    #[serde(default)]
    pub operator: Operator,
    pub coordinator: Coordinator,
    #[serde(default)]
    pub identity: Identity,
    #[serde(default, rename = "backends")]
    pub backends: Vec<Backend>,
    pub pricing: Pricing,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub observability: Observability,
}

#[derive(Debug, Deserialize, Default)]
pub struct Operator {
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub wallet: String,
    #[serde(default)]
    pub contact_email: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Coordinator {
    pub url: String,
    #[serde(default)]
    pub enrollment_code: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Identity {
    #[serde(default = "default_key_path")]
    pub key_path: String,
}

impl Default for Identity {
    fn default() -> Self {
        Self { key_path: default_key_path() }
    }
}

fn default_key_path() -> String {
    "~/.usepod-agent/identity.key".to_string()
}

impl Identity {
    /// Resolve `~` and return the absolute path to the identity file.
    pub fn expanded_key_path(&self) -> Result<PathBuf> {
        expand_home(&self.key_path)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backend {
    pub kind: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub markup: Option<f64>,
    #[serde(default)]
    pub models: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct Pricing {
    pub default_input_per_1m: u64,
    pub default_output_per_1m: u64,
    #[serde(default)]
    pub models: std::collections::BTreeMap<String, ModelPrice>,
}

#[derive(Debug, Deserialize)]
pub struct ModelPrice {
    pub input_per_1m: u64,
    pub output_per_1m: u64,
}

#[derive(Debug, Deserialize)]
pub struct Limits {
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: u32,
    #[serde(default)]
    pub max_tokens_per_minute: Option<u64>,
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_concurrent: default_max_concurrent(), max_tokens_per_minute: None }
    }
}

fn default_max_concurrent() -> u32 {
    8
}

#[derive(Debug, Deserialize, Default)]
pub struct Observability {
    #[serde(default = "default_prom_addr")]
    pub prometheus_addr: String,
    #[serde(default = "default_log_level")]
    pub log_level: String,
}

fn default_prom_addr() -> String {
    "127.0.0.1:9090".to_string()
}

fn default_log_level() -> String {
    "info".to_string()
}

/// Load and validate the agent config.
///
/// Lookup order if `path` is None:
///   1. `$XDG_CONFIG_HOME/usepod-agent/agent.toml` (or platform equivalent)
///   2. `./agent.toml`
pub fn load(path: Option<&Path>, allow_insecure: bool) -> Result<Config> {
    let resolved: PathBuf = match path {
        Some(p) => p.to_path_buf(),
        None => default_config_path()?,
    };
    let raw = std::fs::read_to_string(&resolved)
        .with_context(|| format!("reading config from {}", resolved.display()))?;
    let cfg: Config = toml::from_str(&raw)
        .with_context(|| format!("parsing TOML in {}", resolved.display()))?;
    validate(&cfg, allow_insecure)?;
    Ok(cfg)
}

fn default_config_path() -> Result<PathBuf> {
    if let Some(dirs) = directories::ProjectDirs::from("ai", "usepod", "usepod-agent") {
        let p = dirs.config_dir().join("agent.toml");
        if p.exists() {
            return Ok(p);
        }
    }
    let cwd = std::env::current_dir()?.join("agent.toml");
    if cwd.exists() {
        return Ok(cwd);
    }
    bail!(
        "no agent.toml found; pass --config or place one at \
         $XDG_CONFIG_HOME/usepod-agent/agent.toml or ./agent.toml"
    )
}

fn expand_home(p: &str) -> Result<PathBuf> {
    if let Some(rest) = p.strip_prefix("~/") {
        let dirs =
            directories::UserDirs::new().ok_or_else(|| anyhow!("could not resolve home dir"))?;
        return Ok(dirs.home_dir().join(rest));
    }
    if p == "~" {
        let dirs =
            directories::UserDirs::new().ok_or_else(|| anyhow!("could not resolve home dir"))?;
        return Ok(dirs.home_dir().to_path_buf());
    }
    Ok(PathBuf::from(p))
}

/// Validate per spec §3.1.
pub fn validate(cfg: &Config, allow_insecure: bool) -> Result<()> {
    // coordinator URL scheme
    let parsed = url::Url::parse(&cfg.coordinator.url)
        .with_context(|| format!("invalid coordinator.url: {}", cfg.coordinator.url))?;
    match parsed.scheme() {
        "wss" => {}
        "ws" if allow_insecure => {}
        "ws" => bail!("coordinator.url must be wss:// in production (use --allow-insecure to override)"),
        other => bail!("coordinator.url scheme must be wss or ws, got {other}"),
    }

    // backends: unique kind+url / kind+api_key_env, per-kind validation
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for (i, b) in cfg.backends.iter().enumerate() {
        let key = match b.kind.as_str() {
            "vllm" | "llamacpp" | "lmstudio" | "ollama" => {
                let url = b
                    .url
                    .as_deref()
                    .ok_or_else(|| anyhow!("backend[{i}] kind={} requires `url`", b.kind))?;
                url::Url::parse(url)
                    .with_context(|| format!("backend[{i}] has invalid url {url}"))?;
                (b.kind.clone(), url.to_string())
            }
            "openrouter" | "venice" => {
                let env = b.api_key_env.as_deref().ok_or_else(|| {
                    anyhow!("backend[{i}] kind={} requires `api_key_env`", b.kind)
                })?;
                // Per spec, presence in env is checked at startup. We don't fail validate-only
                // runs on a missing env var; the operator may be testing the config offline.
                (b.kind.clone(), env.to_string())
            }
            other => bail!("backend[{i}] has unknown kind {other}"),
        };
        if !seen.insert(key.clone()) {
            bail!("duplicate backend entry for {} / {}", key.0, key.1);
        }
    }

    // pricing required (defaults)
    if cfg.pricing.default_input_per_1m == 0 || cfg.pricing.default_output_per_1m == 0 {
        bail!("pricing.default_input_per_1m and default_output_per_1m must be > 0");
    }

    // limits
    if cfg.limits.max_concurrent < 1 || cfg.limits.max_concurrent > 256 {
        bail!(
            "limits.max_concurrent must be in [1, 256], got {}",
            cfg.limits.max_concurrent
        );
    }

    Ok(())
}
