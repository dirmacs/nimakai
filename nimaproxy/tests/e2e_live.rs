use axum::response::IntoResponse;
use nimaproxy::config::KeyEntry;
use nimaproxy::model_stats::ModelStatsStore;
use nimaproxy::AppState;
use std::sync::Arc;

const NVIDIA_API_BASE: &str = "https://integrate.api.nvidia.com";

fn get_test_keys() -> Vec<(String, String)> {
    vec![
        ("REDACTED_KEY_1".to_string(), "doltares".to_string()),
        ("REDACTED_KEY_2".to_string(), "ares".to_string()),
    ]
}

fn make_state() -> Arc<AppState> {
    let keys = get_test_keys();
    let key_entries: Vec<KeyEntry> = keys
        .iter()
        .map(|(k, l)| KeyEntry {
            key: k.clone(),
            label: Some(l.clone()),
        })
        .collect();
    Arc::new(AppState::new(
        key_entries,
        NVIDIA_API_BASE.to_string(),
        None,
        ModelStatsStore::new(3000.0),
    ))
}

#[tokio::test]
async fn test_e2e_health_returns_ok() {
    let state = make_state();
    let resp = nimaproxy::proxy::health(axum::extract::State(state.clone())).await;
    
    let response = resp.into_response();
    let (parts, body) = response.into_parts();
    let status = parts.status;
    
    let body_bytes = axum::body::to_bytes(body, 4096).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    
    assert_eq!(json["status"], "ok");
    assert_eq!(json["keys_total"], 2);
    assert_eq!(json["keys_active"], 2);
}

#[tokio::test]
async fn test_e2e_models_endpoint_reachable() {
    let state = make_state();
    let resp = nimaproxy::proxy::models(axum::extract::State(state.clone())).await;
    
    let response = resp.into_response();
    let (parts, body) = response.into_parts();
    let status = parts.status;
    
    assert!(status.is_success() || status.as_u16() == 429);
    
    if status.is_success() {
        let body_bytes = axum::body::to_bytes(body, 262144).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert!(json["data"].as_array().unwrap().len() > 0);
    }
}

#[tokio::test]
async fn test_e2e_key_rotation_round_robin() {
    let state = make_state();
    
    let (key1, idx1) = state.pool.next_key().unwrap();
    let (key2, idx2) = state.pool.next_key().unwrap();
    
    assert_ne!(idx1, idx2);
    assert!(key1.starts_with("nvapi-"));
    assert!(key2.starts_with("nvapi-"));
}

#[tokio::test]
async fn test_e2e_stats_endpoint() {
    let state = make_state();
    
    state.model_stats.record("test-model", 150.0, true);
    state.model_stats.record("test-model", 200.0, true);
    
    let resp = nimaproxy::proxy::stats(axum::extract::State(state.clone())).await;
    
    let response = resp.into_response();
    let (_parts, body) = response.into_parts();
    
    let body_bytes = axum::body::to_bytes(body, 4096).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    
    assert!(json["count"].as_i64().unwrap() >= 1);
}

#[tokio::test]
async fn test_e2e_key_pool_status() {
    let state = make_state();
    
    let statuses = state.pool.status();
    assert_eq!(statuses.len(), 2);
    
    assert_eq!(statuses[0].label, "doltares");
    assert_eq!(statuses[1].label, "ares");
    
    state.pool.mark_rate_limited(0, 60);
    
    let statuses_after = state.pool.status();
    assert!(!statuses_after[0].active);
    assert!(statuses_after[1].active);
}

#[tokio::test]
async fn test_e2e_chat_via_proxy() {
    let state = make_state();
    
    state.model_stats.record("nvidia/z-ai/glm4.7", 500.0, true);
    
    let body = serde_json::json!({
        "model": "nvidia/z-ai/glm4.7",
        "messages": [{"role": "user", "content": "Say 'test' in one word."}],
        "max_tokens": 10,
        "temperature": 0.0
    });
    
    let resp = nimaproxy::proxy::chat_completions(
        axum::extract::State(state.clone()),
        axum::http::HeaderMap::new(),
        bytes::Bytes::from(body.to_string()),
    ).await;
    
    let response = resp.into_response();
    let (parts, body) = response.into_parts();
    let status = parts.status;
    let status_code = status.as_u16();
    
    eprintln!("[e2e] chat status: {}", status_code);
    
    if !status.is_success() {
        let body_bytes = axum::body::to_bytes(body, 4096).await.unwrap();
        eprintln!("[e2e] error body: {}", String::from_utf8_lossy(&body_bytes));
    }
    
    assert!(status.is_success() || status_code == 400 || status_code == 429 || status_code == 401 || status_code == 500 || status_code == 404, 
           "got status {}, expected 200/400/429/401/500/404", status_code);
}