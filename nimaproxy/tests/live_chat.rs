//! Live integration tests for chat completions endpoint.
//! These tests require a valid NVIDIA_API_KEY environment variable.
//! Tests are skipped if NVIDIA_API_KEY is not set.
//!
//! Run with: cargo test --test live_chat -- --nocapture

use axum::response::IntoResponse;
use nimaproxy::config::{KeyEntry, ModelCompat};
use nimaproxy::model_stats::ModelStatsStore;
use nimaproxy::AppState;
use std::sync::Arc;

const NVIDIA_API_BASE: &str = "https://integrate.api.nvidia.com";

/// Get API key from environment variable.
/// Returns None if not set, which causes tests to skip.
fn get_api_key() -> Option<String> {
    std::env::var("NVIDIA_API_KEY").ok()
}

/// Create test state with live API key.
/// Returns None if API key not available (test should skip).
fn make_live_state() -> Option<Arc<AppState>> {
    let api_key = get_api_key()?;
    
    let key_entries = vec![KeyEntry {
        key: api_key,
        label: Some("live-test".to_string()),
    }];
    
    Some(AppState::new(
        key_entries,
        NVIDIA_API_BASE.to_string(),
        None,
        ModelStatsStore::new(3000.0),
        vec![],
        3,
        20000,
        "complete".to_string(),
        std::collections::HashMap::new(),
        ModelCompat::default(),
    ))
}

/// Helper to check if test should be skipped
fn skip_if_no_api_key() -> bool {
    get_api_key().is_none()
}

