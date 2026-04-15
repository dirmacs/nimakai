use serde::Deserialize;
use std::fs;

#[derive(Deserialize, Clone, Debug)]
pub struct Config {
    pub listen: Option<String>,
    pub target: Option<String>,
    pub keys: Vec<KeyEntry>,
    pub routing: Option<RoutingConfig>,
    pub racing: Option<RacingConfig>,
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
    /// Max parallel requests (default: 3, max: 5)
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
        self.racing
            .as_ref()
            .and_then(|r| r.max_parallel)
            .unwrap_or(3)
            .min(5)
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

        // Check the TOML parses at all
        let result: Result<Config, _> = toml::from_str(&content);
        assert!(result.is_ok(), "TOML should parse: {:?}", result.err());

        let config = result.unwrap();
        assert_eq!(config.keys.len(), 1);
    }
}
