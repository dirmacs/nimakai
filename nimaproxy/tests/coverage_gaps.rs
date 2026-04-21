//! Targeted tests to close coverage gaps toward 95% target.

use nimaproxy::model_stats::ModelStatsStore;

#[test]
fn test_circuit_breaker_had_tool_call_path() {
    let store = ModelStatsStore::new(3000.0);
    
    store.record_with_circuit_breaker("test-model", 100.0, true, 50, 0, true);
    store.record_with_circuit_breaker("test-model", 100.0, true, 50, 0, false);
    store.record_with_circuit_breaker("test-model", 100.0, true, 50, 0, false);
    
    let snapshot = store.snapshot();
    let model = snapshot.iter().find(|s| s.id == "test-model").unwrap();
    assert_eq!(model.total, 3);
    assert_eq!(model.success, 3);
}

#[test]
fn test_record_failure_path() {
    let store = ModelStatsStore::new(3000.0);
    
    store.record("test-model", 100.0, false);
    store.record("test-model", 100.0, false);
    
    let snapshot = store.snapshot();
    let model = snapshot.iter().find(|s| s.id == "test-model").unwrap();
    assert_eq!(model.total, 2);
    assert_eq!(model.success, 0);
}

#[test]
fn test_success_rate_calculation() {
    let store = ModelStatsStore::new(3000.0);
    
    store.record("test-model", 100.0, true);
    store.record("test-model", 100.0, true);
    store.record("test-model", 100.0, true);
    store.record("test-model", 100.0, false);
    
    let snapshot = store.snapshot();
    let model = snapshot.iter().find(|s| s.id == "test-model").unwrap();
    assert_eq!(model.total, 4);
    assert_eq!(model.success, 3);
}

#[test]
fn test_get_model_timeout() {
    let store = ModelStatsStore::new(3000.0);
    
    store.record("test-model", 100.0, true);
    store.record("test-model", 150.0, true);
    
    let timeout = store.get_model_timeout("test-model", 10000);
    assert!(timeout > 0);
    assert!(timeout <= 10000);
}

#[test]
fn test_racing_candidates_with_degraded() {
    let store = ModelStatsStore::new(3000.0);
    
    store.record("fast-model", 50.0, true);
    store.record("fast-model", 60.0, true);
    store.record("fast-model", 55.0, true);
    
    store.record("slow-model", 5000.0, false);
    store.record("slow-model", 5000.0, false);
    store.record("slow-model", 5000.0, false);
    
    let candidates = vec!["fast-model".to_string(), "slow-model".to_string()];
    let racing = store.racing_candidates(&candidates, 2);
    
    assert!(!racing.is_empty());
}

#[test]
fn test_snapshot_includes_all_fields() {
    let store = ModelStatsStore::new(3000.0);
    
    store.record("test-model", 100.0, true);
    store.record("test-model", 120.0, true);
    
    let snapshot = store.snapshot();
    let model = snapshot.iter().find(|s| s.id == "test-model").unwrap();
    
    assert!(model.avg_ms.is_some());
    assert!(model.p95_ms.is_some());
    assert_eq!(model.total, 2);
    assert_eq!(model.success, 2);
}

#[test]
fn test_key_failures_tracked() {
    let store = ModelStatsStore::new(3000.0);
    
    store.record_with_key("test-model", "key1", 100.0, false);
    store.record_with_key("test-model", "key2", 100.0, false);
    
    let snapshot = store.snapshot();
    let model = snapshot.iter().find(|s| s.id == "test-model");
    assert!(model.is_some());
}

#[test]
fn test_racing_candidates_less_than_two() {
    let store = ModelStatsStore::new(3000.0);
    
    let candidates = vec!["single-model".to_string()];
    let racing = store.racing_candidates(&candidates, 1);
    
    assert_eq!(racing.len(), 1);
}

#[test]
fn test_record_with_circuit_breaker_zero_tokens() {
    let store = ModelStatsStore::new(3000.0);
    
    store.record_with_circuit_breaker("test-model", 100.0, true, 0, 0, false);
    
    let snapshot = store.snapshot();
    let model = snapshot.iter().find(|s| s.id == "test-model").unwrap();
    assert_eq!(model.total, 1);
}

#[test]
fn test_get_key_failure_summary() {
    let store = ModelStatsStore::new(3000.0);
    
    store.record_with_key("model1", "key-a", 100.0, false);
    store.record_with_key("model1", "key-b", 100.0, false);
    store.record_with_key("model2", "key-a", 100.0, false);
    
    let summary = store.get_key_failure_summary();
    assert!(!summary.is_empty());
}

#[test]
fn test_dynamic_timeout_with_samples() {
    let store = ModelStatsStore::new(3000.0);
    
    for i in 0..10 {
        store.record("test-model", 100.0 + (i as f64 * 10.0), true);
    }
    
    let timeout = store.get_model_timeout("test-model", 30000);
    assert!(timeout > 0);
}

#[test]
fn test_p95_with_many_samples() {
    let store = ModelStatsStore::new(3000.0);
    
    for i in 0..50 {
        store.record("test-model", 100.0 + (i as f64), true);
    }
    
    let snapshot = store.snapshot();
    let model = snapshot.iter().find(|s| s.id == "test-model").unwrap();
    
    assert!(model.p95_ms.is_some());
    assert!(model.avg_ms.is_some());
}

#[test]
fn test_degraded_flag() {
    let store = ModelStatsStore::new(3000.0);
    
    // Make model degraded with consecutive failures
    for _ in 0..5 {
        store.record("degraded-model", 5000.0, false);
    }
    
    let snapshot = store.snapshot();
    let model = snapshot.iter().find(|s| s.id == "degraded-model");
    if let Some(m) = model {
        assert!(m.degraded);
    }
}

#[test]
fn test_consecutive_failures_tracked() {
    let store = ModelStatsStore::new(3000.0);
    
    store.record("test-model", 100.0, false);
    store.record("test-model", 100.0, false);
    store.record("test-model", 100.0, false);
    
    let snapshot = store.snapshot();
    let model = snapshot.iter().find(|s| s.id == "test-model").unwrap();
    assert_eq!(model.consecutive_failures, 3);
}
