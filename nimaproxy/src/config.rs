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
        // If list has entries, only transform models NOT in the list
        if let Some(models) = &self.supports_developer_role {
            // List exists: transform if model is NOT in the list
            return !models.iter().any(|m| m == model_id);
        }
        // No config: transform all models (default behavior)
        true
    }

    pub fn should_transform_tool_messages(&self, model_id: &str) -> bool {
        // If model is in supports_tool_messages list, it supports tool messages
        // and should NOT be transformed. Return false for these models.
        if let Some(models) = &self.supports_tool_messages {
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
    /// Penalty for frequency of repeated tokens (reduces repetition)
    pub frequency_penalty: Option<f64>,
    pub presence_penalty: Option<f64>,
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
    /// Strategy: "first_token" (return on first SSE token) or "complete" (default)
    pub strategy: Option<String>,
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
        self.racing
            .as_ref()
            .and_then(|r| r.enabled)
            .unwrap_or(false)
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

    pub fn racing_strategy(&self) -> String {
        self.racing
            .as_ref()
            .and_then(|r| r.strategy.clone())
            .unwrap_or_else(|| "complete".to_string())
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
            .unwrap_or(false)
    }

    pub fn get_model_params(&self, model_id: &str) -> Option<&ModelParams> {
        self.model_params.as_ref()?.get(model_id)
    }

    pub fn should_transform_developer_role(&self, model_id: &str) -> bool {
        self.model_compat
            .as_ref()
            .map(|c| c.should_transform_developer_role(model_id))
            .unwrap_or(false)
    }

    pub fn should_transform_tool_messages(&self, model_id: &str) -> bool {
        self.model_compat
            .as_ref()
            .map(|c| c.should_transform_tool_messages(model_id))
            .unwrap_or(false)
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

[Routing]
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

[Routing]
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

        assert!(compat.should_transform_developer_role("mistralai/model1"));
        assert!(compat.should_transform_developer_role("mistralai/model2"));
        assert!(!compat.should_transform_developer_role("unknown-model"));
        assert!(!compat.should_transform_developer_role("qwen/qwen3.5-122b-a10b"));

        assert!(compat.should_transform_tool_messages("mistralai/model1"));
        assert!(!compat.should_transform_tool_messages("mistralai/model2"));
        assert!(!compat.should_transform_tool_messages("unknown-model"));
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

        assert!(!config.should_transform_developer_role("any-model"));
        assert!(!config.should_transform_tool_messages("any-model"));
    }

    #[test]
    fn test_transform_role_helper_logic() {
        let compat = ModelCompat {
            supports_developer_role: Some(vec!["allowed-model".to_string()]),
            supports_tool_messages: Some(vec!["allowed-model".to_string()]),
        };

        assert!(compat.should_transform_developer_role("allowed-model"));
        assert!(compat.should_transform_tool_messages("allowed-model"));
        assert!(!compat.should_transform_developer_role("blocked-model"));
        assert!(!compat.should_transform_tool_messages("blocked-model"));
    }
}
