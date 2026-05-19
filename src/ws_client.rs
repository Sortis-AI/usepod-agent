//! Long-lived WSS client. Performs the signed handshake (`auth_challenge` →
//! `auth_response` → `auth_ok`) and runs heartbeat/job loops thereafter.
//! Reconnect with exponential backoff per `plan/V2_AGENT_SPEC.md` §9.
//!
//! NOTE: this is the scaffold. Job dispatch and capability discovery are
//! implemented in subsequent tasks (#15/#16). For now we authenticate, send a
//! placeholder `capabilities` payload, then drive heartbeats while listening
//! for and tracing-logging incoming messages.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

use crate::backend::{Job, WireFormat};
use crate::config::Config;
use crate::discovery;
use crate::heartbeat;
use crate::identity::Identity;
use crate::job_executor::JobExecutor;

const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Connect-once outcome. We split pre-auth from post-auth failures so a deploy
/// of the coordinator (which drops a steady-state session) doesn't escalate
/// the reconnect backoff the same way a real outage does.
#[derive(Debug, Error)]
enum ConnectError {
    /// Disconnected before reaching steady state (dial, TLS, handshake, auth
    /// response). Treated as a real failure; reconnect backoff escalates.
    #[error("pre-auth: {0:#}")]
    PreAuth(anyhow::Error),
    /// Disconnected after `auth_ok` and discovery succeeded — i.e. an
    /// established session ended. Treated as a planned cycle (coordinator
    /// restart, network blip); reconnect backoff resets.
    #[error("post-auth: {0:#}")]
    PostAuth(anyhow::Error),
}

/// Connect, authenticate, and run forever with reconnect.
pub async fn run(cfg: Config, mut identity: Identity) -> Result<()> {
    let mut backoff_ms: u64 = 1000;
    let mut consecutive_failures: u32 = 0;

    loop {
        match connect_once(&cfg, &mut identity).await {
            Ok(()) => {
                // Clean disconnect (server closed). Restart with the post-success backoff schedule.
                warn!("coordinator connection closed; reconnecting");
                consecutive_failures = 0;
                backoff_ms = 1000;
            }
            Err(ConnectError::PostAuth(err)) => {
                // Steady-state session ended — almost always a coordinator
                // deploy or transient network issue. Reset like a clean close
                // so we don't stretch the gap during a blue-green rotation.
                warn!(?err, "coordinator session ended; reconnecting");
                consecutive_failures = 0;
                backoff_ms = 1000;
            }
            Err(ConnectError::PreAuth(err)) => {
                consecutive_failures += 1;
                error!(?err, attempts = consecutive_failures, "coordinator connection failed");
                if consecutive_failures == 10 {
                    error!("coordinator unreachable after 10 attempts; will keep retrying");
                }
            }
        }

        let jitter: f64 = rand::thread_rng().gen_range(0.8..1.2);
        let sleep_ms = ((backoff_ms as f64) * jitter) as u64;
        tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
        backoff_ms = (backoff_ms.saturating_mul(2)).min(60_000);
    }
}

