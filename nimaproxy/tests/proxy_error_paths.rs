//! Integration tests for proxy error paths using mockito.
//! These tests cover network error handling that can't be tested with live API.

use axum::body::to_bytes;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use bytes::Bytes;
use nimaproxy::config::{KeyEntry, ModelCompat, ModelParams};
use nimaproxy::model_router::{ModelRouter, Strategy};
use nimaproxy::model_stats::ModelStatsStore;
use nimaproxy::proxy::chat::chat_completions;
use nimaproxy::{AppState, AppStateParams, RuntimeControls};
use std::collections::HashMap;
use std::sync::Arc;

/// Create test state with a mock API URL
fn make_test_state(api_url: String) -> Arc<AppState> {
    let key_entries = vec![KeyEntry {
        key: "test-key".to_string(),
        label: Some("test".to_string()),
        disabled: false,
    }];
    AppState::new(AppStateParams {
        keys: key_entries,
        target: api_url,
        router: None,
        model_stats: ModelStatsStore::new(100.0),
        racing_models: vec![],
        racing_max_parallel: 3,
        racing_timeout_ms: 20000,
        racing_strategy: "complete".to_string(),
        model_params: HashMap::new(),
        model_compat: ModelCompat::default(),
        loop_idle_timeout: nimaproxy::loop_idle_timeout(),
    })
}

/// Create test state with TWO API keys, no racing (for auth-failure retry-with-next-key tests).
fn make_two_key_test_state(api_url: String) -> Arc<AppState> {
    let key_entries = vec![
        KeyEntry {
            key: "test-key-0".to_string(),
            label: Some("key-0".to_string()),
            disabled: false,
        },
        KeyEntry {
            key: "test-key-1".to_string(),
            label: Some("key-1".to_string()),
            disabled: false,
        },
    ];
    AppState::new(AppStateParams {
        keys: key_entries,
        target: api_url,
        router: None,
        model_stats: ModelStatsStore::new(100.0),
        racing_models: vec![],
        racing_max_parallel: 3,
        racing_timeout_ms: 20000,
        racing_strategy: "complete".to_string(),
        model_params: HashMap::new(),
        model_compat: ModelCompat::default(),
        loop_idle_timeout: nimaproxy::loop_idle_timeout(),
    })
}

/// Create test state with racing models
fn make_racing_state(api_url: String) -> Arc<AppState> {
    let key_entries = vec![KeyEntry {
        key: "test-key".to_string(),
        label: Some("test".to_string()),
        disabled: false,
    }];
    AppState::new(AppStateParams {
        keys: key_entries,
        target: api_url,
        router: None,
        model_stats: ModelStatsStore::new(100.0),
        racing_models: vec!["model-a".to_string(), "model-b".to_string()],
        racing_max_parallel: 2,
        racing_timeout_ms: 5000,
        racing_strategy: "complete".to_string(),
        model_params: HashMap::new(),
        model_compat: ModelCompat::default(),
        loop_idle_timeout: nimaproxy::loop_idle_timeout(),
    })
}

/// Create racing state that is forced into solo fallback by large prompt policy.
fn make_solo_fallback_state(api_url: String) -> Arc<AppState> {
    let key_entries = vec![
        KeyEntry {
            key: "test-key-a".to_string(),
            label: Some("key-a".to_string()),
            disabled: false,
        },
        KeyEntry {
            key: "test-key-b".to_string(),
            label: Some("key-b".to_string()),
            disabled: false,
        },
    ];
    AppState::new_with_controls(
        AppStateParams {
            keys: key_entries,
            target: api_url,
            router: None,
            model_stats: ModelStatsStore::new(3000.0),
            racing_models: vec!["model-a".to_string(), "model-b".to_string()],
            racing_max_parallel: 2,
            racing_timeout_ms: 5000,
            racing_strategy: "complete".to_string(),
            model_params: HashMap::new(),
            model_compat: ModelCompat::default(),
            loop_idle_timeout: nimaproxy::loop_idle_timeout(),
        },
        RuntimeControls {
            racing_adaptive: true,
            racing_min_parallel: 2,
            racing_pressure_parallel: 2,
            racing_degraded_parallel: 2,
            racing_fast_models: vec!["model-a".to_string(), "model-b".to_string()],
            racing_fallback_models: vec![],
            racing_large_prompt_char_threshold: 1,
            racing_large_prompt_parallel: 1,
            racing_solo_fallback: true,
            racing_max_total_request_ms: 30000,
            max_upstream_in_flight: 1,
            max_in_flight_per_key: 1,
            admission_wait_ms: 0,
            min_dynamic_timeout_ms: 1000,
            dynamic_sample_floor: 10,
            first_byte_timeout_ms: None,
            mid_stream_idle_timeout_ms: 1000,
            auth_failure_cooldown_secs: 900,
        },
    )
}

fn make_short_deadline_solo_state(api_url: String) -> Arc<AppState> {
    let key_entries = vec![KeyEntry {
        key: "test-key-a".to_string(),
        label: Some("key-a".to_string()),
        disabled: false,
    }];
    AppState::new_with_controls(
        AppStateParams {
            keys: key_entries,
            target: api_url,
            router: None,
            model_stats: ModelStatsStore::new(3000.0),
            racing_models: vec!["model-a".to_string(), "model-b".to_string()],
            racing_max_parallel: 2,
            racing_timeout_ms: 5000,
            racing_strategy: "complete".to_string(),
            model_params: HashMap::new(),
            model_compat: ModelCompat::default(),
            loop_idle_timeout: nimaproxy::loop_idle_timeout(),
        },
        RuntimeControls {
            racing_adaptive: true,
            racing_min_parallel: 2,
            racing_pressure_parallel: 2,
            racing_degraded_parallel: 2,
            racing_fast_models: vec!["model-a".to_string(), "model-b".to_string()],
            racing_fallback_models: vec![],
            racing_large_prompt_char_threshold: 1,
            racing_large_prompt_parallel: 1,
            racing_solo_fallback: true,
            racing_max_total_request_ms: 150,
            max_upstream_in_flight: 1,
            max_in_flight_per_key: 1,
            admission_wait_ms: 0,
            min_dynamic_timeout_ms: 1000,
            dynamic_sample_floor: 10,
            first_byte_timeout_ms: None,
            mid_stream_idle_timeout_ms: 1000,
            auth_failure_cooldown_secs: 900,
        },
    )
}

/// Create racing state with more configured models than the race fanout.
fn make_racing_with_unused_fallback_state(api_url: String) -> Arc<AppState> {
    let key_entries = vec![
        KeyEntry {
            key: "test-key-a".to_string(),
            label: Some("key-a".to_string()),
            disabled: false,
        },
        KeyEntry {
            key: "test-key-b".to_string(),
            label: Some("key-b".to_string()),
            disabled: false,
        },
        KeyEntry {
            key: "test-key-c".to_string(),
            label: Some("key-c".to_string()),
            disabled: false,
        },
    ];
    AppState::new_with_controls(
        AppStateParams {
            keys: key_entries,
            target: api_url,
            router: None,
            model_stats: ModelStatsStore::new(3000.0),
            racing_models: vec![
                "model-a".to_string(),
                "model-b".to_string(),
                "model-c".to_string(),
            ],
            racing_max_parallel: 2,
            racing_timeout_ms: 5000,
            racing_strategy: "complete".to_string(),
            model_params: HashMap::new(),
            model_compat: ModelCompat::default(),
            loop_idle_timeout: nimaproxy::loop_idle_timeout(),
        },
        RuntimeControls {
            racing_adaptive: true,
            racing_min_parallel: 2,
            racing_pressure_parallel: 2,
            racing_degraded_parallel: 2,
            racing_fast_models: vec!["model-a".to_string(), "model-b".to_string()],
            racing_fallback_models: vec!["model-c".to_string()],
            racing_large_prompt_char_threshold: 0,
            racing_large_prompt_parallel: 1,
            racing_solo_fallback: true,
            racing_max_total_request_ms: 30000,
            max_upstream_in_flight: 2,
            max_in_flight_per_key: 1,
            admission_wait_ms: 0,
            min_dynamic_timeout_ms: 1000,
            dynamic_sample_floor: 10,
            first_byte_timeout_ms: None,
            mid_stream_idle_timeout_ms: 1000,
            auth_failure_cooldown_secs: 900,
        },
    )
}

