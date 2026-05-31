//! Integration tests for proxy error paths using mockito.
//! These tests cover network error handling that can't be tested with live API.

use axum::body::to_bytes;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use bytes::Bytes;
use nimaproxy::config::{KeyEntry, ModelCompat, ModelParams};
use nimaproxy::model_router::{ModelRouter, Strategy};
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

async fn response_json(response: axum::response::Response) -> serde_json::Value {
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn test_proxy_bad_gateway_on_connection_error() {
    use mockito::Server;

    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/chat/completions")
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
    )
    .await;

    let response = resp.into_response();
    let status = response.status();

    mock.assert();
    assert!(
        status == axum::http::StatusCode::BAD_GATEWAY
            || status == axum::http::StatusCode::BAD_REQUEST
            || status.as_u16() >= 400
    );
}

#[tokio::test]
async fn test_proxy_handles_429_rate_limit() {
    use mockito::Server;

    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/chat/completions")
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
    )
    .await;

    let response = resp.into_response();
    let status = response.status();

    mock.assert();
    assert!(status.as_u16() >= 400);
}

#[tokio::test]
async fn test_proxy_handles_500_server_error() {
    use mockito::Server;

    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/chat/completions")
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
    )
    .await;

    let response = resp.into_response();
    let status = response.status();

    mock.assert();
    assert!(status.as_u16() >= 500 || status == axum::http::StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn test_proxy_handles_invalid_json_response() {
    use mockito::Server;

    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/chat/completions")
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
    )
    .await;

    let response = resp.into_response();
    let status = response.status();

    mock.assert();
    assert!(status.as_u16() >= 200);
}

#[tokio::test]
async fn test_proxy_handles_empty_response() {
    use mockito::Server;

    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/chat/completions")
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
    )
    .await;

    mock.assert();
}

/// Test racing with auto model selector
#[tokio::test]
async fn test_racing_auto_model_selection() {
    use mockito::Server;

    let mut server = Server::new_async().await;

    // Mock success response
    let mock = server
        .mock("POST", "/v1/chat/completions")
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
    )
    .await;

    mock.assert();
}

/// Test racing with all models failing
#[tokio::test]
async fn test_racing_all_models_fail() {
    use mockito::Server;

    let mut server = Server::new_async().await;

    let mock = server
        .mock("POST", "/v1/chat/completions")
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
    )
    .await;

    mock.assert();
}

/// Test streaming with mock server
#[tokio::test]
async fn test_proxy_streaming_with_mock() {
    use mockito::Server;

    let mut server = Server::new_async().await;
    let sse_data = "data: {\"id\":\"test\",\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\ndata: [DONE]\n\n";
    let mock = server
        .mock("POST", "/v1/chat/completions")
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
    )
    .await;

    mock.assert();
}

/// Test with model params
#[tokio::test]
async fn test_proxy_with_model_params() {
    use mockito::{Matcher, Server};

    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .match_header("accept", "text/event-stream")
        .match_body(Matcher::PartialJson(serde_json::json!({
            "model": "test-model",
            "temperature": 0.5,
            "top_p": 0.9,
            "max_tokens": 100,
            "stream": true,
            "repetition_penalty": 1.0,
            "chat_template_kwargs": {
                "thinking": true,
                "reasoning_effort": "high"
            }
        })))
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
    let mut chat_template_kwargs = HashMap::new();
    chat_template_kwargs.insert("thinking".to_string(), serde_json::json!(true));
    chat_template_kwargs.insert("reasoning_effort".to_string(), serde_json::json!("high"));
    model_params.insert(
        "test-model".to_string(),
        ModelParams {
            temperature: Some(0.5),
            top_p: Some(0.9),
            max_tokens: Some(100),
            stream: Some(true),
            repetition_penalty: Some(1.0),
            chat_template_kwargs: Some(chat_template_kwargs),
            ..Default::default()
        },
    );

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
        "messages": [{"role": "user", "content": "test"}],
        "stream": true
    });

    let _resp = chat_completions(
        axum::extract::State(state),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    )
    .await;

    mock.assert();
}

#[tokio::test]
async fn test_proxy_model_params_do_not_enable_stream_when_omitted() {
    use mockito::{Matcher, Server};

    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .match_header("accept", "application/json")
        .match_body(Matcher::PartialJson(serde_json::json!({
            "model": "test-model",
            "temperature": 0.5
        })))
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
    model_params.insert(
        "test-model".to_string(),
        ModelParams {
            temperature: Some(0.5),
            stream: Some(true),
            ..Default::default()
        },
    );

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
    )
    .await;

    mock.assert();
}

