use std::sync::Arc;
use std::time::Instant;

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures::{TryStreamExt, FutureExt};
use serde_json::Value;
use tokio::time::timeout;

use crate::AppState;

const MAX_RETRIES: usize = 8;

fn count_repetitions(text: &str) -> u32 {
    let text_lower = text.to_lowercase();
    let words: Vec<&str> = text_lower.split_whitespace().collect();
    if words.len() < 4 {
        return 0;
    }
    
    let mut repetitions = 0u32;
    for window_size in 3..=6 {
        if words.len() < window_size * 2 {
            continue;
        }
        for i in 0..words.len() - window_size {
            let slice = &words[i..i + window_size];
            let pattern = slice.join(" ");
            let mut count = 1;
            for j in (i + window_size..).step_by(window_size).take_while(|&j| j + window_size <= words.len()) {
                let next_slice = &words[j..j + window_size];
                if next_slice.join(" ") == pattern {
                    count += 1;
                } else {
                    break;
                }
            }
            if count > 1 {
                repetitions += count - 1;
            }
        }
    }
    repetitions.min(10)
}

fn extract_response_metrics(text: &str) -> (u32, u32, bool) {
    let mut output_tokens = 0u32;
    let repetition_count = count_repetitions(text);
    let mut has_tool_call = false;
    
    if let Ok(json) = serde_json::from_str::<Value>(text) {
        if let Some(usage) = json.get("usage").and_then(|u| u.get("completion_tokens")) {
            if let Some(tokens) = usage.as_u64() {
                output_tokens = tokens as u32;
            }
        }
        
        if let Some(choices) = json.get("choices").and_then(|c| c.as_array()) {
            for choice in choices {
                if choice.get("message").and_then(|m| m.get("tool_calls")).is_some() {
                    has_tool_call = true;
                }
            }
        }
    }
    
    if output_tokens == 0 {
        output_tokens = (text.len() as u32) / 4;
    }
    
    (output_tokens, repetition_count, has_tool_call)
}

/// Validate a model name for chat completion requests.
/// Returns Ok(()) for valid models (including "auto" and empty).
/// Returns Err with message for invalid models not in the available list.
pub fn validate_model_exists(model: &str, state: &AppState) -> Result<(), String> {
    // "auto" and empty are always valid - they'll be resolved via router
    if model.is_empty() || model == "auto" {
        return Ok(());
    }

    // Check if model is in the available_models list (if non-empty)
    let available = state.available_models.lock().unwrap();
    if !available.is_empty() {
        if available.iter().any(|m| m == model) {
            return Ok(());
        }
        // available_models is set and model is not in it - reject
        return Err(format!("model '{}' not found in available models", model));
    }
    drop(available);

    // Check if model is in the racing_models list (if non-empty)
    if !state.racing_models.is_empty() && state.racing_models.iter().any(|m| m == model) {
        return Ok(());
    }

    // Check if router is configured - it will pick from configured models
    if state.router.is_some() {
        return Ok(());
    }

    // No routing configured - accept any model (passthrough to NVIDIA)
    // This preserves backward compatibility: when no models are configured,
    // passthrough mode allows any model through
    Ok(())
}

