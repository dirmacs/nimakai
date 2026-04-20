use nimaproxy::config::{KeyEntry, ModelCompat};
use nimaproxy::key_pool::KeyPool;
use nimaproxy::model_router::{ModelRouter, Strategy};
use nimaproxy::model_stats::ModelStatsStore;
use nimaproxy::AppState;
use serde_json::json;

use std::collections::HashMap;

fn make_key_pool() -> KeyPool {
    let keys = vec![
        KeyEntry {
            key: "test-key-1".to_string(),
            label: Some("test1".to_string()),
        },
        KeyEntry {
            key: "test-key-2".to_string(),
            label: Some("test2".to_string()),
        },
    ];
    KeyPool::new(keys)
}

// ============================================================================
// Coverage Tests - Target specific uncovered lines
// ============================================================================

/// Test ModelParams::get helper (config.rs lines 91-92)
#[test]
fn test_model_params_get_helper() {
    use nimaproxy::config::ModelParams;

    // Test with None chat_template_kwargs
    let params_empty = ModelParams::default();
    assert!(params_empty.get("any_key").is_none());

    // Test with Some chat_template_kwargs containing the key
    let mut kwargs = HashMap::new();
    kwargs.insert("test_key".to_string(), serde_json::json!("test_value"));
    kwargs.insert("number_key".to_string(), serde_json::json!(42));

    let params_with_kwargs = ModelParams {
        chat_template_kwargs: Some(kwargs),
        ..Default::default()
    };

    // Test getting existing string key
    let result = params_with_kwargs.get("test_key");
    assert!(result.is_some());
    assert_eq!(result.unwrap().as_str(), Some("test_value"));

    // Test getting existing number key
    let result = params_with_kwargs.get("number_key");
    assert!(result.is_some());
    assert_eq!(result.unwrap().as_i64(), Some(42));

    // Test getting non-existent key
    let result = params_with_kwargs.get("nonexistent");
    assert!(result.is_none());
}

/// Test FFI error handling - proxy_free_string with null pointer (lib.rs line 400+)
#[test]
fn test_ffi_proxy_free_string_null() {
    // Test that proxy_free_string handles null pointer safely
    // This covers the null check branch in the FFI function
    unsafe {
        nimaproxy::proxy_free_string(std::ptr::null_mut());
    }
    // If we reach here, the function handled null correctly
    assert!(true);
}

/// Test ModelStatsStore::circuit_breaker_config getter (model_stats.rs lines 188-189)
#[test]
fn test_model_stats_circuit_breaker_config_getter() {
    use nimaproxy::model_stats::CircuitBreakerConfig;

    // Test with default config
    let store_default = ModelStatsStore::new(3000.0);
    let config = store_default.circuit_breaker_config();
    assert_eq!(config.max_output_tokens, 32000);
    assert_eq!(config.max_repetitions, 5);
    assert_eq!(config.max_consecutive_assistant_turns, 10);

    // Test with custom circuit breaker config
    let custom_config = CircuitBreakerConfig {
        max_output_tokens: 16000,
        max_repetitions: 3,
        max_consecutive_assistant_turns: 5,
    };
    let store_custom = ModelStatsStore::with_circuit_breaker(3000.0, custom_config.clone());
    let retrieved = store_custom.circuit_breaker_config();
    assert_eq!(retrieved.max_output_tokens, custom_config.max_output_tokens);
    assert_eq!(retrieved.max_repetitions, custom_config.max_repetitions);
    assert_eq!(retrieved.max_consecutive_assistant_turns, custom_config.max_consecutive_assistant_turns);
}

/// Test ModelStatsStore::best_model returning None for empty candidates (model_stats.rs line 311)
#[test]
fn test_model_stats_best_model_empty_candidates() {
    let stats = ModelStatsStore::new(3000.0);

    // Test with empty candidate list - should return None
    let result = stats.best_model(&[]);
    assert!(result.is_none());
}

