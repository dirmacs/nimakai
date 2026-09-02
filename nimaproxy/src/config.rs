use serde::Deserialize;
use std::fs;

#[derive(Deserialize, Clone, Debug, Default)]
pub struct ModelCompat {
    pub supports_developer_role: Option<Vec<String>>,
    pub supports_tool_messages: Option<Vec<String>>,
}

impl ModelCompat {
    pub fn should_transform_developer_role(&self, model_id: &str) -> bool {
        // If list is None (not configured) or empty, transform ALL models
        // If list has entries:
        //   - If list contains "all", transform NO models (all support the feature)
        //   - Otherwise, only transform models NOT in the list
        if let Some(models) = &self.supports_developer_role {
            // List exists
            if models.iter().any(|m| m == "all") {
                // Special case: "all" means all models support the feature
                return false; // Don't transform any models
            }
            // List exists but doesn't contain "all": transform if model is NOT in the list
            return !models.iter().any(|m| m == model_id);
        }
        // No config: transform all models (default behavior)
        true
    }

    pub fn should_transform_tool_messages(&self, model_id: &str) -> bool {
        // If model is in supports_tool_messages list, it supports tool messages
        // and should NOT be transformed. Return false for these models.
        if let Some(models) = &self.supports_tool_messages {
            // Special case: "all" means all models support tool messages
            if models.iter().any(|m| m == "all") {
                return false; // Model supports tool messages, don't transform
            }
            if models.iter().any(|m| m == model_id) {
                return false; // Model supports tool messages, don't transform
            }
        }
        true // Model not in list, transform tool messages
    }
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct CircuitBreakerConfig {
    pub max_output_tokens: Option<u32>,
    pub max_repetitions: Option<u32>,
    pub max_consecutive_assistant_turns: Option<u32>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Config {
    pub listen: Option<String>,
    pub target: Option<String>,
    pub keys: Vec<KeyEntry>,
    pub routing: Option<RoutingConfig>,
    pub racing: Option<RacingConfig>,
    pub limits: Option<LimitsConfig>,
    pub timeouts: Option<TimeoutsConfig>,
    pub logging: Option<LoggingConfig>,
    pub model_params: Option<std::collections::HashMap<String, ModelParams>>,
    pub model_compat: Option<ModelCompat>,
    pub circuit_breaker: Option<CircuitBreakerConfig>,
}

impl Config {
    pub fn circuit_breaker_config(&self) -> crate::model_stats::CircuitBreakerConfig {
        crate::model_stats::CircuitBreakerConfig {
            max_output_tokens: self
                .circuit_breaker
                .as_ref()
                .and_then(|c| c.max_output_tokens)
                .unwrap_or(32000),
            max_repetitions: self
                .circuit_breaker
                .as_ref()
                .and_then(|c| c.max_repetitions)
                .unwrap_or(5),
            max_consecutive_assistant_turns: self
                .circuit_breaker
                .as_ref()
                .and_then(|c| c.max_consecutive_assistant_turns)
                .unwrap_or(10),
        }
    }
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct ModelParams {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<i32>,
    pub max_tokens: Option<i32>,
    pub reasoning_budget: Option<i32>,
    pub stream: Option<bool>,
    /// Penalty for frequency of repeated tokens (reduces repetition)
    pub frequency_penalty: Option<f64>,
    pub presence_penalty: Option<f64>,
    pub repetition_penalty: Option<f64>,
    pub min_p: Option<f64>,
    pub reasoning_effort: Option<String>,
    pub seed: Option<i32>,
    pub chat_template_kwargs: Option<std::collections::HashMap<String, serde_json::Value>>,
}

impl ModelParams {
    pub fn get(&self, key: &str) -> Option<&serde_json::Value> {
        self.chat_template_kwargs.as_ref()?.get(key)
    }
}

#[derive(Deserialize, Clone, Debug)]
pub struct KeyEntry {
    pub key: String,
    pub label: Option<String>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct RoutingConfig {
    /// "round_robin" (default) or "latency_aware"
    pub strategy: Option<String>,
    /// Model list for auto-routing. When the request contains `"model": "auto"`,
    /// the router picks from this list.
    pub models: Option<Vec<String>>,
    /// Avg TTFC above this value marks a model as degraded (default 3000ms).
    pub spike_threshold_ms: Option<f64>,
}

/// Racing config for speculative execution.
///
/// When enabled, fires N parallel requests to N models and returns the first response.
/// This is "model racing" — trades N×token budget for min(P50 latency).
#[derive(Deserialize, Clone, Debug)]
pub struct RacingConfig {
    /// Enable speculative execution (default: false)
    pub enabled: Option<bool>,
    /// List of models to race. Must have 2+ models.
    pub models: Option<Vec<String>>,
    /// Max parallel requests (default: 3, no upper cap - config value is trusted)
    pub max_parallel: Option<usize>,
    /// Timeout per request in ms (default: 8000ms)
    pub timeout_ms: Option<u64>,
    /// End-to-end wall-clock budget for racing plus sequential fallback. 0 disables.
    pub max_total_request_ms: Option<u64>,
    /// Strategy: "first_token" (return on first SSE token) or "complete" (default)
    pub strategy: Option<String>,
    /// Enable adaptive runtime fan-out under key/upstream pressure.
    pub adaptive: Option<bool>,
    /// Minimum racing fan-out when adaptive mode has enough capacity.
    pub min_parallel: Option<usize>,
    /// Fan-out target when the gateway is under moderate pressure.
    pub pressure_parallel: Option<usize>,
    /// Fan-out target when keys are cooling down or the gateway is heavily loaded.
    pub degraded_parallel: Option<usize>,
    /// Preferred fast models for normal racing.
    pub fast_models: Option<Vec<String>>,
    /// Slower or more expensive models used as backfill/fallback candidates.
    pub fallback_models: Option<Vec<String>>,
    /// If prompt text exceeds this many chars, cap racing fan-out.
    pub large_prompt_char_threshold: Option<usize>,
    /// Fan-out cap for large prompt requests. A value of 1 enables solo fallback.
    pub large_prompt_parallel: Option<usize>,
    /// Allow racing to degrade to a single model when fewer than two racers are viable.
    pub solo_fallback: Option<bool>,
    /// Interval in seconds between background upstream `/v1/models` rechecks that mark
    /// configured-but-vanished models degraded. Default 3600. `0` disables the recheck task.
    pub model_check_interval_secs: Option<u64>,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct LimitsConfig {
    pub max_upstream_in_flight: Option<usize>,
    pub max_in_flight_per_key: Option<usize>,
    pub admission_wait_ms: Option<u64>,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct TimeoutsConfig {
    pub min_dynamic_timeout_ms: Option<u64>,
    pub dynamic_sample_floor: Option<usize>,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct LoggingConfig {
    pub enabled: Option<bool>,
    pub path: Option<String>,
}

impl Config {
    pub fn listen_addr(&self) -> String {
        self.listen
            .clone()
            .unwrap_or_else(|| "127.0.0.1:8080".to_string())
    }

    pub fn target_url(&self) -> String {
        self.target
            .clone()
            .unwrap_or_else(|| "https://integrate.api.nvidia.com".to_string())
    }

    pub fn racing_enabled(&self) -> bool {
        self.racing.as_ref().and_then(|r| r.enabled).unwrap_or(true)
    }

    pub fn racing_models(&self) -> Vec<String> {
        self.racing
            .as_ref()
            .and_then(|r| r.models.clone())
            .unwrap_or_default()
    }

    pub fn racing_max_parallel(&self) -> usize {
        // Config value or default (3), with minimum of 2
        // No upper cap - config value is trusted
        self.racing
            .as_ref()
            .and_then(|r| r.max_parallel)
            .unwrap_or(3)
            .max(2)
    }

    pub fn racing_timeout_ms(&self) -> u64 {
        self.racing
            .as_ref()
            .and_then(|r| r.timeout_ms)
            .unwrap_or(8000)
    }

    pub fn racing_max_total_request_ms(&self) -> u64 {
        self.racing
            .as_ref()
            .and_then(|r| r.max_total_request_ms)
            .unwrap_or(30000)
    }

    pub fn racing_strategy(&self) -> String {
        self.racing
            .as_ref()
            .and_then(|r| r.strategy.clone())
            .unwrap_or_else(|| "complete".to_string())
    }

    pub fn racing_adaptive(&self) -> bool {
        self.racing
            .as_ref()
            .and_then(|r| r.adaptive)
            .unwrap_or(false)
    }

    pub fn racing_min_parallel(&self) -> usize {
        self.racing
            .as_ref()
            .and_then(|r| r.min_parallel)
            .unwrap_or(2)
            .max(2)
    }

    pub fn racing_pressure_parallel(&self) -> usize {
        self.racing
            .as_ref()
            .and_then(|r| r.pressure_parallel)
            .unwrap_or(6)
            .max(2)
    }

    pub fn racing_degraded_parallel(&self) -> usize {
        self.racing
            .as_ref()
            .and_then(|r| r.degraded_parallel)
            .unwrap_or(3)
            .max(2)
    }

    pub fn racing_fast_models(&self) -> Vec<String> {
        self.racing
            .as_ref()
            .and_then(|r| r.fast_models.clone())
            .unwrap_or_default()
    }

    pub fn racing_fallback_models(&self) -> Vec<String> {
        self.racing
            .as_ref()
            .and_then(|r| r.fallback_models.clone())
            .unwrap_or_default()
    }

    pub fn racing_large_prompt_char_threshold(&self) -> usize {
        self.racing
            .as_ref()
            .and_then(|r| r.large_prompt_char_threshold)
            .unwrap_or(0)
    }

    pub fn racing_large_prompt_parallel(&self) -> usize {
        self.racing
            .as_ref()
            .and_then(|r| r.large_prompt_parallel)
            .unwrap_or(1)
            .max(1)
    }

    pub fn racing_solo_fallback(&self) -> bool {
        self.racing
            .as_ref()
            .and_then(|r| r.solo_fallback)
            .unwrap_or(true)
    }

    /// Interval in seconds between periodic upstream model rechecks. Default 3600 (1h).
    /// `0` disables the periodic recheck task.
    pub fn racing_model_check_interval_secs(&self) -> u64 {
        self.racing
            .as_ref()
            .and_then(|r| r.model_check_interval_secs)
            .unwrap_or(3600)
    }

    pub fn max_upstream_in_flight(&self) -> usize {
        self.limits
            .as_ref()
            .and_then(|l| l.max_upstream_in_flight)
            .unwrap_or(48)
            .max(1)
    }

    pub fn max_in_flight_per_key(&self) -> usize {
        self.limits
            .as_ref()
            .and_then(|l| l.max_in_flight_per_key)
            .unwrap_or(3)
            .max(1)
    }

    pub fn admission_wait_ms(&self) -> u64 {
        self.limits
            .as_ref()
            .and_then(|l| l.admission_wait_ms)
            .unwrap_or(1500)
    }

    pub fn min_dynamic_timeout_ms(&self) -> u64 {
        self.timeouts
            .as_ref()
            .and_then(|t| t.min_dynamic_timeout_ms)
            .unwrap_or(8000)
            .max(1000)
    }

    pub fn dynamic_sample_floor(&self) -> usize {
        self.timeouts
            .as_ref()
            .and_then(|t| t.dynamic_sample_floor)
            .unwrap_or(10)
            .max(2)
    }

    pub fn routing_models(&self) -> Vec<String> {
        self.routing
            .as_ref()
            .and_then(|r| r.models.clone())
            .unwrap_or_default()
    }

    pub fn routing_strategy(&self) -> String {
        self.routing
            .as_ref()
            .and_then(|r| r.strategy.clone())
            .unwrap_or_else(|| "round_robin".to_string())
    }

    pub fn routing_spike_threshold_ms(&self) -> f64 {
        self.routing
            .as_ref()
            .and_then(|r| r.spike_threshold_ms)
            .unwrap_or(3000.0)
    }

    pub fn routing_enabled(&self) -> bool {
        self.routing
            .as_ref()
            .and_then(|r| r.models.as_ref())
            .map(|m| !m.is_empty())
            .unwrap_or(true)
    }

    pub fn get_model_params(&self, model_id: &str) -> Option<&ModelParams> {
        self.model_params.as_ref()?.get(model_id)
    }

    pub fn should_transform_developer_role(&self, model_id: &str) -> bool {
        self.model_compat
            .as_ref()
            .map(|c| c.should_transform_developer_role(model_id))
            .unwrap_or(true)
    }

    pub fn should_transform_tool_messages(&self, model_id: &str) -> bool {
        self.model_compat
            .as_ref()
            .map(|c| c.should_transform_tool_messages(model_id))
            .unwrap_or(true)
    }

    pub fn logging_enabled(&self) -> bool {
        self.logging
            .as_ref()
            .and_then(|l| l.enabled)
            .unwrap_or(false)
    }

    pub fn logging_path(&self) -> String {
        self.logging
            .as_ref()
            .and_then(|l| l.path.clone())
            .unwrap_or_else(|| "/var/log/nimaproxy/turns.jsonl".to_string())
    }
}

pub fn load(path: &str) -> Result<Config, String> {
    let raw =
        fs::read_to_string(path).map_err(|e| format!("cannot read config '{}': {}", path, e))?;
    toml::from_str(&raw).map_err(|e| format!("invalid config '{}': {}", path, e))
}

#[cfg(test)]
mod tests {
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
label = "doltares"

[[keys]]
key = "key2"
label = "ares"

[[keys]]
key = "key3"
"#,
        );

        let config = load(file.path().to_str().unwrap()).unwrap();
        assert_eq!(config.keys.len(), 3);
        assert_eq!(config.keys[0].label, Some("doltares".to_string()));
        assert_eq!(config.keys[1].label, Some("ares".to_string()));
        assert_eq!(config.keys[2].label, None);
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
        assert_eq!(config.racing_enabled(), true);
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
        assert_eq!(config.racing_enabled(), true);
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
        assert_eq!(config.routing_enabled(), true);
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
        assert_eq!(config.routing_enabled(), true);
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
}
