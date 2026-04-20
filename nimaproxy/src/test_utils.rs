//! Test utilities for nimaproxy testing infrastructure.
//!
//! This module provides helper functions and builders for creating test fixtures,
//! mock configurations, and common test scenarios.
//!
//! # Example
//!
//! ```no_run
//! use nimaproxy::test_utils::{MockAppStateBuilder, create_test_state};
//!
//! // Quick test state setup
//! let state = create_test_state();
//!
//! // Or use the builder for customization
//! let state = MockAppStateBuilder::new()
//!     .with_keys(10)
//!     .with_racing_models(5)
//!     .build();
//! ```

use crate::{AppState, KeyEntry, ModelParams, ModelStatsStore, config};
use std::collections::HashMap;
use std::sync::Arc;

/// Builder for creating mock AppState instances with configurable parameters.
///
/// Provides sensible defaults for all fields while allowing fine-grained control.
///
/// # Example
///
/// ```no_run
/// use nimaproxy::test_utils::MockAppStateBuilder;
///
/// let state = MockAppStateBuilder::new()
///     .with_keys(5)
///     .with_target("http://localhost:8080")
///     .with_racing_models(3)
///     .build();
/// ```
pub struct MockAppStateBuilder {
    keys: Vec<KeyEntry>,
    target: String,
    racing_models: Vec<String>,
    racing_max_parallel: usize,
    racing_timeout_ms: u64,
    racing_strategy: String,
    model_params: HashMap<String, ModelParams>,
    model_compat: config::ModelCompat,
}

impl MockAppStateBuilder {
    /// Creates a new builder with default values.
    ///
    /// Defaults:
    /// - 1 API key with key "test-key-1"
    /// - Target: "http://127.0.0.1:8080"
    /// - No racing models
    /// - Max parallel: 3
    /// - Timeout: 5000ms
    /// - Strategy: "round-robin"
    pub fn new() -> Self {
        MockAppStateBuilder {
            keys: vec![KeyEntry {
                key: "test-key-1".to_string(),
                label: Some("test-key".to_string()),
            }],
            target: "http://127.0.0.1:8080".to_string(),
            racing_models: Vec::new(),
            racing_max_parallel: 3,
            racing_timeout_ms: 5000,
            racing_strategy: "round-robin".to_string(),
            model_params: HashMap::new(),
            model_compat: config::ModelCompat::default(),
        }
    }

    /// Sets the number of API keys to generate.
    ///
    /// Keys are auto-generated as "test-key-{n}" mapped to "test-key-{n}" label.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use nimaproxy::test_utils::MockAppStateBuilder;
    ///
    /// let state = MockAppStateBuilder::new().with_keys(10).build();
    /// assert_eq!(state.pool.len(), 10);
    /// ```
    pub fn with_keys(mut self, count: usize) -> Self {
        self.keys = (1..=count)
            .map(|n| KeyEntry {
                key: format!("test-key-{}", n),
                label: Some(format!("test-key-{}", n)),
            })
            .collect();
        self
    }

    /// Sets custom API keys.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use nimaproxy::test_utils::MockAppStateBuilder;
    /// use nimaproxy::KeyEntry;
    ///
    /// let state = MockAppStateBuilder::new()
    ///     .with_custom_keys(vec![
    ///         KeyEntry { key: "api-key-1".into(), label: Some("key-1".into()) },
    ///         KeyEntry { key: "api-key-2".into(), label: Some("key-2".into()) },
    ///     ])
    ///     .build();
    /// ```
    pub fn with_custom_keys(mut self, keys: Vec<KeyEntry>) -> Self {
        self.keys = keys;
        self
    }

    /// Sets the target URL for the proxy.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use nimaproxy::test_utils::MockAppStateBuilder;
    ///
    /// let state = MockAppStateBuilder::new()
    ///     .with_target("http://localhost:3000")
    ///     .build();
    /// ```
    pub fn with_target(mut self, target: &str) -> Self {
        self.target = target.to_string();
        self
    }

    /// Sets the number of racing models to generate.
    ///
    /// Models are auto-generated as "nvidia/test-model-{n}".
    ///
    /// # Example
    ///
    /// ```no_run
    /// use nimaproxy::test_utils::MockAppStateBuilder;
    ///
    /// let state = MockAppStateBuilder::new()
    ///     .with_racing_models(5)
    ///     .build();
    /// assert_eq!(state.racing_models.len(), 5);
    /// ```
    pub fn with_racing_models(mut self, count: usize) -> Self {
        self.racing_models = (1..=count)
            .map(|n| format!("nvidia/test-model-{}", n))
            .collect();
        self
    }