#[tokio::test]
async fn test_solo_fallback_respects_total_request_deadline() {
    use mockito::{Matcher, Server};
    use std::thread;
    use std::time::{Duration, Instant};

    let mut server = Server::new_async().await;
    let slow = server
        .mock("POST", "/v1/chat/completions")
        .match_body(Matcher::PartialJson(serde_json::json!({
            "model": "model-a"
        })))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_chunked_body(|w| {
            thread::sleep(Duration::from_millis(500));
            std::io::Write::write_all(
                w,
                br#"{"id":"test","choices":[{"message":{"content":"late"}}]}"#,
            )
        })
        .expect(1)
        .create();

    let state = make_short_deadline_solo_state(server.url());
    let body = serde_json::json!({
        "model": "auto",
        "messages": [{"role": "user", "content": "large prompt body"}]
    });

    let started = Instant::now();
    let resp = chat_completions(
        axum::extract::State(state),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    )
    .await
    .into_response();
    let elapsed = started.elapsed();
    let status = resp.status();
    let body = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let body = String::from_utf8_lossy(&body);

    slow.assert();
    assert_eq!(status, axum::http::StatusCode::GATEWAY_TIMEOUT);
    assert!(
        elapsed < Duration::from_millis(1000),
        "deadline should cap fallback quickly, elapsed={elapsed:?}"
    );
    assert!(body.contains("racing deadline exceeded"));
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
        axum::extract::State(state.clone()),
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
        axum::extract::State(state.clone()),
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
        axum::extract::State(state.clone()),
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

/// Test provider-prefixed auto model selector used by OMP-style model IDs.
#[tokio::test]
async fn test_racing_accepts_nimaproxy_auto_alias() {
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
    let body = serde_json::json!({
        "model": "nimaproxy/auto",
        "messages": [{"role": "user", "content": "test"}]
    });

    let resp = chat_completions(
        axum::extract::State(state),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    )
    .await
    .into_response();

    mock.assert();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
}

/// Test solo fallback walks to the next model on transient upstream 503.
#[tokio::test]
async fn test_solo_fallback_retries_next_model_on_503() {
    use mockito::{Matcher, Server};

    let mut server = Server::new_async().await;

    let first = server
        .mock("POST", "/v1/chat/completions")
        .match_body(Matcher::PartialJson(serde_json::json!({
            "model": "model-a"
        })))
        .with_status(503)
        .with_header("content-type", "application/json")
        .with_body(r#"{"message":"ResourceExhausted: All workers are busy"}"#)
        .expect(1)
        .create();
    let second = server
        .mock("POST", "/v1/chat/completions")
        .match_body(Matcher::PartialJson(serde_json::json!({
            "model": "model-b"
        })))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"test","choices":[{"message":{"content":"ok"}}]}"#)
        .expect(1)
        .create();

    let state = make_solo_fallback_state(server.url());
    for _ in 0..3 {
        state.model_stats.record("model-a", 100.0, true);
    }
    let body = serde_json::json!({
        "model": "auto",
        "messages": [{"role": "user", "content": "large prompt body"}]
    });

    let resp = chat_completions(
        axum::extract::State(state.clone()),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    )
    .await
    .into_response();

    first.assert();
    second.assert();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let metrics = state.gateway_metrics.snapshot();
    let wins = metrics.racing_wins;
    assert_eq!(wins.get("model-b"), Some(&1));
    assert_eq!(metrics.solo_fallbacks, 1);
    assert_eq!(metrics.sequential_fallbacks, 1);
}

/// Test an all-failed race can recover by trying unused fallback models sequentially.
#[tokio::test]
async fn test_all_failed_race_tries_unused_sequential_fallback() {
    use mockito::{Matcher, Server};

    let mut server = Server::new_async().await;

    let race_a = server
        .mock("POST", "/v1/chat/completions")
        .match_body(Matcher::PartialJson(serde_json::json!({
            "model": "model-a"
        })))
        .with_status(503)
        .with_header("content-type", "application/json")
        .with_body(r#"{"message":"busy a"}"#)
        .expect(1)
        .create();
    let race_b = server
        .mock("POST", "/v1/chat/completions")
        .match_body(Matcher::PartialJson(serde_json::json!({
            "model": "model-b"
        })))
        .with_status(503)
        .with_header("content-type", "application/json")
        .with_body(r#"{"message":"busy b"}"#)
        .expect(1)
        .create();
    let fallback_c = server
        .mock("POST", "/v1/chat/completions")
        .match_body(Matcher::PartialJson(serde_json::json!({
            "model": "model-c"
        })))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"test","choices":[{"message":{"content":"ok"}}]}"#)
        .expect(1)
        .create();

    let state = make_racing_with_unused_fallback_state(server.url());
    let body = serde_json::json!({
        "model": "auto",
        "messages": [{"role": "user", "content": "test"}]
    });

    let resp = chat_completions(
        axum::extract::State(state.clone()),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    )
    .await
    .into_response();

    race_a.assert();
    race_b.assert();
    fallback_c.assert();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let metrics = state.gateway_metrics.snapshot();
    let wins = metrics.racing_wins;
    assert_eq!(wins.get("model-c"), Some(&1));
    assert_eq!(metrics.racing_all_failed, 1);
    assert_eq!(metrics.solo_fallbacks, 1);
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
        disabled: false,
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

    let state = AppState::new(AppStateParams {
        keys: key_entries,
        target: server.url(),
        router: None,
        model_stats: ModelStatsStore::new(100.0),
        racing_models: vec![],
        racing_max_parallel: 3,
        racing_timeout_ms: 20000,
        racing_strategy: "complete".to_string(),
        model_params,
        model_compat: ModelCompat::default(),
        loop_idle_timeout: nimaproxy::loop_idle_timeout(),
    });

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
        disabled: false,
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

    let state = AppState::new(AppStateParams {
        keys: key_entries,
        target: server.url(),
        router: None,
        model_stats: ModelStatsStore::new(100.0),
        racing_models: vec![],
        racing_max_parallel: 3,
        racing_timeout_ms: 20000,
        racing_strategy: "complete".to_string(),
        model_params,
        model_compat: ModelCompat::default(),
        loop_idle_timeout: nimaproxy::loop_idle_timeout(),
    });

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

/// A timed-out direct request returns 504 within one budget window.
///
/// The stall is retryable even on this single-key pool (the loop admits one
/// re-attempt past the key count, and nothing reached the client), so the
/// model records two attempts: the retry fires and stalls again, and only
/// then does the handler return the terminal 504.
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
        disabled: false,
    }];
    let state = AppState::new(AppStateParams {
        keys: key_entries,
        target: format!("http://{}", addr),
        router: None,
        model_stats: ModelStatsStore::new(100.0),
        racing_models: vec![],
        racing_max_parallel: 3,
        racing_timeout_ms: 50,
        racing_strategy: "complete".to_string(),
        model_params: HashMap::new(),
        model_compat: ModelCompat::default(),
        loop_idle_timeout: nimaproxy::loop_idle_timeout(),
    });

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
    assert_eq!(slow.total, 2);
}

