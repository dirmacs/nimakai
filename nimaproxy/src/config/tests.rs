use super::*;
use std::io::Write;
use tempfile::NamedTempFile;

fn write_temp_config(content: &str) -> NamedTempFile {
    let mut file = tempfile::Builder::new().suffix(".toml").tempfile().unwrap();
    file.write_all(content.as_bytes()).unwrap();
    file
}

#[test]
fn test_load_valid_config() {
    let file = write_temp_config(
        r#"
listen = "127.0.0.1:9000"
target = "https://custom.api.com"

[[keys]]
key = "nvapi-test"
label = "test-key"

[routing]
strategy = "latency_aware"
"#,
    );

    let path = file.path().to_str().unwrap();
    let config = load(path).unwrap();

    assert_eq!(config.listen_addr(), "127.0.0.1:9000");
    assert_eq!(config.target_url(), "https://custom.api.com");
    assert_eq!(config.keys.len(), 1);
    assert_eq!(config.keys[0].key, "nvapi-test");
}

#[test]
fn test_load_missing_file() {
    let result = load("/nonexistent/path/config.toml");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("cannot read config"));
}

#[test]
fn test_load_invalid_toml() {
    let file = write_temp_config("this is not valid toml = ");
    let path = file.path().to_str().unwrap();
    let result = load(path);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("invalid config"));
}

#[test]
fn test_defaults() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"
"#,
    );

    let config = load(file.path().to_str().unwrap()).unwrap();

    assert_eq!(config.listen_addr(), "127.0.0.1:8080");
    assert_eq!(config.target_url(), "https://integrate.api.nvidia.com");
}

#[test]
fn test_multiple_keys() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "key1"
label = "k-alpha"

[[keys]]
key = "key2"
label = "k-beta"

[[keys]]
key = "key3"
"#,
    );

    let config = load(file.path().to_str().unwrap()).unwrap();
    assert_eq!(config.keys.len(), 3);
    assert_eq!(config.keys[0].label, Some("k-alpha".to_string()));
    assert_eq!(config.keys[1].label, Some("k-beta".to_string()));
    assert_eq!(config.keys[2].label, None);
}

#[test]
fn test_disabled_key_is_parsed_not_silently_dropped() {
    // Regression: `disabled = true` used to be ignored by serde (unknown
    // field), so a parked key was leased and failed per-request.
    let file = write_temp_config(
        r#"
[[keys]]
key = "key1"
label = "k-alpha"

[[keys]]
key = "key2"
label = "k-beta"
disabled = true

[[keys]]
key = "key3"
label = "backup"
"#,
    );

    let config = load(file.path().to_str().unwrap()).unwrap();
    // All three still load (the entry is retained for reference)...
    assert_eq!(config.keys.len(), 3);
    // ...but only the parked one is flagged.
    assert!(!config.keys[0].disabled);
    assert!(config.keys[1].disabled, "parked key must parse as disabled");
    assert!(!config.keys[2].disabled);

    // Assert against the real function main.rs uses — not a re-implementation
    // of the filter in the test, which would pass even if the filter broke.
    let (leasable, parked) = config.partition_keys();
    assert_eq!(
        leasable.iter().map(|k| k.key.as_str()).collect::<Vec<_>>(),
        vec!["key1", "key3"]
    );
    assert_eq!(parked.len(), 1);
    assert_eq!(parked[0].label.as_deref(), Some("k-beta"));
}

#[test]
fn test_partition_keys_parks_only_disabled() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "k1"
label = "k-alpha"
disabled = true

[[keys]]
key = "k2"

[[keys]]
key = "k3"
label = "backup"
disabled = true
"#,
    );

    let config = load(file.path().to_str().unwrap()).unwrap();
    let (leasable, parked) = config.partition_keys();
    assert_eq!(
        leasable.iter().map(|k| k.key.as_str()).collect::<Vec<_>>(),
        vec!["k2"]
    );
    assert_eq!(parked.len(), 2);
}

