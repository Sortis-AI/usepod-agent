//! Discovery integration test: a vLLM backend pointed at an unreachable port
//! must produce an empty capability list without panicking. Graceful failure
//! is the contract for offline backends.

use provider_agent::config;
use provider_agent::discovery;
use std::io::Write;

fn write_config(toml: &str) -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(toml.as_bytes()).unwrap();
    f.flush().unwrap();
    f
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreachable_vllm_yields_empty_capabilities() {
    // 127.0.0.1:1 is reserved/closed on every sane system; connect attempts
    // fail fast.
    let cfg_file = write_config(
        r#"
[operator]
display_name = "test"
wallet = "fake-wallet"

[coordinator]
url = "wss://example.invalid/connect"

[[backends]]
kind = "vllm"
url  = "http://127.0.0.1:1"

[pricing]
default_input_per_1m  = 500000
default_output_per_1m = 750000
"#,
    );

    let cfg = config::load(Some(cfg_file.path()), false).expect("config parses");
    let result = discovery::run(&cfg).await;
    assert!(
        result.backends.is_empty(),
        "unreachable backend must be dropped, got {} healthy",
        result.backends.len()
    );
    assert!(
        result.capability_models.is_empty(),
        "no models should be advertised when no backend is healthy"
    );

    // The capabilities payload should still serialize and have an empty
    // `models` array — the agent must register *something* so the coordinator
    // knows it's online with no models.
    let caps = result.to_capabilities(&cfg);
    assert_eq!(caps.get("type").and_then(|v| v.as_str()), Some("capabilities"));
    assert_eq!(
        caps.get("models").and_then(|v| v.as_array()).map(|a| a.len()),
        Some(0)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_kind_is_skipped() {
    // `config::load` rejects unknown kinds during validation, so we go through
    // TOML deserialization directly to exercise `build_backend`'s defensive
    // fallback path.
    let from_toml: config::Backend = toml::from_str(
        r#"
kind = "totally-fake"
url  = "http://127.0.0.1:1"
"#,
    )
    .unwrap();
    assert!(discovery::build_backend(&from_toml).is_none());
}
