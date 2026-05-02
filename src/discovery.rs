//! Backend discovery. See `plan/V2_AGENT_SPEC.md` §6.
//!
//! At startup (and every 5 minutes thereafter, when wired into the run loop)
//! the agent walks every `[[backends]]` entry in `agent.toml`, instantiates
//! the right adapter, calls `health()`, then `list_models()`, and merges the
//! results with the operator's pricing table to produce the
//! `WireMessage::Capabilities` payload.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use crate::backend::{
    Backend, LlamaCppBackend, LmStudioBackend, OllamaBackend, OpenRouterBackend, VeniceBackend,
    VllmBackend,
};
use crate::config::{Backend as CfgBackend, Config};

const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Priority order when the same `model_id` appears across multiple backends.
/// Local backends always beat remote BYOK passthroughs.
fn kind_priority(kind: &str) -> u8 {
    match kind {
        "vllm" => 0,
        "llamacpp" => 1,
        "lmstudio" => 2,
        "ollama" => 3,
        "venice" => 4,
        "openrouter" => 5,
        _ => 100,
    }
}

/// A model offering, after pricing has been resolved.
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedModel {
    pub model_id: String,
    pub input_per_1m: u64,
    pub output_per_1m: u64,
    pub max_concurrent: u32,
    pub backend: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
}

/// A discovered, ready-to-serve backend instance plus its current model list.
pub struct DiscoveredBackend {
    pub backend: Arc<dyn Backend>,
    pub models: Vec<ResolvedModel>,
}

/// Result of a discovery pass.
pub struct DiscoveryResult {
    pub backends: Vec<DiscoveredBackend>,
    /// Final, deduplicated model list to send to the coordinator.
    pub capability_models: Vec<ResolvedModel>,
}

impl DiscoveryResult {
    /// Build the `capabilities` wire message from this discovery result.
    pub fn to_capabilities(&self, cfg: &Config) -> Value {
        let limits = json!({
            "max_concurrent_total": cfg.limits.max_concurrent,
            "max_tokens_per_minute": cfg.limits.max_tokens_per_minute,
        });
        json!({
            "type": "capabilities",
            "models": self.capability_models,
            "limits": limits,
            "metadata": {
                "agent_version": AGENT_VERSION,
            }
        })
    }
}

/// Build a backend instance for a config entry. Returns `None` (with a logged
/// warning) when the entry is misconfigured — e.g. missing API key env var.
pub fn build_backend(cfg: &CfgBackend) -> Option<Arc<dyn Backend>> {
    match cfg.kind.as_str() {
        "vllm" => cfg
            .url
            .as_deref()
            .map(|u| Arc::new(VllmBackend::new(u)) as Arc<dyn Backend>),
        "llamacpp" => cfg
            .url
            .as_deref()
            .map(|u| Arc::new(LlamaCppBackend::new(u)) as Arc<dyn Backend>),
        "lmstudio" => cfg
            .url
            .as_deref()
            .map(|u| Arc::new(LmStudioBackend::new(u)) as Arc<dyn Backend>),
        "ollama" => cfg
            .url
            .as_deref()
            .map(|u| Arc::new(OllamaBackend::new(u)) as Arc<dyn Backend>),
        "openrouter" => match cfg
            .api_key_env
            .as_deref()
            .map(OpenRouterBackend::from_env)
        {
            Some(Ok(b)) => Some(Arc::new(b) as Arc<dyn Backend>),
            Some(Err(e)) => {
                warn!(?e, "skipping openrouter backend (no api key)");
                None
            }
            None => None,
        },
        "venice" => match cfg.api_key_env.as_deref().map(VeniceBackend::from_env) {
            Some(Ok(b)) => Some(Arc::new(b) as Arc<dyn Backend>),
            Some(Err(e)) => {
                warn!(?e, "skipping venice backend (no api key)");
                None
            }
            None => None,
        },
        other => {
            warn!(kind = other, "unknown backend kind in config; skipping");
            None
        }
    }
}

/// Run a discovery pass against every configured backend. Health failures are
/// logged and the offending backend is dropped from the result; the agent
/// keeps running with whatever backends responded successfully.
pub async fn run(cfg: &Config) -> DiscoveryResult {
    let mut discovered: Vec<DiscoveredBackend> = Vec::new();

    for cfg_backend in &cfg.backends {
        let Some(backend) = build_backend(cfg_backend) else {
            continue;
        };

        let health = match backend.health().await {
            Ok(h) => h,
            Err(e) => {
                warn!(backend = backend.id(), ?e, "health check failed");
                continue;
            }
        };
        if !health.reachable {
            warn!(
                backend = backend.id(),
                error = ?health.last_error,
                "backend unreachable; skipping"
            );
            continue;
        }
        debug!(
            backend = backend.id(),
            latency_ms = ?health.latency_ms,
            "backend healthy"
        );

        let models = match backend.list_models().await {
            Ok(m) => m,
            Err(e) => {
                warn!(backend = backend.id(), ?e, "list_models failed");
                continue;
            }
        };

        // Apply operator's optional model allow-list, then resolve pricing.
        let allow: Option<&Vec<String>> = cfg_backend.models.as_ref();
        let resolved: Vec<ResolvedModel> = models
            .into_iter()
            .filter(|m| match allow {
                Some(list) => list.iter().any(|x| x == &m.model_id),
                None => true,
            })
            .filter_map(|m| {
                let (input_per_1m, output_per_1m) = match cfg.pricing.models.get(&m.model_id) {
                    Some(p) => (p.input_per_1m, p.output_per_1m),
                    None => (
                        cfg.pricing.default_input_per_1m,
                        cfg.pricing.default_output_per_1m,
                    ),
                };
                if input_per_1m == 0 || output_per_1m == 0 {
                    warn!(model = %m.model_id, "no pricing; dropping model");
                    return None;
                }
                Some(ResolvedModel {
                    model_id: m.model_id,
                    input_per_1m,
                    output_per_1m,
                    max_concurrent: cfg.limits.max_concurrent,
                    backend: backend.kind().to_string(),
                    context_window: m.context_window,
                })
            })
            .collect();

        info!(
            backend = backend.id(),
            kind = backend.kind(),
            models = resolved.len(),
            "backend discovered"
        );
        discovered.push(DiscoveredBackend {
            backend,
            models: resolved,
        });
    }

    let capability_models = dedupe_by_priority(&discovered);
    DiscoveryResult {
        backends: discovered,
        capability_models,
    }
}

/// When the same model_id appears across multiple backends, keep only the
/// one whose backend kind has the lowest priority number.
fn dedupe_by_priority(discovered: &[DiscoveredBackend]) -> Vec<ResolvedModel> {
    let mut by_id: HashMap<String, ResolvedModel> = HashMap::new();
    for db in discovered {
        for m in &db.models {
            match by_id.get(&m.model_id) {
                Some(existing) if kind_priority(&existing.backend) <= kind_priority(&m.backend) => {
                    continue;
                }
                _ => {
                    by_id.insert(m.model_id.clone(), m.clone());
                }
            }
        }
    }
    let mut out: Vec<ResolvedModel> = by_id.into_values().collect();
    out.sort_by(|a, b| a.model_id.cmp(&b.model_id));
    out
}