#[test]
fn test_missing_disabled_defaults_to_false() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "key1"
label = "k-alpha"
"#,
    );

    let config = load(file.path().to_str().unwrap()).unwrap();
    assert!(!config.keys[0].disabled);
}

#[test]
fn test_routing_config_parsing() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"

[routing]
strategy = "latency_aware"
spike_threshold_ms = 5000.0
"#,
    );

    let content = std::fs::read_to_string(file.path()).unwrap();

    let result: Result<Config, _> = toml::from_str(&content);
    assert!(result.is_ok(), "TOML should parse: {:?}", result.err());

    let config = result.unwrap();
    assert_eq!(config.keys.len(), 1);
}

#[test]
fn test_model_params_parsing() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"

[model_params."nvidia/llama"]
temperature = 0.7
top_p = 0.95
top_k = 40
max_tokens = 16384
reasoning_budget = 16384
stream = false
repetition_penalty = 1.0
reasoning_effort = "high"
chat_template_kwargs = { thinking = true, enable_thinking = true }

[model_params."nvidia/coder"]
temperature = 0.3
top_p = 0.9
max_tokens = 4096
"#,
    );

    let config = load(file.path().to_str().unwrap()).unwrap();

    let llama_params = config.get_model_params("nvidia/llama");
    assert!(llama_params.is_some());
    let llama = llama_params.unwrap();
    assert_eq!(llama.temperature, Some(0.7));
    assert_eq!(llama.top_p, Some(0.95));
    assert_eq!(llama.top_k, Some(40));
    assert_eq!(llama.max_tokens, Some(16384));
    assert_eq!(llama.reasoning_budget, Some(16384));
    assert_eq!(llama.stream, Some(false));
    assert_eq!(llama.repetition_penalty, Some(1.0));
    assert_eq!(llama.reasoning_effort, Some("high".to_string()));
    assert_eq!(llama.get("thinking"), Some(&serde_json::json!(true)));
    assert_eq!(llama.get("enable_thinking"), Some(&serde_json::json!(true)));

    let coder_params = config.get_model_params("nvidia/coder");
    assert!(coder_params.is_some());
    let coder = coder_params.unwrap();
    assert_eq!(coder.temperature, Some(0.3));
    assert_eq!(coder.top_p, Some(0.9));
    assert_eq!(coder.max_tokens, Some(4096));
}

#[test]
fn test_model_params_returns_none_for_unknown_model() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"

[model_params."known-model"]
temperature = 0.5
"#,
    );

    let config = load(file.path().to_str().unwrap()).unwrap();
    assert!(config.get_model_params("unknown-model").is_none());
}

#[test]
fn test_model_params_returns_none_when_not_configured() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"
"#,
    );

    let config = load(file.path().to_str().unwrap()).unwrap();
    assert!(config.get_model_params("any-model").is_none());
}

#[test]
fn test_model_params_partial_config() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"

[model_params."fast-model"]
temperature = 1.0
"#,
    );

    let config = load(file.path().to_str().unwrap()).unwrap();
    let params = config.get_model_params("fast-model").unwrap();
    assert_eq!(params.temperature, Some(1.0));
    assert_eq!(params.top_p, None);
    assert_eq!(params.top_k, None);
    assert_eq!(params.max_tokens, None);
    assert_eq!(params.reasoning_budget, None);
    assert_eq!(params.stream, None);
    assert_eq!(params.repetition_penalty, None);
}

#[test]
fn test_circuit_breaker_config_parsing() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"

[circuit_breaker]
max_output_tokens = 16000
max_repetitions = 3
max_consecutive_assistant_turns = 5
"#,
    );

    let config = load(file.path().to_str().unwrap()).unwrap();
    let cb = config.circuit_breaker_config();

    assert_eq!(cb.max_output_tokens, 16000);
    assert_eq!(cb.max_repetitions, 3);
    assert_eq!(cb.max_consecutive_assistant_turns, 5);
}

