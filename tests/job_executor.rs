//! Integration tests for the agent-side job executor (Task #34, V2_AGENT_SPEC §7).
//!
//! Uses a mock `Backend` that emits a fixed byte sequence and returns a known
//! `JobResult`, so we can assert wire-frame shape without a live HTTP server.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use provider_agent::backend::{
    Backend, BackendHealth, BackendModel, BackendResult, Job, JobResult, JobSink, WireFormat,
};
use provider_agent::discovery::{DiscoveredBackend, ResolvedModel};
use provider_agent::job_executor::JobExecutor;

/// Mock backend that emits N fixed chunks then returns a fixed JobResult.
struct MockBackend {
    id: String,
    chunks: Vec<Bytes>,
    delay_ms: u64,
    started: Arc<AtomicU32>,
    cancel_marker: Arc<AtomicU32>,
}

#[async_trait]
impl Backend for MockBackend {
    fn kind(&self) -> &'static str {
        "mock"
    }
    fn id(&self) -> &str {
        &self.id
    }
    async fn list_models(&self) -> BackendResult<Vec<BackendModel>> {
        Ok(vec![])
    }
    async fn health(&self) -> BackendResult<BackendHealth> {
        Ok(BackendHealth {
            reachable: true,
            latency_ms: Some(0),
            last_error: None,
        })
    }
    async fn execute(&self, _job: &Job, sink: &mut dyn JobSink) -> BackendResult<JobResult> {
        self.started.fetch_add(1, Ordering::Relaxed);
        for c in &self.chunks {
            sink.send_chunk(c.clone()).await?;
            if self.delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
            }
        }
        // If aborted before this returns, the marker increment never fires.
        self.cancel_marker.fetch_add(1, Ordering::Relaxed);
        Ok(JobResult {
            input_tokens: Some(7),
            output_tokens: Some(13),
            duration_ms: 42,
        })
    }
}

fn mock_route(model_id: &str) -> (DiscoveredBackend, Arc<AtomicU32>, Arc<AtomicU32>) {
    let started = Arc::new(AtomicU32::new(0));
    let finished = Arc::new(AtomicU32::new(0));
    let backend = Arc::new(MockBackend {
        id: format!("mock:{model_id}"),
        chunks: vec![Bytes::from_static(b"hello "), Bytes::from_static(b"world")],
        delay_ms: 0,
        started: started.clone(),
        cancel_marker: finished.clone(),
    });
    let db = DiscoveredBackend {
        backend: backend as Arc<dyn Backend>,
        models: vec![ResolvedModel {
            model_id: model_id.into(),
            input_per_1m: 1,
            output_per_1m: 1,
            max_concurrent: 4,
            backend: "mock".into(),
            context_window: None,
        }],
    };
    (db, started, finished)
}

fn make_job(model_id: &str) -> Job {
    Job {
        job_id: Uuid::new_v4(),
        model_id: model_id.into(),
        request: json!({"messages": []}),
        format: WireFormat::Openai,
        deadline_ms: 5_000,
    }
}

