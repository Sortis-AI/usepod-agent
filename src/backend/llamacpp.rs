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