#[test]
fn test_circuit_breaker_config_uses_defaults() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"
"#,
    );

    let config = load(file.path().to_str().unwrap()).unwrap();
    let cb = config.circuit_breaker_config();

    assert_eq!(cb.max_output_tokens, 32000);
    assert_eq!(cb.max_repetitions, 5);
    assert_eq!(cb.max_consecutive_assistant_turns, 10);
}

#[test]
fn test_model_compat_parsing() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"

[model_compat]
supports_developer_role = ["mistralai/model1", "mistralai/model2"]
supports_tool_messages = ["mistralai/model1"]
"#,
    );

    let config = load(file.path().to_str().unwrap()).unwrap();

    assert!(config.model_compat.is_some());
    let compat = config.model_compat.as_ref().unwrap();

    assert!(!compat.should_transform_developer_role("mistralai/model1"));
    assert!(!compat.should_transform_developer_role("mistralai/model2"));
    assert!(compat.should_transform_developer_role("unknown-model"));
    assert!(compat.should_transform_developer_role("stepfun-ai/step-3.7-flash"));

    assert!(!compat.should_transform_tool_messages("mistralai/model1"));
    assert!(compat.should_transform_tool_messages("mistralai/model2"));
    assert!(compat.should_transform_tool_messages("unknown-model"));
}

#[test]
fn test_model_compat_empty_returns_false() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"
"#,
    );

    let config = load(file.path().to_str().unwrap()).unwrap();

    assert!(config.should_transform_developer_role("any-model"));
    assert!(config.should_transform_tool_messages("any-model"));
}

#[test]
fn test_transform_role_helper_logic() {
    let compat = ModelCompat {
        supports_developer_role: Some(vec!["allowed-model".to_string()]),
        supports_tool_messages: Some(vec!["allowed-model".to_string()]),
    };

    assert!(!compat.should_transform_developer_role("allowed-model"));
    assert!(!compat.should_transform_tool_messages("allowed-model"));
    assert!(compat.should_transform_developer_role("blocked-model"));
    assert!(compat.should_transform_tool_messages("blocked-model"));
}
#[test]
fn test_listen_addr_default() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert_eq!(config.listen_addr(), "127.0.0.1:8080");
}

#[test]
fn test_listen_addr_custom() {
    let file = write_temp_config(
        r#"
listen = "0.0.0.0:9090"
[[keys]]
key = "test"
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert_eq!(config.listen_addr(), "0.0.0.0:9090");
}

#[test]
fn test_target_url_default() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert_eq!(config.target_url(), "https://integrate.api.nvidia.com");
}

#[test]
fn test_target_url_custom() {
    let file = write_temp_config(
        r#"
target = "https://custom.endpoint.com"
[[keys]]
key = "test"
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert_eq!(config.target_url(), "https://custom.endpoint.com");
}

#[test]
fn test_racing_enabled_default() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert!(config.racing_enabled());
}

#[test]
fn test_racing_enabled_true() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"

[racing]
enabled = true
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert!(config.racing_enabled());
}

#[test]
fn test_racing_models_empty() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert!(config.racing_models().is_empty());
}

#[test]
fn test_racing_models_with_config() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"

[racing]
models = ["model1", "model2", "model3"]
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    let models = config.racing_models();
    assert_eq!(models.len(), 3);
    assert_eq!(models[0], "model1");
    assert_eq!(models[2], "model3");
}

#[test]
fn test_racing_max_parallel_default() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert_eq!(config.racing_max_parallel(), 3);
}

#[test]
fn test_racing_max_parallel_configured() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"

[racing]
max_parallel = 5
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert_eq!(config.racing_max_parallel(), 5);
}

#[test]
fn test_racing_timeout_ms_default() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert_eq!(config.racing_timeout_ms(), 8000);
}

#[test]
fn test_racing_timeout_ms_configured() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"

