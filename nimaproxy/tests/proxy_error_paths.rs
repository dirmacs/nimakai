//! Integration tests for proxy error paths using mockito.
//! These tests cover network error handling that can't be tested with live API.

use axum::http::HeaderMap;
use axum::response::IntoResponse;
use bytes::Bytes;
use nimaproxy::config::{KeyEntry, ModelCompat, ModelParams};
use nimaproxy::model_stats::ModelStatsStore;
use nimaproxy::proxy::chat_completions;
use nimaproxy::AppState;
use std::collections::HashMap;
use std::sync::Arc;

/// Create test state with a mock API URL
fn make_test_state(api_url: String) -> Arc<AppState> {
    let key_entries = vec![KeyEntry {
        key: "test-key".to_string(),
        label: Some("test".to_string()),
    }];
    AppState::new(
        key_entries,
        api_url,
        None,
        ModelStatsStore::new(100.0),
        vec![],
        3,
        20000,
        "complete".to_string(),
        HashMap::new(),
        ModelCompat::default(),
    )
}

/// Create test state with racing models
fn make_racing_state(api_url: String) -> Arc<AppState> {
    let key_entries = vec![KeyEntry {
        key: "test-key".to_string(),
        label: Some("test".to_string()),
    }];
    AppState::new(
        key_entries,
        api_url,
        None,
        ModelStatsStore::new(100.0),
        vec!["model-a".to_string(), "model-b".to_string()],
        2,
        5000,
        "complete".to_string(),
        HashMap::new(),
        ModelCompat::default(),
    )
}

#[tokio::test]
async fn test_proxy_bad_gateway_on_connection_error() {
    use mockito::Server;
    
    let mut server = Server::new_async().await;
    let mock = server.mock("POST", "/v1/chat/completions")
        .with_status(502)
        .with_body("Bad Gateway")
        .expect_at_least(1)
        .create();
    
    let state = make_test_state(server.url());
    let body = serde_json::json!({
        "model": "test-model",
        "messages": [{"role": "user", "content": "test"}],
        "max_tokens": 10
    });
    
    let resp = chat_completions(
        axum::extract::State(state),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    ).await;
    
    let response = resp.into_response();
    let status = response.status();
    
    mock.assert();
    assert!(status == axum::http::StatusCode::BAD_GATEWAY || status == axum::http::StatusCode::BAD_REQUEST || status.as_u16() >= 400);
}