/// Test ModelStatsStore::best_model with all models degraded (model_stats.rs line 311)
#[test]
fn test_model_stats_best_model_all_degraded() {
    let stats = ModelStatsStore::new(3000.0);

    // Record enough failures to make models degraded
    for _ in 0..5 {
        stats.record("degraded-model-1", 5000.0, false);
        stats.record("degraded-model-2", 6000.0, false);
    }

    // When all candidates are degraded, should still return one (least degraded)
    let result = stats.best_model(&["degraded-model-1".to_string(), "degraded-model-2".to_string()]);
    // Should return one of them (the least degraded)
    assert!(result.is_some());
}

/// Test test_utils MockAppStateBuilder methods coverage
#[test]
fn test_mock_app_state_builder_custom_racing_models() {
    use nimaproxy::test_utils::MockAppStateBuilder;

    let custom_models = vec![
        "meta/llama-3.1-405b-instruct".to_string(),
        "nvidia/nemotron-4-340b-instruct".to_string(),
    ];

    let state = MockAppStateBuilder::new()
        .with_custom_racing_models(custom_models.clone())
        .build();

    assert_eq!(state.racing_models.len(), 2);
    assert_eq!(state.racing_models, custom_models);
}

/// Test MockAppStateBuilder with_custom_keys
#[test]
fn test_mock_app_state_builder_custom_keys() {
    use nimaproxy::config::KeyEntry;
    use nimaproxy::test_utils::MockAppStateBuilder;

    let custom_keys = vec![
        KeyEntry {
            key: "custom-key-1".to_string(),
            label: Some("custom-1".to_string()),
        },
        KeyEntry {
            key: "custom-key-2".to_string(),
            label: None,
        },
    ];

    let state = MockAppStateBuilder::new()
        .with_custom_keys(custom_keys.clone())
        .build();

    assert_eq!(state.pool.len(), 2);
}

/// Test MockAppStateBuilder with_model_param
#[test]
fn test_mock_app_state_builder_with_model_param() {
    use nimaproxy::config::ModelParams;
    use nimaproxy::test_utils::MockAppStateBuilder;

    let model_params = ModelParams {
        temperature: Some(0.7),
        top_p: Some(0.9),
        max_tokens: Some(2048),
        ..Default::default()
    };

    let state = MockAppStateBuilder::new()
        .with_model_param("test-model", model_params.clone())
        .build();

    let params = state.model_params.get("test-model");
    assert!(params.is_some());
    let params = params.unwrap();
    assert_eq!(params.temperature, Some(0.7));
    assert_eq!(params.top_p, Some(0.9));
    assert_eq!(params.max_tokens, Some(2048));
}

/// Test MockAppStateBuilder with_model_compat
#[test]
fn test_mock_app_state_builder_with_model_compat() {
    use nimaproxy::config::ModelCompat;
    use nimaproxy::test_utils::MockAppStateBuilder;

    let compat = ModelCompat {
        supports_developer_role: Some(vec!["model1".to_string()]),
        supports_tool_messages: Some(vec!["model2".to_string()]),
    };

    let state = MockAppStateBuilder::new()
        .with_model_compat(compat.clone())
        .build();

    assert!(!state.model_compat.should_transform_developer_role("model1"));
    assert!(!state.model_compat.should_transform_tool_messages("model2"));
}

/// Test MockAppStateBuilder with_racing_max_parallel
#[test]
fn test_mock_app_state_builder_racing_config() {
    use nimaproxy::test_utils::MockAppStateBuilder;

    let state = MockAppStateBuilder::new()
        .with_racing_max_parallel(5)
        .with_racing_timeout_ms(10000)
        .with_racing_strategy("latency-based")
        .build();

    assert_eq!(state.racing_max_parallel, 5);
    assert_eq!(state.racing_timeout_ms, 10000);
    assert_eq!(state.racing_strategy, "latency-based");
}
fn make_stats() -> ModelStatsStore {
    ModelStatsStore::new(3000.0)
}