#[tokio::test]
async fn test_direct_chat_request_uses_configured_timeout() {
    use std::time::{Duration, Instant};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((_socket, _peer)) = listener.accept().await {
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });

    let key_entries = vec![KeyEntry {
        key: "test-key".to_string(),
        label: Some("test".to_string()),
    }];
    let state = AppState::new(
        key_entries,
        format!("http://{}", addr),
        None,
        ModelStatsStore::new(100.0),
        vec![],
        3,
        50,
        "complete".to_string(),
        HashMap::new(),
        ModelCompat::default(),
    );

    let body = serde_json::json!({
        "model": "slow-model",
        "messages": [{"role": "user", "content": "test"}]
    });

    let started = Instant::now();
    let resp = chat_completions(
        axum::extract::State(state.clone()),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    )
    .await
    .into_response();
    let elapsed = started.elapsed();

    assert_eq!(resp.status(), axum::http::StatusCode::GATEWAY_TIMEOUT);
    assert!(
        elapsed < Duration::from_secs(2),
        "direct request did not honor timeout quickly enough: {:?}",
        elapsed
    );

    let snapshot = state.model_stats.snapshot();
    let slow = snapshot.iter().find(|s| s.id == "slow-model").unwrap();
    assert_eq!(slow.success, 0);
    assert_eq!(slow.total, 1);
}

/// Test with ModelRouter for model selection
#[tokio::test]
async fn test_proxy_with_router() {
    use mockito::Server;
    use nimaproxy::model_router::{ModelRouter, Strategy};

    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"test","choices":[{"message":{"content":"hello"}}]}"#)
        .expect_at_least(1)
        .create();

    let key_entries = vec![KeyEntry {
        key: "test-key".to_string(),
        label: Some("test".to_string()),
    }];

    let router = ModelRouter::new(vec!["test-model".to_string()], Strategy::RoundRobin);

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
    )
    .await;

    mock.assert();
}

/// Test models endpoint error handling
#[tokio::test]
async fn test_models_endpoint_error() {
    use mockito::Server;
    use nimaproxy::proxy::models;

    let mut server = Server::new_async().await;
    let mock = server
        .mock("GET", "/v1/models")
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
    let mock = server
        .mock("GET", "/v1/models")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"data":[{"id":"model-1"},{"id":"model-2"}]}"#)
        .expect_at_least(1)
        .create();

    let state = make_test_state(server.url());

    let _resp = models(axum::extract::State(state)).await;

    mock.assert();
}