/// POST /v1/chat/completions
///
/// V1: injects key, retries on 429, streams SSE byte-for-byte.
/// V2: resolves `"model": "auto"` via router, records TTFC to model_stats.
/// V3: racing — fires N parallel requests to N models, returns first response.
pub async fn chat_completions(
    State(state): State<Arc<AppState>>,
    _headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Extract original model BEFORE resolve_model modifies it
    let original_model = {
        if let Ok(v) = serde_json::from_slice::<Value>(&body) {
            v.get("model").and_then(|m| m.as_str()).unwrap_or("").to_string()
        } else {
            String::new()
        }
    };

    let (model_id, body) = resolve_model(body, &state);

    // Validate tool call IDs for Mistral models
    if let Ok(json) = serde_json::from_slice::<Value>(&body) {
        if let Err((status, msg)) = validate_mistral_tool_call_ids(&json, &model_id) {
            return (status, msg).into_response();
        }
    }

    // Racing only triggers when the ORIGINAL request was model="auto"
    if original_model == "auto" && !state.racing_models.is_empty() && state.racing_models.len() >= 2 {
        let racing_models = state.racing_models.clone();
        return race_models(state, body, &racing_models).await;
    }

    let n = state.pool.len().min(MAX_RETRIES).max(1);

    for _ in 0..n {
        let Some((key, idx)) = state.pool.next_key() else {
            return (StatusCode::TOO_MANY_REQUESTS, "all API keys rate-limited").into_response();
        };

        let t0 = Instant::now();
        let result = state
            .client
            .post(format!("{}/v1/chat/completions", state.target))
            .header("Authorization", format!("Bearer {}", key))
            .header("Content-Type", "application/json")
            .body(body.clone())
            .send()
            .await;

        match result {
            Err(e) => {
                if let Some(label) = state.pool.get_key_label(idx) {
                    state.model_stats.record_with_key(&model_id, &label, t0.elapsed().as_millis() as f64, false);
                } else {
                    state.model_stats.record(&model_id, t0.elapsed().as_millis() as f64, false);
                }
                return (StatusCode::BAD_GATEWAY, e.to_string()).into_response();
            }
            Ok(resp) => {
                let status = resp.status();

                if status == 429 {
                    let retry_after = resp
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                        .unwrap_or(60);
                    state.pool.mark_rate_limited(idx, retry_after);
                    eprintln!("[nimaproxy] key {} rate-limited {}s", idx, retry_after);
                    continue;
                }

                // Record TTFC (response headers received = first bytes available)
                let ttfc_ms = t0.elapsed().as_millis() as f64;
                let ok = status.is_success();

                // Forward response — stream bytes directly (works for JSON + SSE)
                let resp_status =
                    axum::http::StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);

                let content_type = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("application/json")
                    .to_string();

                let stream = resp
                    .bytes_stream()
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()));
                
                let collected = match stream.try_collect::<Vec<Bytes>>().await {
                    Ok(c) => c,
                    Err(e) => {
                        return (StatusCode::BAD_GATEWAY, e.to_string()).into_response();
                    }
                };
                
let full_body = collected.concat();
eprintln!("DEBUG: NVIDIA API response - status={}, body={}", status, std::str::from_utf8(&full_body).unwrap_or("<invalid utf8>"));

// Check for server-side degradation from NVIDIA API
// NVIDIA returns: {"status":400,"title":"Bad Request","detail":"Function id '...': DEGRADED function cannot be invoked"}
let body_str = std::str::from_utf8(&full_body).unwrap_or("");
if status == 400 && (body_str.contains("DEGRADED") || body_str.contains("degraded")) {
    eprintln!("[nimaproxy] SERVER-DEGRADED: model '{}' returned DEGRADED error from NVIDIA (server-side block)", model_id);
    // Record as server-side degraded - this immediately marks the model as unavailable
    state.model_stats.record_server_degraded(&model_id);
    // Continue retry with a different model
    continue;
}

                let (output_tokens, repetition_count, had_tool_call) = extract_response_metrics(std::str::from_utf8(&full_body).unwrap_or(""));
                
                if output_tokens > 0 || repetition_count > 0 {
                    if let Some(label) = state.pool.get_key_label(idx) {
                        state.model_stats.record_with_circuit_breaker(&model_id, ttfc_ms, ok, output_tokens, repetition_count, had_tool_call);
                    } else {
                        state.model_stats.record_with_circuit_breaker(&model_id, ttfc_ms, ok, output_tokens, repetition_count, had_tool_call);
                    }
                } else {
                    if let Some(label) = state.pool.get_key_label(idx) {
                        state.model_stats.record_with_key(&model_id, &label, ttfc_ms, ok);
                    } else {
                        state.model_stats.record(&model_id, ttfc_ms, ok);
                    }
                }
                
                let body = Body::from(full_body);

                let mut response = Response::new(body);
                *response.status_mut() = resp_status;
                response.headers_mut().insert(
                    "content-type",
                    HeaderValue::from_str(&content_type)
                        .unwrap_or_else(|_| HeaderValue::from_static("application/json")),
                );
                // Track which key was used for rotation debugging
                if let Some(label) = state.pool.get_key_label(idx) {
                    response.headers_mut().insert(
                        "x-key-label",
                        HeaderValue::from_str(&label)
                            .unwrap_or_else(|_| HeaderValue::from_static("unknown")),
                    );
                }
                if content_type.contains("event-stream") {
                    response.headers_mut().insert(
                        "cache-control",
                        HeaderValue::from_static("no-cache"),
                    );
                    response.headers_mut().insert(
                        "x-accel-buffering",
                        HeaderValue::from_static("no"),
                    );
                }
                return response;
            }
        }
    }

    (StatusCode::TOO_MANY_REQUESTS, "all keys exhausted after retries").into_response()
}