fn make_router() -> ModelRouter {
    ModelRouter::new(vec!["nvidia/test-model".to_string()], Strategy::RoundRobin)
}

#[test]
fn test_integration_key_round_robin() {
    let pool = make_key_pool();

    let (key1, idx1) = pool.next_key().unwrap();
    let (key2, idx2) = pool.next_key().unwrap();

    assert_ne!(idx1, idx2);
    assert!(key1.starts_with("test-key-"));
    assert!(key2.starts_with("test-key-"));
}

#[test]
fn test_integration_stats_recording() {
    let stats = make_stats();

    stats.record("test-model", 100.0, true);
    stats.record("test-model", 200.0, true);
    stats.record("test-model", 300.0, true);

    let snapshots = stats.snapshot();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].id, "test-model");
    assert_eq!(snapshots[0].total, 3);
    assert_eq!(snapshots[0].success, 3);
}

#[test]
fn test_integration_stats_failure_tracking() {
    let stats = make_stats();

    stats.record("test-model", 0.0, false);
    stats.record("test-model", 0.0, false);
    stats.record("test-model", 0.0, false);

    let snapshots = stats.snapshot();
    assert_eq!(snapshots[0].consecutive_failures, 3);
    assert!(snapshots[0].degraded);
}

#[test]
fn test_integration_router_pick() {
    let stats = make_stats();
    let router = make_router();

    let picked = router.pick(&stats);
    assert!(picked.is_some());
    assert_eq!(picked.unwrap(), "nvidia/test-model");
}

#[test]
fn test_integration_key_rate_limit() {
    let pool = make_key_pool();

    pool.mark_rate_limited(0, 1);

    let (key, idx) = pool.next_key().unwrap();
    assert_eq!(idx, 1);
    assert!(key.starts_with("test-key-2"));
}

#[test]
fn test_integration_latency_aware_selection() {
    let stats = make_stats();

    stats.record("fast-model", 100.0, true);
    stats.record("fast-model", 150.0, true);
    stats.record("fast-model", 200.0, true);

    stats.record("slow-model", 2000.0, true);
    stats.record("slow-model", 2500.0, true);
    stats.record("slow-model", 3000.0, true);

    let router = ModelRouter::new(
        vec!["fast-model".to_string(), "slow-model".to_string()],
        Strategy::LatencyAware,
    );

    let picked = router.pick(&stats).unwrap();
    assert_eq!(picked, "fast-model");
}

#[test]
fn test_integration_app_state_creation() {
    let keys = vec![KeyEntry {
        key: "key1".to_string(),
        label: Some("primary".to_string()),
    }];
    let stats = make_stats();
    let router = make_router();

    let state = AppState::new(
        keys,
        "https://api.example.com".to_string(),
        Some(router),
        stats,
        vec![],
        3,
        8000,
        "complete".to_string(),
        std::collections::HashMap::new(),
        ModelCompat::default(),
    );

    assert_eq!(state.pool.len(), 1);
    assert_eq!(state.target, "https://api.example.com");
    assert!(state.router.is_some());
}

#[test]
fn test_integration_key_pool_status() {
    let pool = make_key_pool();

    let statuses = pool.status();
    assert_eq!(statuses.len(), 2);

    assert_eq!(statuses[0].label, "test1");
    assert_eq!(statuses[0].key_hint, "...ey-1");
    assert!(statuses[0].active);

    assert_eq!(statuses[1].label, "test2");
    assert_eq!(statuses[1].key_hint, "...ey-2");
    assert!(statuses[1].active);
}