async fn next_text(rx: &mut mpsc::Receiver<Message>) -> Value {
    let msg = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout waiting for outbound msg")
        .expect("channel closed");
    let txt = match msg {
        Message::Text(t) => t.to_string(),
        other => panic!("expected text frame, got {other:?}"),
    };
    serde_json::from_str(&txt).expect("valid JSON")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_model_emits_model_not_loaded() {
    let (db, _started, _finished) = mock_route("served-model");
    let (tx, mut rx) = mpsc::channel::<Message>(16);
    let exec = JobExecutor::new(vec![db], 4, tx);

    let mut job = make_job("unknown-model");
    job.job_id = Uuid::new_v4();
    let job_id = job.job_id;
    exec.dispatch(job).await;

    let v = next_text(&mut rx).await;
    assert_eq!(v["type"], "job_error");
    assert_eq!(v["error_code"], "model_not_loaded");
    assert_eq!(
        v["job_id"].as_str().unwrap().parse::<Uuid>().unwrap(),
        job_id
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn known_model_streams_chunks_then_done() {
    let (db, started, finished) = mock_route("served-model");
    let (tx, mut rx) = mpsc::channel::<Message>(16);
    let exec = JobExecutor::new(vec![db], 4, tx);

    let job = make_job("served-model");
    let job_id = job.job_id;
    exec.dispatch(job).await;

    let c1 = next_text(&mut rx).await;
    let c2 = next_text(&mut rx).await;
    let done = next_text(&mut rx).await;

    assert_eq!(c1["type"], "job_chunk");
    assert_eq!(c2["type"], "job_chunk");
    assert_eq!(done["type"], "job_done");
    assert_eq!(
        done["job_id"].as_str().unwrap().parse::<Uuid>().unwrap(),
        job_id
    );
    assert_eq!(done["tokens"]["input"], 7);
    assert_eq!(done["tokens"]["output"], 13);
    assert_eq!(done["tokens"]["input_tokens"], 7);
    assert_eq!(done["tokens"]["output_tokens"], 13);

    // base64 of "hello " is "aGVsbG8g"
    assert_eq!(c1["data"], "aGVsbG8g");
    // base64 of "world" is "d29ybGQ="
    assert_eq!(c2["data"], "d29ybGQ=");

    assert_eq!(started.load(Ordering::Relaxed), 1);
    assert_eq!(finished.load(Ordering::Relaxed), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn over_capacity_yields_out_of_capacity() {
    // Slow backend so jobs stay in-flight while we pile more on.
    let started = Arc::new(AtomicU32::new(0));
    let finished = Arc::new(AtomicU32::new(0));
    let backend = Arc::new(MockBackend {
        id: "mock:slow".into(),
        chunks: vec![Bytes::from_static(b"x")],
        delay_ms: 200,
        started: started.clone(),
        cancel_marker: finished.clone(),
    });
    let db = DiscoveredBackend {
        backend: backend as Arc<dyn Backend>,
        models: vec![ResolvedModel {
            model_id: "slow".into(),
            input_per_1m: 1,
            output_per_1m: 1,
            max_concurrent: 1,
            backend: "mock".into(),
            context_window: None,
        }],
    };

    let (tx, mut rx) = mpsc::channel::<Message>(64);
    let exec = JobExecutor::new(vec![db], 1, tx);

    // First job claims the only slot.
    exec.dispatch(make_job("slow")).await;
    // Second job must be rejected with out_of_capacity.
    let rejected = make_job("slow");
    let rejected_id = rejected.job_id;
    exec.dispatch(rejected).await;

    // Drain frames until we see the rejection. The accepted job's chunk may
    // arrive before the rejection.
    let mut saw_rejection = false;
    for _ in 0..5 {
        let v = next_text(&mut rx).await;
        if v["type"] == "job_error"
            && v["error_code"] == "out_of_capacity"
            && v["job_id"].as_str().unwrap().parse::<Uuid>().unwrap() == rejected_id
        {
            saw_rejection = true;
            break;
        }
    }
    assert!(
        saw_rejection,
        "expected out_of_capacity for second dispatch"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_aborts_in_flight_job() {
    // Slow backend so we have time to cancel before completion.
    let started = Arc::new(AtomicU32::new(0));
    let finished = Arc::new(AtomicU32::new(0));
    let backend = Arc::new(MockBackend {
        id: "mock:slow".into(),
        chunks: vec![Bytes::from_static(b"a"); 50],
        delay_ms: 50,
        started: started.clone(),
        cancel_marker: finished.clone(),
    });
    let db = DiscoveredBackend {
        backend: backend as Arc<dyn Backend>,
        models: vec![ResolvedModel {
            model_id: "slow".into(),
            input_per_1m: 1,
            output_per_1m: 1,
            max_concurrent: 4,
            backend: "mock".into(),
            context_window: None,
        }],
    };

    let (tx, mut rx) = mpsc::channel::<Message>(64);
    let exec = JobExecutor::new(vec![db], 4, tx);

    let job = make_job("slow");
    let job_id = job.job_id;
    exec.dispatch(job).await;

    // Wait for the backend to start emitting.
    let _ = next_text(&mut rx).await;
    assert_eq!(started.load(Ordering::Relaxed), 1);

    exec.cancel(job_id).await;

    // After cancel, queue_depth must drop and the backend's "finished"
    // marker must NOT increment (task aborted before completion).
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(exec.queue_depth(), 0);
    assert_eq!(
        finished.load(Ordering::Relaxed),
        0,
        "task should have been aborted before completion"
    );
}