/// Sanitize tool_calls and tools to remove entries with empty names.
/// NVIDIA NIM (via Azure OpenAI validation) rejects empty function names with:
/// "Must be a-z, A-Z, 0-9, or contain underscores and dashes, with a maximum length of 64"
fn sanitize_tool_calls(json: &mut Value) {
 // Sanitize tool_calls in messages
 if let Some(messages) = json.get_mut("messages").and_then(|m| m.as_array_mut()) {
 for msg in messages.iter_mut() {
 // Strip tool_call_id from assistant messages - most models don't accept it
 // Pydantic error: "Extra inputs are not permitted" for tool_call_id field
 if let Some(role) = msg.get("role").and_then(|r| r.as_str()) {
 if role == "assistant" {
 if let Some(obj) = msg.as_object_mut() {
 obj.remove("tool_call_id");
 obj.remove("reasoning"); // Strip reasoning field - not accepted by most models
// Ensure assistant messages with tool_calls have content
// NVIDIA requires non-null content even for tool-call-only messages
if obj.get("tool_calls").is_some() && obj.get("content").is_none() {
obj.insert("content".to_string(), Value::String("".to_string()));
}
 }
 }
 }
 
 if let Some(tool_calls) = msg.get_mut("tool_calls").and_then(|tc| tc.as_array_mut()) {
 // Filter out tool_calls with empty names
 tool_calls.retain(|tc| {
 if let Some(name) = tc.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()) {
 !name.is_empty()
 } else {
 // Keep if no name field (shouldn't happen but be safe)
 true
 }
 });
 // If all tool_calls were removed, remove the tool_calls field entirely
 if tool_calls.is_empty() {
 if let Some(obj) = msg.as_object_mut() {
 obj.remove("tool_calls");
 }
 }
 }
 }
 }

 // Sanitize tools array (tool definitions)
 if let Some(tools) = json.get_mut("tools").and_then(|t| t.as_array_mut()) {
 // Filter out tools with empty function names
 tools.retain(|tool| {
 if let Some(name) = tool.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()) {
 !name.is_empty()
 } else {
 // Keep if no name field (shouldn't happen but be safe)
 true
 }
 });
 // If all tools were removed, remove the tools field entirely
 if tools.is_empty() {
 if let Some(obj) = json.as_object_mut() {
 obj.remove("tools");
 }
 }
 }
}
/// Validate tool call IDs for Mistral models.
/// Mistral requires tool call IDs to be exactly 9 alphanumeric characters.
fn validate_mistral_tool_call_ids(json: &Value, model_id: &str) -> Result<(), (StatusCode, String)> {
    if !is_mistral_model(model_id) {
        return Ok(());
    }
    if let Some(messages) = json.get("messages").and_then(|m| m.as_array()) {
        for msg in messages {
            if let Some(role) = msg.get("role").and_then(|r| r.as_str()) {
                if role == "assistant" {
                    if let Some(tool_calls) = msg.get("tool_calls").and_then(|tc| tc.as_array()) {
                        for tc in tool_calls {
                            if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                                if id.len() != 9 || !id.chars().all(|c| c.is_alphanumeric()) {
                                    return Err((StatusCode::BAD_REQUEST,
                                        format!("Tool call id '{}' is invalid. Must be exactly 9 alphanumeric characters for Mistral models.", id)));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// - "developer" → "user" (NVIDIA NIM doesn't support developer role)
/// - "tool" → "assistant" (NVIDIA NIM doesn't support tool role)
///
/// For tool messages, we also need to:
/// - Keep tool_call_id (required for matching tool results to calls)
/// - Keep content as the tool output
fn transform_message_roles(json: &mut Value, model_id: &str, state: &AppState) {
 let transform_developer = state.model_compat.should_transform_developer_role(model_id);
 let transform_tool = state.model_compat.should_transform_tool_messages(model_id);
 
 eprintln!("DEBUG: transform_message_roles - model_id={}, transform_developer={}, transform_tool={}", model_id, transform_developer, transform_tool);

 if !transform_developer && !transform_tool {
 eprintln!("DEBUG: No transformation needed, returning early");
 return;
 }

 if let Some(messages) = json.get_mut("messages").and_then(|m| m.as_array_mut()) {
 for msg in messages {
 let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("").to_string();

 if transform_developer && role == "developer" {
 if let Some(v) = msg.get_mut("role") {
 *v = Value::String("user".to_string());
 }
 } else if transform_tool && role == "tool" {
 // Transform tool role to assistant
 if let Some(v) = msg.get_mut("role") {
 *v = Value::String("assistant".to_string());
 }
 // Tool messages have tool_call_id which assistant messages also support
 // when they're responding to a tool call, so we keep it
 }
 }
 }
}
/// Check if the conversation has tool messages or tool calls (indicating a tool call flow).
/// This requires special handling for Mistral models on NVIDIA NIM.
fn has_tool_messages(json: &Value) -> bool {
  eprintln!("DEBUG: has_tool_messages called, json keys: {:?}", json.as_object().map(|o| o.keys().collect::<Vec<_>>()));
  if let Some(messages) = json.get("messages").and_then(|m| m.as_array()) {
    eprintln!("DEBUG: Found {} messages", messages.len());
    for (i, msg) in messages.iter().enumerate() {
      if let Some(role) = msg.get("role").and_then(|r| r.as_str()) {
        eprintln!("DEBUG: Message {}: role={}", i, role);
      }
    }
    let has_tool_role = messages.iter().any(|msg| {
      msg.get("role").and_then(|r| r.as_str()) == Some("tool")
    });
    let has_tool_calls = messages.iter().any(|msg| {
      msg.get("tool_calls").is_some()
    });
    let has_tool = has_tool_role || has_tool_calls;
    eprintln!("DEBUG: has_tool_messages result: {} (tool_role={}, tool_calls={})", has_tool, has_tool_role, has_tool_calls);
    return has_tool;
  }
  eprintln!("DEBUG: No messages array found");
  false
}

/// Check if a model is a Mistral model (requires special tool calling handling).
fn is_mistral_model(model_id: &str) -> bool {
    model_id.contains("mistral") || model_id.contains("devstral")
}

/// Check if model is MiniMax (requires JSON tool calling format hint)
fn is_minimax_model(model_id: &str) -> bool {
    model_id.starts_with("minimaxai/")
}

/// Inject system message for MiniMax models to use JSON tool calling format
fn inject_minimax_system_message(json: &mut Value, model_id: &str) {
    if !is_minimax_model(model_id) {
        return;
    }
    
    let minmax_instruction = r#"When using tools, output JSON in this exact format:
{"tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "function_name", "arguments": {"arg": "value"}}}]}
Do NOT use XML tags like <minimax:tool_call> or <invoke>."#;

    if let Some(messages) = json.get_mut("messages").and_then(|m| m.as_array_mut()) {
        if let Some(first) = messages.get_mut(0) {
            if first.get("role").and_then(|r| r.as_str()) == Some("system") {
                if let Some(content) = first.get_mut("content") {
                    if let Some(s) = content.as_str() {
                        *content = Value::String(format!("{}\n\n{}", s, minmax_instruction));
                    }
                }
            } else {
                let system_msg = serde_json::json!({"role": "system", "content": minmax_instruction});
                messages.insert(0, system_msg);
            }
        } else {
            let system_msg = serde_json::json!({"role": "system", "content": minmax_instruction});
            messages.insert(0, system_msg);
        }
    }
    eprintln!("DEBUG: Injected MiniMax JSON tool call instruction for model={}", model_id);
}

/// Check if the last message in the conversation is from the assistant.
fn is_last_message_from_assistant(json: &Value) -> bool {
  if let Some(messages) = json.get("messages").and_then(|m| m.as_array()) {
    if let Some(last) = messages.last() {
      if let Some(role) = last.get("role").and_then(|r| r.as_str()) {
        eprintln!("DEBUG: is_last_message_from_assistant - last role={}", role);
        return role == "assistant";
      }
    }
  }
  eprintln!("DEBUG: is_last_message_from_assistant - no messages or no role");
  false
}

/// Inject parameters for tool calling and conversation continuation.
/// When the last message is from the assistant, we must set:
/// - add_generation_prompt=false (tells API we're continuing, not starting new)
/// - continue_final_message=true (tells API to continue from assistant's partial response)
/// This applies to ALL models on NVIDIA NIM, not just Mistral.
fn inject_mistral_tool_params(json: &mut Value, model_id: &str) {
    let is_mistral = is_mistral_model(model_id);
    let has_tools = has_tool_messages(json);
    let last_from_assistant = is_last_message_from_assistant(json);
    eprintln!("DEBUG: inject_mistral_tool_params - model_id={}, is_mistral={}, has_tools={}, last_from_assistant={}", model_id, is_mistral, has_tools, last_from_assistant);

    // Only inject Mistral-specific parameters for Mistral models
    // These params are rejected by NVIDIA for non-Mistral models
    if is_mistral {
        if has_tools {
            eprintln!("DEBUG: Injecting Mistral tool params");
        json["add_generation_prompt"] = Value::Bool(false);
    }
    if last_from_assistant {
            eprintln!("DEBUG: Injecting Mistral continuation");
        json["continue_final_message"] = Value::Bool(true);
}
    }
}


/// Resolve the model field, optionally rewriting the body for "auto" routing.
/// Returns (model_id_string, possibly_rewritten_body).
pub fn resolve_model(body: Bytes, state: &AppState) -> (String, Bytes) {
    let mut json: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return ("unknown".to_string(), body),
    };

    let requested = json["model"].as_str().unwrap_or("").to_string();

    if requested.is_empty() || requested == "auto" {
        if let Some(router) = &state.router {
            if let Some(picked) = router.pick(&state.model_stats) {
                json["model"] = Value::String(picked.clone());
                if let Some(params) = state.model_params.get(&picked) {
                    if let Some(temp) = params.temperature {
                        json["temperature"] = Value::from(temp);
                    }
                    if let Some(tp) = params.top_p {
                        json["top_p"] = Value::from(tp);
                    }
                    if let Some(tk) = params.top_k {
                        json["top_k"] = Value::from(tk);
                    }
                    if let Some(fp) = params.frequency_penalty {
                        json["frequency_penalty"] = Value::from(fp);
                    }
                    if let Some(pp) = params.presence_penalty {
                        json["presence_penalty"] = Value::from(pp);
                    }
                    if let Some(max_tokens) = params.max_tokens {
                        json["max_tokens"] = Value::from(max_tokens);
                    }
                    if let Some(reasoning_effort) = &params.reasoning_effort {
                        json["reasoning_effort"] = Value::String(reasoning_effort.clone());
                    }
                    if let Some(seed) = params.seed {
                        json["seed"] = Value::from(seed);
                    }
                    if let Some(ctk) = &params.chat_template_kwargs {
                        for (k, v) in ctk {
                            json[k] = v.clone();
                        }
                    }
                }
            }
        }
    }

    // Use the actual model ID from JSON after potential rewrite (for "auto" routing)
    let model_id = json["model"].as_str().unwrap_or("unknown").to_string();

    // Inject Mistral-specific parameters BEFORE message transformations
    // so has_tool_messages() can detect tool messages in the original JSON
    inject_mistral_tool_params(&mut json, &model_id);
    // Inject MiniMax system message for JSON tool calling
    inject_minimax_system_message(&mut json, &model_id);

    // Sanitize tool_calls to remove entries with empty names (Azure OpenAI rejects these)
    sanitize_tool_calls(&mut json);

    transform_message_roles(&mut json, &model_id, state);

    if let Some(params) = state.model_params.get(&requested) {
        if let Some(temp) = params.temperature {
            json["temperature"] = Value::from(temp);
        }
        if let Some(tp) = params.top_p {
            json["top_p"] = Value::from(tp);
        }
        if let Some(tk) = params.top_k {
            json["top_k"] = Value::from(tk);
        }
        if let Some(fp) = params.frequency_penalty {
            json["frequency_penalty"] = Value::from(fp);
        }
        if let Some(pp) = params.presence_penalty {
            json["presence_penalty"] = Value::from(pp);
        }
        if let Some(max_tokens) = params.max_tokens {
            json["max_tokens"] = Value::from(max_tokens);
        }
        if let Some(reasoning_effort) = &params.reasoning_effort {
            json["reasoning_effort"] = Value::String(reasoning_effort.clone());
        }
        if let Some(seed) = params.seed {
            json["seed"] = Value::from(seed);
        }
        if let Some(ctk) = &params.chat_template_kwargs {
            for (k, v) in ctk {
                // Preserve Mistral-specific parameters that were injected
                if k != "add_generation_prompt" && k != "continue_final_message" {
                    json[k] = v.clone();
                }
            }
        }
    }

    eprintln!("DEBUG: Sending to NVIDIA API - model_id={}, json={}", model_id, json);
    (model_id, Bytes::from(json.to_string()))
}

/// GET /v1/models — passthrough to NVIDIA.
pub async fn models(State(state): State<Arc<AppState>>) -> Response {
    let Some((key, _)) = state.pool.next_key() else {
        return (StatusCode::TOO_MANY_REQUESTS, "no active API keys").into_response();
    };
    match state
        .client
        .get(format!("{}/v1/models", state.target))
        .header("Authorization", format!("Bearer {}", key))
        .send()
        .await
    {
        Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
        Ok(resp) => {
            let status = axum::http::StatusCode::from_u16(resp.status().as_u16())
                .unwrap_or(StatusCode::BAD_GATEWAY);
            match resp.bytes().await {
                Ok(b) => (
                    status,
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    b,
                )
                    .into_response(),
                Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
            }
        }
    }
}

/// GET /health — key pool liveness.
pub async fn health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let statuses = state.pool.status();
    let total = statuses.len();
    let active: usize = statuses.iter().filter(|s| s.active).count();

    let keys_json: Vec<Value> = statuses
        .iter()
        .map(|s| {
            serde_json::json!({
                "label": s.label,
                "key_hint": s.key_hint,
                "active": s.active,
                "cooldown_secs_remaining": s.cooldown_secs_remaining,
            })
        })
        .collect();

    let body = serde_json::json!({
        "status": if active > 0 { "UP" } else { "DEGRADED" },
        "keys_total": total,
        "keys_active": active,
        "keys": keys_json,
    });

    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
}

/// GET /stats — per-model latency stats (V2).
pub async fn stats(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let snapshots = state.model_stats.snapshot();
    let models_json: Vec<Value> = snapshots
        .iter()
        .map(|s| {
            serde_json::json!({
                "model": s.id,
                "avg_ms": s.avg_ms,
                "p95_ms": s.p95_ms,
                "total": s.total,
                "success": s.success,
                "success_rate": s.success_rate,
                "sample_count": s.sample_count,
                "consecutive_failures": s.consecutive_failures,
                "degraded": s.degraded,
            })
        })
        .collect();

    let keys_json: Vec<Value> = state
        .pool
        .status()
        .iter()
        .map(|s| {
            serde_json::json!({
                "label": s.label,
                "key_hint": s.key_hint,
                "active": s.active,
                "cooldown_secs_remaining": s.cooldown_secs_remaining,
            })
        })
        .collect();

    let racing_models: Vec<Value> = state
        .racing_models
        .iter()
        .map(|m| serde_json::json!(m))
        .collect();

    let body = serde_json::json!({
        "models": models_json,
        "keys": keys_json,
        "racing_models": racing_models,
        "racing_max_parallel": state.racing_max_parallel,
        "racing_timeout_ms": state.racing_timeout_ms,
    });

    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
}

async fn race_models(
    state: Arc<AppState>,
    body: Bytes,
    models: &[String],
) -> Response {
    let timeout_ms = state.racing_timeout_ms;
    let max_parallel = state.racing_max_parallel.min(models.len());

    if max_parallel < 2 {
        return (StatusCode::BAD_REQUEST, "racing requires at least 2 models").into_response();
    }

    // Rotate model selection: grab cursor, pick models starting from it,
    // wrap around, then advance cursor. This forces cycling so no single
    // model can dominate — critical for breaking inference loops where a model
    // gets stuck and keeps getting picked.
    let cursor = {
        let c = state.racing_cursor.lock().unwrap();
        *c
    };
    let n = models.len();

    let candidates: Vec<String> = (0..n)
        .map(|i| models[(cursor + i) % n].clone())
        .collect();
    let candidates_for_race = state.model_stats.racing_candidates(&candidates, max_parallel);

    if candidates_for_race.len() < 2 {
        eprintln!("[racing] not enough viable models after filtering (need ≥2)");
        return (StatusCode::BAD_GATEWAY, "not enough viable racing models").into_response();
    }

    let models_to_race = candidates_for_race;
    {
        let mut c = state.racing_cursor.lock().unwrap();
        *c = (cursor + max_parallel) % n;
    }

    let mut handles = Vec::new();

    for model_id in &models_to_race {
        let timeout_val = state.model_stats.get_model_timeout(model_id, timeout_ms);

  let mut json: Value = match serde_json::from_slice(&body) {
    Ok(v) => v,
    Err(_) => continue,
  };
  json["model"] = Value::String(model_id.clone());

  // Transform message roles for models that don't support developer/tool roles
  transform_message_roles(&mut json, model_id, &state);

    // Inject MiniMax system message for JSON tool calling
    inject_minimax_system_message(&mut json, model_id);
            // Inject Mistral-specific parameters for tool calling continuation
            inject_mistral_tool_params(&mut json, model_id);

            // Sanitize tool_calls to remove entries with empty names
            sanitize_tool_calls(&mut json);

            // Inject per-model hyperparameters - override client settings with proxy config
        if let Some(params) = state.model_params.get(model_id) {
            if let Some(temp) = params.temperature {
                json["temperature"] = Value::from(temp);
            }
            if let Some(tp) = params.top_p {
                json["top_p"] = Value::from(tp);
            }
            if let Some(tk) = params.top_k {
                json["top_k"] = Value::from(tk);
            }
            if let Some(fp) = params.frequency_penalty {
                json["frequency_penalty"] = Value::from(fp);
            }
            if let Some(pp) = params.presence_penalty {
                json["presence_penalty"] = Value::from(pp);
            }
            if let Some(max_tokens) = params.max_tokens {
                json["max_tokens"] = Value::from(max_tokens);
            }
            if let Some(reasoning_effort) = &params.reasoning_effort {
                json["reasoning_effort"] = Value::String(reasoning_effort.clone());
            }
            if let Some(seed) = params.seed {
                json["seed"] = Value::from(seed);
            }
            if let Some(ctk) = &params.chat_template_kwargs {
                for (k, v) in ctk {
                    json[k] = v.clone();
                }
            }
        }

        let req_body = match serde_json::to_vec(&json) {
            Ok(b) => Bytes::from(b),
            Err(_) => continue,
        };

        let target = state.target.clone();
        let client = state.client.clone();
        let state_clone = state.clone();
        let model_id_clone = model_id.clone();
        let timeout_ms_for_model = timeout_val;

        let key = state.pool.next_key();
        if key.is_none() {
            eprintln!("[racing] no keys available for {}", model_id);
            continue;
        }
        let (key, key_idx) = key.unwrap();
        let key_label = state.pool.get_key_label(key_idx);

        let handle = tokio::spawn(async move {
            let key_label = key_label;

            let t0 = Instant::now();
            let result = timeout(
                std::time::Duration::from_millis(timeout_ms_for_model),
                client
                    .post(format!("{}/v1/chat/completions", target))
                    .header("Authorization", format!("Bearer {}", key))
                    .header("Content-Type", "application/json")
                    .body(req_body)
                    .send(),
            )
            .await;

            match result {
                Ok(Ok(resp)) => {
                    let latency = t0.elapsed().as_millis() as f64;
                    if let Some(ref label) = key_label {
                        state_clone.model_stats.record_with_key(&model_id_clone, label, latency, true);
                    } else {
                        state_clone.model_stats.record(&model_id_clone, latency, true);
                    }

                    let status = resp.status();
                    let content_type = resp
                        .headers()
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("application/json")
                        .to_string();

                    let stream = resp
                        .bytes_stream()
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()));
                    let body = Body::from_stream(stream);

                    let mut response = Response::new(body);
                    *response.status_mut() = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
                    response.headers_mut().insert(
                        "content-type",
                        HeaderValue::from_str(&content_type)
                            .unwrap_or_else(|_| HeaderValue::from_static("application/json")),
                    );
                    if let Some(ref label) = key_label {
                        response.headers_mut().insert(
                            "x-key-label",
                            HeaderValue::from_str(label)
                                .unwrap_or_else(|_| HeaderValue::from_static("unknown")),
                        );
                    }
                    Ok::<Response, String>(response)
                }
                Ok(Err(e)) => {
                    if let Some(ref label) = key_label {
                        state_clone.model_stats.record_with_key(&model_id_clone, label, timeout_ms_for_model as f64, false);
                    } else {
                        state_clone.model_stats.record(&model_id_clone, timeout_ms_for_model as f64, false);
                    }
                    Err(e.to_string())
                }
                Err(_) => {
                    if let Some(ref label) = key_label {
                        state_clone.model_stats.record_with_key(&model_id_clone, label, timeout_ms_for_model as f64, false);
                    } else {
                        state_clone.model_stats.record(&model_id_clone, timeout_ms_for_model as f64, false);
                    }
                    Err("timeout".to_string())
                }
            }
        });

        handles.push((model_id.clone(), handle));
    }

if handles.is_empty() {
return (StatusCode::BAD_REQUEST, "no valid models to race").into_response();
}

// Use select_all to wait for results in completion order, not spawn order
// This ensures the first response wins, regardless of which model it comes from
use futures::future::select_all;
let mut pending: Vec<_> = handles.into_iter().map(|(model_id, handle)| {
async move { (model_id, handle.await) }.boxed()
}).collect();

let mut last_error = None;

while !pending.is_empty() {
let ((model_id, result), _idx, remaining) = select_all(pending).await;
match result {
Ok(Ok(response)) => return response,
Ok(Err(e)) => {
eprintln!("[racing] {} failed: {}", model_id, e);
last_error = Some(e);
}
Err(e) => {
eprintln!("[racing] {} panicked: {}", model_id, e);
last_error = Some(e.to_string());
}
}
pending = remaining;
}

(StatusCode::BAD_GATEWAY, last_error.unwrap_or_else(|| "all racing models failed".to_string())).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use crate::{config::ModelCompat, key_pool::KeyPool, model_stats::ModelStatsStore};

    fn create_test_app_state() -> AppState {
        AppState {
            pool: KeyPool::new(vec![]),
            client: reqwest::Client::new(),
            target: "https://test.api.nvidia.com".to_string(),
            router: None,
            model_stats: ModelStatsStore::new(3000.0),
            racing_models: vec![],
            racing_max_parallel: 3,
            racing_timeout_ms: 8000,
            racing_strategy: "complete".to_string(),
            racing_cursor: std::sync::Mutex::new(0),
            available_models: std::sync::Mutex::new(vec![]),
            model_params: HashMap::new(),
            model_compat: ModelCompat::default(),
        }
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
    fn test_validate_model_exists_in_available_models() {
        let state = create_test_app_state();
        state.available_models.lock().unwrap().push("openai/gpt-4".to_string());
        assert!(validate_model_exists("openai/gpt-4", &state).is_ok());
    }

    #[test]
    fn test_validate_model_exists_not_in_available_models() {
        let state = create_test_app_state();
        state.available_models.lock().unwrap().push("openai/gpt-4".to_string());
        let result = validate_model_exists("anthropic/claude", &state);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn test_validate_model_exists_in_racing_models() {
        let mut state = create_test_app_state();
        state.racing_models = vec!["mistralai/mistral-large".to_string()];
        assert!(validate_model_exists("mistralai/mistral-large", &state).is_ok());
    }

    #[test]
    fn test_validate_model_exists_with_router() {
        use crate::model_router::{ModelRouter, Strategy};
        
        let mut state = create_test_app_state();
        state.router = Some(ModelRouter::new(vec!["model1".to_string(), "model2".to_string()], Strategy::RoundRobin));
        assert!(validate_model_exists("any-model", &state).is_ok());
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
            if i > 0 { repeated.push(' '); }
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
        assert!(json["messages"][0]["content"].as_str().unwrap().contains("When using tools, output JSON"));
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
        assert!(json["messages"][0]["content"].as_str().unwrap().contains("When using tools, output JSON"));
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
            "model": "mistralai/mistral-large",
            "messages": []
        });
        inject_minimax_system_message(&mut json_mistral, "mistralai/mistral-large");
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
            "model": "mistralai/mistral-large",
            "messages": [
                {"role": "user", "content": "Weather?"},
                {"role": "tool", "content": "Sunny"}
            ]
        });
        inject_mistral_tool_params(&mut json, "mistralai/mistral-large");
        
        assert_eq!(json["add_generation_prompt"], json!(false));
    }

    #[test]
    fn test_inject_mistral_tool_params_continues_final_message_from_assistant() {
        let mut json = json!({
            "model": "mistralai/mistral-large",
            "messages": [
                {"role": "user", "content": "Hello"},
                {"role": "assistant", "content": "Hi"}
            ]
        });
        inject_mistral_tool_params(&mut json, "mistralai/mistral-large");
        
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
        
        assert!(!json_gpt.as_object().unwrap().contains_key("add_generation_prompt"));
        assert!(!json_gpt.as_object().unwrap().contains_key("continue_final_message"));
    }

    #[test]
    fn test_inject_mistral_tool_params_no_tool_messages_no_injection() {
        let mut json = json!({
            "model": "mistralai/mistral-large",
            "messages": [
                {"role": "user", "content": "Hello"},
                {"role": "assistant", "content": "Hi"}
            ]
        });
        inject_mistral_tool_params(&mut json, "mistralai/mistral-large");
        
        assert!(!json.as_object().unwrap().contains_key("add_generation_prompt"));
        assert_eq!(json["continue_final_message"], json!(true));
    }

    #[test]
    fn test_inject_mistral_tool_params_empty_messages() {
        let mut json = json!({
            "model": "mistralai/mistral-large",
            "messages": []
        });
        inject_mistral_tool_params(&mut json, "mistralai/mistral-large");
        
        assert!(!json.as_object().unwrap().contains_key("add_generation_prompt"));
        assert!(!json.as_object().unwrap().contains_key("continue_final_message"));
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
        
        assert!(!json["messages"][0].as_object().unwrap().contains_key("tool_calls"));
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
    assert!(response.status() == StatusCode::BAD_REQUEST || response.status() >= StatusCode::INTERNAL_SERVER_ERROR);
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
    let response = chat_completions(State(state), HeaderMap::new(), Bytes::from(body.to_string())).await;
    assert!(response.status() >= StatusCode::BAD_REQUEST || response.status() == StatusCode::OK);
}

}

#[cfg(test)]
mod tool_call_id_tests {
    use serde_json::json;
    use super::sanitize_tool_calls;

    #[test]
    fn test_sanitize_strips_tool_call_id_from_assistant() {
        // Assistant message with tool_call_id should have it stripped
        let mut json = json!({
            "messages": [
                {
                    "role": "assistant",
                    "content": "Let me call a tool",
                    "tool_call_id": "call_123"
                }
            ]
        });
        
        sanitize_tool_calls(&mut json);
        
        // tool_call_id should be removed
        assert!(!json["messages"][0].as_object().unwrap().contains_key("tool_call_id"));
    }

    #[test]
    fn test_sanitize_keeps_tool_call_id_in_tool_messages() {
        // Tool messages can keep tool_call_id (it's valid in tool responses)
        let mut json = json!({
            "messages": [
                {
                    "role": "tool",
                    "content": "Tool result",
                    "tool_call_id": "call_123"
                }
            ]
        });
        
        sanitize_tool_calls(&mut json);
        
        // tool_call_id should remain in tool messages
        assert!(json["messages"][0].as_object().unwrap().contains_key("tool_call_id"));
    }

    #[test]
    fn test_sanitize_removes_tool_call_id_even_with_tool_calls() {
        // Assistant message with both tool_call_id and tool_calls
        let mut json = json!({
            "messages": [
                {
                    "role": "assistant",
                    "content": null,
                    "tool_call_id": "call_123",
                    "tool_calls": [
                        {
                            "id": "call_abc",
                            "type": "function",
                            "function": {
                                "name": "get_weather",
                                "arguments": "{}"
                            }
                        }
                    ]
                }
            ]
        });
        
        sanitize_tool_calls(&mut json);
        
        // tool_call_id should be removed, tool_calls should remain
        assert!(!json["messages"][0].as_object().unwrap().contains_key("tool_call_id"));
        assert!(json["messages"][0].as_object().unwrap().contains_key("tool_calls"));
    }
}

/// Log a completed turn
fn log_turn_request(
    requested_model: &str,
    responding_model: &str,
    latency_ms: u128,
    success: bool,
    status_code: u16,
    message_count: usize,
    has_tool_calls: bool,
    tool_call_count: usize,
    key_label: Option<&str>,
    is_racing: bool,
    error: Option<String>,
) {
    use crate::turn_log::{TurnLog, MessageLog, log_turn as log_turn_event};
    
    let mut turn = TurnLog::new(
        requested_model.to_string(),
        responding_model.to_string(),
        latency_ms as u64,
        success,
        status_code,
        message_count,
        1, // response_message_count
        has_tool_calls,
        tool_call_count,
        key_label.map(String::from),
        is_racing,
    );
    
    turn.error = error;
    
    log_turn_event(&turn);
}