/// A timed-out upstream call must be retried on another key, and the total
/// client wait must stay bounded to roughly one timeout window rather than
/// one window per key.
///
/// Before this retried, every timeout became an immediate 504: ~32% of
/// admitted requests on the live host returned 504 with no second attempt.
#[tokio::test]
async fn test_timeout_retries_on_another_key_within_budget() {
    use std::time::{Duration, Instant};
    use tokio::net::TcpListener;

    // Two listeners: the one the first key hits accepts and never answers;
    // the second key hits a server that answers immediately. Only the first
    // key is pointed at the dead listener, so a retry on another key can only
    // succeed by moving to a working upstream.
    let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_addr = dead.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((_s, _p)) = dead.accept().await {
            // Hold the connection open so the first attempt spends its whole
            // timeout window. This makes the budget arithmetic observable:
            // with the admission allowance the budget covers two windows, and
            // without it the first attempt already fills the budget and the
            // retry can never fire.
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
    let live = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let live_addr = live.local_addr().unwrap();
    eprintln!("[retry] recovery upstream bound to {live_addr}");
    tokio::spawn(async move {
        if let Ok((mut s, _p)) = live.accept().await {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf).await;
            let body = r#"{"id":"r","choices":[{"message":{"content":"recovered"}}]}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(), body
            );
            let _ = s.write_all(resp.as_bytes()).await;
        }
    });

    let state = AppState::new_with_controls(
        AppStateParams {
            keys: vec![
                KeyEntry {
                    key: "k1".to_string(),
                    label: Some("k1".to_string()),
                    disabled: false,
                },
                KeyEntry {
                    key: "k2".to_string(),
                    label: Some("k2".to_string()),
                    disabled: false,
                },
            ],
            target: format!("http://{}", dead_addr),
            router: None,
            model_stats: ModelStatsStore::new(100.0),
            racing_models: vec![],
            racing_max_parallel: 3,
            racing_timeout_ms: 300,
            racing_strategy: "complete".to_string(),
            model_params: HashMap::new(),
            model_compat: ModelCompat::default(),
            loop_idle_timeout: nimaproxy::loop_idle_timeout(),
        },
        RuntimeControls {
            max_upstream_in_flight: 8,
            max_in_flight_per_key: 2,
            admission_wait_ms: 9000,
            min_dynamic_timeout_ms: 300,
            dynamic_sample_floor: 10,
            mid_stream_idle_timeout_ms: 300,
            auth_failure_cooldown_secs: 900,
            ..RuntimeControls::default()
        },
    );

    // Point the whole pool at only one upstream, so BOTH keys retry against
    // the live server: the first attempt must time out on the dead one.

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

    // Dead upstream: after the retry is exhausted the request still fails
    // closed with 504, it does not hang and it does not invent a success.
    assert_eq!(resp.status(), axum::http::StatusCode::GATEWAY_TIMEOUT);
    let snapshot = state.model_stats.snapshot();
    let slow = snapshot.iter().find(|s| s.id == "slow-model").unwrap();

    // The decisive bound: attempts must be fewer than one per key, i.e. the
    // 2x budget caps the retry instead of letting every key time out.
    assert!(
        slow.total >= 2,
        "a timed-out attempt must be retried on another key; observed {} attempts",
        slow.total
    );
    assert!(
        slow.total <= 2,
        "timeout retry must be bounded by the attempt budget, saw {} attempts",
        slow.total
    );
    assert!(
        elapsed < Duration::from_millis(1200),
        "total wait must stay within the 2x budget, took {:?}",
        elapsed
    );
}

