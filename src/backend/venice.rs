//! Venice.ai BYOK passthrough.
//!
//! Forwards to `https://api.venice.ai/api/v1/...` with the operator's key.
//! `venice_parameters` in the request body is preserved unmodified — the
//! generic `stream_chat_completions` helper forwards the entire JSON body, so
//! Venice-specific fields ride along automatically.

use async_trait::async_trait;

use super::http::{build_client, get_json, parse_openai_models, probe, stream_chat_completions};
use super::{
    Backend, BackendError, BackendHealth, BackendModel, BackendResult, Job, JobResult, JobSink,
};

const BASE_URL: &str = "https://api.venice.ai/api/v1";

pub struct VeniceBackend {
    id: String,
    api_key: String,
    client: reqwest::Client,
}

impl VeniceBackend {
    pub fn from_env(api_key_env: &str) -> BackendResult<Self> {
        let api_key = std::env::var(api_key_env)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .ok_or(BackendError::MissingApiKey("venice"))?;
        Ok(Self {
            id: "venice".to_string(),
            api_key,
            client: build_client(),
        })
    }
}

#[async_trait]
impl Backend for VeniceBackend {
    fn kind(&self) -> &'static str {
        "venice"
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
