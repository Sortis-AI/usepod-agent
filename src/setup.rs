//! `usepod-agent setup` — device-flow pairing on first run.
//!
//! Replaces the v0.1.x ceremony of "enroll on the dashboard, copy a
//! 40-char host_token, paste it into agent.toml". The flow now:
//!
//!   1. Generate (or load) the agent's Ed25519 identity.
//!   2. Probe well-known local backend ports (vLLM :8000, llama.cpp
//!      :8080, LM Studio :1234, Ollama :11434).
//!   3. POST /v1/host/pair/issue to the coordinator with the agent
//!      pubkey; receive a short pair_code + poll_token.
//!   4. Print the pair_code prominently and start long-polling
//!      /v1/host/pair/poll, sending detected backends as capabilities
//!      so the dashboard can render the model picker live.
//!   5. When the operator hits Activate in the dashboard, the next poll
//!      response delivers host_token + provider_id + activated_models.
//!   6. Write a complete agent.toml from the discovered + operator-
//!      configured state. Operator never edits a file.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::identity::Identity;

const DEFAULT_COORDINATOR: &str = "https://api.usepod.ai";
const POLL_INTERVAL: Duration = Duration::from_millis(800);

#[derive(Debug, Clone)]
pub struct SetupArgs {
    pub coordinator: String,
    pub config_path: PathBuf,
    pub identity_path: PathBuf,
}

impl SetupArgs {
    pub fn defaults() -> Result<Self> {
        let config_path = default_config_path()?;
        let identity_path = default_identity_path()?;
        Ok(Self {
            coordinator: DEFAULT_COORDINATOR.into(),
            config_path,
            identity_path,
        })
    }
}

pub async fn run(args: SetupArgs) -> Result<()> {
    println!("usepod-agent setup");
    println!();

    // Identity — generate if missing. Persisting the keypair before pairing
    // means a re-run of `setup` after a partial pair stays continuous.
    let identity = crate::identity::load_or_create(&args.identity_path)
        .context("identity load/create")?;
    info!(public_key = %identity.public_key_b64(), "identity ready");

    // Backend autodetection. Probes well-known ports with a short timeout.
    let backends = probe_local_backends().await;
    if backends.is_empty() {
        println!("No local backends detected on standard ports.");
        println!("That's OK — you can pair anyway and configure backends later");
        println!("from the dashboard, or install one of:");
        println!("  - Ollama:    https://ollama.ai");
        println!("  - vLLM:      pip install vllm && vllm serve <model>");
        println!("  - llama.cpp: ./llama-server -m <model>.gguf");
        println!();
    } else {
        println!("Detected backends:");
        for b in &backends {
            println!(
                "  ✓ {:<10} at {} ({} models)",
                b.kind,
                b.url,
                b.models.len()
            );
        }
        println!();
    }

    // Issue pair code.
    let http = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let issue = issue_pair_code(&http, &args.coordinator, &identity).await?;

    print_pair_banner(&issue.pair_code, &args.coordinator);

    // Long-poll for claim.
    let active = match poll_until_active(&http, &args.coordinator, &issue.poll_token, &backends)
        .await?
    {
        PollOutcome::Active(a) => a,
        PollOutcome::Expired => {
            println!();
            println!("✗ Pair code expired. Run `usepod-agent setup` again.");
            std::process::exit(1);
        }
    };

    println!();
    println!("✓ Paired as provider {}", active.provider_id);
    if !active.activated_models.is_empty() {
        println!("  Operator activated {} model(s):", active.activated_models.len());
        for m in &active.activated_models {
            println!("    - {}", m.model_id);
        }
    }

    // Write complete agent.toml.
    let toml_text = render_paired_config(&args, &identity, &backends, &active);
    if let Some(parent) = args.config_path.parent() {
        std::fs::create_dir_all(parent).context("create config dir")?;
    }
    std::fs::write(&args.config_path, toml_text).context("write agent.toml")?;
    println!();
    println!("Wrote {}", args.config_path.display());
    println!();
    println!("Run the agent:");
    println!("  usepod-agent run");
    println!();

    Ok(())
}

// ---------------------------------------------------------------------------
// Pair code banner
// ---------------------------------------------------------------------------

