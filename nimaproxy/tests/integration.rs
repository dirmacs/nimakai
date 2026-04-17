use nimaproxy::config::KeyEntry;
use nimaproxy::key_pool::KeyPool;
use nimaproxy::model_router::{ModelRouter, Strategy};
use nimaproxy::model_stats::ModelStatsStore;
use nimaproxy::AppState;

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
    );

    let result = validate_model_exists("any-model", &state);
    assert!(result.is_ok(), "should pass when no cache available");
}