[racing]
timeout_ms = 12000
max_total_request_ms = 30000
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert_eq!(config.racing_timeout_ms(), 12000);
    assert_eq!(config.racing_max_total_request_ms(), 30000);
}

#[test]
fn test_racing_max_total_request_ms_default() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert_eq!(config.racing_max_total_request_ms(), 30000);
}

#[test]
fn test_racing_max_total_request_ms_can_be_disabled() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"

[racing]
max_total_request_ms = 0
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert_eq!(config.racing_max_total_request_ms(), 0);
}

#[test]
fn test_racing_strategy_default() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert_eq!(config.racing_strategy(), "complete");
}

#[test]
fn test_racing_strategy_configured() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"

[racing]
strategy = "first_token"
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert_eq!(config.racing_strategy(), "first_token");
}

#[test]
fn test_racing_model_check_interval_secs_default() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert_eq!(config.racing_model_check_interval_secs(), 3600);
}

#[test]
fn test_racing_model_check_interval_secs_configured() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"

[racing]
model_check_interval_secs = 900
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert_eq!(config.racing_model_check_interval_secs(), 900);
}

#[test]
fn test_racing_model_check_interval_secs_can_be_disabled() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"

[racing]
model_check_interval_secs = 0
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert_eq!(config.racing_model_check_interval_secs(), 0);
}

#[test]
fn test_adaptive_racing_limits_and_timeouts_configured() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"

[racing]
adaptive = true
min_parallel = 2
pressure_parallel = 6
degraded_parallel = 3
fast_models = ["fast-a", "fast-b"]
fallback_models = ["slow-a"]
large_prompt_char_threshold = 12000
large_prompt_parallel = 1
solo_fallback = true
max_total_request_ms = 45000

[limits]
max_upstream_in_flight = 48
max_in_flight_per_key = 3
admission_wait_ms = 5000

[logging]
enabled = true
path = "/tmp/nimaproxy-turns.jsonl"

[timeouts]
min_dynamic_timeout_ms = 8000
dynamic_sample_floor = 10
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert!(config.racing_adaptive());
    assert_eq!(config.racing_min_parallel(), 2);
    assert_eq!(config.racing_pressure_parallel(), 6);
    assert_eq!(config.racing_degraded_parallel(), 3);
    assert_eq!(config.racing_fast_models(), vec!["fast-a", "fast-b"]);
    assert_eq!(config.racing_fallback_models(), vec!["slow-a"]);
    assert_eq!(config.racing_large_prompt_char_threshold(), 12000);
    assert_eq!(config.racing_large_prompt_parallel(), 1);
    assert!(config.racing_solo_fallback());
    assert_eq!(config.racing_max_total_request_ms(), 45000);
    assert_eq!(config.max_upstream_in_flight(), 48);
    assert_eq!(config.max_in_flight_per_key(), 3);
    assert_eq!(config.admission_wait_ms(), 5000);
    assert!(config.logging_enabled());
    assert_eq!(config.logging_path(), "/tmp/nimaproxy-turns.jsonl");
    assert_eq!(config.min_dynamic_timeout_ms(), 8000);
    assert_eq!(config.dynamic_sample_floor(), 10);
    // Unset first-byte override stays unset: resolution to the per-model
    // dynamic window happens in AppState. The idle rail always resolves
    // (180 s default).
    assert_eq!(config.first_byte_timeout_ms(), None);
    assert_eq!(config.mid_stream_idle_timeout_ms(), 180_000);
}

#[test]
fn test_first_byte_and_idle_window_overrides_configured() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"

[timeouts]
min_dynamic_timeout_ms = 8000
dynamic_sample_floor = 10
first_byte_timeout_ms = 12000
mid_stream_idle_timeout_ms = 60000
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert_eq!(config.first_byte_timeout_ms(), Some(12000));
    assert_eq!(config.mid_stream_idle_timeout_ms(), 60000);
}

