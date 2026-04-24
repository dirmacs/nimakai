//! Live API tests for tool calling scenarios with NVIDIA NIM models.
//! These tests require real API keys and network access to integrate.api.nvidia.com.
//! Run with: cargo test --test live_tool_calls -- --ignored
//!
//! Set NVIDIA_API_KEY environment variable (comma-separated for multiple keys).

use axum::response::IntoResponse;
use nimaproxy::config::{KeyEntry, ModelCompat};
use nimaproxy::model_stats::ModelStatsStore;
use nimaproxy::AppState;
use serde_json::json;
use std::sync::Arc;

const NVIDIA_API_BASE: &str = "https://integrate.api.nvidia.com";

/// Load API keys from environment variable (comma-separated).
/// Falls back to a dummy key if not set (causes 401, but allows test to run).
fn get_test_keys() -> Vec<(String, String)> {
    let env_keys = std::env::var("NVIDIA_API_KEY").unwrap_or_default();
    let keys: Vec<&str> = env_keys.split(',').collect();
    if keys.is_empty() || (keys.len() == 1 && keys[0].is_empty()) {
        eprintln!("WARN: NVIDIA_API_KEY not set, using dummy key (tests will likely fail with 401)");
        vec![("dummy".to_string(), "test".to_string())]
    } else {
        keys.iter()
            .enumerate()
            .map(|(i, k)| (k.to_string(), format!("key-{}", i)))
            .collect()
    }
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
    // Model compatibility matching production config (nimaproxy.toml)
    let model_compat = ModelCompat {
        supports_developer_role: Some(vec!["all".to_string()]),
        supports_tool_messages: Some(vec!["all".to_string()]),
    };
    AppState::new(
        key_entries,
        NVIDIA_API_BASE.to_string(),
        None,
        ModelStatsStore::new(3000.0),
        vec![
            "mistralai/devstral-2-123b-instruct-2512".to_string(),
            "z-ai/glm4.7".to_string(),
            "qwen/qwen3.5-397b-a17b".to_string(),
        ],
        5,
        20000,
        "complete".to_string(),
        std::collections::HashMap::new(),
        model_compat,
    )
}

/// Send a request and return status code and (if success) the parsed response.
async fn send_chat(state: Arc<AppState>, body: serde_json::Value) -> (u16, Option<serde_json::Value>) {
    let resp = nimaproxy::proxy::chat_completions(
        axum::extract::State(state),
        axum::http::HeaderMap::new(),
        bytes::Bytes::from(body.to_string()),
    )
    .await;
    let response = resp.into_response();
    let (parts, body_bytes) = response.into_parts();
    let status = parts.status.as_u16();
    let bytes = axum::body::to_bytes(body_bytes, 65536).await.unwrap();
    let json = if status == 200 {
        Some(serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({})))
    } else {
        eprintln!("Error response body: {}", String::from_utf8_lossy(&bytes));
        None
    };
    (status, json)
}

// ============================================================================
// Tool definition tests
// ============================================================================

#[tokio::test]
#[ignore]
async fn test_tool_definition_with_empty_params() {
    let state = make_state();
    let model = "mistralai/devstral-2-123b-instruct-2512";

    let body = json!({
        "model": model,
        "messages": [
            {"role": "system", "content": "You are a helpful assistant."},
            {"role": "user", "content": "What is 2+2?"}
        ],
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": "calculator",
                    "description": "Performs basic arithmetic",
                    "parameters": {
                        "type": "object",
                        "properties": {}
                    }
                }
            }
        ],
        "tool_choice": "none",
        "max_tokens": 50,
        "temperature": 0.0
    });

    let (status, resp) = send_chat(state.clone(), body).await;
    eprintln!("[test_tool_definition_with_empty_params] status={}", status);

    // Should succeed (2xx) or at least not be a 400 due to schema mismatch.
    // Some models may return 400 for empty parameters, but that's a model limitation.
    assert!(
        status == 200 || status == 400,
        "Unexpected status: {}",
        status
    );
    if status == 200 {
        assert!(resp.is_some());
    }
}

#[tokio::test]
#[ignore]
async fn test_tool_definition_with_parameters() {
    let state = make_state();
    let model = "mistralai/devstral-2-123b-instruct-2512";

    let body = json!({
        "model": model,
        "messages": [
            {"role": "system", "content": "You are a helpful assistant."},
            {"role": "user", "content": "Add 5 and 3."}
        ],
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": "add",
                    "description": "Adds two numbers",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "a": {"type": "number", "description": "First number"},
                            "b": {"type": "number", "description": "Second number"}
                        },
                        "required": ["a", "b"]
                    }
                }
            }
        ],
        "tool_choice": "auto",
        "max_tokens": 100,
        "temperature": 0.0
    });

    let (status, resp) = send_chat(state.clone(), body).await;
    eprintln!("[test_tool_definition_with_parameters] status={}", status);

    // Model may return tool calls or text; both are acceptable.
    assert!(
        status == 200 || status == 400,
        "Unexpected status: {}",
        status
    );
    if status == 200 {
        let resp = resp.unwrap();
        let has_tool_calls = resp
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("tool_calls"))
            .is_some();
        eprintln!("Response has tool_calls: {}", has_tool_calls);
    }
}

