//! Shared HTTP helpers used by every backend adapter.
//!
//! - [`Client`] is a thin wrapper around `reqwest::Client` with a sensible
//!   default timeout and a single user-agent string.
//! - [`stream_chat_completions`] performs the canonical OpenAI-compatible
//!   `POST /v1/chat/completions` with `stream: true`, pipes upstream bytes into
//!   a [`JobSink`] verbatim, and records advisory usage from SSE chunks.

use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde_json::Value;
use tracing::{debug, warn};

use super::{BackendError, BackendResult, Job, JobResult, JobSink};

const USER_AGENT: &str = concat!("usepod-agent/", env!("CARGO_PKG_VERSION"));
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Build a `reqwest::Client` configured for backend traffic.
pub fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
        // Streaming responses can sit idle longer than the overall timeout
        // when the upstream is mid-generation, so disable the per-read timeout
        // separately.
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("reqwest client builder is infallible with valid TLS roots")
}

/// GET `<base>/<path>` with optional bearer token. Returns parsed JSON.
pub async fn get_json(
    client: &reqwest::Client,
    url: &str,
    bearer: Option<&str>,
) -> BackendResult<Value> {
    let mut req = client.get(url);
    if let Some(t) = bearer {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await.map_err(map_send_err)?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(BackendError::BadStatus {
            status: status.as_u16(),
            body,
        });
    }
    let v: Value = resp.json().await?;
    Ok(v)
}

/// Probe an endpoint with GET and return latency. The endpoint is considered
/// healthy if it returns any 2xx.
pub async fn probe(
    client: &reqwest::Client,
    url: &str,
    bearer: Option<&str>,
) -> BackendResult<u32> {
    let start = Instant::now();
    let mut req = client.get(url);
    if let Some(t) = bearer {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await.map_err(map_send_err)?;
    let elapsed = start.elapsed().as_millis() as u32;
    if !resp.status().is_success() {
        return Err(BackendError::BadStatus {
            status: resp.status().as_u16(),
            body: resp.text().await.unwrap_or_default(),
        });
    }
    Ok(elapsed)
}

/// POST a chat-completions request to an OpenAI-compatible endpoint with
/// `stream: true` and relay upstream byte chunks into `sink`.
///
/// Token counts in the returned [`JobResult`] are advisory; the coordinator's
/// tokenizer is authoritative.
pub async fn stream_chat_completions(
    client: &reqwest::Client,
    endpoint: &str,
    bearer: Option<&str>,
    job: &Job,
    sink: &mut dyn JobSink,
) -> BackendResult<JobResult> {
    let mut body = job.request.clone();
    // Force streaming on. Coordinator should already have set this, but be
    // defensive — a non-streaming backend response would hang the byte relay.
    if let Some(obj) = body.as_object_mut() {
        obj.insert("stream".into(), Value::Bool(true));
        obj.entry("stream_options").or_insert_with(|| {
            serde_json::json!({
                "include_usage": true
            })
        });
        if !obj.contains_key("model") {
            obj.insert("model".into(), Value::String(job.model_id.clone()));
        }
    }

    let mut req = client
        .post(endpoint)
        .timeout(Duration::from_millis(job.deadline_ms as u64).max(Duration::from_secs(5)))
        .json(&body);
    if let Some(t) = bearer {
        req = req.bearer_auth(t);
    }

    let start = Instant::now();
    let resp = req.send().await.map_err(map_send_err)?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(BackendError::BadStatus {
            status: status.as_u16(),
            body,
        });
    }

    let mut stream = resp.bytes_stream();
    let mut byte_count: usize = 0;
    let mut usage_parser = SseUsageParser::default();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                byte_count += bytes.len();
                usage_parser.push(&bytes);
                if let Err(e) = sink.send_chunk(bytes).await {
                    warn!(?e, "JobSink rejected chunk; aborting stream");
                    return Err(e);
                }
            }
            Err(e) => {
                if e.is_timeout() {
                    return Err(BackendError::Timeout);
                }
                return Err(BackendError::Transport(e));
            }
        }
    }

    let duration_ms = start.elapsed().as_millis() as u32;
    debug!(
        job_id = %job.job_id,
        bytes = byte_count,
        duration_ms,
        "stream complete"
    );

    let usage = usage_parser.finish();
    Ok(JobResult {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        duration_ms,
    })
}

