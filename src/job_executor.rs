//! Agent-side job executor. See `plan/V2_AGENT_SPEC.md` §7.
//!
//! Owns the dispatch table (`Vec<DiscoveredBackend>`) produced by
//! [`crate::discovery`] and the outbound WSS channel. Receives `Job` payloads
//! from the WSS read loop, looks up the backend that serves the requested
//! `model_id`, spawns a tokio task that drives `Backend::execute`, and pumps
//! emitted bytes back as `job_chunk` messages followed by either `job_done`
//! or `job_error`.
//!
//! Concurrency is bounded by `limits.max_concurrent` from `agent.toml`: any
//! job that arrives once that many are in flight is rejected immediately
//! with `out_of_capacity`. Active jobs are tracked in a map so `job_cancel`
//! from the coordinator can abort the underlying task.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use bytes::Bytes;
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tokio::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::backend::{Backend, BackendError, Job, JobResult, JobSink};
use crate::discovery::DiscoveredBackend;

/// Maximum time to wait for a backpressured outbound channel before treating
/// the WSS as fatally stuck for this job.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// Live record of a dispatched job. Held in [`JobExecutor::active`] until the
/// driver task finishes (success, error, or cancel).
struct JobHandle {
    task: JoinHandle<()>,
}

/// Per-model dispatch entry derived from [`DiscoveredBackend`].
#[derive(Clone)]
struct ModelRoute {
    backend: Arc<dyn Backend>,
}

pub struct JobExecutor {
    /// model_id → backend. Local backends already win priority during
    /// discovery, so the first hit per model_id is canonical.
    routes: HashMap<String, ModelRoute>,
    /// `limits.max_concurrent` from `agent.toml`.
    max_concurrent: u32,
    /// Currently in-flight jobs, by job_id. We `lock().await` on insert and
    /// remove only — never held across `await` of work.
    active: Arc<Mutex<HashMap<Uuid, JobHandle>>>,
    /// Counter for capacity enforcement. Incremented on accept, decremented
    /// on completion. Atomic so the WSS read loop never blocks on accept.
    in_flight: Arc<AtomicU32>,
    /// Shared outbound channel into the WSS write half.
    out_tx: mpsc::Sender<Message>,
}

impl JobExecutor {
    pub fn new(
        backends: Vec<DiscoveredBackend>,
        max_concurrent: u32,
        out_tx: mpsc::Sender<Message>,
    ) -> Self {
        let mut routes: HashMap<String, ModelRoute> = HashMap::new();
        for db in backends {
            for m in &db.models {
                routes
                    .entry(m.model_id.clone())
                    .or_insert_with(|| ModelRoute {
                        backend: db.backend.clone(),
                    });
            }
        }
        info!(models = routes.len(), max_concurrent, "job executor ready");
        Self {
            routes,
            max_concurrent,
            active: Arc::new(Mutex::new(HashMap::new())),
            in_flight: Arc::new(AtomicU32::new(0)),
            out_tx,
        }
    }

    /// Number of currently-executing jobs. Useful for heartbeat reporting.
    pub fn queue_depth(&self) -> u32 {
        self.in_flight.load(Ordering::Relaxed)
    }

    /// Accept a job from the WSS read loop and spawn its driver task.
    ///
    /// Never blocks the caller: capacity violations and unknown models are
    /// signalled to the coordinator via `job_error` and the function returns
    /// immediately.
    pub async fn dispatch(&self, job: Job) {
        // Capacity check first — cheap, no lookup needed.
        let prev = self.in_flight.fetch_add(1, Ordering::AcqRel);
        if prev >= self.max_concurrent {
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            warn!(job_id = %job.job_id, "rejecting job: out_of_capacity");
            let _ = send_error(
                &self.out_tx,
                job.job_id,
                "out_of_capacity",
                "agent at max_concurrent",
                0,
            )
            .await;
            return;
        }

        let route = match self.routes.get(&job.model_id).cloned() {
            Some(r) => r,
            None => {
                self.in_flight.fetch_sub(1, Ordering::AcqRel);
                warn!(
                    job_id = %job.job_id,
                    model_id = %job.model_id,
                    "rejecting job: model_not_loaded"
                );
                let _ = send_error(
                    &self.out_tx,
                    job.job_id,
                    "model_not_loaded",
                    "no backend serves this model",
                    0,
                )
                .await;
                return;
            }
        };

        let job_id = job.job_id;
        let out_tx = self.out_tx.clone();
        let active = self.active.clone();
        let in_flight = self.in_flight.clone();
        let deadline = Duration::from_millis(job.deadline_ms.max(1) as u64);

        let task = tokio::spawn(async move {
            let started = Instant::now();
            let mut sink = WsJobSink::new(job_id, out_tx.clone());

            let exec = route.backend.execute(&job, &mut sink);
            let outcome = tokio::time::timeout(deadline, exec).await;

            let final_msg: Value = match outcome {
                Ok(Ok(JobResult {
                    input_tokens,
                    output_tokens,
                    duration_ms,
                })) => {
                    let dur = if duration_ms == 0 {
                        started.elapsed().as_millis().min(u32::MAX as u128) as u32
                    } else {
                        duration_ms
                    };
                    json!({
                        "type": "job_done",
                        "job_id": job_id,
                        "tokens": {
                            "input": input_tokens.unwrap_or(0),
                            "output": output_tokens.unwrap_or(0),
                            "input_tokens": input_tokens.unwrap_or(0),
                            "output_tokens": output_tokens.unwrap_or(0),
                        },
                        "duration_ms": dur,
                    })
                }
                Ok(Err(err)) => {
                    let (code, msg) = map_backend_error(&err);
                    warn!(%job_id, error_code = code, error = %err, "backend error");
                    json!({
                        "type": "job_error",
                        "job_id": job_id,
                        "error_code": code,
                        "message": msg,
                        "tokens_emitted": sink.bytes_sent(),
                    })
                }
                Err(_elapsed) => {
                    warn!(%job_id, "backend timeout exceeded deadline");
                    json!({
                        "type": "job_error",
                        "job_id": job_id,
                        "error_code": "backend_timeout",
                        "message": "backend exceeded deadline_ms",
                        "tokens_emitted": sink.bytes_sent(),
                    })
                }
            };

            // Best-effort terminal frame; if the WSS sender is dead the
            // surrounding read loop will tear down anyway.
            if out_tx
                .send(Message::Text(final_msg.to_string().into()))
                .await
                .is_err()
            {
                debug!(%job_id, "outbound closed before terminal frame");
            }

            in_flight.fetch_sub(1, Ordering::AcqRel);
            active.lock().await.remove(&job_id);
        });

        self.active.lock().await.insert(job_id, JobHandle { task });
    }