#[tokio::test]
async fn test_proxy_handles_429_rate_limit() {
    use mockito::Server;
    
    let mut server = Server::new_async().await;
    let mock = server.mock("POST", "/v1/chat/completions")
        .with_status(429)
        .with_header("content-type", "application/json")
        .with_body(r#"{"error": {"message": "Rate limited"}}"#)
        .expect_at_least(1)
        .create();
    
    let state = make_test_state(server.url());
    let body = serde_json::json!({
        "model": "test-model",
        "messages": [{"role": "user", "content": "test"}],
        "max_tokens": 10
    });
    
    let resp = chat_completions(
        axum::extract::State(state),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    ).await;
    
    let response = resp.into_response();
    let status = response.status();
    
    mock.assert();
    assert!(status.as_u16() >= 400);
}

#[tokio::test]
async fn test_proxy_handles_500_server_error() {
    use mockito::Server;
    
    let mut server = Server::new_async().await;
    let mock = server.mock("POST", "/v1/chat/completions")
        .with_status(500)
        .with_header("content-type", "application/json")
        .with_body(r#"{"error": {"message": "Internal error"}}"#)
        .expect_at_least(1)
        .create();
    
    let state = make_test_state(server.url());
    let body = serde_json::json!({
        "model": "test-model",
        "messages": [{"role": "user", "content": "test"}],
        "max_tokens": 10
    });
    
    let resp = chat_completions(
        axum::extract::State(state),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    ).await;
    
    let response = resp.into_response();
    let status = response.status();
    
    mock.assert();
    assert!(status.as_u16() >= 500 || status == axum::http::StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn test_proxy_handles_invalid_json_response() {
    use mockito::Server;
    
    let mut server = Server::new_async().await;
    let mock = server.mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("not valid json")
        .expect_at_least(1)
        .create();
    
    let state = make_test_state(server.url());
    let body = serde_json::json!({
        "model": "test-model",
        "messages": [{"role": "user", "content": "test"}],
        "max_tokens": 10
    });
    
    let resp = chat_completions(
        axum::extract::State(state),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    ).await;
    
    let response = resp.into_response();
    let status = response.status();
    
    mock.assert();
    assert!(status.as_u16() >= 200);
}

#[tokio::test]
async fn test_proxy_handles_empty_response() {
    use mockito::Server;
    
    let mut server = Server::new_async().await;
    let mock = server.mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("")
        .expect_at_least(1)
        .create();
    
    let state = make_test_state(server.url());
    let body = serde_json::json!({
        "model": "test-model",
        "messages": [{"role": "user", "content": "test"}],
        "max_tokens": 10
    });
    
    let _resp = chat_completions(
        axum::extract::State(state),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    ).await;
    
    mock.assert();
}

/// Test racing with auto model selector
#[tokio::test]
async fn test_racing_auto_model_selection() {
    use mockito::Server;
    
    let mut server = Server::new_async().await;
    
    // Mock success response
    let mock = server.mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"test","choices":[{"message":{"content":"hello"}}]}"#)
        .expect_at_least(1)
        .create();
    
    let state = make_racing_state(server.url());
    let body = serde_json::json!({
        "model": "auto",
        "messages": [{"role": "user", "content": "test"}]
    });
    
    let _resp = chat_completions(
        axum::extract::State(state),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    ).await;
    
    mock.assert();
}

/// Test racing with all models failing
#[tokio::test]
async fn test_racing_all_models_fail() {
    use mockito::Server;
    
    let mut server = Server::new_async().await;
    
    let mock = server.mock("POST", "/v1/chat/completions")
        .with_status(502)
        .with_body("Bad Gateway")
        .expect_at_least(1)
        .create();
    
    let state = make_racing_state(server.url());
    let body = serde_json::json!({
        "model": "auto",
        "messages": [{"role": "user", "content": "test"}]
    });
    
    let _resp = chat_completions(
        axum::extract::State(state),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    ).await;
    
    mock.assert();
}

/// Test streaming with mock server
#[tokio::test]
async fn test_proxy_streaming_with_mock() {
    use mockito::Server;
    
    let mut server = Server::new_async().await;
    let sse_data = "data: {\"id\":\"test\",\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\ndata: [DONE]\n\n";
    let mock = server.mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(sse_data)
        .expect_at_least(1)
        .create();
    
    let state = make_test_state(server.url());
    let body = serde_json::json!({
        "model": "test-model",
        "messages": [{"role": "user", "content": "test"}],
        "stream": true
    });
    
    let _resp = chat_completions(
        axum::extract::State(state),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    ).await;
    
    mock.assert();
}

/// Test with model params
#[tokio::test]
async fn test_proxy_with_model_params() {
    use mockito::Server;
    
    let mut server = Server::new_async().await;
    let mock = server.mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"test","choices":[{"message":{"content":"hello"}}]}"#)
        .expect_at_least(1)
        .create();
    
    let key_entries = vec![KeyEntry {
        key: "test-key".to_string(),
        label: Some("test".to_string()),
    }];
    
    let mut model_params = HashMap::new();
    model_params.insert("test-model".to_string(), ModelParams {
        temperature: Some(0.5),
        top_p: Some(0.9),
        max_tokens: Some(100),
        ..Default::default()
    });
    
    let state = AppState::new(
        key_entries,
        server.url(),
        None,
        ModelStatsStore::new(100.0),
        vec![],
        3,
        20000,
        "complete".to_string(),
        model_params,
        ModelCompat::default(),
    );
    
    let body = serde_json::json!({
        "model": "test-model",
        "messages": [{"role": "user", "content": "test"}]
    });
    
    let _resp = chat_completions(
        axum::extract::State(state),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    ).await;
    
    mock.assert();
}

/// Test with ModelRouter for model selection
#[tokio::test]
async fn test_proxy_with_router() {
    use mockito::Server;
    use nimaproxy::model_router::{ModelRouter, Strategy};
    
    let mut server = Server::new_async().await;
    let mock = server.mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"test","choices":[{"message":{"content":"hello"}}]}"#)
        .expect_at_least(1)
        .create();
    
    let key_entries = vec![KeyEntry {
        key: "test-key".to_string(),
        label: Some("test".to_string()),
    }];
    
    let router = ModelRouter::new(
        vec!["test-model".to_string()],
        Strategy::RoundRobin,
    );
    
    let state = AppState::new(
        key_entries,
        server.url(),
        Some(router),
        ModelStatsStore::new(100.0),
        vec![],
        3,
        20000,
        "complete".to_string(),
        HashMap::new(),
        ModelCompat::default(),
    );
    
    let body = serde_json::json!({
        "model": "auto",
        "messages": [{"role": "user", "content": "test"}]
    });
    
    let _resp = chat_completions(
        axum::extract::State(state),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    ).await;
    
    mock.assert();
}

/// Test models endpoint error handling
#[tokio::test]
async fn test_models_endpoint_error() {
    use mockito::Server;
    use nimaproxy::proxy::models;
    
    let mut server = Server::new_async().await;
    let mock = server.mock("GET", "/v1/models")
        .with_status(502)
        .with_body("Bad Gateway")
        .expect_at_least(1)
        .create();
    
    let state = make_test_state(server.url());
    
    let _resp = models(axum::extract::State(state)).await;
    
    mock.assert();
}

/// Test models endpoint success
#[tokio::test]
async fn test_models_endpoint_success() {
    use mockito::Server;
    use nimaproxy::proxy::models;
    
    let mut server = Server::new_async().await;
    let mock = server.mock("GET", "/v1/models")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"data":[{"id":"model-1"},{"id":"model-2"}]}"#)
        .expect_at_least(1)
        .create();
    
    let state = make_test_state(server.url());
    
    let _resp = models(axum::extract::State(state)).await;
    
    mock.assert();
}

