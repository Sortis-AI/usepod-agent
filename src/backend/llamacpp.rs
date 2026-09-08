//! llama.cpp `server` adapter. OpenAI-compatible.
//!
//! - Default port: 8080
//! - Health: `GET /health`
//! - Models: `GET /v1/models` (often a single entry — the loaded GGUF)
//! - Execute: `POST /v1/chat/completions`

use async_trait::async_trait;

use super::http::{
    build_client, get_json, parse_openai_models, probe, stream_chat_completions, trim_url,
};
use super::{Backend, BackendHealth, BackendModel, BackendResult, Job, JobResult, JobSink};

pub struct LlamaCppBackend {
    id: String,
    base_url: String,
    client: reqwest::Client,
}

impl LlamaCppBackend {
    pub fn new(url: &str) -> Self {
        let base_url = trim_url(url).to_string();
        Self {
            id: format!("llamacpp:{base_url}"),
            base_url,
            client: build_client(),
        }
    }
}

#[async_trait]
impl Backend for LlamaCppBackend {
    fn kind(&self) -> &'static str {
        "llamacpp"
    }

    fn id(&self) -> &str {
        &self.id
    }

    async fn list_models(&self) -> BackendResult<Vec<BackendModel>> {
        let url = format!("{}/v1/models", self.base_url);
        let v = get_json(&self.client, &url, None).await?;
        // llama.cpp returns OpenAI-shaped `data: [...]`, but some older builds
        // return a bare object. Try both.
        let mut models = parse_openai_models(&v, true);
        if models.is_empty() {
            if let Some(id) = v.get("id").and_then(|s| s.as_str()) {
                models.push(BackendModel {
                    model_id: id.to_string(),
                    context_window: None,
                    native: true,
                });
            }
        }
        // llama.cpp's /v1/models carries only n_ctx_train (the model's
        // TRAINED window, nested under `meta`), but the server validates
        // requests against the CONFIGURED window — `-c/--ctx-size`, exposed
        // as `default_generation_settings.n_ctx` on GET /props. That is the
        // number the coordinator must clamp against, so probe it and apply
        // to every model still missing a window. Best-effort: a build
        // without /props just leaves the field unset.
        let props_url = format!("{}/props", self.base_url);
        if models.iter().any(|m| m.context_window.is_none())
            && let Ok(props) = get_json(&self.client, &props_url, None).await
            && let Some(n_ctx) = parse_props_n_ctx(&props)
        {
            for m in models.iter_mut() {
                m.context_window.get_or_insert(n_ctx);
            }
        }
        Ok(models)
    }

    async fn health(&self) -> BackendResult<BackendHealth> {
        let url = format!("{}/health", self.base_url);
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

/// Pull the configured context window out of a llama.cpp `GET /props`
/// response (`default_generation_settings.n_ctx`).
fn parse_props_n_ctx(props: &serde_json::Value) -> Option<u32> {
    props
        .get("default_generation_settings")?
        .get("n_ctx")?
        .as_u64()
        .filter(|n| *n > 0)
        .map(|n| n as u32)
}

#[cfg(test)]
mod tests {
    use super::parse_props_n_ctx;

    #[test]
    fn props_n_ctx_is_read_from_default_generation_settings() {
        // Shape per tools/server/README.md (verified 2026-09-08).
        let props = serde_json::json!({
            "default_generation_settings": { "id": 0, "n_ctx": 70000 },
            "total_slots": 1
        });
        assert_eq!(parse_props_n_ctx(&props), Some(70_000));
    }

    #[test]
    fn missing_or_zero_n_ctx_yields_none() {
        assert_eq!(parse_props_n_ctx(&serde_json::json!({})), None);
        let zero = serde_json::json!({"default_generation_settings": {"n_ctx": 0}});
        assert_eq!(parse_props_n_ctx(&zero), None);
    }
}