    /// Sets custom racing models.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use nimaproxy::test_utils::MockAppStateBuilder;
    ///
    /// let state = MockAppStateBuilder::new()
    ///     .with_custom_racing_models(vec![
    ///         "meta/llama-3.1-405b-instruct".into(),
    ///         "nvidia/nemotron-4-340b-instruct".into(),
    ///     ])
    ///     .build();
    /// ```
    pub fn with_custom_racing_models(mut self, models: Vec<String>) -> Self {
        self.racing_models = models;
        self
    }

    /// Sets the maximum parallel racing requests.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use nimaproxy::test_utils::MockAppStateBuilder;
    ///
    /// let state = MockAppStateBuilder::new()
    ///     .with_racing_max_parallel(5)
    ///     .build();
    /// ```
    pub fn with_racing_max_parallel(mut self, max: usize) -> Self {
        self.racing_max_parallel = max;
        self
    }

    /// Sets the racing timeout in milliseconds.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use nimaproxy::test_utils::MockAppStateBuilder;
    ///
    /// let state = MockAppStateBuilder::new()
    ///     .with_racing_timeout_ms(10000)
    ///     .build();
    /// ```
    pub fn with_racing_timeout_ms(mut self, timeout: u64) -> Self {
        self.racing_timeout_ms = timeout;
        self
    }

    /// Sets the racing strategy.
    ///
    /// Valid strategies: "round-robin", "latency-based", "error-rate"
    ///
    /// # Example
    ///
    /// ```no_run
    /// use nimaproxy::test_utils::MockAppStateBuilder;
    ///
    /// let state = MockAppStateBuilder::new()
    ///     .with_racing_strategy("latency-based")
    ///     .build();
    /// ```
    pub fn with_racing_strategy(mut self, strategy: &str) -> Self {
        self.racing_strategy = strategy.to_string();
        self
    }

    /// Adds model parameters for a specific model.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use nimaproxy::test_utils::MockAppStateBuilder;
    ///
    /// let state = MockAppStateBuilder::new()
    ///     .with_model_param("nvidia/test", nimaproxy::ModelParams {
    ///         max_tokens: Some(4096),
    ///         ..Default::default()
    ///     })
    ///     .build();
    /// ```
    pub fn with_model_param(mut self, model: &str, params: ModelParams) -> Self {
        self.model_params.insert(model.to_string(), params);
        self
    }

    /// Sets model compatibility mode.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use nimaproxy::test_utils::MockAppStateBuilder;
    ///
    /// let state = MockAppStateBuilder::new()
    ///     .with_model_compat(nimaproxy::config::ModelCompat::default())
    ///     .build();
    /// ```
    pub fn with_model_compat(mut self, compat: config::ModelCompat) -> Self {
        self.model_compat = compat;
        self
    }

    /// Builds the AppState with configured parameters.
    ///
    /// Returns an Arc<AppState> ready for use in tests.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use nimaproxy::test_utils::MockAppStateBuilder;
    ///
    /// let state = MockAppStateBuilder::new().build();
    /// ```
    pub fn build(self) -> Arc<AppState> {
        AppState::new(
            self.keys,
            self.target,
            None, // router
            ModelStatsStore::new(5000.0),
            self.racing_models,
            self.racing_max_parallel,
            self.racing_timeout_ms,
            self.racing_strategy,
            self.model_params,
            self.model_compat,
        )
    }
}

impl Default for MockAppStateBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Creates a minimal test AppState with default configuration.
///
/// Quick helper for simple tests that don't need customization.
///
/// # Example
///
/// ```no_run
/// use nimaproxy::test_utils::create_test_state;
///
/// let state = create_test_state();
/// assert_eq!(state.pool.len(), 1);
/// ```
pub fn create_test_state() -> Arc<AppState> {
    MockAppStateBuilder::new().build()
}

/// Creates a test AppState with configurable key pool size.
///
/// # Arguments
///
/// * `key_count` - Number of API keys to generate
///
/// # Example
///
/// ```no_run
/// use nimaproxy::test_utils::create_test_key_pool;
///
/// let state = create_test_key_pool(10);
/// assert_eq!(state.pool.len(), 10);
/// ```
pub fn create_test_key_pool(key_count: usize) -> Arc<AppState> {
    MockAppStateBuilder::new().with_keys(key_count).build()
}

