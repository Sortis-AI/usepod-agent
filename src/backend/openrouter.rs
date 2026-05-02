//! OpenRouter BYOK passthrough.
//!
//! The agent forwards requests to `https://openrouter.ai/api/v1/...` using the
//! operator's API key (read at startup from the env var named in
//! `agent.toml`). The user's identity is never proxied upstream — OpenRouter
//! sees only the operator.

use async_trait::async_trait;

use super::http::{build_client, get_json, parse_openai_models, probe, stream_chat_completions};
use super::{
    Backend, BackendError, BackendHealth, BackendModel, BackendResult, Job, JobResult, JobSink,
};

const BASE_URL: &str = "https://openrouter.ai/api/v1";

pub struct OpenRouterBackend {
    id: String,
    api_key: String,
    client: reqwest::Client,
}

impl OpenRouterBackend {
    /// Construct from an env-var name. Returns `MissingApiKey` if the env
    /// resolves to nothing useful.
    pub fn from_env(api_key_env: &str) -> BackendResult<Self> {
        let api_key = std::env::var(api_key_env)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .ok_or(BackendError::MissingApiKey("openrouter"))?;
        Ok(Self {
            id: "openrouter".to_string(),
            api_key,
            client: build_client(),
        })
    }
}

#[async_trait]
impl Backend for OpenRouterBackend {
    fn kind(&self) -> &'static str {
        "openrouter"
    }

    fn id(&self) -> &str {
        &self.id
    }

    async fn list_models(&self) -> BackendResult<Vec<BackendModel>> {
        let url = format!("{BASE_URL}/models");
        let v = get_json(&self.client, &url, Some(&self.api_key)).await?;
        Ok(parse_openai_models(&v, false))
    }

    async fn health(&self) -> BackendResult<BackendHealth> {
        let url = format!("{BASE_URL}/models");
        match probe(&self.client, &url, Some(&self.api_key)).await {
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
        let endpoint = format!("{BASE_URL}/chat/completions");
        stream_chat_completions(&self.client, &endpoint, Some(&self.api_key), job, sink).await
    }
}
