use nimaproxy::config::KeyEntry;
use nimaproxy::key_pool::KeyPool;
use nimaproxy::model_router::{ModelRouter, Strategy};
use nimaproxy::model_stats::ModelStatsStore;
use nimaproxy::AppState;
use std::sync::Arc;

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

    let state = Arc::new(AppState::new(
        keys,
        "https://api.example.com".to_string(),
        Some(router),
        stats,
        vec![],
        3,
        8000,
        "complete".to_string(),
    ));

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