#[derive(Debug, Default)]
struct ParsedUsage {
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
}

#[derive(Debug, Default)]
struct SseUsageParser {
    pending: String,
    usage: ParsedUsage,
}

impl SseUsageParser {
    fn push(&mut self, bytes: &[u8]) {
        self.pending.push_str(&String::from_utf8_lossy(bytes));
        while let Some(index) = self.pending.find('\n') {
            let line: String = self.pending.drain(..=index).collect();
            self.parse_line(line.trim_end_matches(['\r', '\n']));
        }
    }

    fn finish(mut self) -> ParsedUsage {
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            self.parse_line(line.trim_end_matches(['\r', '\n']));
        }
        self.usage
    }

    fn parse_line(&mut self, line: &str) {
        let Some(data) = line.strip_prefix("data:") else {
            return;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            return;
        };
        let Some(usage) = value.get("usage").filter(|usage| usage.is_object()) else {
            return;
        };
        if let Some(input_tokens) = pick_u32(usage, &["input_tokens", "prompt_tokens", "input"]) {
            self.usage.input_tokens = Some(input_tokens);
        }
        if let Some(output_tokens) =
            pick_u32(usage, &["output_tokens", "completion_tokens", "output"])
        {
            self.usage.output_tokens = Some(output_tokens);
        }
    }
}

fn pick_u32(value: &Value, keys: &[&str]) -> Option<u32> {
    keys.iter()
        .filter_map(|key| value.get(*key).and_then(|value| value.as_u64()))
        .find_map(|value| u32::try_from(value).ok())
}

#[cfg(test)]
mod tests {
    use super::SseUsageParser;

    #[test]
    fn parses_openai_stream_usage() {
        let mut parser = SseUsageParser::default();

        parser.push(
            br#"data: {"choices":[{"delta":{"content":"hi"}}],"usage":null}
data: {"choices":[],"usage":{"prompt_tokens":17,"completion_tokens":23,"total_tokens":40}}
data: [DONE]
"#,
        );

        let usage = parser.finish();
        assert_eq!(usage.input_tokens, Some(17));
        assert_eq!(usage.output_tokens, Some(23));
    }

    #[test]
    fn parses_usage_split_across_chunks() {
        let mut parser = SseUsageParser::default();

        parser.push(br#"data: {"choices":[],"usage":{"input_tokens":12"#);
        parser.push(br#","output_tokens":34}}"#);
        parser.push(b"\n\n");

        let usage = parser.finish();
        assert_eq!(usage.input_tokens, Some(12));
        assert_eq!(usage.output_tokens, Some(34));
    }
}

fn map_send_err(e: reqwest::Error) -> BackendError {
    if e.is_timeout() {
        BackendError::Timeout
    } else if e.is_connect() {
        BackendError::Unreachable(e.to_string())
    } else {
        BackendError::Transport(e)
    }
}

/// Strip a trailing `/` from a URL so we can join paths uniformly.
pub fn trim_url(url: &str) -> &str {
    url.trim_end_matches('/')
}

/// Parse an OpenAI-compatible `/v1/models` response into `BackendModel`s.
pub fn parse_openai_models(v: &Value, native: bool) -> Vec<super::BackendModel> {
    let Some(data) = v.get("data").and_then(|d| d.as_array()) else {
        return Vec::new();
    };
    data.iter()
        .filter_map(|item| {
            let id = item.get("id").and_then(|s| s.as_str())?.to_string();
            let context_window = item
                .get("context_length")
                .or_else(|| item.get("max_model_len"))
                .or_else(|| item.get("context_window"))
                .and_then(|n| n.as_u64())
                .map(|n| n as u32);
            Some(super::BackendModel {
                model_id: id,
                context_window,
                native,
            })
        })
        .collect()
}