#[test]
fn test_first_byte_override_alone_leaves_idle_unset() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"

[timeouts]
first_byte_timeout_ms = 12000
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert_eq!(config.first_byte_timeout_ms(), Some(12000));
    assert_eq!(config.mid_stream_idle_timeout_ms(), 180_000);
}

#[test]
fn test_idle_override_alone_leaves_first_byte_unset() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"

[timeouts]
mid_stream_idle_timeout_ms = 60000
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert_eq!(config.mid_stream_idle_timeout_ms(), 60000);
    assert_eq!(config.first_byte_timeout_ms(), None);
}

#[test]
fn test_adaptive_racing_defaults() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert!(!config.racing_adaptive());
    assert_eq!(config.racing_min_parallel(), 2);
    assert_eq!(config.racing_pressure_parallel(), 6);
    assert_eq!(config.racing_degraded_parallel(), 3);
    assert_eq!(config.racing_large_prompt_char_threshold(), 0);
    assert_eq!(config.racing_large_prompt_parallel(), 1);
    assert!(config.racing_solo_fallback());
    assert_eq!(config.racing_max_total_request_ms(), 30000);
    assert_eq!(config.max_upstream_in_flight(), 48);
    assert_eq!(config.max_in_flight_per_key(), 3);
    assert_eq!(config.admission_wait_ms(), 1500);
    assert!(!config.logging_enabled());
    assert_eq!(config.logging_path(), "/var/log/nimaproxy/turns.jsonl");
    assert_eq!(config.min_dynamic_timeout_ms(), 8000);
    assert_eq!(config.dynamic_sample_floor(), 10);
}

#[test]
fn test_auth_failure_cooldown_secs_default() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert_eq!(config.auth_failure_cooldown_secs(), 900);
}

#[test]
fn test_auth_failure_cooldown_secs_configured() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"

[limits]
auth_failure_cooldown_secs = 120
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert_eq!(config.auth_failure_cooldown_secs(), 120);
}

#[test]
fn test_auth_failure_cooldown_secs_zero_disables() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"

[limits]
auth_failure_cooldown_secs = 0
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert_eq!(config.auth_failure_cooldown_secs(), 0);
}

#[test]
fn test_routing_models_empty() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert!(config.routing_models().is_empty());
}

#[test]
fn test_routing_models_with_config() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"

[routing]
models = ["auto-model-1", "auto-model-2"]
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    let models = config.routing_models();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0], "auto-model-1");
    assert_eq!(models[1], "auto-model-2");
}

#[test]
fn test_routing_strategy_default() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert_eq!(config.routing_strategy(), "round_robin");
}

#[test]
fn test_routing_strategy_configured() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"

[routing]
strategy = "latency_aware"
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert_eq!(config.routing_strategy(), "latency_aware");
}

#[test]
fn test_routing_spike_threshold_default() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert_eq!(config.routing_spike_threshold_ms(), 3000.0);
}

#[test]
fn test_routing_spike_threshold_configured() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"

[routing]
spike_threshold_ms = 4500.0
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert_eq!(config.routing_spike_threshold_ms(), 4500.0);
}

#[test]
fn test_routing_enabled_with_models() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"

[routing]
models = ["model1"]
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert!(config.routing_enabled());
}

#[test]
fn test_routing_enabled_empty() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"

[routing]
strategy = "round_robin"
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert!(config.routing_enabled());
}

#[test]
fn test_get_model_params_none() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    assert!(config.get_model_params("any-model").is_none());
}

#[test]
fn test_circuit_breaker_defaults() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    let cb = config.circuit_breaker_config();
    assert_eq!(cb.max_output_tokens, 32000);
    assert_eq!(cb.max_repetitions, 5);
    assert_eq!(cb.max_consecutive_assistant_turns, 10);
}