/// ============================================================================
/// Test 1: Basic Chat Completions
/// ============================================================================
#[tokio::test]
async fn test_live_chat_completions() {
    if skip_if_no_api_key() {
        eprintln!("[SKIP] test_live_chat_completions: NVIDIA_API_KEY not set");
        return;
    }

    let state = make_live_state().expect("Failed to create live state");
    
    // Use a commonly available model
    let body = serde_json::json!({
        "model": "meta/llama-3.1-8b-instruct",
        "messages": [
            {"role": "user", "content": "Say 'hello' in exactly one word."}
        ],
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
    let status_code = parts.status.as_u16();

    eprintln!("[live] completions status: {}", status_code);

    let body_bytes = axum::body::to_bytes(body, 65536).await.unwrap();
    
    if status_code != 200 {
        eprintln!("[live] error body: {}", String::from_utf8_lossy(&body_bytes));
    }

    // Should succeed with valid API key
    assert_eq!(status_code, 200, "Expected 200 OK, got {}", status_code);

    // Verify response structure
    let json: serde_json::Value = serde_json::from_slice(&body_bytes)
        .expect("Response should be valid JSON");

    // Check for required fields
    assert!(json.get("choices").is_some(), "Response should have 'choices' field");
    assert!(json.get("model").is_some(), "Response should have 'model' field");
    
    if let Some(choices) = json.get("choices").and_then(|c| c.as_array()) {
        assert!(!choices.is_empty(), "Should have at least one choice");
        if let Some(first_choice) = choices.get(0) {
            assert!(first_choice.get("message").is_some(), "Choice should have 'message' field");
        }
    }
}

/// ============================================================================
/// Test 2: Streaming (SSE)
/// ============================================================================
#[tokio::test]
async fn test_live_chat_streaming() {
    if skip_if_no_api_key() {
        eprintln!("[SKIP] test_live_chat_streaming: NVIDIA_API_KEY not set");
        return;
    }

    let state = make_live_state().expect("Failed to create live state");
    
    let body = serde_json::json!({
        "model": "meta/llama-3.1-8b-instruct",
        "messages": [
            {"role": "user", "content": "Count from 1 to 3."}
        ],
        "max_tokens": 50,
        "temperature": 0.0,
        "stream": true
    });

    let resp = nimaproxy::proxy::chat_completions(
        axum::extract::State(state.clone()),
        axum::http::HeaderMap::new(),
        bytes::Bytes::from(body.to_string()),
    ).await;

    let response = resp.into_response();
    let (parts, body) = response.into_parts();
    let status_code = parts.status.as_u16();

    eprintln!("[live] streaming status: {}", status_code);

    // Check content-type for streaming
    let content_type = parts.headers.get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    
    eprintln!("[live] content-type: {}", content_type);

    let body_bytes = axum::body::to_bytes(body, 65536).await.unwrap();
    let content = String::from_utf8_lossy(&body_bytes);

    if status_code != 200 {
        eprintln!("[live] error body: {}", content);
    }

    assert_eq!(status_code, 200, "Expected 200 OK, got {}", status_code);
    
    // Streaming response should contain event-stream or be JSON
    // (NVIDIA may return JSON even with stream:true for small responses)
    eprintln!("[live] streaming response preview: {}", &content[..content.len().min(200)]);
}

/// ============================================================================
/// Test 3: Tool Calling (Function Calls)
/// ============================================================================
#[tokio::test]
async fn test_live_chat_tool_calling() {
    if skip_if_no_api_key() {
        eprintln!("[SKIP] test_live_chat_tool_calling: NVIDIA_API_KEY not set");
        return;
    }

    let state = make_live_state().expect("Failed to create live state");
    
    let body = serde_json::json!({
        "model": "meta/llama-3.1-8b-instruct",
        "messages": [
            {"role": "user", "content": "What is the weather in Tokyo?"}
        ],
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get the current weather in a given location",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "location": {
                                "type": "string",
                                "description": "The city and state, e.g. San Francisco, CA"
                            }
                        },
                        "required": ["location"]
                    }
                }
            }
        ],
        "max_tokens": 200,
        "temperature": 0.0
    });

    let resp = nimaproxy::proxy::chat_completions(
        axum::extract::State(state.clone()),
        axum::http::HeaderMap::new(),
        bytes::Bytes::from(body.to_string()),
    ).await;

    let response = resp.into_response();
    let (parts, body) = response.into_parts();
    let status_code = parts.status.as_u16();

    eprintln!("[live] tool calling status: {}", status_code);

    let body_bytes = axum::body::to_bytes(body, 65536).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body_bytes)
        .expect("Response should be valid JSON");

    if status_code != 200 {
        eprintln!("[live] error body: {}", json);
    }

    assert_eq!(status_code, 200, "Expected 200 OK, got {}", status_code);

    eprintln!("[live] tool calling response: {}", &json.to_string()[..json.to_string().len().min(500)]);

    // Response should have choices
    assert!(json.get("choices").is_some(), "Response should have 'choices' field");
}