/// Creates a test AppState with pre-populated model statistics.
///
/// Generates stats for the configured racing models.
///
/// # Example
///
/// ```no_run
/// use nimaproxy::test_utils::create_test_model_stats;
///
/// let state = create_test_model_stats(5);
/// // Stats store is initialized and ready
/// ```
pub fn create_test_model_stats(model_count: usize) -> Arc<AppState> {
    MockAppStateBuilder::new()
        .with_racing_models(model_count)
        .build()
}

/// Creates a test AppState configured for racing scenarios.
///
/// Sets up multiple racing models with default timeout and parallel settings.
///
/// # Arguments
///
/// * `model_count` - Number of models to race
/// * `parallel` - Maximum parallel racing requests
///
/// # Example
///
/// ```no_run
/// use nimaproxy::test_utils::create_racing_scenario;
///
/// let state = create_racing_scenario(3, 2);
/// assert_eq!(state.racing_models.len(), 3);
/// assert_eq!(state.racing_max_parallel, 2);
/// ```
pub fn create_racing_scenario(model_count: usize, parallel: usize) -> Arc<AppState> {
    MockAppStateBuilder::new()
        .with_racing_models(model_count)
        .with_racing_max_parallel(parallel)
        .build()
}

/// Creates a test AppState with custom model parameters.
///
/// # Arguments
///
/// * `models` - List of model names
/// * `max_tokens` - Max tokens per model
///
/// # Example
///
/// ```no_run
/// use nimaproxy::test_utils::create_model_with_params;
///
/// let state = create_model_with_params(
///     vec!["nvidia/test"],
///     4096,
/// );
/// ```
pub fn create_model_with_params(models: Vec<&str>, max_tokens: i32) -> Arc<AppState> {
    let mut builder = MockAppStateBuilder::new().with_custom_racing_models(
        models.iter().map(|s| s.to_string()).collect(),
    );

    for model in models {
        builder = builder.with_model_param(
            model,
            ModelParams {
                max_tokens: Some(max_tokens),
                ..Default::default()
            },
        );
    }

    builder.build()
}

/// Creates a test AppState with a specific racing strategy.
///
/// # Arguments
///
/// * `strategy` - Racing strategy: "round-robin", "latency-based", or "error-rate"
///
/// # Example
///
/// ```no_run
/// use nimaproxy::test_utils::create_strategy_test;
///
/// let state = create_strategy_test("latency-based");
/// assert_eq!(state.racing_strategy, "latency-based");
/// ```
pub fn create_strategy_test(strategy: &str) -> Arc<AppState> {
    MockAppStateBuilder::new()
        .with_racing_strategy(strategy)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mock_app_state_builder_default() {
        let state = MockAppStateBuilder::new().build();
        assert_eq!(state.pool.len(), 1);
        assert_eq!(state.racing_max_parallel, 3);
        assert_eq!(state.racing_timeout_ms, 5000);
    }

    #[test]
    fn test_mock_app_state_builder_keys() {
        let state = MockAppStateBuilder::new().with_keys(10).build();
        assert_eq!(state.pool.len(), 10);
    }

    #[test]
    fn test_mock_app_state_builder_racing_models() {
        let state = MockAppStateBuilder::new()
            .with_racing_models(5)
            .build();
        assert_eq!(state.racing_models.len(), 5);
    }

    #[test]
    fn test_mock_app_state_builder_custom_target() {
        let state = MockAppStateBuilder::new()
            .with_target("http://custom:9000")
            .build();
        assert!(state.target.contains("custom"));
    }

    #[test]
    fn test_create_test_state() {
        let state = create_test_state();
        assert!(state.pool.len() >= 1);
    }

    #[test]
    fn test_create_test_key_pool() {
        let state = create_test_key_pool(20);
        assert_eq!(state.pool.len(), 20);
    }

    #[test]
    fn test_create_test_model_stats() {
        let state = create_test_model_stats(7);
        assert_eq!(state.racing_models.len(), 7);
    }

    #[test]
    fn test_create_racing_scenario() {
        let state = create_racing_scenario(4, 2);
        assert_eq!(state.racing_models.len(), 4);
        assert_eq!(state.racing_max_parallel, 2);
    }

    #[test]
    fn test_create_strategy_test() {
        let state = create_strategy_test("error-rate");
        assert_eq!(state.racing_strategy, "error-rate");
    }
}