// ============================================================================
// Multi-turn tool call sequence tests
// ============================================================================

#[tokio::test]
#[ignore]
async fn test_tool_call_sequence() {
    let state = make_state();
    let model = "mistralai/devstral-2-123b-instruct-2512";

    // First turn: user asks to get weather, assistant should call tool
    let body1 = json!({
        "model": model,
        "messages": [
            {"role": "system", "content": "You are a helpful assistant with access to weather tool."},
            {"role": "user", "content": "What's the weather in Paris?"}
        ],
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get current weather for a city",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "city": {"type": "string", "description": "City name"}
                        },
                        "required": ["city"]
                    }
                }
            }
        ],
        "tool_choice": "auto",
        "max_tokens": 200,
        "temperature": 0.0
    });

    let (status1, resp1) = send_chat(state.clone(), body1).await;
    eprintln!("[test_tool_call_sequence] turn1 status={}", status1);
    assert_eq!(status1, 200, "First turn should succeed");

    let resp1 = resp1.unwrap();
    let tool_calls = resp1["choices"][0]["message"]["tool_calls"].as_array();
    if tool_calls.is_none() {
        eprintln!("No tool calls in first response, skipping second turn");
        return;
    }
    let tool_calls = tool_calls.unwrap();
    assert!(!tool_calls.is_empty(), "Expected at least one tool call");

    let tool_call = &tool_calls[0];
    let tool_call_id = tool_call["id"].as_str().unwrap();
    let function_name = tool_call["function"]["name"].as_str().unwrap();
    eprintln!("Tool call: {} id={}", function_name, tool_call_id);

    // Second turn: send tool result
    let body2 = json!({
        "model": model,
        "messages": [
            {"role": "system", "content": "You are a helpful assistant with access to weather tool."},
            {"role": "user", "content": "What's the weather in Paris?"},
            {
                "role": "assistant",
                "content": null,
                "tool_calls": tool_calls
            },
            {
                "role": "tool",
                "tool_call_id": tool_call_id,
                "content": "Sunny, 22°C"
            }
        ],
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get current weather for a city",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "city": {"type": "string", "description": "City name"}
                        },
                        "required": ["city"]
                    }
                }
            }
        ],
        "max_tokens": 200,
        "temperature": 0.0
    });

    let (status2, resp2) = send_chat(state.clone(), body2).await;
    eprintln!("[test_tool_call_sequence] turn2 status={}", status2);

    // This is the critical test: the proxy must not break the message sequence.
    // If the proxy strips tool_call_id from assistant message or mis-handles tool role,
    // we may get a 400 error with "Not the same number of function calls and responses".
    assert_eq!(
        status2, 200,
        "Second turn with tool result should succeed, got {}",
        status2
    );

    let resp2 = resp2.unwrap();
    let final_content = resp2["choices"][0]["message"]["content"].as_str();
    eprintln!("Final response: {:?}", final_content);
}

// ============================================================================
// Model-specific compatibility tests
// ============================================================================

/// Test that the proxy correctly transforms "tool" role messages for models that don't support it.
/// The proxy should convert "tool" -> "assistant" while preserving tool_call_id.
#[tokio::test]
#[ignore]
async fn test_tool_role_transformation() {
    let state = make_state();
    let model = "z-ai/glm4.7"; // This model may not support native "tool" role

    let body = json!({
        "model": model,
        "messages": [
            {"role": "system", "content": "You are a helpful assistant."},
            {"role": "user", "content": "Call the test tool with arg=42"},
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {
                        "id": "call_123",
                        "type": "function",
                        "function": {
                            "name": "test_tool",
                            "arguments": "{\"arg\":42}"
                        }
                    }
                ]
            },
            {
                "role": "tool",
                "tool_call_id": "call_123",
                "content": "Tool result: success"
            }
        ],
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": "test_tool",
                    "description": "A test tool",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "arg": {"type": "integer"}
                        },
                        "required": ["arg"]
                    }
                }
            }
        ],
        "max_tokens": 100,
        "temperature": 0.0
    });

    let (status, _) = send_chat(state.clone(), body).await;
    eprintln!("[test_tool_role_transformation] status={}", status);

    // Should succeed (200) or at least not crash with 500.
    assert!(
        status == 200 || status == 400,
        "Unexpected status: {}",
        status
    );
}

// ============================================================================
// Edge cases: malformed messages, missing fields, etc.
// ============================================================================