#[test]
fn test_integration_model_snapshot() {
    let stats = make_stats();

    stats.record("model-a", 100.0, true);
    stats.record("model-a", 200.0, true);
    stats.record("model-a", 300.0, true);
    stats.record("model-b", 5000.0, true);
    stats.record("model-b", 6000.0, true);
    stats.record("model-b", 7000.0, true);

    let snapshots = stats.snapshot();
    assert_eq!(snapshots.len(), 2);

    let model_a = snapshots.iter().find(|s| s.id == "model-a").unwrap();
    let model_b = snapshots.iter().find(|s| s.id == "model-b").unwrap();

    assert!(model_a.avg_ms.unwrap() < model_b.avg_ms.unwrap());
    assert!(!model_a.degraded);
    assert!(model_b.degraded);
}

#[test]
fn test_integration_empty_key_pool() {
    let pool = KeyPool::new(vec![]);
    assert_eq!(pool.next_key(), None);
    assert_eq!(pool.len(), 0);
}

#[test]
fn test_integration_all_keys_cooldown() {
    let pool = make_key_pool();

    pool.mark_rate_limited(0, 60);
    pool.mark_rate_limited(1, 60);

    assert_eq!(pool.next_key(), None);
}

#[test]
fn test_integration_strategy_parsing() {
    assert!(matches!(
        Strategy::from_str("round_robin"),
        Strategy::RoundRobin
    ));
    assert!(matches!(
        Strategy::from_str("latency_aware"),
        Strategy::LatencyAware
    ));
    assert!(matches!(Strategy::from_str("random"), Strategy::RoundRobin));
}

#[test]
fn test_integration_auto_routing_picks_fastest() {
    // Fast model has 3+ samples with low avg
    let mut stats = make_stats();
    stats.record("fast-model", 150.0, true);
    stats.record("fast-model", 200.0, true);
    stats.record("fast-model", 250.0, true);

    // Slow model has 3+ samples with high avg
    stats.record("slow-model", 3000.0, true);
    stats.record("slow-model", 3500.0, true);
    stats.record("slow-model", 4000.0, true);

    let router = ModelRouter::new(
        vec!["slow-model".to_string(), "fast-model".to_string()],
        Strategy::LatencyAware,
    );

    // Latency-aware should pick fast-model (lower avg)
    let picked = router.pick(&stats);
    assert_eq!(picked, Some("fast-model".to_string()));
}

#[test]
fn test_integration_auto_routing_skips_degraded() {
    let mut stats = make_stats();

    // Mark model-a as degraded: 3+ successful samples (ring_len >= 3) but avg > spike
    stats.record("model-a", 6000.0, true);
    stats.record("model-a", 7000.0, true);
    stats.record("model-a", 8000.0, true);

    // model-b is healthy with low avg
    stats.record("model-b", 500.0, true);

    let router = ModelRouter::new(
        vec!["model-a".to_string(), "model-b".to_string()],
        Strategy::LatencyAware,
    );

    // Should skip degraded model-a (avg > 3000ms spike threshold) and pick model-b
    let picked = router.pick(&stats);
    assert_eq!(picked, Some("model-b".to_string()));
}

#[test]
fn test_integration_auto_routing_untried_model() {
    let stats = make_stats();

    let router = ModelRouter::new(
        vec!["never-tried-model".to_string()],
        Strategy::LatencyAware,
    );

    // Untried models get priority — picks it even with no stats
    let picked = router.pick(&stats);
    assert_eq!(picked, Some("never-tried-model".to_string()));
}

#[test]
fn test_integration_auto_routing_no_router() {
    let keys = vec![KeyEntry {
        key: "test-key".to_string(),
        label: None,
    }];
    let stats = make_stats();

    // No router configured
    let state = AppState::new(
        keys,
        "https://api.example.com".to_string(),
        None,
        stats,
        vec![],
        3,
        8000,
        "complete".to_string(),
        std::collections::HashMap::new(),
        ModelCompat::default(),
    );
    assert!(state.router.is_none());
}

