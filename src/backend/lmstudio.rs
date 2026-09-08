//! LM Studio adapter. OpenAI-compatible.
//!
//! - Default port: 1234
//! - Health: `GET /v1/models` (no dedicated `/health` endpoint)
//! - Models: `GET /v1/models`
//! - Execute: `POST /v1/chat/completions`

use async_trait::async_trait;

use super::http::{
    build_client, get_json, parse_openai_models, probe, stream_chat_completions, trim_url,
};
use super::{Backend, BackendHealth, BackendModel, BackendResult, Job, JobResult, JobSink};

pub struct LmStudioBackend {
    id: String,
    base_url: String,
    client: reqwest::Client,
}

impl LmStudioBackend {
    pub fn new(url: &str) -> Self {
        let base_url = trim_url(url).to_string();
        Self {
            id: format!("lmstudio:{base_url}"),
            base_url,
            client: build_client(),
        }
    }
}

#[async_trait]
impl Backend for LmStudioBackend {
    fn kind(&self) -> &'static str {
        "lmstudio"
    }

    fn id(&self) -> &str {
        &self.id
    }

    async fn list_models(&self) -> BackendResult<Vec<BackendModel>> {
        let url = format!("{}/v1/models", self.base_url);
        let v = get_json(&self.client, &url, None).await?;
        let mut models = parse_openai_models(&v, true);
        // LM Studio's OpenAI-compat listing has no context field, but its
        // native REST listing does (`max_context_length`, plus
        // `loaded_context_length` for the value actually in effect). The v0
        // API is deprecated upstream but still served; treat the probe as
        // pure best-effort — any failure leaves the field unset.
        if models.iter().any(|m| m.context_window.is_none()) {
            let native_url = format!("{}/api/v0/models", self.base_url);
            if let Ok(native) = get_json(&self.client, &native_url, None).await {
                let by_id = parse_lmstudio_context_lengths(&native);
                for m in models.iter_mut() {
                    if m.context_window.is_none() {
                        m.context_window = by_id.get(&m.model_id).copied();
                    }
                }
            }
        }
        Ok(models)
    }

    async fn health(&self) -> BackendResult<BackendHealth> {
        let url = format!("{}/v1/models", self.base_url);
        match probe(&self.client, &url, None).await {
            Ok(latency_ms) => Ok(BackendHealth {
                reachable: true,
                latency_ms: Some(latency_ms),
                last_error: None,
            }),
            Err(e) => Ok(BackendHealth {
                reachable: false,
                latency_ms: None,
                last_error: Some(e.to_string()),
            }),
        }
    }

    async fn execute(&self, job: &Job, sink: &mut dyn JobSink) -> BackendResult<JobResult> {
        let endpoint = format!("{}/v1/chat/completions", self.base_url);
        stream_chat_completions(&self.client, &endpoint, None, job, sink).await
    }
}

/// Map of model id → context window from LM Studio's native
/// `GET /api/v0/models` listing. `loaded_context_length` (the window in
/// effect for a loaded model) wins over `max_context_length` (the model's
/// ceiling) — the loaded value is what requests are validated against.
fn parse_lmstudio_context_lengths(v: &serde_json::Value) -> std::collections::HashMap<String, u32> {
    let mut out = std::collections::HashMap::new();
    let items = v
        .get("data")
        .and_then(|d| d.as_array())
        .map(|a| a.as_slice())
        .unwrap_or_default();
    for item in items {
        let Some(id) = item.get("id").and_then(|s| s.as_str()) else {
            continue;
        };
        let ctx = item
            .get("loaded_context_length")
            .or_else(|| item.get("max_context_length"))
            .and_then(|n| n.as_u64())
            .filter(|n| *n > 0);
        if let Some(n) = ctx {
            out.insert(id.to_string(), n as u32);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::parse_lmstudio_context_lengths;

    #[test]
    fn loaded_context_wins_over_max() {
        let v = serde_json::json!({ "data": [
            { "id": "a", "max_context_length": 131072, "loaded_context_length": 8192 },
            { "id": "b", "max_context_length": 32768 }
        ]});
        let m = parse_lmstudio_context_lengths(&v);
        assert_eq!(m.get("a"), Some(&8_192));
        assert_eq!(m.get("b"), Some(&32_768));
    }

    #[test]
    fn malformed_listing_yields_empty_map() {
        assert!(parse_lmstudio_context_lengths(&serde_json::json!({})).is_empty());
    }
}