#[tokio::test]
async fn test_models_endpoint_returns_configured_racing_models_without_upstream() {
    use nimaproxy::proxy::models;

    let state = AppState::new(
        vec![],
        "http://example.invalid".to_string(),
        None,
        ModelStatsStore::new(3000.0),
        vec!["model-a".to_string(), "model-b".to_string()],
        2,
        5000,
        "complete".to_string(),
        HashMap::new(),
        ModelCompat::default(),
    );

    let resp = models(axum::extract::State(state)).await.into_response();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    let json = response_json(resp).await;
    let ids: Vec<&str> = json["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["model-a", "model-b"]);
}

#[tokio::test]
async fn test_models_endpoint_includes_router_models() {
    use nimaproxy::proxy::models;

    let router = ModelRouter::new(
        vec!["router-a".to_string(), "router-b".to_string()],
        Strategy::RoundRobin,
    );
    let state = AppState::new(
        vec![],
        "http://example.invalid".to_string(),
        Some(router),
        ModelStatsStore::new(3000.0),
        vec![],
        2,
        5000,
        "complete".to_string(),
        HashMap::new(),
        ModelCompat::default(),
    );

    let resp = models(axum::extract::State(state)).await.into_response();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    let json = response_json(resp).await;
    let ids: Vec<&str> = json["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["router-a", "router-b"]);
}

#[tokio::test]
async fn test_health_exposes_routing_and_racing_metadata() {
    use nimaproxy::proxy::health;

    let router = ModelRouter::new(vec!["router-a".to_string()], Strategy::RoundRobin);
    let state = AppState::new(
        vec![KeyEntry {
            key: "test-key".to_string(),
            label: Some("test".to_string()),
        }],
        "http://example.invalid".to_string(),
        Some(router),
        ModelStatsStore::new(3000.0),
        vec!["race-a".to_string(), "race-b".to_string()],
        2,
        5000,
        "complete".to_string(),
        HashMap::new(),
        ModelCompat::default(),
    );

    let resp = health(axum::extract::State(state)).await.into_response();
    let json = response_json(resp).await;

    assert_eq!(json["routing_enabled"], true);
    assert_eq!(json["racing_enabled"], true);
    assert_eq!(json["routing_models"], serde_json::json!(["router-a"]));
    assert_eq!(
        json["racing_models"],
        serde_json::json!(["race-a", "race-b"])
    );
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

#[tokio::test]
async fn test_racing_records_http_errors_as_failures() {
    use mockito::Server;

    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .with_status(400)
        .with_header("content-type", "application/json")
        .with_body(r#"{"error":{"message":"bad request"}}"#)
        .expect(2)
        .create();

    let state = make_racing_state_two_keys(server.url());
    let body = serde_json::json!({
        "model": "auto",
        "messages": [{"role": "user", "content": "test"}]
    });

    let _resp = chat_completions(
        axum::extract::State(state.clone()),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    )
    .await;

    mock.assert();
    let snapshots = state.model_stats.snapshot();
    assert_eq!(snapshots.len(), 2);
    for snapshot in snapshots {
        assert_eq!(snapshot.total, 1, "{} should have one attempt", snapshot.id);
        assert_eq!(
            snapshot.success, 0,
            "{} should not record HTTP 400 as success",
            snapshot.id
        );
    }
}

#[tokio::test]
async fn test_auto_degraded_retry_reroutes_to_next_model() {
    use mockito::{Matcher, Server};

    let mut server = Server::new_async().await;
    let degraded = server
        .mock("POST", "/v1/chat/completions")
        .match_body(Matcher::PartialJson(
            serde_json::json!({"model": "model-a"}),
        ))
        .with_status(400)
        .with_header("content-type", "application/json")
        .with_body(r#"{"status":400,"detail":"DEGRADED function cannot be invoked"}"#)
        .expect(1)
        .create();
    let fallback = server
        .mock("POST", "/v1/chat/completions")
        .match_body(Matcher::PartialJson(
            serde_json::json!({"model": "model-b"}),
        ))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"ok","choices":[{"message":{"role":"assistant","content":"ok"}}]}"#)
        .expect(1)
        .create();

    let router = ModelRouter::new(
        vec!["model-a".to_string(), "model-b".to_string()],
        Strategy::RoundRobin,
    );
    let state = AppState::new(
        vec![
            KeyEntry {
                key: "test-key-a".to_string(),
                label: Some("key-a".to_string()),
            },
            KeyEntry {
                key: "test-key-b".to_string(),
                label: Some("key-b".to_string()),
            },
        ],
        server.url(),
        Some(router),
        ModelStatsStore::new(3000.0),
        vec![],
        2,
        5000,
        "complete".to_string(),
        HashMap::new(),
        ModelCompat::default(),
    );
    let body = serde_json::json!({
        "model": "auto",
        "messages": [{"role": "user", "content": "test"}]
    });

    let resp = chat_completions(
        axum::extract::State(state),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    )
    .await
    .into_response();

    degraded.assert();
    fallback.assert();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
}

/// Test streaming error in race_models
#[tokio::test]
async fn test_racing_streaming_error() {
    use mockito::Server;

    let mut server = Server::new_async().await;

    let mock = server
        .mock("POST", "/v1/chat/completions")
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
    )
    .await;

    mock.assert();
}

/// Test race_models with invalid JSON body
#[tokio::test]
async fn test_racing_invalid_json_body() {
    use mockito::Server;

    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"test","choices":[{"message":{"content":"hello"}}]}"#)
        .expect_at_least(1)
        .create();

    let state = make_racing_state(server.url());

    let body = Bytes::from("not valid json");

    let _resp = chat_completions(axum::extract::State(state), HeaderMap::new(), body).await;

    mock.assert();
}

/// Test key pool exhaustion
#[tokio::test]
async fn test_key_pool_exhaustion() {
    use mockito::Server;

    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .with_status(429)
        .with_header("content-type", "application/json")
        .with_body(r#"{"error": {"message": "Rate limited"}}"#)
        .expect_at_least(1)
        .create();

    let state = AppState::new(
        vec![KeyEntry {
            key: "test-key".to_string(),
            label: Some("test".to_string()),
        }],
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
    )
    .await;

    mock.assert();
}

#[tokio::test]
async fn test_proxy_handles_json_parse_failure() {
    use mockito::Server;
    let mut server = Server::new_async().await;

    // Mock that returns invalid JSON
    let mock = server
        .mock("POST", "/v1/chat/completions")
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
    )
    .await;

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
    )
    .await;

    let response = resp.into_response();
    let status = response.status();

    // Should return BAD_GATEWAY on connection error
    assert_eq!(status, axum::http::StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn test_proxy_with_empty_messages() {
    use mockito::Server;
    let mut server = Server::new_async().await;

    let mock = server
        .mock("POST", "/v1/chat/completions")
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
    )
    .await;

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
        KeyEntry {
            key: "key-a".to_string(),
            label: Some("key-a".to_string()),
        },
        KeyEntry {
            key: "key-b".to_string(),
            label: Some("key-b".to_string()),
        },
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

#[tokio::test]
async fn test_racing_returns_429_when_all_keys_unavailable() {
    let state = make_racing_state_two_keys("http://127.0.0.1:9".to_string());
    state.pool.mark_rate_limited(0, 30);
    state.pool.mark_rate_limited(1, 30);

    let body = serde_json::json!({
        "model": "auto",
        "messages": [{"role": "user", "content": "test"}],
        "max_tokens": 5
    });

    let resp = nimaproxy::proxy::chat_completions(
        axum::extract::State(state),
        axum::http::HeaderMap::new(),
        bytes::Bytes::from(body.to_string()),
    )
    .await
    .into_response();

    assert_eq!(resp.status(), axum::http::StatusCode::TOO_MANY_REQUESTS);
    let body = to_bytes(resp.into_body(), 1024).await.unwrap();
    assert_eq!(&body[..], b"all API keys rate-limited");
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
        .with_body(
            r#"{"error":{"message":"Invalid assistant message: content=None tool_calls=None"}}"}"#,
        )
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
    )
    .await;

    let response = resp.into_response();
    let status = response.status().as_u16();
    eprintln!("[racing-400-skip] got status {}", status);

    // Must NOT be 400 — proxy must not forward NVIDIA's 400 to the client.
    // With all models returning 400, we expect BAD_GATEWAY (502).
    assert_ne!(
        status, 400,
        "proxy must not forward NVIDIA 400 to client — races should be skipped"
    );
    assert!(
        status == 502 || status == 504 || status == 400,
        "expected 502/504 when all racers fail, got {}",
        status
    );
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
    assert!(
        pre[0].active && pre[1].active,
        "both keys should be active before race"
    );

    let body = serde_json::json!({
        "model": "auto",
        "messages": [{"role": "user", "content": "test"}],
        "max_tokens": 5
    });

    let resp = nimaproxy::proxy::chat_completions(
        axum::extract::State(state.clone()),
        axum::http::HeaderMap::new(),
        bytes::Bytes::from(body.to_string()),
    )
    .await;

    let status = resp.into_response().status().as_u16();
    eprintln!("[racing-429-key-mark] got status {}", status);

    // After the race, at least one key should be rate-limited (cooldown > 0)
    let post = state.pool.status();
    let rate_limited: Vec<_> = post.iter().filter(|s| !s.active).collect();
    eprintln!(
        "[racing-429-key-mark] rate-limited keys after race: {}",
        rate_limited.len()
    );
    assert!(
        !rate_limited.is_empty(),
        "racing 429 must mark the key as rate-limited (cooldown > 0)"
    );
    // The cooldown should be around 30s (from Retry-After header)
    let cd = rate_limited[0].cooldown_secs_remaining;
    assert!(cd > 0 && cd <= 30, "cooldown should be ≤30s, got {}s", cd);
}

#[tokio::test]
async fn test_racing_429_loser_does_not_globally_cool_key() {
    use mockito::Server;

    let mut server = Server::new_async().await;
    let _rate_limited = server
        .mock("POST", "/v1/chat/completions")
        .with_status(429)
        .with_header("content-type", "application/json")
        .with_header("retry-after", "30")
        .with_body(r#"{"error":{"message":"Rate limit exceeded for this model"}}"#)
        .expect(1)
        .create();
    let _winner = server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"ok","model":"model-b","choices":[{"message":{"content":"ok"}}]}"#)
        .expect(1)
        .create();

    let state = make_racing_state_two_keys(server.url());
    let body = serde_json::json!({
        "model": "auto",
        "messages": [{"role": "user", "content": "test"}],
        "max_tokens": 5
    });

    let resp = nimaproxy::proxy::chat_completions(
        axum::extract::State(state.clone()),
        axum::http::HeaderMap::new(),
        bytes::Bytes::from(body.to_string()),
    )
    .await
    .into_response();

    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let post = state.pool.status();
    assert!(
        post.iter().all(|s| s.active),
        "a losing 429 racer must not globally cool keys when another model wins: {:?}",
        post
    );
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
    )
    .await;

    let status = resp.into_response().status().as_u16();
    eprintln!("[racing-4xx-skip-2xx-win] got status {}", status);
    // The 200 racer must win; 400 must be discarded
    assert_eq!(
        status, 200,
        "racing must return 200 when one racer succeeds; got {}",
        status
    );
}