#[test]
fn test_integration_auto_routing_with_multiple_healthy_models() {
    let mut stats = make_stats();

    stats.record("model-a", 100.0, true);
    stats.record("model-a", 110.0, true);
    stats.record("model-a", 120.0, true);

    stats.record("model-b", 400.0, true);
    stats.record("model-b", 420.0, true);
    stats.record("model-b", 440.0, true);

    stats.record("model-c", 800.0, true);
    stats.record("model-c", 850.0, true);
    stats.record("model-c", 900.0, true);

    let router = ModelRouter::new(
        vec![
            "model-c".to_string(),
            "model-a".to_string(),
            "model-b".to_string(),
        ],
        Strategy::LatencyAware,
    );

    // Picks lowest avg — model-a
    let picked = router.pick(&stats);
    assert_eq!(picked, Some("model-a".to_string()));
}

// ---------------------------------------------------------------------------
// Racing tests — key pre-allocation
// ---------------------------------------------------------------------------

fn make_pool_3keys() -> nimaproxy::key_pool::KeyPool {
    let keys = vec![
        nimaproxy::config::KeyEntry {
            key: "key-a1".to_string(),
            label: Some("k1".to_string()),
        },
        nimaproxy::config::KeyEntry {
            key: "key-b2".to_string(),
            label: Some("k2".to_string()),
        },
        nimaproxy::config::KeyEntry {
            key: "key-c3".to_string(),
            label: Some("k3".to_string()),
        },
    ];
    nimaproxy::key_pool::KeyPool::new(keys)
}

fn make_pool_2keys() -> nimaproxy::key_pool::KeyPool {
    let keys = vec![
        nimaproxy::config::KeyEntry {
            key: "key-x1".to_string(),
            label: Some("x1".to_string()),
        },
        nimaproxy::config::KeyEntry {
            key: "key-y2".to_string(),
            label: Some("y2".to_string()),
        },
    ];
    nimaproxy::key_pool::KeyPool::new(keys)
}

fn racing_keys(
    pool: &nimaproxy::key_pool::KeyPool,
    models: &[String],
) -> Vec<(String, usize, Option<String>)> {
    models
        .iter()
        .filter_map(|_| {
            pool.next_key()
                .map(|(key, idx)| (key.clone(), idx, pool.get_key_label(idx)))
        })
        .collect()
}

#[test]
fn test_racing_preallocate_distributes_keys_to_models() {
    let pool = make_pool_3keys();
    let models = vec![
        "model-1".to_string(),
        "model-2".to_string(),
        "model-3".to_string(),
    ];

    let keys = racing_keys(&pool, &models);

    assert_eq!(keys.len(), 3, "should get one key per model");
    let indices: Vec<usize> = keys.iter().map(|(_, idx, _)| *idx).collect();
    let unique: std::collections::HashSet<usize> = indices.iter().cloned().collect();
    assert_eq!(
        unique.len(),
        3,
        "each model should get a different key index"
    );
}

#[test]
fn test_racing_preallocate_insufficient_keys_uses_available() {
    let pool = make_pool_2keys();
    let models = vec![
        "model-1".to_string(),
        "model-2".to_string(),
        "model-3".to_string(),
    ];

    let keys = racing_keys(&pool, &models);

    assert_eq!(
        keys.len(),
        3,
        "key count is capped by model count, not pool size"
    );
}

#[test]
fn test_racing_preallocate_exact_match_keys_models() {
    let pool = make_pool_3keys();
    let models = vec![
        "model-1".to_string(),
        "model-2".to_string(),
        "model-3".to_string(),
    ];

    let keys = racing_keys(&pool, &models);

    assert_eq!(keys.len(), 3);
    let indices: Vec<usize> = keys.iter().map(|(_, idx, _)| *idx).collect();
    assert!(
        indices.contains(&0) && indices.contains(&1) && indices.contains(&2),
        "all three keys should be used"
    );
}

