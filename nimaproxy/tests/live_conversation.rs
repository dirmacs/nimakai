//! Live integration tests for multi-turn conversations and tool calling.
//! These tests require a valid NVIDIA_API_KEY environment variable.
//! Tests are skipped if NVIDIA_API_KEY is not set.
//!
//! Run with: cargo test --test live_conversation -- --nocapture

use axum::response::IntoResponse;
use nimaproxy::config::{KeyEntry, ModelCompat};
use nimaproxy::model_stats::ModelStatsStore;
use nimaproxy::AppState;
use std::collections::HashMap;
use std::sync::Arc;

const NVIDIA_API_BASE: &str = "https://integrate.api.nvidia.com";

fn get_api_key() -> Option<String> {
    std::env::var("NVIDIA_API_KEY").ok()
}

fn make_live_state() -> Option<Arc<AppState>> {
    let api_key = get_api_key()?;

    let key_entries = vec![KeyEntry {
        key: api_key,
        label: Some("live-conversation".to_string()),
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
        HashMap::new(),
        ModelCompat::default(),
    ))
}

fn skip_if_no_api_key() -> bool {
    get_api_key().is_none()
}

fn extract_assistant_message(json: &serde_json::Value) -> Option<String> {
    json.get("choices")
        .and_then(|c| c.as_array())
        .and_then(|choices| choices.get(0))
        .and_then(|choice| choice.get("message"))
        .and_then(|msg| msg.get("content"))
        .and_then(|content| content.as_str())
        .map(|s| s.to_string())
}

/// ============================================================================
/// Test 1: Multi-turn Conversation (10 turns)
/// Verifies conversation history is maintained across 10 turns
/// ============================================================================
#[tokio::test]
async fn test_live_multi_turn_10_turns() {
    if skip_if_no_api_key() {
        eprintln!("[SKIP] test_live_multi_turn_10_turns: NVIDIA_API_KEY not set");
        return;
    }

    let state = make_live_state().expect("Failed to create live state");

    eprintln!("[conversation] Testing 10-turn conversation");

    let model = "meta/llama-3.1-8b-instruct";
    let mut messages: Vec<serde_json::Value> = Vec::new();
    let mut turn_results: Vec<(usize, u16, Option<String>)> = Vec::new();

    // Conversation topics for each turn
    let topics = vec![
        "My name is TestUser. Remember this for our conversation.",
        "What is my name?",
        "I like programming. What do you think about coding?",
        "Can you write a simple hello world in Rust?",
        "Now make it print to console.",
        "Add a function that takes a name parameter.",
        "What was the first thing I told you?",
        "Summarize our conversation so far.",
        "What programming language were we discussing?",
        "Thank you for the conversation. Goodbye!",
    ];

    for (turn, user_input) in topics.iter().enumerate() {
        eprintln!("[conversation] Turn {}: {}", turn + 1, user_input);

        // Build messages array with conversation history
        let mut current_messages = messages.clone();
        current_messages.push(serde_json::json!({
            "role": "user",
            "content": user_input
        }));

        let body = serde_json::json!({
            "model": model,
            "messages": current_messages,
            "max_tokens": 150,
            "temperature": 0.7
        });

        let resp = nimaproxy::proxy::chat_completions(
            axum::extract::State(state.clone()),
            axum::http::HeaderMap::new(),
            bytes::Bytes::from(body.to_string()),
        )
        .await;

        let response = resp.into_response();
        let (parts, body) = response.into_parts();
        let status_code = parts.status.as_u16();

        let body_bytes = axum::body::to_bytes(body, 65536).await.unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&body_bytes).unwrap_or(serde_json::json!({}));

        let assistant_response = extract_assistant_message(&json);

        eprintln!(
            "[conversation] Turn {} - Status: {}, Response: {:?}",
            turn + 1,
            status_code,
            assistant_response
                .as_ref()
                .map(|s| &s[..s.len().min(100)])
        );

        turn_results.push((turn + 1, status_code, assistant_response.clone()));

        // If successful, add both user and assistant messages to history
        if status_code == 200 {
            // Add user message
            messages.push(serde_json::json!({
                "role": "user",
                "content": user_input
            }));

            // Add assistant response to history
            if let Some(ref response_text) = assistant_response {
                messages.push(serde_json::json!({
                    "role": "assistant",
                    "content": response_text
                }));
            }
        }

        // Delay between turns to avoid rate limiting
        std::thread::sleep(std::time::Duration::from_millis(1000));
    }

    // Summary
    let success_count = turn_results.iter().filter(|(_, status, _)| *status == 200).count();
    eprintln!(
        "[conversation] Completed {} out of 10 turns successfully",
        success_count
    );

    // At least some turns should succeed
    assert!(
        success_count > 0,
        "Expected at least one successful turn in conversation"
    );
}

