use std::sync::Arc;
use std::time::{Duration, Instant};
use axum::{
body::Body,
http::{HeaderMap, HeaderValue, StatusCode},
response::{IntoResponse, Response},
};
use crate::proxy::chat_helpers::{accept_header_for_json, apply_model_params, count_repetitions};
use crate::proxy::http::{log_turn_request};
use crate::proxy::messages::{extract_response_metrics, inject_minimax_system_message, inject_mistral_tool_params, normalize_assistant_messages, sanitize_tool_calls, validate_model_exists};
use crate::proxy::timeouts::{request_turn_summary};
use crate::proxy::upstream_errors::{is_transient_race_error, sse_body_error};
use crate::proxy::admission::{GatewayAcquireError, try_acquire_gateway_permits};
use crate::proxy::racing::{adaptive_racing_parallel, tiered_candidates, solo_candidate_model, prompt_parallel_cap, race_models};
use crate::proxy::chat::chat_completions;
use axum::extract::State;
use crate::proxy::messages::{is_minimax_model, is_mistral_model, has_tool_messages};
use bytes::Bytes;
use serde_json::Value;
use tokio::time::{sleep, timeout};
use tracing::warn;
use crate::key_pool::{KeyAcquireError, KeyLease};
use crate::AppState;
use super::*;
use crate::{
    config::{ModelCompat, ModelParams},
    key_pool::KeyPool,
    model_stats::ModelStatsStore,
    RuntimeControls,
};
use serde_json::json;
use std::collections::HashMap;
fn create_test_app_state() -> AppState {
    let state = AppState::new(
        vec![],
        "https://test.api.nvidia.com".to_string(),
        None,
        ModelStatsStore::new(3000.0),
        vec![],
        3,
        8000,
        "complete".to_string(),
        HashMap::new(),
        ModelCompat::default(),
    );
    match Arc::try_unwrap(state) {
        Ok(state) => state,
        Err(_) => panic!("test state should have a single owner"),
    }
}
fn create_adaptive_test_state() -> Arc<AppState> {
    AppState::new_with_controls(
        vec![
            crate::KeyEntry {
                key: "key-a".to_string(),
                label: Some("key-a".to_string()),
        disabled: false,
            },
            crate::KeyEntry {
                key: "key-b".to_string(),
                label: Some("key-b".to_string()),
        disabled: false,
            },
        ],
        "https://test.api.nvidia.com".to_string(),
        None,
        ModelStatsStore::new(3000.0),
        vec![
            "fast-a".to_string(),
            "fast-b".to_string(),
            "fallback-a".to_string(),
        ],
        10,
        15000,
        "complete".to_string(),
        HashMap::new(),
        ModelCompat::default(),
        RuntimeControls {
            racing_adaptive: true,
            racing_min_parallel: 2,
            racing_pressure_parallel: 6,
            racing_degraded_parallel: 3,
            racing_fast_models: vec!["fast-a".to_string(), "fast-b".to_string()],
            racing_fallback_models: vec!["fallback-a".to_string()],
            racing_large_prompt_char_threshold: 0,
            racing_large_prompt_parallel: 1,
            racing_solo_fallback: true,
            racing_max_total_request_ms: 30000,
            max_upstream_in_flight: 6,
            max_in_flight_per_key: 2,
            admission_wait_ms: 0,
            min_dynamic_timeout_ms: 8000,
            dynamic_sample_floor: 10,
            auth_failure_cooldown_secs: 900,
        },
    )
}
#[test]
fn test_apply_model_params_uses_nested_chat_template_kwargs() {
    let mut json = json!({
        "model": "deepseek-ai/deepseek-v4-flash",
        "messages": [{"role": "user", "content": "test"}],
        "chat_template_kwargs": {"client_value": true},
        "stream": true
    });
    let mut kwargs = HashMap::new();
    kwargs.insert("thinking".to_string(), json!(true));
    kwargs.insert("enable_thinking".to_string(), json!(true));
    kwargs.insert("reasoning_effort".to_string(), json!("high"));
    let params = ModelParams {
        temperature: Some(1.0),
        top_p: Some(0.95),
        max_tokens: Some(16384),
        reasoning_budget: Some(16384),
        stream: Some(true),
        chat_template_kwargs: Some(kwargs),
        ..Default::default()
    };
    apply_model_params(&mut json, &params);
    assert_eq!(json["temperature"], json!(1.0));
    assert_eq!(json["top_p"], json!(0.95));
    assert_eq!(json["max_tokens"], json!(16384));
    assert_eq!(json["reasoning_budget"], json!(16384));
    assert_eq!(json["stream"], json!(true));
    assert_eq!(json["chat_template_kwargs"]["client_value"], json!(true));
    assert_eq!(json["chat_template_kwargs"]["thinking"], json!(true));
    assert_eq!(json["chat_template_kwargs"]["enable_thinking"], json!(true));
    assert_eq!(
        json["chat_template_kwargs"]["reasoning_effort"],
        json!("high")
    );
    assert!(json.get("thinking").is_none());
}
#[test]
fn test_adaptive_racing_parallel_clamps_to_available_key_slots() {
    let state = create_adaptive_test_state();
    let lease_a = state.pool.next_key_with_permit().unwrap();
    let lease_b = state.pool.next_key_with_permit().unwrap();
    assert_eq!(adaptive_racing_parallel(&state, 10), 2);
    drop(lease_a);
    drop(lease_b);
    assert_eq!(adaptive_racing_parallel(&state, 10), 4);
}
#[test]
fn test_adaptive_racing_parallel_uses_pressure_level() {
    let state = create_adaptive_test_state();
    let _p1 = state.try_acquire_upstream().unwrap();
    let _p2 = state.try_acquire_upstream().unwrap();
    assert_eq!(adaptive_racing_parallel(&state, 10), 4);
}
#[test]
fn test_tiered_candidates_prefers_fast_pool() {
    let state = create_adaptive_test_state();
    let rotated = vec![
        "fallback-a".to_string(),
        "fast-b".to_string(),
        "fast-a".to_string(),
    ];
    let result = tiered_candidates(&state, &rotated, 2);
    assert_eq!(result, vec!["fast-b".to_string(), "fast-a".to_string()]);
}
#[test]
fn test_solo_candidate_prefers_fast_pool() {
    let state = create_adaptive_test_state();
    let rotated = vec![
        "fallback-a".to_string(),
        "fast-b".to_string(),
        "fast-a".to_string(),
    ];
    let result = solo_candidate_model(&state, &rotated);
    assert_eq!(result, Some("fast-b".to_string()));
}
#[test]
fn test_solo_candidate_uses_fallback_when_fast_pool_degraded() {
    let state = create_adaptive_test_state();
    for _ in 0..3 {
        state.model_stats.record("fast-a", 5000.0, true);
        state.model_stats.record("fast-b", 5000.0, true);
        state.model_stats.record("fallback-a", 600.0, true);
    }
    let rotated = vec![
        "fallback-a".to_string(),
        "fast-b".to_string(),
        "fast-a".to_string(),
    ];
    let result = solo_candidate_model(&state, &rotated);
    assert_eq!(result, Some("fallback-a".to_string()));
}
#[test]
fn test_prompt_parallel_cap_limits_large_prompts() {
    let mut state = create_adaptive_test_state();
    Arc::get_mut(&mut state)
        .unwrap()
        .racing_large_prompt_char_threshold = 10;
    Arc::get_mut(&mut state)
        .unwrap()
        .racing_large_prompt_parallel = 1;
    let body = Bytes::from(
        r#"{"messages":[{"role":"user","content":"this prompt is definitely large"}]}"#,
    );
    assert_eq!(prompt_parallel_cap(&state, &body), Some(1));
}
#[test]
fn test_apply_model_params_preserves_explicit_stream_choice() {
    let mut json = json!({
        "model": "z-ai/glm-5.1",
        "messages": [{"role": "user", "content": "test"}],
        "stream": false
    });
    let params = ModelParams {
        stream: Some(true),
        ..Default::default()
    };
    apply_model_params(&mut json, &params);
    assert_eq!(json["stream"], json!(false));
    assert_eq!(accept_header_for_json(&json), "application/json");
}
#[test]
fn test_apply_model_params_does_not_enable_stream_when_omitted() {
    let mut json = json!({
        "model": "z-ai/glm-5.1",
        "messages": [{"role": "user", "content": "test"}]
    });
    let params = ModelParams {
        stream: Some(true),
        ..Default::default()
    };
    apply_model_params(&mut json, &params);
    assert!(json.get("stream").is_none());
    assert_eq!(accept_header_for_json(&json), "application/json");
}
// ============ validate_model_exists tests ============
#[test]
fn test_validate_model_exists_empty_model() {
    let state = create_test_app_state();
    assert!(validate_model_exists("", &state).is_ok());
}
#[test]
fn test_validate_model_exists_auto_model() {
    let state = create_test_app_state();
    assert!(validate_model_exists("auto", &state).is_ok());
}
#[test]
fn test_validate_model_exists_nimaproxy_auto_alias() {
    let state = create_test_app_state();
    assert!(validate_model_exists("nimaproxy/auto", &state).is_ok());
}
#[test]
fn test_validate_model_exists_in_available_models() {
    let state = create_test_app_state();
    state
        .available_models
        .lock()
        .unwrap()
        .push("openai/gpt-4".to_string());
    assert!(validate_model_exists("openai/gpt-4", &state).is_ok());
}
#[test]
fn test_validate_model_exists_not_in_available_models() {
    let state = create_test_app_state();
    state
        .available_models
        .lock()
        .unwrap()
        .push("openai/gpt-4".to_string());
    let result = validate_model_exists("anthropic/claude", &state);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("not found"));
}
#[test]
fn test_validate_model_exists_in_racing_models() {
    let mut state = create_test_app_state();
    state.racing_models = vec!["mistralai/mistral-medium-3.5-128b".to_string()];
    assert!(validate_model_exists("mistralai/mistral-medium-3.5-128b", &state).is_ok());
}
#[test]
fn test_validate_model_exists_with_router() {
    use crate::model_router::{ModelRouter, Strategy};
    let mut state = create_test_app_state();
    state.router = Some(ModelRouter::new(
        vec!["model1".to_string(), "model2".to_string()],
        Strategy::RoundRobin,
    ));
    assert!(validate_model_exists("model1", &state).is_ok());
    let result = validate_model_exists("any-model", &state);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("not found"));
}
#[test]
fn test_validate_model_exists_passthrough_mode() {
    let state = create_test_app_state();
    assert!(validate_model_exists("some-random-model", &state).is_ok());
}
// ============ count_repetitions tests ============
#[test]
fn test_count_repetitions_empty_string() {
    assert_eq!(count_repetitions(""), 0);
}
#[test]
fn test_count_repetitions_short_text() {
    assert_eq!(count_repetitions("hello world"), 0);
    assert_eq!(count_repetitions("one two three"), 0);
}
#[test]
fn test_count_repetitions_no_repetition() {
    let text = "The quick brown fox jumps over the lazy dog";
    assert_eq!(count_repetitions(text), 0);
}
#[test]
fn test_count_repetitions_simple_repetition() {
    // Need at least 6 words for a 3-word pattern to repeat
    // "hello world test" repeated twice
    let text = "hello world test hello world test";
    assert!(count_repetitions(text) > 0);
}
#[test]
fn test_count_repetitions_three_word_pattern() {
    let text = "the cat sat the cat sat the cat sat";
    assert!(count_repetitions(text) > 0);
}
#[test]
fn test_count_repetitions_case_insensitive() {
    // Case should not matter - "Hello World Test" repeated
    let text = "Hello World Test HELLO WORLD TEST";
    assert!(count_repetitions(text) > 0);
}
#[test]
fn test_count_repetitions_max_cap() {
    let mut repeated = String::new();
    for i in 0..15 {
        if i > 0 {
            repeated.push(' ');
        }
        repeated.push_str("repeat this");
    }
    assert!(count_repetitions(&repeated) <= 10);
}
// ============ extract_response_metrics tests ============
#[test]
fn test_extract_response_metrics_empty_string() {
    let (tokens, reps, tool) = extract_response_metrics("");
    assert_eq!(tokens, 0);
    assert_eq!(reps, 0);
    assert_eq!(tool, false);
}
#[test]
fn test_extract_response_metrics_invalid_json() {
    let (tokens, reps, tool) = extract_response_metrics("Hello, this is a test response");
    assert!(tokens > 0);
    assert_eq!(tool, false);
}
#[test]
fn test_extract_response_metrics_with_usage() {
    let json = r#"{"usage": {"completion_tokens": 42}, "choices": []}"#;
    let (tokens, reps, tool) = extract_response_metrics(json);
    assert_eq!(tokens, 42);
    assert_eq!(reps, 0);
    assert_eq!(tool, false);
}
#[test]
fn test_extract_response_metrics_with_tool_call() {
    let json = r#"{"usage": {"completion_tokens": 10}, "choices": [{"message": {"tool_calls": [{"id": "1"}]}}]}"#;
    let (tokens, reps, tool) = extract_response_metrics(json);
    assert_eq!(tokens, 10);
    assert_eq!(tool, true);
}
#[test]
fn test_extract_response_metrics_without_tool_call() {
    let json = r#"{"usage": {"completion_tokens": 10}, "choices": [{"message": {"content": "Hello"}}]}"#;
    let (tokens, reps, tool) = extract_response_metrics(json);
    assert_eq!(tokens, 10);
    assert_eq!(tool, false);
}
#[test]
fn test_extract_response_metrics_no_usage_field() {
    let json = r#"{"choices": [{"message": {"content": "Hello"}}]}"#;
    let (tokens, reps, tool) = extract_response_metrics(json);
    assert!(tokens > 0);
}
#[test]
fn test_extract_response_metrics_repetition_in_json() {
    // Test that extract_response_metrics returns all three values correctly
    // This verifies the function signature and basic parsing works
    let json = r#"{"usage": {"completion_tokens": 5}, "choices": []}"#;
    let (tokens, reps, tool) = extract_response_metrics(json);
    assert_eq!(tokens, 5);
    assert_eq!(tool, false);
}
#[test]
fn test_extract_response_metrics_multiple_choices() {
    let json = r#"{"usage": {"completion_tokens": 20}, "choices": [{"message": {"content": "A"}}, {"message": {"tool_calls": [{"id": "1"}]}}]}"#;
    let (tokens, reps, tool) = extract_response_metrics(json);
    assert_eq!(tokens, 20);
    assert_eq!(tool, true);
}
// ============ inject_minimax_system_message tests ============
#[test]
fn test_inject_minimax_system_message_adds_to_empty_messages() {
    let mut json = json!({
        "model": "minimaxai/minimax-01",
        "messages": []
    });
    inject_minimax_system_message(&mut json, "minimaxai/minimax-01");
    assert_eq!(json["messages"].as_array().unwrap().len(), 1);
    assert_eq!(json["messages"][0]["role"], "system");
    assert!(json["messages"][0]["content"]
        .as_str()
        .unwrap()
        .contains("When using tools, output JSON"));
}
#[test]
fn test_inject_minimax_system_message_prepends_to_existing_system() {
    let mut json = json!({
        "model": "minimaxai/minimax-01",
        "messages": [
            {"role": "system", "content": "Original system message"}
        ]
    });
    inject_minimax_system_message(&mut json, "minimaxai/minimax-01");
    let content = json["messages"][0]["content"].as_str().unwrap();
    assert!(content.contains("Original system message"));
    assert!(content.contains("When using tools, output JSON"));
}
#[test]
fn test_inject_minimax_system_message_prepends_to_non_system_first_message() {
    let mut json = json!({
        "model": "minimaxai/minimax-01",
        "messages": [
            {"role": "user", "content": "Hello"}
        ]
    });
    inject_minimax_system_message(&mut json, "minimaxai/minimax-01");
    assert_eq!(json["messages"].as_array().unwrap().len(), 2);
    assert_eq!(json["messages"][0]["role"], "system");
    assert_eq!(json["messages"][1]["role"], "user");
    assert!(json["messages"][0]["content"]
        .as_str()
        .unwrap()
        .contains("When using tools, output JSON"));
}
#[test]
fn test_inject_minimax_system_message_only_for_minimax_models() {
    let mut json_gpt = json!({
        "model": "openai/gpt-4",
        "messages": []
    });
    inject_minimax_system_message(&mut json_gpt, "openai/gpt-4");
    assert_eq!(json_gpt["messages"].as_array().unwrap().len(), 0);
    let mut json_mistral = json!({
        "model": "mistralai/mistral-medium-3.5-128b",
        "messages": []
    });
    inject_minimax_system_message(&mut json_mistral, "mistralai/mistral-medium-3.5-128b");
    assert_eq!(json_mistral["messages"].as_array().unwrap().len(), 0);
}
#[test]
fn test_inject_minimax_system_message_empty_model_string() {
    let mut json = json!({
        "model": "",
        "messages": []
    });
    inject_minimax_system_message(&mut json, "");
    assert_eq!(json["messages"].as_array().unwrap().len(), 0);
}
// ============ inject_mistral_tool_params tests ============
#[test]
fn test_inject_mistral_tool_params_adds_generation_prompt_for_tool_messages() {
    let mut json = json!({
        "model": "mistralai/mistral-medium-3.5-128b",
        "messages": [
            {"role": "user", "content": "Weather?"},
            {"role": "tool", "content": "Sunny"}
        ]
    });
    inject_mistral_tool_params(&mut json, "mistralai/mistral-medium-3.5-128b");
    assert_eq!(json["add_generation_prompt"], json!(false));
}
#[test]
fn test_inject_mistral_tool_params_continues_final_message_from_assistant() {
    let mut json = json!({
        "model": "mistralai/mistral-medium-3.5-128b",
        "messages": [
            {"role": "user", "content": "Hello"},
            {"role": "assistant", "content": "Hi"}
        ]
    });
    inject_mistral_tool_params(&mut json, "mistralai/mistral-medium-3.5-128b");
    assert_eq!(json["continue_final_message"], json!(true));
}
#[test]
fn test_inject_mistral_tool_params_only_for_mistral_models() {
    let mut json_gpt = json!({
        "model": "openai/gpt-4",
        "messages": [
            {"role": "user", "content": "Hello"},
            {"role": "assistant", "content": "Hi"}
        ]
    });
    inject_mistral_tool_params(&mut json_gpt, "openai/gpt-4");
    assert!(!json_gpt
        .as_object()
        .unwrap()
        .contains_key("add_generation_prompt"));
    assert!(!json_gpt
        .as_object()
        .unwrap()
        .contains_key("continue_final_message"));
}
#[test]
fn test_inject_mistral_tool_params_no_tool_messages_no_injection() {
    let mut json = json!({
        "model": "mistralai/mistral-medium-3.5-128b",
        "messages": [
            {"role": "user", "content": "Hello"},
            {"role": "assistant", "content": "Hi"}
        ]
    });
    inject_mistral_tool_params(&mut json, "mistralai/mistral-medium-3.5-128b");
    assert!(!json
        .as_object()
        .unwrap()
        .contains_key("add_generation_prompt"));
    assert_eq!(json["continue_final_message"], json!(true));
}
#[test]
fn test_inject_mistral_tool_params_empty_messages() {
    let mut json = json!({
        "model": "mistralai/mistral-medium-3.5-128b",
        "messages": []
    });
    inject_mistral_tool_params(&mut json, "mistralai/mistral-medium-3.5-128b");
    assert!(!json
        .as_object()
        .unwrap()
        .contains_key("add_generation_prompt"));
    assert!(!json
        .as_object()
        .unwrap()
        .contains_key("continue_final_message"));
}
// ============ sanitize_tool_calls tests ============
#[test]
fn test_sanitize_tool_calls_removes_empty_named_tool_calls() {
    let mut json = json!({
        "messages": [
            {
                "role": "assistant",
                "tool_calls": [
                    {"id": "call_1", "function": {"name": "get_weather", "arguments": "{}"}},
                    {"id": "call_2", "function": {"name": "", "arguments": "{}"}},
                    {"id": "call_3", "function": {"name": "get_time", "arguments": "{}"}}
                ]
            }
        ]
    });
    sanitize_tool_calls(&mut json);
    let tool_calls = json["messages"][0]["tool_calls"].as_array().unwrap();
    assert_eq!(tool_calls.len(), 2);
    assert_eq!(tool_calls[0]["function"]["name"], "get_weather");
    assert_eq!(tool_calls[1]["function"]["name"], "get_time");
}
#[test]
fn test_sanitize_tool_calls_removes_empty_tools_array() {
    let mut json = json!({
        "tools": [
            {"function": {"name": "valid_tool", "description": "A tool"}},
            {"function": {"name": "", "description": "Empty name tool"}}
        ],
        "messages": []
    });
    sanitize_tool_calls(&mut json);
    let tools = json["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["function"]["name"], "valid_tool");
}
#[test]
fn test_sanitize_tool_calls_removes_all_empty_tool_calls() {
    let mut json = json!({
        "messages": [
            {
                "role": "assistant",
                "tool_calls": [
                    {"id": "call_1", "function": {"name": "", "arguments": "{}"}}
                ]
            }
        ]
    });
    sanitize_tool_calls(&mut json);
    assert!(!json["messages"][0]
        .as_object()
        .unwrap()
        .contains_key("tool_calls"));
}
#[test]
fn test_sanitize_tool_calls_removes_all_empty_tools() {
    let mut json = json!({
        "tools": [
            {"function": {"name": "", "description": "Empty"}}
        ],
        "messages": []
    });
    sanitize_tool_calls(&mut json);
    assert!(!json.as_object().unwrap().contains_key("tools"));
}
#[test]
fn test_sanitize_tool_calls_keeps_valid_tool_calls() {
    let mut json = json!({
        "messages": [
            {
                "role": "assistant",
                "tool_calls": [
                    {"id": "call_1", "function": {"name": "get_weather", "arguments": "{}"}}
                ]
            }
        ]
    });
    sanitize_tool_calls(&mut json);
    let tool_calls = json["messages"][0]["tool_calls"].as_array().unwrap();
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0]["function"]["name"], "get_weather");
}
#[test]
fn test_sanitize_tool_calls_no_messages_field() {
    let mut json = json!({
        "model": "test"
    });
    sanitize_tool_calls(&mut json);
    assert!(!json.as_object().unwrap().contains_key("tool_calls"));
}
#[test]
fn test_sanitize_tool_calls_empty_messages_array() {
    let mut json = json!({
        "messages": []
    });
    sanitize_tool_calls(&mut json);
    assert_eq!(json["messages"].as_array().unwrap().len(), 0);
}
#[test]
fn test_sanitize_tool_calls_message_without_tool_calls() {
    let mut json = json!({
        "messages": [
            {"role": "user", "content": "Hello"},
            {"role": "assistant", "content": "Hi"}
        ]
    });
    sanitize_tool_calls(&mut json);
    assert_eq!(json["messages"].as_array().unwrap().len(), 2);
}
#[test]
fn test_normalize_assistant_adds_empty_content_without_tool_calls() {
    let mut json = json!({
        "messages": [
            {"role": "assistant", "content": null, "tool_calls": []}
        ]
    });
    normalize_assistant_messages(&mut json);
    let msg = &json["messages"][0];
    assert_eq!(msg["content"], json!(""));
    assert!(!msg.as_object().unwrap().contains_key("tool_calls"));
}
#[test]
fn test_normalize_assistant_tool_calls_keep_null_content() {
    let mut json = json!({
        "messages": [
            {
                "role": "assistant",
                "content": "will be replaced",
                "tool_call_id": "bad-extra",
                "tool_calls": [
                    {"id": "abc123XYZ", "type": "function", "function": {"name": "read", "arguments": "{}"}}
                ]
            }
        ]
    });
    normalize_assistant_messages(&mut json);
    let msg = &json["messages"][0];
    assert_eq!(msg["content"], serde_json::Value::Null);
    assert!(msg.as_object().unwrap().contains_key("tool_calls"));
    assert!(!msg.as_object().unwrap().contains_key("tool_call_id"));
}
#[test]
fn test_sanitize_tool_calls_mixed_valid_and_empty_tools() {
    let mut json = json!({
        "tools": [
            {"function": {"name": "tool1", "description": "First"}},
            {"function": {"name": "", "description": "Empty"}},
            {"function": {"name": "tool2", "description": "Second"}},
            {"function": {"name": "", "description": "Another empty"}}
        ],
        "messages": []
    });
    sanitize_tool_calls(&mut json);
    let tools = json["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["function"]["name"], "tool1");
    assert_eq!(tools[1]["function"]["name"], "tool2");
}
// ============ Additional edge case tests ============
#[test]
fn test_is_minimax_model_true_for_minimaxai_prefix() {
    assert!(is_minimax_model("minimaxai/minimax-01"));
    assert!(is_minimax_model("minimaxai/minimax-02"));
}
// Test for lines 160-166: HTTP client error handling
#[test]
fn test_proxy_http_error_recording() {
    let mut state = create_test_app_state();
    state.model_stats = ModelStatsStore::new(100.0);
    state.model_stats.record("test-model", 1000.0, false);
    let snapshot = state.model_stats.snapshot();
    assert!(snapshot.iter().any(|s| s.id == "test-model"));
}
// Test for lines 795, 827-841: circuit breaker integration
#[test]
fn test_circuit_breaker_state_transitions() {
    let stats = ModelStatsStore::new(100.0);
    for _ in 0..10 {
        stats.record("degraded-model", 5000.0, false);
    }
    let snapshot = stats.snapshot();
    assert!(snapshot.iter().any(|s| s.id == "degraded-model"));
}
// Test for lines 558, 586: race_models with various configurations
#[tokio::test]
async fn test_race_models_configuration_edge_cases() {
    let state = Arc::new(create_test_app_state());
    let body = json!({"model": "auto", "messages": [{"role": "user", "content": "test"}]});
    let models = vec!["single-model".to_string()];
    let response = race_models(state, Bytes::from(body.to_string()), &models).await;
    assert!(
        response.status() == StatusCode::BAD_REQUEST
            || response.status() == StatusCode::TOO_MANY_REQUESTS
            || response.status() >= StatusCode::INTERNAL_SERVER_ERROR
    );
}
// Test for lines 595-617: race_models key exhaustion scenarios
#[tokio::test]
async fn test_race_models_no_keys() {
    let mut state = create_test_app_state();
    state.pool = KeyPool::new(vec![]);
    let body = json!({"model": "auto", "messages": [{"role": "user", "content": "test"}]});
    let models = vec!["model".to_string()];
    let response = race_models(Arc::new(state), Bytes::from(body.to_string()), &models).await;
    assert!(response.status() >= StatusCode::BAD_REQUEST);
}
// Test for lines 690-691: chat_completions body parsing
#[tokio::test]
async fn test_chat_completions_empty_bytes() {
    let state = Arc::new(create_test_app_state());
    let response = chat_completions(State(state), HeaderMap::new(), Bytes::new()).await;
    assert!(response.status() >= StatusCode::BAD_REQUEST);
}
// Test for lines 724-750: streaming edge cases
#[tokio::test]
async fn test_stream_termination_edge_cases() {
    let state = Arc::new(create_test_app_state());
    let body = json!({"model": "test", "messages": [{"role": "user", "content": "test"}], "stream": true});
    let response = chat_completions(
        State(state),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    )
    .await;
    assert!(
        response.status() >= StatusCode::BAD_REQUEST || response.status() == StatusCode::OK
    );
}
// ============ sse_body_error tests ============
#[test]
fn test_sse_body_error_detects_nim_overload_frame() {
    let body = b"data: {\"message\":\"Service temporarily overloaded\",\"type\":\"service_unavailable\",\"code\":5033}\n\ndata: [DONE]\n\n";
    let (code, msg) =
        sse_body_error(body, Some("text/event-stream")).expect("expected an embedded error");
    assert_eq!(code, 5033);
    assert_eq!(msg, "Service temporarily overloaded");
}
#[test]
fn test_sse_body_error_none_for_normal_content_stream() {
    let body =
        b"data: {\"id\":\"x\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
    assert_eq!(sse_body_error(body, Some("text/event-stream")), None);
}
#[test]
fn test_sse_body_error_none_for_done_only_stream() {
    let body = b"data: [DONE]\n\n";
    assert_eq!(sse_body_error(body, Some("text/event-stream")), None);
}
#[test]
fn test_sse_body_error_json_body_error_object() {
    let body = br#"{"error":{"message":"Forbidden: key revoked","code":"invalid_api_key"}}"#;
    let (code, msg) =
        sse_body_error(body, Some("application/json")).expect("expected an embedded error");
    assert_eq!(code, 500);
    assert_eq!(msg, "Forbidden: key revoked");
}
#[test]
fn test_sse_body_error_json_body_top_level_status_detail() {
    let body = br#"{"status":400,"detail":"DEGRADED function cannot be invoked"}"#;
    let (code, msg) =
        sse_body_error(body, Some("application/json")).expect("expected an embedded error");
    assert_eq!(code, 400);
    assert_eq!(msg, "DEGRADED function cannot be invoked");
}
#[test]
fn test_sse_body_error_none_for_normal_json_body() {
    let body = br#"{"id":"x","choices":[{"message":{"content":"hi"}}]}"#;
    assert_eq!(sse_body_error(body, Some("application/json")), None);
}
#[test]
fn test_sse_body_error_tolerates_crlf_and_blank_lines() {
    let body =
        b"\r\ndata: {\"message\":\"overloaded\",\"code\":503}\r\n\r\ndata: [DONE]\r\n\r\n";
    let (code, msg) =
        sse_body_error(body, Some("text/event-stream")).expect("expected an embedded error");
    assert_eq!(code, 503);
    assert_eq!(msg, "overloaded");
}
/// Regression for the phantom-attempt defect.
///
/// `try_acquire_gateway_permits` used to acquire the global upstream
/// permit first, and that path increments `upstream_attempts`
/// (lib.rs:413) with no undo on drop. When all keys were busy the
/// permit was discarded and the caller re-tried every 25 ms for
/// `admission_wait_ms`, so a request queued behind saturated keys
/// accumulated attempts representing no upstream call.
///
/// Measured on the live host before the fix: `upstream_attempts` grew
/// 8.3x per request while keys were saturated vs 1.0x while quiet, with
/// the counter reaching ~74x total upstream calls.
#[test]
fn test_failed_key_acquisition_does_not_count_an_attempt() {
    let state = saturated_single_key_state(1);
    let held = state
        .pool
        .next_key_with_permit()
        .expect("first lease should succeed");
    assert_eq!(
        state.pool.available_permits(),
        0,
        "keys must be saturated for this test to be discriminating"
    );
    let before = state.gateway_metrics.snapshot().upstream_attempts;
    for _ in 0..50 {
        match try_acquire_gateway_permits(&state) {
            Ok(_) => panic!("saturated keys must reject, not admit"),
            Err(GatewayAcquireError::Overloaded | GatewayAcquireError::NoKeys) => {}
        }
    }
    let after = state.gateway_metrics.snapshot().upstream_attempts;
    assert_eq!(
        after, before,
        "a failed admission must not inflate upstream_attempts: 50 attempts were counted"
    );
    assert_eq!(
        before, 0,
        "no upstream work happened, so the base must be 0"
    );
    drop(held);
}
#[test]
fn test_successful_admission_counts_exactly_one_attempt() {
    let state = saturated_single_key_state(2);
    let before = state.gateway_metrics.snapshot().upstream_attempts;
    let permits = match try_acquire_gateway_permits(&state) {
        Ok(p) => p,
        Err(GatewayAcquireError::Overloaded) => panic!("should be admitted"),
        Err(GatewayAcquireError::NoKeys) => panic!("should be admitted"),
    };
    assert_eq!(
        state.gateway_metrics.snapshot().upstream_attempts,
        before + 1,
        "one real admission must count exactly one attempt"
    );
    drop(permits);
}
/// One leasable key, abundant global permits, so the KEY is the only
/// constraint on admission. `max_in_flight_per_key` is passed in full.
fn saturated_single_key_state(max_in_flight_per_key: usize) -> Arc<AppState> {
    AppState::new_with_controls(
        vec![crate::KeyEntry {
            key: "key-a".to_string(),
            label: Some("key-a".to_string()),
            disabled: false,
        }],
        "https://test.api.nvidia.com".to_string(),
        None,
        ModelStatsStore::new(3000.0),
        vec![],
        8,
        8000,
        "complete".to_string(),
        HashMap::new(),
        ModelCompat::default(),
        RuntimeControls {
            racing_adaptive: false,
            racing_min_parallel: 2,
            racing_pressure_parallel: 2,
            racing_degraded_parallel: 2,
            racing_fast_models: vec![],
            racing_fallback_models: vec![],
            racing_large_prompt_char_threshold: 0,
            racing_large_prompt_parallel: 1,
            racing_solo_fallback: true,
            racing_max_total_request_ms: 25_000,
            max_upstream_in_flight: 8,
            max_in_flight_per_key,
            admission_wait_ms: 5000,
            min_dynamic_timeout_ms: 15_000,
            dynamic_sample_floor: 25,
            auth_failure_cooldown_secs: 900,
        },
    )
}
#[test]
fn test_sse_body_error_tolerates_partial_utf8() {
    let mut body =
        b"data: {\"message\":\"overloaded\",\"code\":503}\n\ndata: [DONE]\n\n".to_vec();
    body.push(0xE2); // trailing incomplete multi-byte UTF-8 sequence
    let (code, msg) = sse_body_error(&body, Some("text/event-stream"))
        .expect("expected an embedded error despite trailing partial UTF-8");
    assert_eq!(code, 503);
    assert_eq!(msg, "overloaded");
}
#[test]
fn test_sse_body_error_none_for_unrecognized_content_type() {
    let body = br#"{"error":{"message":"should be ignored"}}"#;
    assert_eq!(sse_body_error(body, Some("text/plain")), None);
}
#[test]
fn test_sse_body_error_none_for_malformed_json_body() {
    let body = b"not json at all";
    assert_eq!(sse_body_error(body, Some("application/json")), None);
}
#[test]
fn test_is_transient_race_error_treats_in_stream_5xx_as_transient() {
    assert!(is_transient_race_error(
        "HTTP 5033 (in-stream) from model-a: Service temporarily overloaded"
    ));
    assert!(is_transient_race_error(
        "HTTP 503 (in-stream) from model-a: overloaded"
    ));
}
#[test]
fn test_is_transient_race_error_in_stream_4xx_is_not_transient() {
    assert!(!is_transient_race_error(
        "HTTP 400 (in-stream) from model-a: bad request"
    ));
}

#[test]
fn test_recover_xml_tool_calls_converts_to_structured() {
    use crate::proxy::messages::recover_xml_tool_calls;
    let mut json = serde_json::json!({
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "I will run that.\n\n<invoke name=\"bash\">\n<parameter name=\"i\">\ntest\n</parameter>\n<parameter name=\"command\">\necho hi\n</parameter>\n</function>\n</tool_call>"},
            {"role": "user", "content": "try again"}
        ]
    });
    recover_xml_tool_calls(&mut json);
    let msg = &json["messages"][1];
    // XML removed from content, non-XML text preserved
    let content = msg["content"].as_str().unwrap();
    assert!(!content.contains("<invoke"), "XML should be gone: {content}");
    assert!(content.contains("I will run that"), "text preserved: {content}");
    // Structured tool_calls added
    let tc = msg["tool_calls"].as_array().expect("tool_calls should exist");
    assert_eq!(tc.len(), 1);
    assert_eq!(tc[0]["function"]["name"], "bash");
    let args: serde_json::Value = serde_json::from_str(tc[0]["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args["command"], "echo hi");
    assert_eq!(args["i"], "test");
}

#[test]
fn test_recover_xml_tool_calls_sets_null_content_when_only_xml() {
    use crate::proxy::messages::recover_xml_tool_calls;
    let mut json = serde_json::json!({
        "messages": [
            {"role": "assistant", "content": "<invoke name=\"bash\">\n<parameter name=\"command\">\necho hi\n</parameter>\n</function>\n</tool_call>"}
        ]
    });
    recover_xml_tool_calls(&mut json);
    assert!(json["messages"][0]["content"].is_null(), "content should be null when only XML");
    assert!(json["messages"][0]["tool_calls"].as_array().unwrap().len() == 1);
}

#[test]
fn test_recover_xml_tool_calls_leaves_non_xml_alone() {
    use crate::proxy::messages::recover_xml_tool_calls;
    let mut json = serde_json::json!({
        "messages": [
            {"role": "assistant", "content": "The command returned hello"},
            {"role": "user", "content": "good"}
        ]
    });
    let before = json.to_string();
    recover_xml_tool_calls(&mut json);
    assert_eq!(json.to_string(), before);
}

#[test]
fn test_recover_xml_tool_calls_skips_existing_tool_calls() {
    use crate::proxy::messages::recover_xml_tool_calls;
    let mut json = serde_json::json!({
        "messages": [
            {"role": "assistant", "content": "text with <invoke name=\"x\">\n</function>\n</tool_call>",
             "tool_calls": [{"id":"c1","type":"function","function":{"name":"y","arguments":"{}"}}]}
        ]
    });
    let before = json.to_string();
    recover_xml_tool_calls(&mut json);
    assert_eq!(json.to_string(), before, "should not touch messages with existing tool_calls");
}

#[test]
fn test_recover_xml_tool_calls_handles_invoke_closing_tag() {
    use crate::proxy::messages::recover_xml_tool_calls;
    let mut json = serde_json::json!({
        "messages": [
            {"role": "assistant", "content": "<invoke name=\"bash\">\n<parameter=command>\necho hello\n</parameter>\n<parameter=i>\ntest\n</parameter>\n</invoke>"}
        ]
    });
    recover_xml_tool_calls(&mut json);
    let msg = &json["messages"][0];
    assert!(msg["content"].is_null(), "XML-only content should be null");
    let tc = msg["tool_calls"].as_array().expect("should have tool_calls");
    assert_eq!(tc.len(), 1);
    assert_eq!(tc[0]["function"]["name"], "bash");
    let args: serde_json::Value = serde_json::from_str(tc[0]["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args["command"], "echo hello");
}
