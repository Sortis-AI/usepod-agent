//! Backend abstraction. See `plan/V2_AGENT_SPEC.md` §5.
//!
//! Each adapter implements [`Backend`], exposing a uniform API over the
//! supported inference engines (vLLM, llama.cpp, LM Studio, Ollama) and remote
//! BYOK passthroughs (OpenRouter, Venice). The agent's discovery and (in a
//! later task) job-executor layers consume backends through this trait.

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub mod llamacpp;
pub mod lmstudio;
pub mod ollama;
pub mod openrouter;
pub mod venice;
pub mod vllm;

mod http;

pub use llamacpp::LlamaCppBackend;
pub use lmstudio::LmStudioBackend;
pub use ollama::OllamaBackend;
pub use openrouter::OpenRouterBackend;
pub use venice::VeniceBackend;
pub use vllm::VllmBackend;

/// Errors that can be returned at the backend boundary.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("backend unreachable: {0}")]
    Unreachable(String),
    #[error("backend returned HTTP {status}: {body}")]
    BadStatus { status: u16, body: String },
    #[error("missing or invalid API key for {0}")]
    MissingApiKey(&'static str),
    #[error("model not found on backend: {0}")]
    ModelNotFound(String),
    #[error("backend timeout")]
    Timeout,
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

pub type BackendResult<T> = std::result::Result<T, BackendError>;

/// A model exposed by a backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendModel {
    pub model_id: String,
    pub context_window: Option<u32>,
    /// `true` for local backends; `false` for BYOK passthroughs.
    pub native: bool,
}

/// Health-check result used by discovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendHealth {
    pub reachable: bool,
    pub latency_ms: Option<u32>,
    pub last_error: Option<String>,
}

/// Wire format expected by the upstream client. The agent does not transcode
/// between these — it picks a backend that natively speaks the required shape.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WireFormat {
    Openai,
    Anthropic,
}

/// A unit of work received from the coordinator.
#[derive(Debug, Clone)]
pub struct Job {
    pub job_id: Uuid,
    pub model_id: String,
    pub request: serde_json::Value,
    pub format: WireFormat,
    pub deadline_ms: u32,
}

/// Result returned to the coordinator when a job completes successfully.
#[derive(Debug, Clone, Default)]
pub struct JobResult {
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub duration_ms: u32,
}

/// Sink used by adapters to push streaming chunks back to the coordinator.
///
/// Adapters do not parse SSE — they relay raw upstream bytes faithfully. The
/// coordinator's own tokenizer is the source of truth for token counts.
#[async_trait]
pub trait JobSink: Send {
    async fn send_chunk(&mut self, bytes: Bytes) -> BackendResult<()>;
}

#[async_trait]
pub trait Backend: Send + Sync {
    fn kind(&self) -> &'static str;

    /// A stable identifier for this backend instance — typically `"<kind>:<url>"`
    /// for local adapters and `"<kind>"` for remote passthroughs.
    fn id(&self) -> &str;

    async fn list_models(&self) -> BackendResult<Vec<BackendModel>>;

    async fn health(&self) -> BackendResult<BackendHealth>;

    async fn execute(&self, job: &Job, sink: &mut dyn JobSink) -> BackendResult<JobResult>;
}