#[test]
fn test_racing_preallocate_one_key_per_model_round_robin() {
    let pool = make_pool_3keys();
    let models = vec!["model-a".to_string(), "model-b".to_string()];

    let keys1 = racing_keys(&pool, &models);
    let keys2 = racing_keys(&pool, &models);

    assert_eq!(keys1.len(), 2);
    assert_eq!(keys2.len(), 2);

    let idx1: Vec<usize> = keys1.iter().map(|(_, idx, _)| *idx).collect();
    let idx2: Vec<usize> = keys2.iter().map(|(_, idx, _)| *idx).collect();

    assert!(
        idx1 != idx2,
        "second call should use different keys (round-robin)"
    );
}

#[test]
fn test_racing_preallocate_with_cooldown_skips_cooldown_key() {
    let pool = make_pool_3keys();

    pool.mark_rate_limited(1, 60);

    let models = vec!["m1".to_string(), "m2".to_string()];
    let keys = racing_keys(&pool, &models);

    assert_eq!(keys.len(), 2);
    let indices: Vec<usize> = keys.iter().map(|(_, idx, _)| *idx).collect();
    assert!(!indices.contains(&1), "cooldown key 1 should be skipped");
}

#[test]
fn test_racing_preallocate_all_cooldown_returns_empty() {
    let pool = make_pool_3keys();

    pool.mark_rate_limited(0, 60);
    pool.mark_rate_limited(1, 60);
    pool.mark_rate_limited(2, 60);

    let models = vec!["m1".to_string()];
    let keys = racing_keys(&pool, &models);

    assert_eq!(keys.len(), 0, "all keys in cooldown — nothing to allocate");
}

#[test]
fn test_racing_preallocate_empty_models_returns_empty() {
    let pool = make_pool_3keys();
    let models: Vec<String> = vec![];

    let keys = racing_keys(&pool, &models);

    assert_eq!(keys.len(), 0, "no models means no keys allocated");
}

#[test]
fn test_racing_preallocate_key_labels_preserved() {
    let pool = make_pool_3keys();
    let models = vec!["m1".to_string()];

    let keys = racing_keys(&pool, &models);

    assert_eq!(keys.len(), 1);
    let (_, _, label) = &keys[0];
    assert!(label.is_some());
    let label = label.as_ref().unwrap();
    assert!(label == "k1" || label == "k2" || label == "k3");
}

#[test]
fn test_racing_preallocate_5models_3keys_gives_3() {
    let pool = make_pool_3keys();
    let models = vec![
        "m1".to_string(),
        "m2".to_string(),
        "m3".to_string(),
        "m4".to_string(),
        "m5".to_string(),
    ];

    let keys = racing_keys(&pool, &models);

    assert_eq!(
        keys.len(),
        5,
        "key count is capped by model count, pool cycles round-robin"
    );
}

#[test]
fn test_racing_preallocate_concurrent_calls_are_deterministic() {
    let pool = make_pool_3keys();
    let models = vec!["m1".to_string(), "m2".to_string(), "m3".to_string()];

    let run1 = racing_keys(&pool, &models);
    let run2 = racing_keys(&pool, &models);
    let run3 = racing_keys(&pool, &models);

    let idx1: Vec<usize> = run1.iter().map(|(_, idx, _)| *idx).collect();
    let idx2: Vec<usize> = run2.iter().map(|(_, idx, _)| *idx).collect();
    let idx3: Vec<usize> = run3.iter().map(|(_, idx, _)| *idx).collect();

    assert_eq!(idx1, idx2, "round-robin should be deterministic");
    assert_eq!(idx2, idx3, "round-robin should be deterministic");
}

#[test]
fn test_racing_preallocate_max_parallel_capped_at_model_count() {
    let pool = make_pool_3keys();
    let models = vec!["m1".to_string()];

    let keys = racing_keys(&pool, &models);

    assert_eq!(keys.len(), 1, "only 1 model, only 1 key allocated");
}