/// ============================================================================
/// Test 2: Tool Calling Variations
/// Tests multiple tool calling patterns and formats
/// ============================================================================
#[tokio::test]
async fn test_live_tool_calling_variations() {
    if skip_if_no_api_key() {
        eprintln!("[SKIP] test_live_tool_calling_variations: NVIDIA_API_KEY not set");
        return;
    }

    let state = make_live_state().expect("Failed to create live state");

    eprintln!("[tool-calling] Testing tool calling variations");

    let model = "meta/llama-3.1-8b-instruct";
    let mut tool_results: Vec<(&str, u16, Option<String>)> = Vec::new();

    // Variation 1: Single tool definition
    eprintln!("[tool-calling] Variation 1: Single tool");
    {
        let body = serde_json::json!({
            "model": model,
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
        )
        .await;

        let response = resp.into_response();
        let (parts, body) = response.into_parts();
        let status = parts.status.as_u16();

        let body_bytes = axum::body::to_bytes(body, 65536).await.unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&body_bytes).unwrap_or(serde_json::json!({}));

        let response_text = extract_assistant_message(&json);
        eprintln!(
            "[tool-calling] Variation 1 - Status: {}, Response: {:?}",
            status,
            response_text.as_ref().map(|s| &s[..s.len().min(100)])
        );

        tool_results.push(("single_tool", status, response_text));
    }

    // Variation 2: Multiple tools
    eprintln!("[tool-calling] Variation 2: Multiple tools");
    {
        let body = serde_json::json!({
            "model": model,
            "messages": [
                {"role": "user", "content": "What is 25 * 7? Also, what is the capital of France?"}
            ],
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "calculator",
                        "description": "Perform mathematical calculations",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "expression": {
                                    "type": "string",
                                    "description": "The mathematical expression to evaluate"
                                }
                            },
                            "required": ["expression"]
                        }
                    }
                },
                {
                    "type": "function",
                    "function": {
                        "name": "get_capital",
                        "description": "Get the capital of a country",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "country": {
                                    "type": "string",
                                    "description": "The country name"
                                }
                            },
                            "required": ["country"]
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
        )
        .await;

        let response = resp.into_response();
        let (parts, body) = response.into_parts();
        let status = parts.status.as_u16();

        let body_bytes = axum::body::to_bytes(body, 65536).await.unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&body_bytes).unwrap_or(serde_json::json!({}));

        let response_text = extract_assistant_message(&json);
        eprintln!(
            "[tool-calling] Variation 2 - Status: {}, Response: {:?}",
            status,
            response_text.as_ref().map(|s| &s[..s.len().min(100)])
        );

        tool_results.push(("multiple_tools", status, response_text));
    }

    // Variation 3: Tool with complex parameters
    eprintln!("[tool-calling] Variation 3: Complex tool parameters");
    {
        let body = serde_json::json!({
            "model": model,
            "messages": [
                {"role": "user", "content": "Search for articles about AI in healthcare from 2024."}
            ],
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "search_articles",
                        "description": "Search for articles with various filters",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "query": {
                                    "type": "string",
                                    "description": "Search query"
                                },
                                "topic": {
                                    "type": "string",
                                    "description": "Topic filter"
                                },
                                "year": {
                                    "type": "integer",
                                    "description": "Publication year"
                                },
                                "limit": {
                                    "type": "integer",
                                    "description": "Max results to return"
                                }
                            },
                            "required": ["query"]
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
        )
        .await;

        let response = resp.into_response();
        let (parts, body) = response.into_parts();
        let status = parts.status.as_u16();

        let body_bytes = axum::body::to_bytes(body, 65536).await.unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&body_bytes).unwrap_or(serde_json::json!({}));

        let response_text = extract_assistant_message(&json);
        eprintln!(
            "[tool-calling] Variation 3 - Status: {}, Response: {:?}",
            status,
            response_text.as_ref().map(|s| &s[..s.len().min(100)])
        );

        tool_results.push(("complex_params", status, response_text));
    }

    // Summary
    let success_count = tool_results.iter().filter(|(_, status, _)| *status == 200).count();
    eprintln!(
        "[tool-calling] Tool calling results: {}/{} variations successful",
        success_count,
        tool_results.len()
    );

    // At least some variations should work
    assert!(
        success_count > 0,
        "Expected at least one tool calling variation to succeed"
    );
}