fn print_pair_banner(pair_code: &str, coordinator: &str) {
    let pair_url = if coordinator.contains("api.usepod.ai") {
        "https://usepod.ai/host/pair".to_string()
    } else {
        format!("{}/host/pair", coordinator.trim_end_matches('/'))
    };
    println!();
    println!("┌─────────────────────────────────────────────────────────┐");
    println!("│ Pair this agent with your Use Pod account:              │");
    println!("│                                                         │");
    println!("│   1. Visit  {:43} │", pair_url);
    println!("│   2. Code:  {:<43} │", pair_code);
    println!("│                                                         │");
    println!("│ Code expires in 10 minutes. Waiting for pairing...      │");
    println!("└─────────────────────────────────────────────────────────┘");
    println!();
}

// ---------------------------------------------------------------------------
// HTTP — pair issue / poll
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct IssueResponse {
    pair_code: String,
    poll_token: String,
    #[allow(dead_code)]
    expires_at: String,
}

async fn issue_pair_code(
    http: &Client,
    coordinator: &str,
    identity: &Identity,
) -> Result<IssueResponse> {
    let url = format!("{}/v1/host/pair/issue", coordinator.trim_end_matches('/'));
    let body = serde_json::json!({
        "agent_pubkey": identity.public_key_b64(),
    });
    let resp = http
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("POST /v1/host/pair/issue")?
        .error_for_status()
        .context("issue response status")?;
    let parsed: IssueResponse = resp.json().await.context("issue response parse")?;
    Ok(parsed)
}

#[derive(Debug, Clone)]
struct ActivePairing {
    #[allow(dead_code)]
    host_token: String,
    provider_id: String,
    activated_models: Vec<ActivatedModel>,
}

enum PollOutcome {
    Active(ActivePairing),
    Expired,
}

#[derive(Debug, Deserialize, Clone)]
struct ActivatedModel {
    model_id: String,
    #[serde(default)]
    input_per_1m: u64,
    #[serde(default)]
    output_per_1m: u64,
    #[serde(default = "default_max_concurrent_dl")]
    max_concurrent: u32,
}

fn default_max_concurrent_dl() -> u32 {
    4
}

async fn poll_until_active(
    http: &Client,
    coordinator: &str,
    poll_token: &str,
    backends: &[ProbedBackend],
) -> Result<PollOutcome> {
    let url = format!("{}/v1/host/pair/poll", coordinator.trim_end_matches('/'));
    let capabilities = capabilities_payload(backends);
    loop {
        let body = serde_json::json!({
            "poll_token":   poll_token,
            "capabilities": capabilities,
        });
        let resp = http
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("POST /v1/host/pair/poll")?
            .error_for_status()
            .context("poll response status")?;
        let v: Value = resp.json().await.context("poll response parse")?;
        let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
        match status {
            "pending" => {
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
            "expired" => return Ok(PollOutcome::Expired),
            "active" => {
                let host_token = v
                    .get("host_token")
                    .and_then(|s| s.as_str())
                    .ok_or_else(|| anyhow!("active response missing host_token"))?
                    .to_string();
                let provider_id = v
                    .get("provider_id")
                    .and_then(|s| s.as_str())
                    .ok_or_else(|| anyhow!("active response missing provider_id"))?
                    .to_string();
                let activated_models: Vec<ActivatedModel> = v
                    .get("model_config")
                    .cloned()
                    .and_then(|mc| serde_json::from_value(mc).ok())
                    .unwrap_or_default();
                return Ok(PollOutcome::Active(ActivePairing {
                    host_token,
                    provider_id,
                    activated_models,
                }));
            }
            other => bail!("unexpected poll status: {other}"),
        }
    }
}

fn capabilities_payload(backends: &[ProbedBackend]) -> Value {
    let backends_json: Vec<Value> = backends
        .iter()
        .map(|b| {
            serde_json::json!({
                "kind":   b.kind,
                "url":    b.url,
                "models": b.models,
            })
        })
        .collect();
    serde_json::json!({
        "backends":      backends_json,
        "agent_version": env!("CARGO_PKG_VERSION"),
    })
}

// ---------------------------------------------------------------------------
// Backend autodetection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ProbedBackend {
    pub kind: String,
    pub url: String,
    pub models: Vec<String>,
}

const PROBE_TIMEOUT: Duration = Duration::from_millis(800);

pub async fn probe_local_backends() -> Vec<ProbedBackend> {
    let probes = vec![
        ("vllm",     "http://localhost:8000",  probe_openai_compat as ProbeFn),
        ("llamacpp", "http://localhost:8080",  probe_openai_compat),
        ("lmstudio", "http://localhost:1234",  probe_openai_compat),
        ("ollama",   "http://localhost:11434", probe_ollama),
    ];
    let mut out = Vec::new();
    for (kind, url, probe) in probes {
        match probe(url).await {
            Ok(models) if !models.is_empty() => {
                out.push(ProbedBackend {
                    kind: kind.into(),
                    url: url.into(),
                    models,
                });
            }
            Ok(_) => debug!(kind, url, "backend reachable but no models"),
            Err(e) => debug!(kind, url, %e, "backend probe failed"),
        }
    }
    out
}