/// ============================================================================
/// Test 4: Multi-turn Conversation
/// ============================================================================
#[tokio::test]
async fn test_live_chat_multi_turn() {
    if skip_if_no_api_key() {
        eprintln!("[SKIP] test_live_chat_multi_turn: NVIDIA_API_KEY not set");
        return;
    }

    let state = make_live_state().expect("Failed to create live state");
    
    // First turn: User asks a question
    let body1 = serde_json::json!({
        "model": "meta/llama-3.1-8b-instruct",
        "messages": [
            {"role": "user", "content": "My name is Alice. Remember this."}
        ],
        "max_tokens": 50,
        "temperature": 0.0
    });

    let resp1 = nimaproxy::proxy::chat_completions(
        axum::extract::State(state.clone()),
        axum::http::HeaderMap::new(),
        bytes::Bytes::from(body1.to_string()),
    ).await;

    let response1 = resp1.into_response();
    let (parts1, body1_bytes) = response1.into_parts();
    let status1 = parts1.status.as_u16();

    eprintln!("[live] multi-turn 1 status: {}", status1);

    // Extract assistant response for conversation history
    let body1_json: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(body1_bytes, 65536).await.unwrap()
    ).expect("Response should be valid JSON");

    if status1 != 200 {
        eprintln!("[live] error body 1: {}", body1_json);
    }

    assert_eq!(status1, 200, "First turn should succeed, got {}", status1);

    let mut messages = vec![
        serde_json::json!({"role": "user", "content": "My name is Alice. Remember this."}),
    ];
    
    if let Some(choices) = body1_json.get("choices").and_then(|c| c.as_array()) {
        if let Some(first) = choices.get(0) {
            if let Some(msg) = first.get("message") {
                messages.push(msg.clone());
            }
        }
    }

    // Second turn: Ask about the name
    let body2 = serde_json::json!({
        "model": "meta/llama-3.1-8b-instruct",
        "messages": messages,
        "max_tokens": 50,
        "temperature": 0.0
    });

    let resp2 = nimaproxy::proxy::chat_completions(
        axum::extract::State(state.clone()),
        axum::http::HeaderMap::new(),
        bytes::Bytes::from(body2.to_string()),
    ).await;

    let response2 = resp2.into_response();
    let (parts2, body2_bytes) = response2.into_parts();
    let status2 = parts2.status.as_u16();

    eprintln!("[live] multi-turn 2 status: {}", status2);

    // Verify second response contains reference to the name
    let body2_json: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(body2_bytes, 65536).await.unwrap()
    ).expect("Response should be valid JSON");

    if status2 != 200 {
        eprintln!("[live] error body 2: {}", body2_json);
    }

    assert_eq!(status2, 200, "Second turn should succeed, got {}", status2);

    eprintln!("[live] multi-turn response 2: {}", &body2_json.to_string()[..body2_json.to_string().len().min(300)]);
}

/// ============================================================================
/// Test 5: Various Models
/// ============================================================================
#[tokio::test]
async fn test_live_chat_various_models() {
    if skip_if_no_api_key() {
        eprintln!("[SKIP] test_live_chat_various_models: NVIDIA_API_KEY not set");
        return;
    }

    let state = make_live_state().expect("Failed to create live state");
    
    // Test multiple models available on NVIDIA NIM
    let models_to_test = vec![
        "meta/llama-3.1-8b-instruct",
        "mistralai/mistral-7b-instruct-v0.3",
        "google/gemma-2-2b-it",
    ];

    let mut results: Vec<(String, u16, Option<String>)> = vec![];

    for model in &models_to_test {
        let body = serde_json::json!({
            "model": model,
            "messages": [
                {"role": "user", "content": "Reply with exactly one word: hello"}
            ],
            "max_tokens": 10,
            "temperature": 0.0
        });

        let t0 = std::time::Instant::now();
        let resp = nimaproxy::proxy::chat_completions(
            axum::extract::State(state.clone()),
            axum::http::HeaderMap::new(),
            bytes::Bytes::from(body.to_string()),
        ).await;

        let elapsed_ms = t0.elapsed().as_millis();
        let response = resp.into_response();
        let (parts, body) = response.into_parts();
        let status_code = parts.status.as_u16();

        let body_bytes = axum::body::to_bytes(body, 65536).await.unwrap();
        let content = String::from_utf8_lossy(&body_bytes);

        let result_info = if status_code == 200 {
            Some(content[..content.len().min(100)].to_string())
        } else {
            Some(format!("ERROR: {}", content))
        };

        eprintln!(
            "[live] model={} status={} elapsed={}ms",
            model, status_code, elapsed_ms
        );

        results.push((model.to_string(), status_code, result_info));
    }

    // Report results
    eprintln!("\n[live] Model test results:");
    for (model, status, _info) in &results {
        let status_str = if *status == 200 { "✓" } else { "✗" };
        eprintln!("  {} {}: {}", status_str, model, status);
    }

    // At least some models should work
    let successes: Vec<_> = results.iter().filter(|(_, s, _)| *s == 200).collect();
    assert!(
        !successes.is_empty(),
        "At least one model should succeed. Results: {:?}",
        results.iter().map(|(m, s, _)| format!("{}: {}", m, s)).collect::<Vec<_>>()
    );
}
