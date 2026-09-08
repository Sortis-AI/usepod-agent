//! Ollama adapter.
//!
//! - Default port: 11434
//! - Health: `GET /api/tags` (Ollama's native endpoint, most reliable)
//! - Models: `GET /v1/models` (OpenAI-compat — uniform shape across backends)
//! - Execute: `POST /v1/chat/completions`

use async_trait::async_trait;

use super::http::{
    build_client, get_json, parse_openai_models, post_json, probe, stream_chat_completions,
    trim_url,
};
use super::{Backend, BackendHealth, BackendModel, BackendResult, Job, JobResult, JobSink};

pub struct OllamaBackend {
    id: String,
    base_url: String,
    client: reqwest::Client,
}

impl OllamaBackend {
    pub fn new(url: &str) -> Self {
        let base_url = trim_url(url).to_string();
        Self {
            id: format!("ollama:{base_url}"),
            base_url,
            client: build_client(),
        }
    }
}

#[async_trait]
impl Backend for OllamaBackend {
    fn kind(&self) -> &'static str {
        "ollama"
    }

    fn id(&self) -> &str {
        &self.id
    }

    async fn list_models(&self) -> BackendResult<Vec<BackendModel>> {
        let url = format!("{}/v1/models", self.base_url);
        let v = get_json(&self.client, &url, None).await?;
        let mut models = parse_openai_models(&v, true);
        // Ollama's OpenAI-compat listing carries no context info; the native
        // POST /api/show does — `model_info` holds the GGUF metadata with an
        // architecture-prefixed context_length key (e.g.
        // "llama.context_length", verified against docs/api.md 2026-09-08).
        // Best-effort per model; a failed probe just leaves the field unset.
        for m in models.iter_mut() {
            if m.context_window.is_some() {
                continue;
            }
            let show_url = format!("{}/api/show", self.base_url);
            let body = serde_json::json!({ "model": m.model_id });
            if let Ok(info) = post_json(&self.client, &show_url, &body).await {
                m.context_window = parse_show_context_length(&info);
            }
        }
        Ok(models)
    }

    async fn health(&self) -> BackendResult<BackendHealth> {
        // /api/tags is the most reliable liveness signal for Ollama.
        let url = format!("{}/api/tags", self.base_url);
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

/// Context length from an Ollama `POST /api/show` response: the
/// `model_info` map keys GGUF metadata by architecture, so the context
/// window lives under `<arch>.context_length` — match on the suffix rather
/// than hardcoding architectures.
fn parse_show_context_length(v: &serde_json::Value) -> Option<u32> {
    v.get("model_info")?
        .as_object()?
        .iter()
        .find(|(k, _)| k.ends_with(".context_length"))
        .and_then(|(_, val)| val.as_u64())
        .filter(|n| *n > 0)
        .map(|n| n as u32)
}

#[cfg(test)]
mod tests {
    use super::parse_show_context_length;

    #[test]
    fn context_length_is_found_under_any_architecture_prefix() {
        let show = serde_json::json!({
            "model_info": {
                "general.architecture": "llama",
                "llama.context_length": 8192,
                "llama.embedding_length": 4096
            }
        });
        assert_eq!(parse_show_context_length(&show), Some(8_192));
        let qwen = serde_json::json!({
            "model_info": { "qwen2.context_length": 32768 }
        });
        assert_eq!(parse_show_context_length(&qwen), Some(32_768));
    }

    #[test]
    fn missing_model_info_yields_none() {
        assert_eq!(parse_show_context_length(&serde_json::json!({})), None);
    }
}