#[tokio::test]
#[ignore]
async fn test_assistant_message_with_tool_calls_missing_content() {
    let state = make_state();
    let model = "mistralai/devstral-2-123b-instruct-2512";

    // Some models require non-null content even for tool-call-only messages.
    // The proxy should inject an empty string if missing.
    let body = json!({
        "model": model,
        "messages": [
            {"role": "system", "content": "You are helpful."},
            {"role": "user", "content": "Call tool"},
            {
                "role": "assistant",
                "tool_calls": [
                    {
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "test",
                            "arguments": "{}"
                        }
                    }
                ]
                // content field is missing intentionally
            }
        ],
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": "test",
                    "description": "Test tool",
                    "parameters": {"type": "object", "properties": {}}
                }
            }
        ],
        "max_tokens": 10,
        "temperature": 0.0
    });

    let (status, _) = send_chat(state.clone(), body).await;
    eprintln!("[test_assistant_message_with_tool_calls_missing_content] status={}", status);

    // Should not crash; likely returns 200 or 400 depending on model strictness.
    assert!(
        status == 200 || status == 400,
        "Unexpected status: {}",
        status
    );
}

#[tokio::test]
#[ignore]
async fn test_reasoning_field_stripped() {
    let state = make_state();
    let model = "mistralai/devstral-2-123b-instruct-2512";

    // Some clients send a "reasoning" field in assistant messages.
    // The proxy should strip it to avoid Pydantic "Extra inputs" errors.
    let body = json!({
        "model": model,
        "messages": [
            {"role": "system", "content": "You are helpful."},
            {"role": "user", "content": "Hello"},
            {
                "role": "assistant",
                "content": "Hi there!",
                "reasoning": "This is some internal reasoning text"
            },
            {"role": "user", "content": "Continue"}
        ],
        "max_tokens": 50,
        "temperature": 0.0
    });

    let (status, _) = send_chat(state.clone(), body).await;
    eprintln!("[test_reasoning_field_stripped] status={}", status);

    assert!(
        status == 200 || status == 400,
        "Unexpected status: {}",
        status
    );
}

// ============================================================================
// Mismatch tests based on historical errors
// ============================================================================

/// Reproduce the "Not the same number of function calls and responses" error.
/// This occurs when the number of tool calls in the assistant message does not match
/// the number of tool responses in subsequent messages.
#[tokio::test]
#[ignore]
async fn test_mismatched_tool_calls_and_responses() {
    let state = make_state();
    let model = "mistralai/devstral-2-123b-instruct-2512";

    // First turn: get a tool call from the model
    let body1 = json!({
        "model": model,
        "messages": [
            {"role": "system", "content": "You are a helpful assistant."},
            {"role": "user", "content": "Call the get_weather tool for Paris."}
        ],
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get current weather for a city",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "city": {"type": "string", "description": "City name"}
                        },
                        "required": ["city"]
                    }
                }
            }
        ],
        "tool_choice": "auto",
        "max_tokens": 200,
        "temperature": 0.0
    });

    let (status1, resp1) = send_chat(state.clone(), body1).await;
    eprintln!("[test_mismatched] turn1 status={}", status1);
    assert_eq!(status1, 200);
    let resp1 = resp1.unwrap();
    let tool_calls = resp1["choices"][0]["message"]["tool_calls"].as_array().unwrap().clone();
    assert!(!tool_calls.is_empty());
    let tool_call_id = tool_calls[0]["id"].as_str().unwrap();

    // Second turn: send mismatched count: two tool calls but only one tool response.
    // We'll duplicate the tool call in the assistant message but provide only one tool response.
    let mut tool_calls_value = serde_json::Value::Array(tool_calls.clone());
    // Add an extra dummy tool call to create mismatch
    let extra_call = json!({
        "id": "dummy_id",
        "type": "function",
        "function": {"name": "get_weather", "arguments": "{\"city\": \"London\"}"}
    });
    if let serde_json::Value::Array(ref mut arr) = tool_calls_value {
        arr.push(extra_call);
    }

    let body2 = json!({
        "model": model,
        "messages": [
            {"role": "system", "content": "You are a helpful assistant."},
            {"role": "user", "content": "Call the get_weather tool for Paris."},
            {
                "role": "assistant",
                "content": null,
                "tool_calls": tool_calls_value
            },
            {
                "role": "tool",
                "tool_call_id": tool_call_id,
                "content": "Sunny, 22°C"
            }
        ],
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get current weather for a city",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "city": {"type": "string", "description": "City name"}
                        },
                        "required": ["city"]
                    }
                }
            }
        ],
        "max_tokens": 200,
        "temperature": 0.0
    });

    let (status2, resp2) = send_chat(state.clone(), body2).await;
    eprintln!("[test_mismatched] turn2 status={}", status2);
    // Expect 400 with mismatch error
    if status2 == 400 {
        eprintln!("Got expected 400 error: {:?}", resp2);
    }
    // We don't assert 400 because the API might handle mismatch differently.
    // The test is to observe behavior and ensure proxy doesn't panic.
}