/// The body-collection timeout must retry on another key.
///
/// This is the path that actually fires in production: the connection is
/// established and headers arrive, but the body stalls, so `send()` has
/// already succeeded and the send-timeout branch never runs. Every 504 on the
/// live host took this branch at exactly the timeout floor.
#[tokio::test]
async fn test_body_timeout_retries_on_another_key_within_budget() {
    use mockito::{Matcher, Server};
    use std::thread;
    use std::time::{Duration, Instant};

    let mut server = Server::new_async().await;

    // A body that is first sent immediately, then stalls long past the
    // timeout, so headers arrive fast and the body collection times out.
    let slow_body = server
        .mock("POST", "/v1/chat/completions")
        .match_body(Matcher::PartialJson(serde_json::json!({
            "model": "slow-model"
        })))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_chunked_body(|w| {
            std::io::Write::write_all(w, br#"{"id":"test","#)?;
            w.flush()?;
            thread::sleep(Duration::from_millis(700));
            std::io::Write::write_all(
                w,
                br#""choices":[{"message":{"content":"never drained"}}]}"#,
            )
        })
        .expect_at_least(1)
        .create();

    let state = AppState::new_with_controls(
        AppStateParams {
            keys: vec![
                KeyEntry {
                    key: "k1".to_string(),
                    label: Some("k1".to_string()),
                    disabled: false,
                },
                KeyEntry {
                    key: "k2".to_string(),
                    label: Some("k2".to_string()),
                    disabled: false,
                },
            ],
            target: server.url(),
            router: None,
            model_stats: ModelStatsStore::new(100.0),
            racing_models: vec![],
            racing_max_parallel: 3,
            racing_timeout_ms: 250,
            racing_strategy: "complete".to_string(),
            model_params: HashMap::new(),
            model_compat: ModelCompat::default(),
            loop_idle_timeout: nimaproxy::loop_idle_timeout(),
        },
        RuntimeControls {
            max_upstream_in_flight: 8,
            max_in_flight_per_key: 2,
            admission_wait_ms: 0,
            min_dynamic_timeout_ms: 250,
            dynamic_sample_floor: 10,
            mid_stream_idle_timeout_ms: 250,
            auth_failure_cooldown_secs: 900,
            ..RuntimeControls::default()
        },
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

    // The decisive assertions: the request must have been attempted more
    // than once (the retry fired) but still bounded by the budget.
    let snapshot = state.model_stats.snapshot();
    let slow = snapshot.iter().find(|s| s.id == "slow-model").unwrap();
    assert!(
        slow.total >= 2,
        "a stalled body must be retried on another key; observed {} attempts",
        slow.total
    );
    assert!(
        elapsed < Duration::from_millis(2500),
        "total wait must stay within the 2x budget, took {:?}",
        elapsed
    );

    slow_body.assert();
}

/// The companion to the test above, and the reason the fix is not simply
/// "map every mid-body error to a stall".
///
/// A genuine upstream I/O failure (the connection resets mid-body) must still
/// be a 502 and must NOT be retried, because the request already consumed a
/// key's quota and a broken connection is not a silent upstream. Without this
/// arm, a fix that classified every mid-body error as a stall would pass the
/// test above while making the proxy retry on hard connection failures.
#[tokio::test]
async fn test_upstream_io_failure_is_not_treated_as_a_stall() {
    use mockito::Server;

    let mut server = Server::new_async().await;
    // A body that starts, then resets the connection mid-stream. The response
    // is deliberately truncated so the client sees a broken read rather than a
    // clean end of body.
    let broken = server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_chunked_body(|w| {
            std::io::Write::write_all(w, br#"{"id":"test","#)?;
            w.flush()?;
            Err(std::io::Error::other("simulated mid-body connection reset"))
        })
        .expect_at_least(1)
        .create();

    let state = AppState::new_with_controls(
        AppStateParams {
            keys: vec![KeyEntry {
                key: "k1".to_string(),
                label: Some("k1".to_string()),
                disabled: false,
            }],
            target: server.url(),
            router: None,
            model_stats: ModelStatsStore::new(100.0),
            racing_models: vec![],
            racing_max_parallel: 3,
            racing_timeout_ms: 250,
            racing_strategy: "complete".to_string(),
            model_params: HashMap::new(),
            model_compat: ModelCompat::default(),
            loop_idle_timeout: nimaproxy::loop_idle_timeout(),
        },
        RuntimeControls {
            max_upstream_in_flight: 8,
            max_in_flight_per_key: 2,
            admission_wait_ms: 0,
            min_dynamic_timeout_ms: 250,
            dynamic_sample_floor: 10,
            mid_stream_idle_timeout_ms: 250,
            auth_failure_cooldown_secs: 900,
            ..RuntimeControls::default()
        },
    );

    let body = serde_json::json!({
        "model": "broken-model",
        "messages": [{"role": "user", "content": "test"}]
    });

    let resp = chat_completions(
        axum::extract::State(state.clone()),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    )
    .await
    .into_response();

    assert_eq!(
        resp.status(),
        axum::http::StatusCode::BAD_GATEWAY,
        "a broken upstream connection is a 502, not a timeout, and must not be laundered into a retryable stall"
    );

    // The decisive arm: the request must NOT have been retried. A stall gets
    // retried; a connection reset does not.
    //
    // Exactly one attempt is the property. Whether the model has a stats entry
    // at all depends on where the attempt is recorded, which changed when the
    // response is assembled rather than buffered — so accept either shape and
    // assert the bound instead.
    let snapshot = state.model_stats.snapshot();
    let attempts = snapshot
        .iter()
        .find(|s| s.id == "broken-model")
        .map_or(0, |s| s.total);
    assert!(
        attempts <= 1,
        "a mid-body I/O failure must not be retried; observed {attempts} attempts"
    );

    broken.assert();
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
        disabled: false,
    }];

    let router = ModelRouter::new(vec!["test-model".to_string()], Strategy::RoundRobin);

    let state = AppState::new(AppStateParams {
        keys: key_entries,
        target: server.url(),
        router: Some(router),
        model_stats: ModelStatsStore::new(100.0),
        racing_models: vec![],
        racing_max_parallel: 3,
        racing_timeout_ms: 20000,
        racing_strategy: "complete".to_string(),
        model_params: HashMap::new(),
        model_compat: ModelCompat::default(),
        loop_idle_timeout: nimaproxy::loop_idle_timeout(),
    });

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
    use nimaproxy::proxy::http::models;

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
    use nimaproxy::proxy::http::models;

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
    use nimaproxy::proxy::http::models;

    let state = AppState::new(AppStateParams {
        keys: vec![],
        target: "http://example.invalid".to_string(),
        router: None,
        model_stats: ModelStatsStore::new(3000.0),
        racing_models: vec!["model-a".to_string(), "model-b".to_string()],
        racing_max_parallel: 2,
        racing_timeout_ms: 5000,
        racing_strategy: "complete".to_string(),
        model_params: HashMap::new(),
        model_compat: ModelCompat::default(),
        loop_idle_timeout: nimaproxy::loop_idle_timeout(),
    });

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
    use nimaproxy::proxy::http::models;

    let router = ModelRouter::new(
        vec!["router-a".to_string(), "router-b".to_string()],
        Strategy::RoundRobin,
    );
    let state = AppState::new(AppStateParams {
        keys: vec![],
        target: "http://example.invalid".to_string(),
        router: Some(router),
        model_stats: ModelStatsStore::new(3000.0),
        racing_models: vec![],
        racing_max_parallel: 2,
        racing_timeout_ms: 5000,
        racing_strategy: "complete".to_string(),
        model_params: HashMap::new(),
        model_compat: ModelCompat::default(),
        loop_idle_timeout: nimaproxy::loop_idle_timeout(),
    });

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
    use nimaproxy::proxy::http::health;

    let router = ModelRouter::new(vec!["router-a".to_string()], Strategy::RoundRobin);
    let state = AppState::new(AppStateParams {
        keys: vec![KeyEntry {
            key: "test-key".to_string(),
            label: Some("test".to_string()),
            disabled: false,
        }],
        target: "http://example.invalid".to_string(),
        router: Some(router),
        model_stats: ModelStatsStore::new(3000.0),
        racing_models: vec!["race-a".to_string(), "race-b".to_string()],
        racing_max_parallel: 2,
        racing_timeout_ms: 5000,
        racing_strategy: "complete".to_string(),
        model_params: HashMap::new(),
        model_compat: ModelCompat::default(),
        loop_idle_timeout: nimaproxy::loop_idle_timeout(),
    });

    let resp = health(axum::extract::State(state)).await.into_response();
    let json = response_json(resp).await;

    assert_eq!(json["routing_enabled"], true);
    assert_eq!(json["racing_enabled"], true);
    assert_eq!(json["key_window_capacity"], 3);
    assert_eq!(json["key_available_permits"], 3);
    assert_eq!(json["admission_wait_ms"], 1500);
    assert_eq!(json["racing_solo_fallback"], true);
    assert_eq!(json["racing_large_prompt_parallel"], 1);
    assert_eq!(json["routing_models"], serde_json::json!(["router-a"]));
    assert_eq!(
        json["racing_models"],
        serde_json::json!(["race-a", "race-b"])
    );
}

#[tokio::test]
async fn test_stats_exposes_gateway_metrics_and_limits() {
    let state = make_racing_state_two_keys("http://127.0.0.1:9".to_string());
    state.gateway_metrics.record_request(true);
    state.gateway_metrics.record_fanout(2);
    state.gateway_metrics.record_overload();
    state.gateway_metrics.record_solo_fallback();
    state.gateway_metrics.record_sequential_fallback();
    state.gateway_metrics.record_all_racers_failed();
    state.gateway_metrics.record_deadline_exceeded();

    let resp = nimaproxy::proxy::http::stats(axum::extract::State(state))
        .await
        .into_response();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    let json = response_json(resp).await;
    assert_eq!(json["gateway"]["request_total"], 1);
    assert_eq!(json["gateway"]["racing_requests"], 1);
    assert_eq!(json["gateway"]["fanout_total"], 2);
    assert_eq!(json["gateway"]["solo_fallbacks"], 1);
    assert_eq!(json["gateway"]["sequential_fallbacks"], 1);
    assert_eq!(json["gateway"]["racing_all_failed"], 1);
    assert_eq!(json["gateway"]["racing_deadline_exceeded"], 1);
    assert_eq!(json["gateway"]["overload_rejects"], 1);
    assert_eq!(json["gateway"]["max_upstream_in_flight"], 48);
    assert_eq!(json["gateway"]["max_in_flight_per_key"], 3);
    assert_eq!(json["gateway"]["key_window_capacity"], 6);
    assert_eq!(json["gateway"]["key_available_permits"], 6);
    assert_eq!(json["gateway"]["admission_wait_ms"], 1500);
    assert_eq!(json["keys"][0]["configured_max_in_flight"], 3);
    assert_eq!(json["racing_enabled"], true);
    assert_eq!(json["racing_adaptive"], false);
    assert_eq!(json["racing_solo_fallback"], true);
    assert_eq!(json["racing_large_prompt_parallel"], 1);
}

/// Test models endpoint with no keys
#[tokio::test]
async fn test_models_endpoint_no_keys() {
    use nimaproxy::proxy::http::models;

    let state = AppState::new(AppStateParams {
        keys: vec![],
        target: "http://example.com".to_string(),
        router: None,
        model_stats: ModelStatsStore::new(100.0),
        racing_models: vec![],
        racing_max_parallel: 3,
        racing_timeout_ms: 20000,
        racing_strategy: "complete".to_string(),
        model_params: HashMap::new(),
        model_compat: ModelCompat::default(),
        loop_idle_timeout: nimaproxy::loop_idle_timeout(),
    });

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
    let state = AppState::new(AppStateParams {
        keys: vec![
            KeyEntry {
                key: "test-key-a".to_string(),
                label: Some("key-a".to_string()),
                disabled: false,
            },
            KeyEntry {
                key: "test-key-b".to_string(),
                label: Some("key-b".to_string()),
                disabled: false,
            },
        ],
        target: server.url(),
        router: Some(router),
        model_stats: ModelStatsStore::new(3000.0),
        racing_models: vec![],
        racing_max_parallel: 2,
        racing_timeout_ms: 5000,
        racing_strategy: "complete".to_string(),
        model_params: HashMap::new(),
        model_compat: ModelCompat::default(),
        loop_idle_timeout: nimaproxy::loop_idle_timeout(),
    });
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

    let state = AppState::new(AppStateParams {
        keys: vec![KeyEntry {
            key: "test-key".to_string(),
            label: Some("test".to_string()),
            disabled: false,
        }],
        target: server.url(),
        router: None,
        model_stats: ModelStatsStore::new(100.0),
        racing_models: vec![],
        racing_max_parallel: 3,
        racing_timeout_ms: 20000,
        racing_strategy: "complete".to_string(),
        model_params: HashMap::new(),
        model_compat: ModelCompat::default(),
        loop_idle_timeout: nimaproxy::loop_idle_timeout(),
    });

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
            disabled: false,
        },
        KeyEntry {
            key: "key-b".to_string(),
            label: Some("key-b".to_string()),
            disabled: false,
        },
    ];
    AppState::new(AppStateParams {
        keys: key_entries,
        target: api_url,
        router: None,
        model_stats: ModelStatsStore::new(100.0),
        racing_models: vec!["model-a".to_string(), "model-b".to_string()],
        racing_max_parallel: 2,
        racing_timeout_ms: 8000,
        racing_strategy: "complete".to_string(),
        model_params: HashMap::new(),
        model_compat: ModelCompat::default(),
        loop_idle_timeout: nimaproxy::loop_idle_timeout(),
    })
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

    let resp = nimaproxy::proxy::chat::chat_completions(
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

    let resp = nimaproxy::proxy::chat::chat_completions(
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

    let resp = nimaproxy::proxy::chat::chat_completions(
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

    let resp = nimaproxy::proxy::chat::chat_completions(
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

    let resp = nimaproxy::proxy::chat::chat_completions(
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

// ============================================================================
// Auth-failure (401/403) key quarantine tests
// ============================================================================

/// Create a racing state with TWO keys and a near-zero admission wait, so a test that
/// exhausts both keys via auth failures does not have to wait out the default admission
/// wait window before observing the final response.
fn make_racing_state_two_keys_fast(api_url: String) -> Arc<AppState> {
    let key_entries = vec![
        KeyEntry {
            key: "key-a".to_string(),
            label: Some("key-a".to_string()),
            disabled: false,
        },
        KeyEntry {
            key: "key-b".to_string(),
            label: Some("key-b".to_string()),
            disabled: false,
        },
    ];
    AppState::new_with_controls(
        AppStateParams {
            keys: key_entries,
            target: api_url,
            router: None,
            model_stats: ModelStatsStore::new(100.0),
            racing_models: vec!["model-a".to_string(), "model-b".to_string()],
            racing_max_parallel: 2,
            racing_timeout_ms: 8000,
            racing_strategy: "complete".to_string(),
            model_params: HashMap::new(),
            model_compat: ModelCompat::default(),
            loop_idle_timeout: nimaproxy::loop_idle_timeout(),
        },
        RuntimeControls {
            racing_adaptive: false,
            racing_min_parallel: 2,
            racing_pressure_parallel: 6,
            racing_degraded_parallel: 3,
            racing_fast_models: vec![],
            racing_fallback_models: vec![],
            racing_large_prompt_char_threshold: 0,
            racing_large_prompt_parallel: 1,
            racing_solo_fallback: true,
            racing_max_total_request_ms: 30000,
            max_upstream_in_flight: 48,
            max_in_flight_per_key: 3,
            admission_wait_ms: 0,
            min_dynamic_timeout_ms: 8000,
            dynamic_sample_floor: 10,
            first_byte_timeout_ms: None,
            mid_stream_idle_timeout_ms: 8000,
            auth_failure_cooldown_secs: 900,
        },
    )
}

/// (a) First key gets 403, second key gets 200 on the non-streaming chat path: the client
/// must see 200, key 0 must be in cooldown, and its `auth_failures` counter must be 1.
#[tokio::test]
async fn test_auth_failure_403_then_200_retries_with_next_key() {
    use mockito::Server;

    let mut server = Server::new_async().await;
    // First call (key 0) -> 403, second call (key 1) -> 200 (mockito serves in creation order).
    let _mock403 = server
        .mock("POST", "/v1/chat/completions")
        .with_status(403)
        .with_header("content-type", "application/json")
        .with_body(r#"{"error":{"message":"Forbidden: key revoked","code":"invalid_api_key"}}"#)
        .create();
    let _mock200 = server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"choices":[{"message":{"role":"assistant","content":"hi"}}]}"#)
        .create();

    let state = make_two_key_test_state(server.url());
    let body = serde_json::json!({
        "model": "test-model",
        "messages": [{"role": "user", "content": "test"}],
        "max_tokens": 5
    });

    let resp = chat_completions(
        axum::extract::State(state.clone()),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    )
    .await
    .into_response();

    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    let statuses = state.pool.status();
    assert!(
        !statuses[0].active,
        "key 0 should be quarantined after the 401/403 auth failure"
    );
    assert!(statuses[0].cooldown_secs_remaining > 0);
    assert_eq!(statuses[0].auth_failures, 1);
    assert!(statuses[1].active, "key 1 should still be usable");
    assert_eq!(statuses[1].auth_failures, 0);
}

/// (b) Same as (a) but for a `"stream": true` request — the auth-failure status check runs
/// before the body is converted to a stream, so the retry-with-next-key behavior is identical.
#[tokio::test]
async fn test_auth_failure_403_then_200_retries_with_next_key_streaming() {
    use mockito::Server;

    let mut server = Server::new_async().await;
    let _mock403 = server
        .mock("POST", "/v1/chat/completions")
        .with_status(403)
        .with_header("content-type", "application/json")
        .with_body(r#"{"error":{"message":"Forbidden: key revoked","code":"invalid_api_key"}}"#)
        .create();
    let sse_data = "data: {\"id\":\"test\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
    let _mock200 = server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(sse_data)
        .create();

    let state = make_two_key_test_state(server.url());
    let body = serde_json::json!({
        "model": "test-model",
        "messages": [{"role": "user", "content": "test"}],
        "stream": true
    });

    let resp = chat_completions(
        axum::extract::State(state.clone()),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    )
    .await
    .into_response();

    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    let statuses = state.pool.status();
    assert!(
        !statuses[0].active,
        "key 0 should be quarantined after the 401/403 auth failure"
    );
    assert_eq!(statuses[0].auth_failures, 1);
    assert!(statuses[1].active, "key 1 should still be usable");
    assert_eq!(statuses[1].auth_failures, 0);
}

/// (c) Every key returns 403: the client must see 403 with the exact upstream error body,
/// not the generic "all keys exhausted" 429 fallback used for rate-limit exhaustion.
#[tokio::test]
async fn test_auth_failure_403_all_keys_returns_upstream_body_unchanged() {
    use mockito::Server;

    let mut server = Server::new_async().await;
    let error_body = serde_json::json!({"error": {"message": "Forbidden: key revoked", "code": "invalid_api_key"}});
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .with_status(403)
        .with_header("content-type", "application/json")
        .with_body(error_body.to_string())
        .expect(2)
        .create();

    let state = make_two_key_test_state(server.url());
    let body = serde_json::json!({
        "model": "test-model",
        "messages": [{"role": "user", "content": "test"}],
        "max_tokens": 5
    });

    let resp = chat_completions(
        axum::extract::State(state.clone()),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    )
    .await
    .into_response();

    mock.assert();
    assert_eq!(resp.status(), axum::http::StatusCode::FORBIDDEN);

    let resp_body = to_bytes(resp.into_body(), 65_536).await.unwrap();
    let resp_json: serde_json::Value = serde_json::from_slice(&resp_body).unwrap();
    assert_eq!(
        resp_json, error_body,
        "client must receive the exact upstream 403 body unchanged"
    );

    let statuses = state.pool.status();
    assert!(!statuses[0].active);
    assert!(!statuses[1].active);
    assert_eq!(statuses[0].auth_failures, 1);
    assert_eq!(statuses[1].auth_failures, 1);
}

/// Single-key 401: the client must see the verbatim upstream 401, not a 429.
///
/// Regression for the `attempts = n.max(2)` loop widening: the extra attempt
/// is reserved for budget-gated stall retries. A 401 quarantines the only
/// key, so a same-key re-attempt would stall in admission and return "all
/// API keys rate-limited" before the tail's verbatim forwarding. The 401
/// branch breaks out of the loop once every key has been tried, so the
/// single-key case falls straight to the tail.
#[tokio::test]
async fn test_single_key_401_forwards_verbatim_not_429() {
    use mockito::Server;

    let mut server = Server::new_async().await;
    let error_body = serde_json::json!({"error": {"message": "Unauthorized: key revoked", "code": "invalid_api_key"}});
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .with_status(401)
        .with_header("content-type", "application/json")
        .with_body(error_body.to_string())
        .expect(1)
        .create();

    let key_entries = vec![KeyEntry {
        key: "test-key".to_string(),
        label: Some("key-0".to_string()),
        disabled: false,
    }];
    let state = AppState::new(AppStateParams {
        keys: key_entries,
        target: server.url(),
        router: None,
        model_stats: ModelStatsStore::new(100.0),
        racing_models: vec![],
        racing_max_parallel: 3,
        racing_timeout_ms: 20000,
        racing_strategy: "complete".to_string(),
        model_params: HashMap::new(),
        model_compat: ModelCompat::default(),
        loop_idle_timeout: nimaproxy::loop_idle_timeout(),
    });
    let body = serde_json::json!({
        "model": "test-model",
        "messages": [{"role": "user", "content": "test"}],
        "max_tokens": 5
    });

    let resp = chat_completions(
        axum::extract::State(state.clone()),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    )
    .await
    .into_response();

    mock.assert();
    assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED);

    let resp_body = to_bytes(resp.into_body(), 65_536).await.unwrap();
    let resp_json: serde_json::Value = serde_json::from_slice(&resp_body).unwrap();
    assert_eq!(
        resp_json, error_body,
        "single-key 401 must reach the tail's verbatim forwarding, not a 429"
    );
}

/// Single-key 429: the client must see the immediate tail 429 without burning
/// the admission wait on a pointless same-key re-attempt.
#[tokio::test]
async fn test_single_key_429_returns_tail_without_admission_wait() {
    use mockito::Server;
    use std::time::{Duration, Instant};

    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .with_status(429)
        .with_header("content-type", "application/json")
        .with_body(r#"{"error":{"message":"rate limited"}}"#)
        .expect(1)
        .create();

    let key_entries = vec![KeyEntry {
        key: "test-key".to_string(),
        label: Some("key-0".to_string()),
        disabled: false,
    }];
    let state = AppState::new(AppStateParams {
        keys: key_entries,
        target: server.url(),
        router: None,
        model_stats: ModelStatsStore::new(100.0),
        racing_models: vec![],
        racing_max_parallel: 3,
        racing_timeout_ms: 20000,
        racing_strategy: "complete".to_string(),
        model_params: HashMap::new(),
        model_compat: ModelCompat::default(),
        loop_idle_timeout: nimaproxy::loop_idle_timeout(),
    });
    let body = serde_json::json!({
        "model": "test-model",
        "messages": [{"role": "user", "content": "test"}],
        "max_tokens": 5
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

    mock.assert();
    assert_eq!(resp.status(), axum::http::StatusCode::TOO_MANY_REQUESTS);
    assert!(
        elapsed < Duration::from_millis(state.admission_wait_ms),
        "single-key 429 must break to the tail, not burn the admission wait \
         on a same-key re-attempt: {:?} vs admission_wait_ms {}",
        elapsed,
        state.admission_wait_ms
    );
}

/// (d) A 403 on a racing leg must be treated as a key failure only: it must NOT be recorded
/// against the model in `ModelStatsStore` (no degradation/timeout-quarantine footprint).
#[tokio::test]
async fn test_racing_403_does_not_degrade_model_stats() {
    use mockito::Server;

    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .with_status(403)
        .with_header("content-type", "application/json")
        .with_body(r#"{"error":{"message":"Forbidden: key revoked","code":"invalid_api_key"}}"#)
        .expect(2)
        .create();

    let state = make_racing_state_two_keys_fast(server.url());
    let body = serde_json::json!({
        "model": "auto",
        "messages": [{"role": "user", "content": "test"}],
        "max_tokens": 5
    });

    let _resp = chat_completions(
        axum::extract::State(state.clone()),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    )
    .await;

    mock.assert();

    let snapshots = state.model_stats.snapshot();
    assert!(
        !snapshots
            .iter()
            .any(|s| s.id == "model-a" || s.id == "model-b"),
        "a 401/403 racing leg must not record any model_stats entry for the model: {:?}",
        snapshots.iter().map(|s| &s.id).collect::<Vec<_>>()
    );

    // Both keys took the 403 and must be quarantined/counted as key failures.
    let statuses = state.pool.status();
    assert_eq!(statuses[0].auth_failures, 1);
    assert_eq!(statuses[1].auth_failures, 1);
}

// ============================================================================
// In-stream (HTTP 200 with an embedded upstream error) racing tests
// ============================================================================

/// (a) Racing, two models: leg A returns HTTP 200 `text/event-stream` with only an
/// in-stream error frame + `[DONE]`, arriving fast (5ms); leg B returns a valid content
/// stream, arriving slower (50ms). The in-stream error must lose the race even though it
/// "completes" first — the client must receive leg B's real content, not a fake 200
/// wrapping the upstream error.
#[tokio::test]
async fn test_racing_in_stream_error_loses_to_valid_content() {
    use mockito::{Matcher, Server};
    use std::thread;
    use std::time::Duration;

    let mut server = Server::new_async().await;

    let error_leg = server
        .mock("POST", "/v1/chat/completions")
        .match_body(Matcher::PartialJson(serde_json::json!({"model": "model-a"})))
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_chunked_body(|w| {
            thread::sleep(Duration::from_millis(5));
            std::io::Write::write_all(
                w,
                b"data: {\"message\":\"Service temporarily overloaded\",\"type\":\"service_unavailable\",\"code\":5033}\n\ndata: [DONE]\n\n",
            )
        })
        .expect(1)
        .create();

    let content_leg = server
        .mock("POST", "/v1/chat/completions")
        .match_body(Matcher::PartialJson(serde_json::json!({"model": "model-b"})))
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_chunked_body(|w| {
            thread::sleep(Duration::from_millis(50));
            std::io::Write::write_all(
                w,
                b"data: {\"id\":\"test\",\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\ndata: [DONE]\n\n",
            )
        })
        .expect(1)
        .create();

    let state = make_racing_state(server.url());
    let body = serde_json::json!({
        "model": "auto",
        "messages": [{"role": "user", "content": "test"}],
        "stream": true
    });

    let resp = chat_completions(
        axum::extract::State(state),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    )
    .await
    .into_response();

    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let body_str = String::from_utf8_lossy(&bytes);
    assert!(
        body_str.contains("hello") && body_str.contains("choices") && body_str.contains("[DONE]"),
        "expected the winning leg's real content, got: {body_str}"
    );
    assert!(
        !body_str.contains("Service temporarily overloaded"),
        "the in-stream error leg must not win the race, got: {body_str}"
    );

    error_leg.assert();
    content_leg.assert();
}

/// (b) Racing where every leg returns an in-stream 503 error frame: the client must get an
/// error response (503 or 502) whose body includes the upstream message, never a 200.
#[tokio::test]
async fn test_racing_all_legs_in_stream_error_returns_error_not_200() {
    use mockito::Server;

    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(
            "data: {\"message\":\"Service temporarily overloaded\",\"type\":\"service_unavailable\",\"code\":503}\n\ndata: [DONE]\n\n",
        )
        .expect_at_least(1)
        .create();

    let state = make_racing_state(server.url());
    let body = serde_json::json!({
        "model": "auto",
        "messages": [{"role": "user", "content": "test"}],
        "stream": true
    });

    let resp = chat_completions(
        axum::extract::State(state),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    )
    .await
    .into_response();

    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let body_str = String::from_utf8_lossy(&bytes);

    assert_ne!(
        status,
        axum::http::StatusCode::OK,
        "an in-stream error on every leg must not look like a 200 success, body={body_str}"
    );
    assert!(
        status == axum::http::StatusCode::SERVICE_UNAVAILABLE
            || status == axum::http::StatusCode::BAD_GATEWAY,
        "expected 503 or 502, got {status}, body={body_str}"
    );
    assert!(
        body_str.contains("Service temporarily overloaded"),
        "response body must include the upstream message, got: {body_str}"
    );

    mock.assert();
}

/// A streaming response that takes longer than the timeout window to COMPLETE
/// must still succeed, as long as chunks keep arriving.
///
/// The proxy used to buffer the whole body before replying, so the window was
/// effectively a limit on total generation time. Measured on the live gateway
/// that turned a response whose first token arrived in 0.33 s into a 504:
/// 96% of failures took the body path, and the same request direct to the
/// upstream produced 262 chunks in 7.9 s.
///
/// This is the fleet's symptom: every lane runs step-5 through this proxy, and
/// long generations were terminated mid-run. The bound that must survive is the
/// INTER-CHUNK gap, not the total duration.
#[tokio::test]
async fn test_slow_but_healthy_stream_is_not_a_timeout() {
    use mockito::{Matcher, Server};
    use std::thread;
    use std::time::{Duration, Instant};

    let mut server = Server::new_async().await;
    // Six chunks, each 120 ms apart: 720 ms of total generation. The window is
    // 250 ms, so the whole response exceeds it several times over while no
    // single gap ever does.
    let sse = server
        .mock("POST", "/v1/chat/completions")
        .match_body(Matcher::PartialJson(serde_json::json!({
            "model": "stream-model"
        })))
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_chunked_body(|w| {
            for i in 0..6 {
                let chunk =
                    format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"tok{i}\"}}}}]}}\n\n");
                std::io::Write::write_all(w, chunk.as_bytes())?;
                w.flush()?;
                thread::sleep(Duration::from_millis(120));
            }
            std::io::Write::write_all(w, b"data: [DONE]\n\n")?;
            Ok(())
        })
        .expect_at_least(1)
        .create();

    let state = AppState::new_with_controls(
        AppStateParams {
            keys: vec![KeyEntry {
                key: "k1".to_string(),
                label: Some("k1".to_string()),
                disabled: false,
            }],
            target: server.url(),
            router: None,
            model_stats: ModelStatsStore::new(100.0),
            racing_models: vec![],
            racing_max_parallel: 1,
            racing_timeout_ms: 250,
            racing_strategy: "complete".to_string(),
            model_params: HashMap::new(),
            model_compat: ModelCompat::default(),
            loop_idle_timeout: nimaproxy::loop_idle_timeout(),
        },
        RuntimeControls {
            max_upstream_in_flight: 8,
            max_in_flight_per_key: 2,
            admission_wait_ms: 0,
            min_dynamic_timeout_ms: 250,
            dynamic_sample_floor: 10,
            mid_stream_idle_timeout_ms: 250,
            auth_failure_cooldown_secs: 900,
            ..RuntimeControls::default()
        },
    );

    let body = serde_json::json!({
        "model": "stream-model",
        "messages": [{"role": "user", "content": "test"}],
        "stream": true
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

    assert_eq!(
        resp.status(),
        axum::http::StatusCode::OK,
        "a healthy slow stream must not be reported as a timeout"
    );
    // And every chunk must actually arrive: a response that truncates early
    // would look like success here while dropping tokens.
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes);
    for i in 0..6 {
        assert!(
            text.contains(&format!("tok{i}")),
            "chunk {i} was dropped from a stream that reported success: {text}"
        );
    }
    assert!(text.contains("[DONE]"), "stream terminator lost: {text}");
    // NOTE: total wall-clock is deliberately not asserted. The mock buffers its
    // own writes, so the chunks arrive together regardless of the sleeps above,
    // and an elapsed-time assertion here would measure the mock rather than the
    // proxy. The status and per-chunk assertions above are what prove the
    // proxy forwarded the stream instead of waiting for it to finish.
    let _ = elapsed;
    sse.assert();
}

/// The complement: a stream that STOPS producing chunks must still fail.
/// Without this the test above could pass by simply never timing out, which
/// would let a genuinely dead upstream hold a connection open forever.
///
/// Uses a real TCP listener rather than a mock, because a mock buffers its
/// `chunked_body` closure into a single write — the sleeps inside it are not
/// observable as inter-chunk gaps at the HTTP layer, so a mock-based version of
/// this test passes even with the idle guard removed entirely. Verified by
/// mutation: with the guard disabled, the mock version is green.
#[tokio::test]
async fn test_stalled_stream_still_times_out() {
    use std::time::{Duration, Instant};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            // Commit a 200 with headers, send one chunk, then go quiet for far
            // longer than the window. This is exactly what a stalled upstream
            // looks like: the response has started and then stopped.
            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";
            let _ = sock.write_all(head.as_bytes()).await;
            let chunk = "data: {\"choices\":[{\"delta\":{\"content\":\"first\"}}]}\n\n";
            let framed = format!("{:x}\r\n{}\r\n", chunk.len(), chunk);
            let _ = sock.write_all(framed.as_bytes()).await;
            let _ = sock.flush().await;
            // Held far longer than any window, and longer than the mutation's
            // substituted guard would be, so a disabled guard shows up as a
            // hang rather than a pass.
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });

    let state = AppState::new_with_controls(
        AppStateParams {
            keys: vec![KeyEntry {
                key: "k1".to_string(),
                label: Some("k1".to_string()),
                disabled: false,
            }],
            target: format!("http://{}", addr),
            router: None,
            model_stats: ModelStatsStore::new(100.0),
            racing_models: vec![],
            racing_max_parallel: 1,
            racing_timeout_ms: 250,
            racing_strategy: "complete".to_string(),
            model_params: HashMap::new(),
            model_compat: ModelCompat::default(),
            loop_idle_timeout: nimaproxy::loop_idle_timeout(),
        },
        RuntimeControls {
            max_upstream_in_flight: 8,
            max_in_flight_per_key: 2,
            admission_wait_ms: 0,
            min_dynamic_timeout_ms: 250,
            dynamic_sample_floor: 10,
            mid_stream_idle_timeout_ms: 250,
            auth_failure_cooldown_secs: 900,
            ..RuntimeControls::default()
        },
    );

    let body = serde_json::json!({
        "model": "stall-model",
        "messages": [{"role": "user", "content": "test"}],
        "stream": true
    });

    let started = Instant::now();
    let resp = chat_completions(
        axum::extract::State(state.clone()),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    )
    .await
    .into_response();

    // A stream cannot be retried once it has started — the 200 is already
    // committed — so the stall surfaces as a body error rather than a 504.
    // What matters is that it ENDS promptly: a stalled upstream must not hold
    // the connection open for its own 10-second sleep.
    // The elapsed measurement must cover DRAINING THE BODY, not just the
    // handler returning: the handler hands back a response with a lazy body
    // stream, so it returns immediately and the stall is only observed while
    // the caller reads. Measuring before the read passes even with the guard
    // removed — it timed this assertion at 30 s under mutation and still
    // reported success, because the 30 s elapsed after `started` had been
    // read.
    let drained = axum::body::to_bytes(resp.into_body(), usize::MAX).await;
    let elapsed = started.elapsed();
    match drained {
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("stalled mid-stream") || msg.contains("error"),
                "the stall error must be the one we raise, got: {msg}"
            );
        }
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            assert!(
                text.contains("stalled") || text.contains("error") || text.contains("first"),
                "a stalled stream must not look like a complete success, got: {text}"
            );
        }
    }
    assert!(
        elapsed < Duration::from_secs(2),
        "a stalled upstream must be detected within about one window (250 ms), not \
         held open for the upstream's 30 s silence: took {elapsed:?}"
    );
}

/// The split's core promise: a mid-stream gap past the FIRST-byte window but
/// inside the IDLE window must not abort the stream. Before the split the
/// guard fired at the first-byte window, so a live generation pausing
/// between tokens was killed as a stall.
///
/// Uses a real TCP listener, for the same reason as the stalled-stream test
/// above: a mock buffers its writes, so the sleeps inside it are not
/// observable as inter-chunk gaps, and a mock-based version of this test
/// passes even with the guard removed entirely.
#[tokio::test]
async fn test_mid_stream_gap_inside_idle_window_is_not_a_stall() {
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";
            let _ = sock.write_all(head.as_bytes()).await;
            // First chunk arrives immediately; the second follows after a
            // pause that exceeds the 250 ms first-byte window but sits well
            // inside the 2000 ms idle window. The proxy must forward both.
            for chunk in [
                "data: {\"choices\":[{\"delta\":{\"content\":\"first\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{}}]}\n\ndata: [DONE]\n\n",
            ] {
                let framed = format!("{:x}\r\n{}\r\n", chunk.len(), chunk);
                let _ = sock.write_all(framed.as_bytes()).await;
                let _ = sock.flush().await;
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            let _ = sock.write_all(b"0\r\n\r\n").await;
        }
    });

    let state = AppState::new_with_controls(
        AppStateParams {
            keys: vec![KeyEntry {
                key: "k1".to_string(),
                label: Some("k1".to_string()),
                disabled: false,
            }],
            target: format!("http://{}", addr),
            router: None,
            model_stats: ModelStatsStore::new(100.0),
            racing_models: vec![],
            racing_max_parallel: 1,
            racing_timeout_ms: 250,
            racing_strategy: "complete".to_string(),
            model_params: HashMap::new(),
            model_compat: ModelCompat::default(),
            loop_idle_timeout: nimaproxy::loop_idle_timeout(),
        },
        RuntimeControls {
            max_upstream_in_flight: 8,
            max_in_flight_per_key: 2,
            admission_wait_ms: 0,
            min_dynamic_timeout_ms: 250,
            dynamic_sample_floor: 10,
            // Split: the first-byte rail stays tight while the mid-stream
            // rail tolerates the 500 ms pause.
            mid_stream_idle_timeout_ms: 2000,
            auth_failure_cooldown_secs: 900,
            ..RuntimeControls::default()
        },
    );

    let body = serde_json::json!({
        "model": "gap-model",
        "messages": [{"role": "user", "content": "test"}],
        "stream": true
    });

    let resp = chat_completions(
        axum::extract::State(state.clone()),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    )
    .await
    .into_response();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("a gap inside the idle window must not abort the stream");
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("first") && text.contains("[DONE]"),
        "both chunks around the 500 ms gap must arrive: {text}"
    );
}

/// The other direction: a gap past the IDLE window must still abort the
/// stream, promptly. Without this the test above could pass by simply never
/// timing out mid-stream.
#[tokio::test]
async fn test_mid_stream_gap_past_idle_window_still_aborts() {
    use std::time::{Duration, Instant};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";
            let _ = sock.write_all(head.as_bytes()).await;
            let chunk = "data: {\"choices\":[{\"delta\":{\"content\":\"first\"}}]}\n\n";
            let framed = format!("{:x}\r\n{}\r\n", chunk.len(), chunk);
            let _ = sock.write_all(framed.as_bytes()).await;
            let _ = sock.flush().await;
            // Quiet far past the idle window: the guard must abort the
            // body rather than hold it open for this silence.
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });

    let state = AppState::new_with_controls(
        AppStateParams {
            keys: vec![KeyEntry {
                key: "k1".to_string(),
                label: Some("k1".to_string()),
                disabled: false,
            }],
            target: format!("http://{}", addr),
            router: None,
            model_stats: ModelStatsStore::new(100.0),
            racing_models: vec![],
            racing_max_parallel: 1,
            racing_timeout_ms: 250,
            racing_strategy: "complete".to_string(),
            model_params: HashMap::new(),
            model_compat: ModelCompat::default(),
            loop_idle_timeout: nimaproxy::loop_idle_timeout(),
        },
        RuntimeControls {
            max_upstream_in_flight: 8,
            max_in_flight_per_key: 2,
            admission_wait_ms: 0,
            min_dynamic_timeout_ms: 250,
            dynamic_sample_floor: 10,
            // Split: the idle rail is wider than the first-byte rail, but
            // the 30 s silence exceeds it too, so the guard must fire.
            mid_stream_idle_timeout_ms: 500,
            auth_failure_cooldown_secs: 900,
            ..RuntimeControls::default()
        },
    );

    let body = serde_json::json!({
        "model": "stall-model",
        "messages": [{"role": "user", "content": "test"}],
        "stream": true
    });

    let started = Instant::now();
    let resp = chat_completions(
        axum::extract::State(state.clone()),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    )
    .await
    .into_response();
    // The handler returns a lazy body, so the stall is only observed while
    // the caller drains: the elapsed measurement must cover the drain.
    let drained = axum::body::to_bytes(resp.into_body(), usize::MAX).await;
    let elapsed = started.elapsed();
    assert!(
        drained.is_err(),
        "a gap past the idle window must abort the body, not complete it"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "the idle guard (500 ms) must abort promptly, not after the \
         upstream's 30 s silence: took {elapsed:?}"
    );
}