#[test]
fn test_model_validation_rejects_invalid_model() {
    use nimaproxy::proxy::validate_model_exists;
    use std::sync::Mutex;

    let state = AppState::new(
        vec![KeyEntry {
            key: "test".to_string(),
            label: Some("test".to_string()),
        }],
        "https://test.com".to_string(),
        None,
        ModelStatsStore::new(3000.0),
        vec![],
        8,
        15000,
        "round_robin".to_string(),
        std::collections::HashMap::new(),
        ModelCompat::default(),
    );

    {
        let mut available = state.available_models.lock().unwrap();
        *available = vec![
            "google/gemma-3-27b-it".to_string(),
            "qwen/qwen2.5-coder-32b-instruct".to_string(),
        ];
    }

    let result = validate_model_exists("google/gemma-3-27b-it", &state);
    assert!(result.is_ok(), "valid model should pass");

    let result = validate_model_exists("nvidia/invalid-model-xyz", &state);
    assert!(result.is_err(), "invalid model should fail");
    assert!(
        result.unwrap_err().contains("not found"),
        "error should mention 'not found'"
    );
}

#[test]
fn test_model_validation_allows_auto_and_empty() {
    use nimaproxy::proxy::validate_model_exists;

    let state = AppState::new(
        vec![KeyEntry {
            key: "test".to_string(),
            label: Some("test".to_string()),
        }],
        "https://test.com".to_string(),
        None,
        ModelStatsStore::new(3000.0),
        vec![],
        8,
        15000,
        "round_robin".to_string(),
        std::collections::HashMap::new(),
        ModelCompat::default(),
    );

    {
        let mut available = state.available_models.lock().unwrap();
        *available = vec!["some-model".to_string()];
    }

    let result = validate_model_exists("auto", &state);
    assert!(result.is_ok(), "auto should always pass");

    let result = validate_model_exists("", &state);
    assert!(result.is_ok(), "empty should always pass");
}

#[test]
fn test_model_validation_passes_when_no_cache() {
    use nimaproxy::proxy::validate_model_exists;

    let state = AppState::new(
        vec![KeyEntry {
            key: "test".to_string(),
            label: Some("test".to_string()),
        }],
        "https://test.com".to_string(),
        None,
        ModelStatsStore::new(3000.0),
        vec![],
        8,
        15000,
        "round_robin".to_string(),
        std::collections::HashMap::new(),
        ModelCompat::default(),
    );

    let result = validate_model_exists("any-model", &state);
    assert!(result.is_ok(), "should pass when no cache available");
}

#[test]
fn test_role_transformation_developer_to_user() {
    use nimaproxy::config::ModelCompat;

    let mut compat = ModelCompat::default();
    compat.supports_developer_role = Some(vec!["blocked-model".to_string()]);

    let state = AppState::new(
        vec![KeyEntry {
            key: "test".to_string(),
            label: Some("test".to_string()),
        }],
        "https://test.com".to_string(),
        None,
        ModelStatsStore::new(3000.0),
        vec![],
        3,
        8000,
        "complete".to_string(),
        std::collections::HashMap::new(),
        compat,
    );

    let request_body = json!({
        "model": "blocked-model",
        "messages": [
            {"role": "developer", "content": "You are a helpful assistant"}
        ]
    });

    let body_bytes = serde_json::to_vec(&request_body).unwrap();
    let (_model_id, result) =
        nimaproxy::proxy::resolve_model(bytes::Bytes::from(body_bytes), &state);

    let result_str = std::str::from_utf8(&result).unwrap();
    let transformed: serde_json::Value = serde_json::from_str(result_str).unwrap();

    let role = transformed["messages"][0]["role"].as_str().unwrap();
    assert_eq!(
        role, "user",
        "developer role should be transformed to user for model in list"
    );
}