#[test]
fn test_circuit_breaker_configured() {
    let file = write_temp_config(
        r#"
[[keys]]
key = "test"

[circuit_breaker]
max_output_tokens = 8000
max_repetitions = 2
max_consecutive_assistant_turns = 3
"#,
    );
    let config = load(file.path().to_str().unwrap()).unwrap();
    let cb = config.circuit_breaker_config();
    assert_eq!(cb.max_output_tokens, 8000);
    assert_eq!(cb.max_repetitions, 2);
    assert_eq!(cb.max_consecutive_assistant_turns, 3);
}

// Test 21: ModelParams::get with missing key
#[test]
fn test_model_params_get_missing_key() {
    let params = ModelParams {
        chat_template_kwargs: Some({
            let mut map = std::collections::HashMap::new();
            map.insert("existing_key".to_string(), serde_json::json!("value"));
            map
        }),
        ..Default::default()
    };

    // Should return Some for existing key
    assert!(params.get("existing_key").is_some());

    // Should return None for missing key
    assert!(params.get("missing_key").is_none());

    // Should return None when chat_template_kwargs is None
    let params_no_kwargs = ModelParams::default();
    assert!(params_no_kwargs.get("any_key").is_none());
}

// Tests below mutate the process environment, which is global and shared
// across test threads, so they serialize on this lock and clean up every
// variable they set.
static ENV_TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

#[test]
fn test_literal_key_passes_through_unchanged() {
    let _guard = ENV_TEST_LOCK.lock();
    let file = write_temp_config(
        r#"
[[keys]]
key = "nvapi-literal-key-material"
label = "literal"
"#,
    );

    let config = load(file.path().to_str().unwrap()).unwrap();

    assert_eq!(config.keys.len(), 1);
    assert_eq!(config.keys[0].key, "nvapi-literal-key-material");
}

#[test]
fn test_placeholder_expands_when_set() {
    let _guard = ENV_TEST_LOCK.lock();
    std::env::set_var("NIMAPROXY_TEST_KEY_A", "nvapi-resolved-secret-a");
    let file = write_temp_config(
        r#"
[[keys]]
key = "${NIMAPROXY_TEST_KEY_A}"
label = "from-env"
"#,
    );

    let result = load(file.path().to_str().unwrap());
    std::env::remove_var("NIMAPROXY_TEST_KEY_A");

    let config = result.unwrap();
    assert_eq!(config.keys.len(), 1);
    assert_eq!(config.keys[0].key, "nvapi-resolved-secret-a");
}

#[test]
fn test_unset_variable_fails_closed_without_key_material() {
    let _guard = ENV_TEST_LOCK.lock();
    std::env::remove_var("NIMAPROXY_TEST_KEY_MISSING");
    let file = write_temp_config(
        r#"
[[keys]]
key = "nvapi-first-literal"
label = "first"

[[keys]]
key = "${NIMAPROXY_TEST_KEY_MISSING}"
label = "second"
"#,
    );

    let result = load(file.path().to_str().unwrap());

    let err = result.unwrap_err();
    assert!(
        err.contains("NIMAPROXY_TEST_KEY_MISSING"),
        "error must name the variable: {err}"
    );
    assert!(
        err.contains('1') && err.contains("second"),
        "error must identify the entry by index and label: {err}"
    );
    assert!(
        !err.contains("nvapi-first-literal"),
        "error must not echo key material: {err}"
    );
    assert!(
        !err.contains("${") && !err.contains('}'),
        "error must not echo the placeholder text: {err}"
    );
}

