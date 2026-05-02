//! Heartbeat loop. Posts a `heartbeat` JSON message every 15s. Real metrics
//! collection lands in a later task; this scaffold sends hardcoded zeroes so
//! the coordinator's connection-liveness logic still works end-to-end.

use std::time::Duration;

use serde_json::json;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, warn};

const INTERVAL: Duration = Duration::from_secs(15);

pub async fn spawn_loop(out: mpsc::Sender<Message>) {
    let mut ticker = tokio::time::interval(INTERVAL);
    // Skip the immediate tick — let the connection settle first.
    ticker.tick().await;

    loop {
        ticker.tick().await;
        let payload = json!({
            "type": "heartbeat",
            "queue_depth": 0_u32,
            "tokens_per_sec_p50": 0.0_f64,
            "p50_latency_ms": 0_u32,
            "p95_latency_ms": 0_u32,
            "error_rate_60s": 0.0_f64,
            "last_error_at": serde_json::Value::Null,
            "last_error_code": serde_json::Value::Null,
        });
        match out.send(Message::Text(payload.to_string().into())).await {
            Ok(()) => debug!("heartbeat sent"),
            Err(_) => {
                warn!("heartbeat channel closed; exiting heartbeat loop");
                break;
            }
        }
    }
}