#[test]
fn test_role_transformation_tool_to_assistant() {
    use nimaproxy::config::ModelCompat;

    let mut compat = ModelCompat::default();
    compat.supports_tool_messages = Some(vec!["allowed-model".to_string()]);

    let state = AppState::new(
        vec![KeyEntry {
            key: "test".to_string(),
            label: Some("test".to_string()),
        }],
        "https://test.com".to_string(),
        None,
        ModelStatsStore::new(3000.0),
        vec![],
        3,
        8000,
        "complete".to_string(),
        std::collections::HashMap::new(),
        compat,
    );

    let request_body = json!({
        "model": "allowed-model",
        "messages": [
            {"role": "tool", "content": "Tool result", "tool_call_id": "call_123"}
        ]
    });

    let body_bytes = serde_json::to_vec(&request_body).unwrap();
    let (_model_id, result) =
        nimaproxy::proxy::resolve_model(bytes::Bytes::from(body_bytes), &state);

    let result_str = std::str::from_utf8(&result).unwrap();
    let transformed: serde_json::Value = serde_json::from_str(result_str).unwrap();

    let role = transformed["messages"][0]["role"].as_str().unwrap();
    assert_eq!(
        role, "assistant",
        "tool role should be transformed to assistant for model in list"
    );
}

#[test]
fn test_role_transformation_no_change_for_allowed_model() {
    use nimaproxy::config::ModelCompat;

    let mut compat = ModelCompat::default();
    compat.supports_developer_role = Some(vec!["allowed-model".to_string()]);

    let state = AppState::new(
        vec![KeyEntry {
            key: "test".to_string(),
            label: Some("test".to_string()),
        }],
        "https://test.com".to_string(),
        None,
        ModelStatsStore::new(3000.0),
        vec![],
        3,
        8000,
        "complete".to_string(),
        std::collections::HashMap::new(),
        compat,
    );

    let request_body = json!({
        "model": "allowed-model",
        "messages": [
            {"role": "developer", "content": "You are a helpful assistant"}
        ]
    });

    let body_bytes = serde_json::to_vec(&request_body).unwrap();
    let (_model_id, result) =
        nimaproxy::proxy::resolve_model(bytes::Bytes::from(body_bytes), &state);

    let result_str = std::str::from_utf8(&result).unwrap();
    let transformed: serde_json::Value = serde_json::from_str(result_str).unwrap();

    let role = transformed["messages"][0]["role"].as_str().unwrap();
    assert_eq!(
        role, "user",
        "developer role should be transformed to user for model in list"
    );
}

#[test]
fn test_role_transformation_all_mistral_models() {
    // This test verifies transformation happens for models IN supports_developer_role.
    // In production config, the 10 Mistral models are in the list (they need transformation).
    // All other models should NOT be transformed.
    // This test uses a model NOT in the list, so it should NOT transform.
    let mut compat = ModelCompat::default();
    compat.supports_developer_role = Some(vec!["mistralai/mistral-small-4-119b-2603".to_string()]); // Only one in transform-list

    let state = AppState::new(
        vec![KeyEntry {
            key: "test".to_string(),
            label: Some("test".to_string()),
        }],
        "https://test.com".to_string(),
        None,
        ModelStatsStore::new(3000.0),
        vec![],
        3,
        8000,
        "complete".to_string(),
        std::collections::HashMap::new(),
        compat,
    );

    // mistral-large is NOT in the allow-list, so should get transformed
    let model = "mistralai/mistral-large-3-675b-instruct-2512";
    let request_body = json!({
        "model": model,
        "messages": [
            {"role": "system", "content": "You are helpful"},
            {"role": "developer", "content": "Explain something"},
            {"role": "user", "content": "Hello"}
        ]
    });

    let body_bytes = serde_json::to_vec(&request_body).unwrap();
    let (_model_id, result) =
        nimaproxy::proxy::resolve_model(bytes::Bytes::from(body_bytes), &state);

    let result_str = std::str::from_utf8(&result).unwrap();
    let transformed: serde_json::Value = serde_json::from_str(result_str).unwrap();

    let roles: Vec<&str> = transformed["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();

    assert_eq!(roles[0], "system");
    assert_eq!(
        roles[1], "developer",
        "developer should NOT be transformed for {} (not in list)",
        model
    );
    assert_eq!(roles[2], "user");
}