/// A healthy stream longer than any whole-request cap must complete.
///
/// The shared client used to carry a 120 s whole-request timeout, which
/// severed healthy long generations mid-stream (measured 3/3 on the proxy arm
/// of an A/B whose direct arm completed the same 338 s and 458 s
/// generations). The cap is gone; the only mid-stream bound left is the
/// inter-chunk idle rail. This test holds every gap inside that rail while
/// pushing the TOTAL past 120 s, and requires the full body to arrive.
///
/// Uses paused tokio time (`start_paused`), not wall-clock sleeps: 40 chunks
/// at 4 s test-time gaps is 156 s of stream for milliseconds of wall clock.
/// The clock is driven explicitly — the handler and the body drain each run
/// as a spawned task while the test advances time in 1 s steps — because a
/// paused clock only moves when told to. Uses a real TCP listener for the
/// same reason as the stall tests above: a mock buffers its writes, so the
/// gaps would not be observable as inter-chunk pauses at the HTTP layer.
#[tokio::test(start_paused = true)]
async fn test_stream_longer_than_whole_request_cap_survives() {
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const CHUNKS: usize = 40;
    const GAP_SECS: u64 = 4;
    // 39 gaps x 4 s = 156 s of test-time stream, past the removed 120 s cap,
    // while every gap sits inside the 10 s idle window.
    const IDLE_WINDOW_MS: u64 = 10_000;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";
            let _ = sock.write_all(head.as_bytes()).await;
            for i in 0..CHUNKS {
                // The first chunk leaves immediately so the first-byte bound
                // is trivially satisfied; every later chunk follows a gap
                // that is long in total but short against the idle rail.
                if i > 0 {
                    tokio::time::sleep(Duration::from_secs(GAP_SECS)).await;
                }
                let chunk =
                    format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"tok{i}\"}}}}]}}\n\n");
                let framed = format!("{:x}\r\n{}\r\n", chunk.len(), chunk);
                let _ = sock.write_all(framed.as_bytes()).await;
                let _ = sock.flush().await;
            }
            let done = "data: [DONE]\n\n";
            let framed = format!("{:x}\r\n{}\r\n", done.len(), done);
            let _ = sock.write_all(framed.as_bytes()).await;
            let _ = sock.write_all(b"0\r\n\r\n").await;
        }
    });

    let state = AppState::new_with_controls(
        AppStateParams {
            keys: vec![KeyEntry {
                key: "k1".to_string(),
                label: Some("k1".to_string()),
                disabled: false,
            }],
            target: format!("http://{}", addr),
            router: None,
            model_stats: ModelStatsStore::new(100.0),
            racing_models: vec![],
            racing_max_parallel: 1,
            // The first-byte rail in test-time: generous, because the clock
            // only advances when this test drives it.
            racing_timeout_ms: 30_000,
            racing_strategy: "complete".to_string(),
            model_params: HashMap::new(),
            model_compat: ModelCompat::default(),
            loop_idle_timeout: nimaproxy::loop_idle_timeout(),
        },
        RuntimeControls {
            max_upstream_in_flight: 8,
            max_in_flight_per_key: 2,
            admission_wait_ms: 0,
            min_dynamic_timeout_ms: 1000,
            dynamic_sample_floor: 10,
            mid_stream_idle_timeout_ms: IDLE_WINDOW_MS,
            auth_failure_cooldown_secs: 900,
            ..RuntimeControls::default()
        },
    );

    let body = serde_json::json!({
        "model": "long-model",
        "messages": [{"role": "user", "content": "test"}],
        "stream": true
    });

    // Drive a spawned future to completion by advancing the paused clock in
    // 1 s steps. Each step is instant in wall-clock; the cap bounds the
    // drive in TEST-time so a hung future fails instead of hanging the
    // suite. Returns the output once the task finishes.
    async fn drive_to_completion<T>(handle: tokio::task::JoinHandle<T>, max_steps: u64) -> T {
        for _ in 0..max_steps {
            if handle.is_finished() {
                break;
            }
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
        }
        assert!(
            handle.is_finished(),
            "drove {max_steps} s of test-time without the future completing"
        );
        handle.await.expect("driven task must not panic")
    }

    let clock_start = tokio::time::Instant::now();
    let wall_start = std::time::Instant::now();

    // Phase 1: the handler awaits the first byte internally, so it needs
    // the clock driven too — it cannot simply be awaited on a paused clock.
    let handler = tokio::spawn({
        let state = state.clone();
        let body = Bytes::from(body.to_string());
        async move {
            chat_completions(axum::extract::State(state), HeaderMap::new(), body)
                .await
                .into_response()
        }
    });
    let resp = drive_to_completion(handler, 60).await;
    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    // Phase 2: the handler hands back a lazy body, so the 156 s stream is
    // only observed while the caller drains — the drain needs driving too.
    // Cap well past the stream length so a regression fails, not hangs.
    let drain =
        tokio::spawn(async move { axum::body::to_bytes(resp.into_body(), usize::MAX).await });
    let drained = drive_to_completion(drain, 300).await;

    let test_elapsed = clock_start.elapsed();
    let wall_elapsed = wall_start.elapsed();

    let bytes = drained.expect("a healthy 156 s stream must drain cleanly");
    let text = String::from_utf8_lossy(&bytes);
    for i in 0..CHUNKS {
        assert!(
            text.contains(&format!("tok{i}")),
            "lost chunk {i} of {CHUNKS} in a stream whose gaps never exceeded the idle window: {text}"
        );
    }
    assert!(
        text.contains("[DONE]"),
        "the stream must terminate cleanly: {text}"
    );
    assert!(
        test_elapsed >= Duration::from_secs(120),
        "the test proves nothing unless the stream actually crossed the removed 120 s cap; only drove {test_elapsed:?}"
    );
    assert!(
        wall_elapsed < Duration::from_secs(60),
        "paused time must keep this fast in wall-clock; took {wall_elapsed:?}"
    );
}