#[test]
fn test_embedded_placeholder_stays_literal() {
    let _guard = ENV_TEST_LOCK.lock();
    std::env::set_var("NIMAPROXY_TEST_KEY_X", "nvapi-should-not-appear");
    let file = write_temp_config(
        r#"
[[keys]]
key = "abc${NIMAPROXY_TEST_KEY_X}def"

[[keys]]
key = "${1BAD}"

[[keys]]
key = "$NIMAPROXY_TEST_KEY_X"
"#,
    );

    let result = load(file.path().to_str().unwrap());
    std::env::remove_var("NIMAPROXY_TEST_KEY_X");

    let config = result.unwrap();
    assert_eq!(config.keys.len(), 3);
    assert_eq!(config.keys[0].key, "abc${NIMAPROXY_TEST_KEY_X}def");
    assert_eq!(config.keys[1].key, "${1BAD}");
    assert_eq!(config.keys[2].key, "$NIMAPROXY_TEST_KEY_X");
}

#[test]
fn test_two_placeholders_resolve_independently() {
    let _guard = ENV_TEST_LOCK.lock();
    std::env::set_var("NIMAPROXY_TEST_KEY_ONE", "nvapi-resolved-secret-one");
    std::env::set_var("NIMAPROXY_TEST_KEY_TWO", "nvapi-resolved-secret-two");
    let file = write_temp_config(
        r#"
[[keys]]
key = "${NIMAPROXY_TEST_KEY_ONE}"

[[keys]]
key = "${NIMAPROXY_TEST_KEY_TWO}"
"#,
    );

    let result = load(file.path().to_str().unwrap());
    std::env::remove_var("NIMAPROXY_TEST_KEY_ONE");
    std::env::remove_var("NIMAPROXY_TEST_KEY_TWO");

    let config = result.unwrap();
    assert_eq!(config.keys.len(), 2);
    assert_eq!(config.keys[0].key, "nvapi-resolved-secret-one");
    assert_eq!(config.keys[1].key, "nvapi-resolved-secret-two");
}

#[test]
fn test_set_but_empty_variable_fails_closed() {
    let _guard = ENV_TEST_LOCK.lock();
    std::env::set_var("NIMAPROXY_TEST_KEY_EMPTY", "");
    let file = write_temp_config(
        r#"
[[keys]]
key = "${NIMAPROXY_TEST_KEY_EMPTY}"
label = "empty"
"#,
    );

    let result = load(file.path().to_str().unwrap());
    std::env::remove_var("NIMAPROXY_TEST_KEY_EMPTY");

    let err = result.unwrap_err();
    assert!(
        err.contains("NIMAPROXY_TEST_KEY_EMPTY"),
        "error must name the variable: {err}"
    );
    assert!(
        err.contains("empty"),
        "error must say the variable is empty: {err}"
    );
    assert!(
        !err.contains("${") && !err.contains('}'),
        "error must not echo the placeholder text: {err}"
    );
}

#[test]
fn test_validate_dynamic_timeout_pair_rejects_inverted() {
    let err = validate_dynamic_timeout_pair(30_000, 25_000, 25).unwrap_err();
    assert!(
        err.contains("min_dynamic_timeout_ms"),
        "error must name the floor key: {err}"
    );
    assert!(
        err.contains("timeout_ms"),
        "error must name the ceiling key: {err}"
    );
    assert!(
        err.contains("30000") && err.contains("25000"),
        "error must carry both values: {err}"
    );
}

#[test]
fn test_validate_dynamic_timeout_pair_accepts_floor_below_ceiling() {
    assert!(validate_dynamic_timeout_pair(8_000, 25_000, 10).is_ok());
}

#[test]
fn test_validate_dynamic_timeout_pair_accepts_deliberate_pin() {
    // EQUALITY is accepted, not exempted: the canonical config pins
    // 25000/25000 so both timeout arms yield 25000, and every all-defaults
    // config resolves to 8000/8000 — including the minimal configs the six
    // `ffi_tests` lifecycle tests boot the real binary on. The
    // equality-collapse behavior is pinned by
    // `model_stats::test_policy_floor_equal_to_max_pins_timeout_to_that_constant`;
    // this guard owns only the strict inversion.
    assert!(validate_dynamic_timeout_pair(25_000, 25_000, 25).is_ok());
    assert!(validate_dynamic_timeout_pair(8_000, 8_000, 10).is_ok());
}
