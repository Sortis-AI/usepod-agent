//! Config parsing + validation tests. Spec: `plan/V2_AGENT_SPEC.md` §3.1.

use std::path::PathBuf;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("agent.example.toml")
}

#[test]
fn parses_example_config() {
    let cfg = provider_agent::config::load(Some(&fixture_path()), false)
        .expect("example config should parse and validate");
    assert_eq!(cfg.operator.display_name, "BlueBoxRig-01");
    assert_eq!(cfg.coordinator.url, "wss://api.usepod.ai/provider/connect");
    assert_eq!(cfg.backends.len(), 2);
    assert_eq!(cfg.backends[0].kind, "vllm");
    assert_eq!(cfg.backends[1].kind, "openrouter");
    assert_eq!(cfg.pricing.default_input_per_1m, 500_000);
    assert_eq!(cfg.pricing.default_output_per_1m, 750_000);
    assert!(cfg.pricing.models.contains_key("meta-llama/Llama-3.3-70B-Instruct"));
    assert_eq!(cfg.limits.max_concurrent, 8);
}

#[test]
fn rejects_ws_without_allow_insecure() {
    let toml_str = r#"
        [operator]
        display_name = "X"
        wallet = "11111111111111111111111111111111"
        [coordinator]
        url = "ws://localhost:8080"
        [[backends]]
        kind = "vllm"
        url = "http://localhost:8000"
        [pricing]
        default_input_per_1m = 1
        default_output_per_1m = 1
    "#;
    let cfg: provider_agent::config::Config = toml::from_str(toml_str).unwrap();
    let err = provider_agent::config::validate(&cfg, false).unwrap_err();
    assert!(format!("{err}").contains("wss"), "got: {err}");
    // With allow_insecure, it should pass.
    provider_agent::config::validate(&cfg, true).unwrap();
}

#[test]
fn rejects_duplicate_backends() {
    let toml_str = r#"
        [operator]
        display_name = "X"
        wallet = "11111111111111111111111111111111"
        [coordinator]
        url = "wss://x.example/connect"
        [[backends]]
        kind = "vllm"
        url = "http://localhost:8000"
        [[backends]]
        kind = "vllm"
        url = "http://localhost:8000"
        [pricing]
        default_input_per_1m = 1
        default_output_per_1m = 1
    "#;
    let cfg: provider_agent::config::Config = toml::from_str(toml_str).unwrap();
    let err = provider_agent::config::validate(&cfg, false).unwrap_err();
    assert!(format!("{err}").to_lowercase().contains("duplicate"), "got: {err}");
}

#[test]
fn rejects_max_concurrent_out_of_range() {
    let toml_str = r#"
        [operator]
        display_name = "X"
        wallet = "11111111111111111111111111111111"
        [coordinator]
        url = "wss://x.example/connect"
        [[backends]]
        kind = "vllm"
        url = "http://localhost:8000"
        [pricing]
        default_input_per_1m = 1
        default_output_per_1m = 1
        [limits]
        max_concurrent = 999
    "#;
    let cfg: provider_agent::config::Config = toml::from_str(toml_str).unwrap();
    let err = provider_agent::config::validate(&cfg, false).unwrap_err();
    assert!(format!("{err}").contains("max_concurrent"), "got: {err}");
}

#[test]
fn requires_default_pricing() {
    let toml_str = r#"
        [operator]
        display_name = "X"
        wallet = "11111111111111111111111111111111"
        [coordinator]
        url = "wss://x.example/connect"
        [[backends]]
        kind = "vllm"
        url = "http://localhost:8000"
        [pricing]
        default_input_per_1m = 0
        default_output_per_1m = 0
    "#;
    let cfg: provider_agent::config::Config = toml::from_str(toml_str).unwrap();
    let err = provider_agent::config::validate(&cfg, false).unwrap_err();
    assert!(format!("{err}").contains("default_"), "got: {err}");
}
