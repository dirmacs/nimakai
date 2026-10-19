//! Configuration loading and accessors.

use serde::Deserialize;
use std::fs;
use std::time::Duration;

mod keys;
mod racing;
#[cfg(test)]
mod tests;

pub use keys::KeyEntry;
pub use racing::RacingConfig;

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
    /// Agent-loop idle timeout in seconds (default 300). See
    /// [`Config::loop_idle_timeout_secs`] for the tuning tradeoff.
    #[serde(default)]
    pub loop_idle_timeout_secs: Option<u64>,
}

impl Config {
    /// Split configured keys into (leasable, parked).
    ///
    /// A parked key (`disabled = true`) stays in the config for reference but
    /// must never be leased. Serde silently ignored the unknown `disabled`
    /// field before this existed, so parked keys entered the rotation and were
    /// only discovered broken per-request (403, quarantined after the fact).
    pub fn partition_keys(&self) -> (Vec<&KeyEntry>, Vec<&KeyEntry>) {
        self.keys.iter().partition(|k| !k.disabled)
    }

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
pub struct RoutingConfig {
    /// "round_robin" (default) or "latency_aware"
    pub strategy: Option<String>,
    /// Model list for auto-routing. When the request contains `"model": "auto"`,
    /// the router picks from this list.
    pub models: Option<Vec<String>>,
    /// Avg TTFC above this value marks a model as degraded (default 3000ms).
    pub spike_threshold_ms: Option<f64>,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct LimitsConfig {
    pub max_upstream_in_flight: Option<usize>,
    pub max_in_flight_per_key: Option<usize>,
    pub admission_wait_ms: Option<u64>,
    /// Cooldown (seconds) applied to a key after an upstream 401/403 (auth failure).
    /// Default 900 (15 min). `0` disables auth-failure quarantine.
    pub auth_failure_cooldown_secs: Option<u64>,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct TimeoutsConfig {
    pub min_dynamic_timeout_ms: Option<u64>,
    pub dynamic_sample_floor: Option<usize>,
    /// Maximum tolerated gap between two chunks of one response, in
    /// milliseconds. Absent (or 0) falls back to the 60 s default. See
    /// [`Config::mid_stream_idle_timeout_ms`] for why the default is wider
    /// than the first-byte rail.
    pub mid_stream_idle_timeout_ms: Option<u64>,
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

    /// Cooldown (seconds) applied to a key after an upstream 401/403 (auth failure) on any
    /// inference path. Default 900 (15 min). `0` disables auth-failure quarantine — the
    /// failure is still counted, but the key is not cooled down.
    pub fn auth_failure_cooldown_secs(&self) -> u64 {
        self.limits
            .as_ref()
            .and_then(|l| l.auth_failure_cooldown_secs)
            .unwrap_or(900)
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

    /// Maximum tolerated gap between two chunks of one response, in
    /// milliseconds.
    ///
    /// This is the mid-stream idle rail, and it is deliberately wider than
    /// the first-byte rail (`timeout_ms` / `min_dynamic_timeout_ms`, 25 s in
    /// production). The two bounds guard different events: the first byte
    /// answers "is the upstream alive", while a mid-stream gap answers "did
    /// a live generation stall". Production turn-log analysis puts healthy
    /// inter-chunk gaps in the sub-second range, so a 60 s window still
    /// catches every pathological stall — the guard fires on single gaps
    /// two orders of magnitude above the healthy mean — while no longer
    /// killing a generation that pauses to think between tokens.
    ///
    /// A configured `0` is treated as absent: an idle window of zero would
    /// abort every stream on its second chunk, so `0` can only be a mistake,
    /// never a meaningful "tolerate no gap" setting.
    pub fn mid_stream_idle_timeout_ms(&self) -> u64 {
        self.timeouts
            .as_ref()
            .and_then(|t| t.mid_stream_idle_timeout_ms)
            .filter(|&ms| ms != 0)
            .unwrap_or(60_000)
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

    /// How long a loop may sit silent before the sweeper closes it.
    ///
    /// A loop is normally closed by the client starting its next turn, but the
    /// LAST turn of a session has no successor, so without a sweep it would
    /// never be summarised at all. The value is a compromise in both
    /// directions: too short and a slow tool call between two rounds of the
    /// SAME turn is mistaken for the end of that turn, which splits one loop
    /// into two and halves the measured depth — the very number this exists to
    /// report; too long and each session's final loop stays invisible for that
    /// long.
    ///
    /// A configured `0` is treated as absent: an immediate sweep would close a
    /// loop the moment it opened, so `0` can only be a mistake, never a
    /// meaningful "close instantly" setting.
    pub fn loop_idle_timeout_secs(&self) -> Duration {
        Duration::from_secs(
            self.loop_idle_timeout_secs
                .filter(|&s| s != 0)
                .unwrap_or(300),
        )
    }
}

pub fn load(path: &str) -> Result<Config, String> {
    let raw =
        fs::read_to_string(path).map_err(|e| format!("cannot read config '{}': {}", path, e))?;
    let mut config: Config =
        toml::from_str(&raw).map_err(|e| format!("invalid config '{}': {}", path, e))?;
    keys::resolve_key_env(&mut config.keys)?;
    Ok(config)
}

#[cfg(test)]
mod loop_idle_timeout_tests {
    use super::*;

    fn config_from_toml(body: &str) -> Config {
        toml::from_str(body).expect("test config must parse")
    }

    /// Absent key resolves to the 300s default, so old configs keep working unchanged.
    #[test]
    fn loop_idle_timeout_absent_means_default() {
        let cfg = config_from_toml(
            r#"
[[keys]]
key = "test"
"#,
        );
        assert_eq!(cfg.loop_idle_timeout_secs(), Duration::from_secs(300));
    }

    /// A configured value is honoured verbatim.
    #[test]
    fn loop_idle_timeout_configured_value_used() {
        let cfg = config_from_toml(
            r#"
loop_idle_timeout_secs = 60
[[keys]]
key = "test"
"#,
        );
        assert_eq!(cfg.loop_idle_timeout_secs(), Duration::from_secs(60));
    }

    /// Zero falls back to the default: an immediate sweep would close a loop the
    /// moment it opened, so 0 cannot mean "close immediately".
    #[test]
    fn loop_idle_timeout_zero_means_default() {
        let cfg = config_from_toml(
            r#"
loop_idle_timeout_secs = 0
[[keys]]
key = "test"
"#,
        );
        assert_eq!(cfg.loop_idle_timeout_secs(), Duration::from_secs(300));
    }
}

#[cfg(test)]
mod mid_stream_idle_timeout_tests {
    use super::*;

    fn config_from_toml(body: &str) -> Config {
        toml::from_str(body).expect("test config must parse")
    }

    /// Absent key resolves to the 60 s default, which is deliberately wider
    /// than the 25 s first-byte rail: healthy inter-chunk gaps are
    /// sub-second, so the wider rail still catches pathological stalls while
    /// tolerating a generation that pauses to think between tokens.
    #[test]
    fn mid_stream_idle_absent_means_sixty_seconds() {
        let cfg = config_from_toml(
            r#"
[[keys]]
key = "test"
"#,
        );
        assert_eq!(cfg.mid_stream_idle_timeout_ms(), 60_000);
    }

    /// A configured value is honoured verbatim, so the rail is tunable
    /// without touching the first-byte window.
    #[test]
    fn mid_stream_idle_configured_value_used() {
        let cfg = config_from_toml(
            r#"
[timeouts]
mid_stream_idle_timeout_ms = 45000
[[keys]]
key = "test"
"#,
        );
        assert_eq!(cfg.mid_stream_idle_timeout_ms(), 45_000);
    }

    /// Zero falls back to the default: a zero idle window would abort every
    /// stream on its second chunk, so 0 cannot mean "tolerate no gap".
    #[test]
    fn mid_stream_idle_zero_means_default() {
        let cfg = config_from_toml(
            r#"
[timeouts]
mid_stream_idle_timeout_ms = 0
[[keys]]
key = "test"
"#,
        );
        assert_eq!(cfg.mid_stream_idle_timeout_ms(), 60_000);
    }
}
