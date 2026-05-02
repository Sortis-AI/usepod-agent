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
        Ok(parse_openai_models(&v, true))
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