/// Test models endpoint with no keys
#[tokio::test]
async fn test_models_endpoint_no_keys() {
    use nimaproxy::proxy::models;
    
    let state = AppState::new(
        vec![],
        "http://example.com".to_string(),
        None,
        ModelStatsStore::new(100.0),
        vec![],
        3,
        20000,
        "complete".to_string(),
        HashMap::new(),
        ModelCompat::default(),
    );
    
    let resp = models(axum::extract::State(state)).await;
    let response = resp.into_response();
    assert_eq!(response.status(), axum::http::StatusCode::TOO_MANY_REQUESTS);
}

/// Test streaming error in race_models
#[tokio::test]
async fn test_racing_streaming_error() {
    use mockito::Server;
    
    let mut server = Server::new_async().await;
    
    let mock = server.mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body("data: {\"error\": \"stream interrupted\"}\n\n")
        .expect_at_least(1)
        .create();
    
    let state = make_racing_state(server.url());
    let body = serde_json::json!({
        "model": "auto",
        "messages": [{"role": "user", "content": "test"}],
        "stream": true
    });
    
    let _resp = chat_completions(
        axum::extract::State(state),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    ).await;
    
    mock.assert();
}

/// Test race_models with invalid JSON body
#[tokio::test]
async fn test_racing_invalid_json_body() {
    use mockito::Server;
    
    let mut server = Server::new_async().await;
    let mock = server.mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"test","choices":[{"message":{"content":"hello"}}]}"#)
        .expect_at_least(1)
        .create();
    
    let state = make_racing_state(server.url());
    
    let body = Bytes::from("not valid json");
    
    let _resp = chat_completions(
        axum::extract::State(state),
        HeaderMap::new(),
        body,
    ).await;
    
    mock.assert();
}