async fn connect_once(cfg: &Config, identity: &mut Identity) -> Result<(), ConnectError> {
    info!(url = %cfg.coordinator.url, "dialing coordinator");
    let (ws, _resp) = tokio_tungstenite::connect_async(&cfg.coordinator.url)
        .await
        .with_context(|| format!("connecting to {}", cfg.coordinator.url))
        .map_err(ConnectError::PreAuth)?;
    let (mut sink, mut stream) = ws.split();

    // 1. Receive auth_challenge
    let challenge = recv_json(&mut stream).await.map_err(ConnectError::PreAuth)?;
    if challenge.get("type").and_then(Value::as_str) != Some("auth_challenge") {
        return Err(ConnectError::PreAuth(anyhow!(
            "expected auth_challenge, got {challenge}"
        )));
    }
    let nonce_b64 = challenge
        .get("nonce")
        .and_then(Value::as_str)
        .ok_or_else(|| ConnectError::PreAuth(anyhow!("auth_challenge missing nonce")))?;
    let nonce = B64
        .decode(nonce_b64.as_bytes())
        .context("decoding challenge nonce")
        .map_err(ConnectError::PreAuth)?;

    // 2. Sign nonce, send auth_response
    let mut auth_response = json!({
        "type": "auth_response",
        "pubkey": identity.public_key_b64(),
        "signature": identity.sign_b64(&nonce),
        "agent_version": AGENT_VERSION,
    });
    if identity.provider_id.is_none() {
        if let Some(code) = cfg.coordinator.enrollment_code.as_deref() {
            auth_response["enrollment_code"] = json!(code);
        }
    }
    sink.send(Message::Text(auth_response.to_string().into()))
        .await
        .map_err(|e| ConnectError::PreAuth(e.into()))?;

    // 3. Await auth_ok
    let ack = recv_json(&mut stream).await.map_err(ConnectError::PreAuth)?;
    match ack.get("type").and_then(Value::as_str) {
        Some("auth_ok") => {}
        Some("auth_failed") => {
            let reason = ack.get("reason").and_then(Value::as_str).unwrap_or("unknown");
            return Err(ConnectError::PreAuth(anyhow!(
                "coordinator rejected auth: {reason}"
            )));
        }
        other => {
            return Err(ConnectError::PreAuth(anyhow!(
                "expected auth_ok, got type={other:?}"
            )));
        }
    }
    if let Some(pid) = ack.get("provider_id").and_then(Value::as_str) {
        if identity.provider_id.as_deref() != Some(pid) {
            info!(provider_id = pid, "persisting provider_id from coordinator");
            identity
                .set_provider_id(pid.to_string())
                .map_err(ConnectError::PreAuth)?;
        }
    }
    info!("authenticated with coordinator");

    // 4. Run backend discovery and send a real `capabilities` payload.
    //    Built fresh on every successful auth_ok so reconnect-after-coordinator-
    //    restart re-registers the model list (Redis state may be cold).
    let discovery_result = discovery::run(cfg).await;
    info!(
        models = discovery_result.capability_models.len(),
        backends = discovery_result.backends.len(),
        "discovery complete"
    );
    let capabilities = discovery_result.to_capabilities(cfg);
    // Past this point, the coordinator has accepted us and we've started the
    // steady-state pump. Any further error is a PostAuth — treat as a planned
    // cycle so a deploy doesn't escalate the reconnect backoff.
    sink.send(Message::Text(capabilities.to_string().into()))
        .await
        .map_err(|e| ConnectError::PostAuth(e.into()))?;
    debug!("sent capabilities");

    // 5. Spawn the heartbeat loop. We funnel both heartbeat and any future
    //    outbound traffic through an mpsc to keep the WS sink single-owner.
    let (out_tx, mut out_rx) = mpsc::channel::<Message>(64);
    let hb_handle = tokio::spawn(heartbeat::spawn_loop(out_tx.clone()));

    // The discovered backends become the dispatch table owned by the executor.
    let executor = JobExecutor::new(
        discovery_result.backends,
        cfg.limits.max_concurrent,
        out_tx.clone(),
    );

    // 6. Read loop / write pump.
    let result: Result<()> = async {
        loop {
            tokio::select! {
                outbound = out_rx.recv() => {
                    match outbound {
                        Some(msg) => sink.send(msg).await?,
                        None => break,
                    }
                }
                inbound = stream.next() => {
                    match inbound {
                        Some(Ok(Message::Text(txt))) => {
                            debug!(%txt, "ws inbound text");
                            handle_inbound_text(&executor, &txt).await;
                        }
                        Some(Ok(Message::Ping(p))) => sink.send(Message::Pong(p)).await?,
                        Some(Ok(Message::Close(_))) => break,
                        Some(Ok(_)) => {}
                        Some(Err(e)) => return Err(anyhow!("ws read error: {e}")),
                        None => break,
                    }
                }
            }
        }
        Ok(())
    }
    .await;

    hb_handle.abort();
    result.map_err(ConnectError::PostAuth)
}

/// Parse an inbound coordinator frame and route `job` / `job_cancel` to the
/// executor. Other types (`config_update`, etc.) are debug-logged for now;
/// adding handlers here is non-invasive.
async fn handle_inbound_text(executor: &JobExecutor, txt: &str) {
    let v: Value = match serde_json::from_str(txt) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "ws inbound: invalid json");
            return;
        }
    };
    match v.get("type").and_then(Value::as_str) {
        Some("job") => match parse_job(&v) {
            Ok(job) => executor.dispatch(job).await,
            Err(e) => warn!(error = %e, "ws inbound: malformed job"),
        },
        Some("job_cancel") => {
            if let Some(id) = v.get("job_id").and_then(Value::as_str) {
                match id.parse::<uuid::Uuid>() {
                    Ok(job_id) => executor.cancel(job_id).await,
                    Err(e) => warn!(error = %e, "ws inbound: bad job_id in job_cancel"),
                }
            }
        }
        Some(other) => debug!(kind = other, "ws inbound: unhandled message type"),
        None => warn!("ws inbound: missing 'type'"),
    }
}

fn parse_job(v: &Value) -> Result<Job> {
    let job_id = v
        .get("job_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("job missing job_id"))?
        .parse::<uuid::Uuid>()
        .context("job_id parse")?;
    let model_id = v
        .get("model_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("job missing model_id"))?
        .to_string();
    let request = v
        .get("request")
        .cloned()
        .ok_or_else(|| anyhow!("job missing request"))?;
    let format = match v.get("format").and_then(Value::as_str).unwrap_or("openai") {
        "anthropic" => WireFormat::Anthropic,
        _ => WireFormat::Openai,
    };
    let deadline_ms = v
        .get("deadline_ms")
        .and_then(Value::as_u64)
        .unwrap_or(60_000) as u32;
    Ok(Job { job_id, model_id, request, format, deadline_ms })
}

async fn recv_json<S>(stream: &mut S) -> Result<Value>
where
    S: StreamExt<Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    loop {
        let msg = stream
            .next()
            .await
            .ok_or_else(|| anyhow!("ws closed before message received"))?
            .context("ws read")?;
        match msg {
            Message::Text(txt) => {
                return serde_json::from_str(&txt).context("parsing ws JSON");
            }
            Message::Binary(_) => bail!("unexpected binary frame during handshake"),
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
            Message::Close(_) => bail!("ws closed during handshake"),
        }
    }
}