type ProbeFn = fn(&str) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<String>>> + Send>>;

fn probe_openai_compat(
    url: &str,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<String>>> + Send>> {
    let url = url.to_string();
    Box::pin(async move {
        let http = Client::builder().timeout(PROBE_TIMEOUT).build()?;
        let v: Value = http
            .get(format!("{url}/v1/models"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let models = v
            .get("data")
            .and_then(|d| d.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        Ok(models)
    })
}

fn probe_ollama(
    url: &str,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<String>>> + Send>> {
    let url = url.to_string();
    Box::pin(async move {
        let http = Client::builder().timeout(PROBE_TIMEOUT).build()?;
        let v: Value = http
            .get(format!("{url}/api/tags"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let models = v
            .get("models")
            .and_then(|d| d.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("name").and_then(|i| i.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        Ok(models)
    })
}

// ---------------------------------------------------------------------------
// Config writer
// ---------------------------------------------------------------------------

fn render_paired_config(
    args: &SetupArgs,
    identity: &Identity,
    backends: &[ProbedBackend],
    active: &ActivePairing,
) -> String {
    let mut s = String::new();
    s.push_str("# usepod-agent config — generated by `usepod-agent setup`.\n");
    s.push_str("# Re-run setup to refresh; or hand-edit, the agent will respect it.\n\n");

    s.push_str("[operator]\n");
    s.push_str("# operator identity is owned by the dashboard since v0.2.0;\n");
    s.push_str("# this section is preserved for back-compat with v0.1.x.\n");
    s.push_str("display_name  = \"\"\n");
    s.push_str("wallet        = \"\"\n\n");

    s.push_str("[coordinator]\n");
    let ws_url = http_to_ws(&args.coordinator);
    s.push_str(&format!("url             = \"{ws_url}/provider/connect\"\n"));
    s.push_str(&format!("# host_token     = \"{}\"  (paired)\n", short_secret(&active.host_token)));
    s.push_str(&format!("# provider_id   = \"{}\"\n\n", active.provider_id));

    s.push_str("[identity]\n");
    s.push_str(&format!(
        "key_path = {:?}\n\n",
        args.identity_path.display().to_string()
    ));
    s.push_str(&format!(
        "# public_key = \"{}\"\n\n",
        identity.public_key_b64()
    ));

    for b in backends {
        s.push_str("[[backends]]\n");
        s.push_str(&format!("kind = \"{}\"\n", b.kind));
        s.push_str(&format!("url  = \"{}\"\n\n", b.url));
    }

    s.push_str("[pricing]\n");
    if active.activated_models.is_empty() {
        s.push_str("default_input_per_1m  = 500_000   # placeholder $0.50/M\n");
        s.push_str("default_output_per_1m = 1_000_000 # placeholder $1.00/M\n\n");
    } else {
        s.push_str("default_input_per_1m  = 500_000\n");
        s.push_str("default_output_per_1m = 1_000_000\n\n");
        for m in &active.activated_models {
            s.push_str(&format!("[pricing.models.{:?}]\n", m.model_id));
            s.push_str(&format!("input_per_1m  = {}\n", m.input_per_1m));
            s.push_str(&format!("output_per_1m = {}\n\n", m.output_per_1m));
        }
    }

    s.push_str("[limits]\n");
    let max_concurrent = active
        .activated_models
        .first()
        .map(|m| m.max_concurrent)
        .unwrap_or(4);
    s.push_str(&format!("max_concurrent = {max_concurrent}\n"));

    s
}

fn http_to_ws(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        trimmed.to_string()
    }
}

fn short_secret(s: &str) -> String {
    if s.len() <= 16 {
        s.to_string()
    } else {
        format!("{}…{}", &s[..8], &s[s.len() - 4..])
    }
}

// ---------------------------------------------------------------------------
// Default paths
// ---------------------------------------------------------------------------

fn default_config_path() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("ai", "usepod", "usepod-agent")
        .ok_or_else(|| anyhow!("could not resolve config home"))?;
    Ok(dirs.config_dir().join("agent.toml"))
}

fn default_identity_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    Ok(Path::new(&home).join(".usepod-agent").join("identity.key"))
}