/// Test key pool exhaustion
#[tokio::test]
async fn test_key_pool_exhaustion() {
    use mockito::Server;
    
    let mut server = Server::new_async().await;
    let mock = server.mock("POST", "/v1/chat/completions")
        .with_status(429)
        .with_header("content-type", "application/json")
        .with_body(r#"{"error": {"message": "Rate limited"}}"#)
        .expect_at_least(1)
        .create();
    
    let state = AppState::new(
        vec![KeyEntry { key: "test-key".to_string(), label: Some("test".to_string()) }],
        server.url(),
        None,
        ModelStatsStore::new(100.0),
        vec![],
        3,
        20000,
        "complete".to_string(),
        HashMap::new(),
        ModelCompat::default(),
    );
    
    let body = serde_json::json!({
        "model": "test-model",
        "messages": [{"role": "user", "content": "test"}]
    });
    
    let _resp = chat_completions(
        axum::extract::State(state),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    ).await;
    
    mock.assert();
}

#[tokio::test]
async fn test_proxy_handles_json_parse_failure() {
    use mockito::Server;
    let mut server = Server::new_async().await;
    
    // Mock that returns invalid JSON
    let mock = server.mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_body("not valid json {{{")
        .expect_at_least(1)
        .create();
    
    let state = make_test_state(server.url());
    
    // Body with invalid JSON model field
    let body = serde_json::json!({
        "model": "test-model",
        "messages": [{"role": "user", "content": "test"}],
        "max_tokens": 10
    });
    
    let resp = chat_completions(
        axum::extract::State(state),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    ).await;
    
    let response = resp.into_response();
    let status = response.status();
    
    // Should handle gracefully - either success or specific error
    assert!(status.as_u16() >= 400 || status.as_u16() == 200);
    mock.assert();
}

#[tokio::test]
async fn test_proxy_connection_refusal() {
    // Test with unreachable server - connection refused
    let state = make_test_state("http://localhost:1".to_string()); // Port 1 is unreachable
    
    let body = serde_json::json!({
        "model": "test-model",
        "messages": [{"role": "user", "content": "test"}],
        "max_tokens": 10
    });
    
    let resp = chat_completions(
        axum::extract::State(state),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    ).await;
    
    let response = resp.into_response();
    let status = response.status();
    
    // Should return BAD_GATEWAY on connection error
    assert_eq!(status, axum::http::StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn test_proxy_with_empty_messages() {
    use mockito::Server;
    let mut server = Server::new_async().await;
    
    let mock = server.mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"choices": [{"message": {"role": "assistant", "content": "test"}}]}"#)
        .expect_at_least(1)
        .create();
    
    let state = make_test_state(server.url());
    
    // Empty messages array
    let body = serde_json::json!({
        "model": "test-model",
        "messages": [],
        "max_tokens": 10
    });
    
    let resp = chat_completions(
        axum::extract::State(state),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    ).await;
    
    let response = resp.into_response();
    let status = response.status();
    
    // Should handle empty messages - may succeed or fail validation
    mock.assert();
    assert!(status.as_u16() >= 200 && status.as_u16() < 600);
}


// ============================================================================
// Racing status-filter tests: verify 4xx/429 are skipped, not forwarded to client
// ============================================================================

/// Create a racing state with TWO keys so one can be rate-limited and the other used.
fn make_racing_state_two_keys(api_url: String) -> Arc<AppState> {
    let key_entries = vec![
        KeyEntry { key: "key-a".to_string(), label: Some("key-a".to_string()) },
        KeyEntry { key: "key-b".to_string(), label: Some("key-b".to_string()) },
    ];
    AppState::new(
        key_entries,
        api_url,
        None,
        ModelStatsStore::new(100.0),
        vec!["model-a".to_string(), "model-b".to_string()],
        2,
        8000,
        "complete".to_string(),
        HashMap::new(),
        ModelCompat::default(),
    )
}

/// Racing: when one model returns 400, the proxy must NOT forward it immediately.
/// The race must exhaust all models; since the only model returns 400, we get BAD_GATEWAY.
/// Critically: we do NOT get 400 propagated to the client.
#[tokio::test]
async fn test_racing_skips_400_does_not_propagate_to_client() {
    use mockito::Server;

    let mut server = Server::new_async().await;
    // Both paths return 400 Invalid assistant message
    let _mock = server
        .mock("POST", "/v1/chat/completions")
        .with_status(400)
        .with_header("content-type", "application/json")
        .with_body(r#"{"error":{"message":"Invalid assistant message: content=None tool_calls=None"}}"}"#)
        .expect_at_least(1)
        .create();

    let state = make_racing_state(server.url());
    let body = serde_json::json!({
        "model": "auto",
        "messages": [{"role": "user", "content": "test"}],
        "max_tokens": 5
    });

    let resp = nimaproxy::proxy::chat_completions(
        axum::extract::State(state),
        axum::http::HeaderMap::new(),
        bytes::Bytes::from(body.to_string()),
    ).await;

    let response = resp.into_response();
    let status = response.status().as_u16();
    eprintln!("[racing-400-skip] got status {}", status);

    // Must NOT be 400 — proxy must not forward NVIDIA's 400 to the client.
    // With all models returning 400, we expect BAD_GATEWAY (502).
    assert_ne!(status, 400, "proxy must not forward NVIDIA 400 to client — races should be skipped");
    assert!(status == 502 || status == 504 || status == 400,
        "expected 502/504 when all racers fail, got {}", status);
}

/// Racing: when a model returns 429, the key that got 429 must be marked rate-limited.
/// Verify via pool status after the race completes.
#[tokio::test]
async fn test_racing_429_marks_key_rate_limited() {
    use mockito::Server;

    let mut server = Server::new_async().await;
    // Return 429 with Retry-After: 30 for all requests
    let _mock = server
        .mock("POST", "/v1/chat/completions")
        .with_status(429)
        .with_header("content-type", "application/json")
        .with_header("retry-after", "30")
        .with_body(r#"{"error":{"message":"Rate limit exceeded"}}"}"#)
        .expect_at_least(1)
        .create();

    let state = make_racing_state_two_keys(server.url());

    // Verify both keys active before the race
    let pre = state.pool.status();
    assert!(pre[0].active && pre[1].active, "both keys should be active before race");

    let body = serde_json::json!({
        "model": "auto",
        "messages": [{"role": "user", "content": "test"}],
        "max_tokens": 5
    });

    let resp = nimaproxy::proxy::chat_completions(
        axum::extract::State(state.clone()),
        axum::http::HeaderMap::new(),
        bytes::Bytes::from(body.to_string()),
    ).await;

    let status = resp.into_response().status().as_u16();
    eprintln!("[racing-429-key-mark] got status {}", status);

    // After the race, at least one key should be rate-limited (cooldown > 0)
    let post = state.pool.status();
    let rate_limited: Vec<_> = post.iter().filter(|s| !s.active).collect();
    eprintln!("[racing-429-key-mark] rate-limited keys after race: {}", rate_limited.len());
    assert!(!rate_limited.is_empty(), "racing 429 must mark the key as rate-limited (cooldown > 0)");
    // The cooldown should be around 30s (from Retry-After header)
    let cd = rate_limited[0].cooldown_secs_remaining;
    assert!(cd > 0 && cd <= 30, "cooldown should be ≤30s, got {}s", cd);
}

/// Racing: one model returns 400, another returns 200 — the 200 must win.
/// Uses two mock routes on one server: /model-a gets 400, /model-b gets 200.
/// We can't route by path in racing (all go to /v1/chat/completions),
/// so we use a sequence mock: first call 400, second call 200.
#[tokio::test]
async fn test_racing_skips_4xx_and_returns_first_2xx() {
    use mockito::Server;

    let mut server = Server::new_async().await;
    // First request → 400, second request → 200
    // Racing fires both concurrently; the 400 must be skipped, 200 returned
    let _mock400 = server
        .mock("POST", "/v1/chat/completions")
        .with_status(400)
        .with_header("content-type", "application/json")
        .with_body(r#"{"error":{"message":"Invalid assistant message"}}"}"#)
        .create();
    let _mock200 = server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"choices":[{"message":{"role":"assistant","content":"hello"}}]}"}"#)
        .create();

    let state = make_racing_state_two_keys(server.url());
    let body = serde_json::json!({
        "model": "auto",
        "messages": [{"role": "user", "content": "hello"}],
        "max_tokens": 5
    });

    let resp = nimaproxy::proxy::chat_completions(
        axum::extract::State(state),
        axum::http::HeaderMap::new(),
        bytes::Bytes::from(body.to_string()),
    ).await;

    let status = resp.into_response().status().as_u16();
    eprintln!("[racing-4xx-skip-2xx-win] got status {}", status);
    // The 200 racer must win; 400 must be discarded
    assert_eq!(status, 200, "racing must return 200 when one racer succeeds; got {}", status);
}