    /// Cancel a job in response to a coordinator `job_cancel`. Aborts the
    /// driver task and removes the entry. The terminal frame the driver was
    /// going to send is suppressed by the abort.
    pub async fn cancel(&self, job_id: Uuid) {
        let removed = self.active.lock().await.remove(&job_id);
        match removed {
            Some(h) => {
                h.task.abort();
                self.in_flight.fetch_sub(1, Ordering::AcqRel);
                info!(%job_id, "job cancelled");
            }
            None => debug!(%job_id, "cancel for unknown job (already done?)"),
        }
    }
}

/// `JobSink` impl that wraps the WSS outbound channel. Each `send_chunk`
/// base64-encodes the bytes and emits one `job_chunk` JSON frame. Backpressure
/// surfaces as a `BackendError::Other("ws send timeout/closed")`, which the
/// driver task converts to a `job_error` terminal frame.
struct WsJobSink {
    job_id: Uuid,
    out_tx: mpsc::Sender<Message>,
    bytes_sent: u64,
}

impl WsJobSink {
    fn new(job_id: Uuid, out_tx: mpsc::Sender<Message>) -> Self {
        Self {
            job_id,
            out_tx,
            bytes_sent: 0,
        }
    }

    fn bytes_sent(&self) -> u64 {
        self.bytes_sent
    }
}

#[async_trait]
impl JobSink for WsJobSink {
    async fn send_chunk(&mut self, bytes: Bytes) -> Result<(), BackendError> {
        let frame = json!({
            "type": "job_chunk",
            "job_id": self.job_id,
            "data": B64.encode(&bytes),
        });
        let msg = Message::Text(frame.to_string().into());
        match tokio::time::timeout(SEND_TIMEOUT, self.out_tx.send(msg)).await {
            Ok(Ok(())) => {
                self.bytes_sent = self.bytes_sent.saturating_add(bytes.len() as u64);
                Ok(())
            }
            Ok(Err(_closed)) => {
                error!(job_id = %self.job_id, "ws outbound closed mid-stream");
                Err(BackendError::Other("ws outbound closed".into()))
            }
            Err(_elapsed) => {
                error!(job_id = %self.job_id, "ws outbound backpressured >5s");
                Err(BackendError::Other("ws outbound send timeout".into()))
            }
        }
    }
}

fn map_backend_error(err: &BackendError) -> (&'static str, String) {
    match err {
        BackendError::Unreachable(_) => ("backend_unreachable", err.to_string()),
        BackendError::Timeout => ("backend_timeout", err.to_string()),
        BackendError::ModelNotFound(_) => ("model_not_loaded", err.to_string()),
        BackendError::MissingApiKey(_) => ("auth_rejected_by_backend", err.to_string()),
        BackendError::BadStatus { status, .. } if *status == 401 || *status == 403 => {
            ("auth_rejected_by_backend", err.to_string())
        }
        _ => ("internal", err.to_string()),
    }
}

async fn send_error(
    out_tx: &mpsc::Sender<Message>,
    job_id: Uuid,
    code: &str,
    msg: &str,
    tokens_emitted: u64,
) -> Result<(), mpsc::error::SendError<Message>> {
    let frame = json!({
        "type": "job_error",
        "job_id": job_id,
        "error_code": code,
        "message": msg,
        "tokens_emitted": tokens_emitted,
    });
    out_tx.send(Message::Text(frame.to_string().into())).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_backend_error_codes() {
        assert_eq!(
            map_backend_error(&BackendError::Unreachable("x".into())).0,
            "backend_unreachable"
        );
        assert_eq!(
            map_backend_error(&BackendError::Timeout).0,
            "backend_timeout"
        );
        assert_eq!(
            map_backend_error(&BackendError::ModelNotFound("m".into())).0,
            "model_not_loaded"
        );
        assert_eq!(
            map_backend_error(&BackendError::MissingApiKey("openrouter")).0,
            "auth_rejected_by_backend"
        );
        assert_eq!(
            map_backend_error(&BackendError::BadStatus {
                status: 401,
                body: "x".into()
            })
            .0,
            "auth_rejected_by_backend"
        );
        assert_eq!(
            map_backend_error(&BackendError::Other("x".into())).0,
            "internal"
        );
    }
}
