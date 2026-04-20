use std::sync::Arc;
use std::time::Instant;

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures::TryStreamExt;
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

/// Transform unsupported roles in messages:
/// - "developer" → "user" (NVIDIA NIM doesn't support developer role)
/// - "tool" → "assistant" (NVIDIA NIM doesn't support tool role)
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
                if let Some(v) = msg.get_mut("role") {
                    *v = Value::String("assistant".to_string());
                }
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

    for (model_id, handle) in handles {
        match handle.await {
            Ok(Ok(response)) => return response,
            Ok(Err(e)) => {
                eprintln!("[racing] {} failed: {}", model_id, e);
                continue;
            }
            Err(e) => {
                eprintln!("[racing] {} panicked: {}", model_id, e);
                continue;
            }
        }
    }

    (StatusCode::BAD_GATEWAY, "all racing models failed").into_response()
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

#[test]
fn test_is_minimax_model_false_for_non_minimax() {
    assert!(!is_minimax_model("openai/gpt-4"));
    assert!(!is_minimax_model("anthropic/claude"));
    assert!(!is_minimax_model("mistralai/mistral-large"));
}

#[test]
fn test_is_minimax_model_false_for_empty_string() {
    assert!(!is_minimax_model(""));
}

#[test]
fn test_is_minimax_model_false_for_partial_match() {
    assert!(!is_minimax_model("ai/minimaxai-model"));
}

#[test]
fn test_inject_minimax_system_message_no_messages_array() {
    let mut json = json!({
        "model": "minimaxai/minimax-01"
    });
    inject_minimax_system_message(&mut json, "minimaxai/minimax-01");
    assert!(!json.as_object().unwrap().contains_key("messages"));
}

#[test]
fn test_inject_minimax_system_message_messages_not_array() {
    let mut json = json!({
        "model": "minimaxai/minimax-01",
        "messages": "not-an-array"
    });
    inject_minimax_system_message(&mut json, "minimaxai/minimax-01");
    assert_eq!(json["messages"], "not-an-array");
}

#[test]
fn test_transform_message_roles_empty_messages() {
    let state = create_test_app_state();
    let mut json = json!({
        "model": "openai/gpt-4",
        "messages": []
    });
    transform_message_roles(&mut json, "openai/gpt-4", &state);
    assert_eq!(json["messages"].as_array().unwrap().len(), 0);
}

#[test]
fn test_transform_message_roles_developer_role_in_middle() {
    let state = create_test_app_state();
    let mut json = json!({
        "model": "openai/gpt-4",
        "messages": [
            {"role": "user", "content": "Hello"},
            {"role": "developer", "content": "You are helpful"},
            {"role": "assistant", "content": "Hi"}
        ]
    });
    transform_message_roles(&mut json, "openai/gpt-4", &state);
    assert_eq!(json["messages"][1]["role"], "user");
}

#[test]
fn test_sanitize_tool_calls_tool_calls_is_null() {
    let mut json = json!({
        "messages": [
            {"role": "assistant", "tool_calls": null}
        ]
    });
    sanitize_tool_calls(&mut json);
    assert_eq!(json["messages"][0]["tool_calls"], json!(null));
}

#[test]
fn test_sanitize_tool_calls_missing_name_field() {
    let mut json = json!({
        "messages": [
            {
                "role": "assistant",
                "tool_calls": [
                    {"id": "call_1", "function": {"arguments": "{}"}}
                ]
            }
        ]
    });
    sanitize_tool_calls(&mut json);
    assert_eq!(json["messages"][0]["tool_calls"].as_array().unwrap().len(), 1);
}

#[test]
fn test_resolve_model_empty_body() {
    let state = create_test_app_state();
    let body = Bytes::from("{}");
    let (model, _) = resolve_model(body, &state);
    assert_eq!(model, "unknown");
}

#[test]
fn test_resolve_model_auto_model_selection() {
    use crate::model_router::{ModelRouter, Strategy};

    let mut state = create_test_app_state();
    state.router = Some(ModelRouter::new(
        vec!["model1".to_string(), "model2".to_string()],
        Strategy::RoundRobin
    ));
    
    let body = Bytes::from(r#"{"model": "auto"}"#);
    let (model, new_body) = resolve_model(body, &state);
    
    assert_ne!(model, "auto");
    assert!(model == "model1" || model == "model2");
    
    let parsed: Value = serde_json::from_slice(&new_body).unwrap();
    assert_eq!(parsed["model"], model);
}

    // ============ Mocked integration tests using MockNvidiaAPI ============

    #[tokio::test]
    async fn test_chat_completions_success() {
        use crate::mock_http::MockNvidiaAPI;
        use crate::test_utils::MockAppStateBuilder;
        use std::sync::Arc;

        let mut mock_api = MockNvidiaAPI::new().await;
        let mock_url = mock_api.url();
        mock_api.mock_chat_success("nvidia/test-model", "Hello from mock!", 200);

        let state = MockAppStateBuilder::new()
            .with_target(&mock_url)
            .with_custom_keys(vec![crate::KeyEntry {
                key: "test-key".to_string(),
                label: Some("test".to_string()),
            }])
            .build();

        let body = Bytes::from(r#"{"model": "nvidia/test-model", "messages": [{"role": "user", "content": "Hi"}]}"#);
        let response = chat_completions(
            State(Arc::clone(&state)),
            HeaderMap::new(),
            body,
        )
        .await;

        assert_eq!(response.status(), 200);
    }

    #[tokio::test]
    async fn test_chat_completions_streaming() {
        use crate::mock_http::MockNvidiaAPI;
        use crate::test_utils::MockAppStateBuilder;
        use std::sync::Arc;

        let mut mock_api = MockNvidiaAPI::new().await;
        let mock_url = mock_api.url();
        mock_api.mock_chat_stream("nvidia/test-model", vec!["Hello", " from", " streaming"]);

        let state = MockAppStateBuilder::new()
            .with_target(&mock_url)
            .with_custom_keys(vec![crate::KeyEntry {
                key: "test-key".to_string(),
                label: Some("test".to_string()),
            }])
            .build();

        let body = Bytes::from(r#"{"model": "nvidia/test-model", "messages": [{"role": "user", "content": "Hi"}], "stream": true}"#);
        let response = chat_completions(
            State(Arc::clone(&state)),
            HeaderMap::new(),
            body,
        )
        .await;

        assert_eq!(response.status(), 200);
    }

    #[tokio::test]
    async fn test_chat_completions_unauthorized() {
        use crate::mock_http::MockNvidiaAPI;
        use crate::test_utils::MockAppStateBuilder;
        use std::sync::Arc;

        let mut mock_api = MockNvidiaAPI::new().await;
        let mock_url = mock_api.url();
        mock_api.mock_unauthorized();

        let state = MockAppStateBuilder::new()
            .with_target(&mock_url)
            .with_custom_keys(vec![crate::KeyEntry {
                key: "test-key".to_string(),
                label: Some("test".to_string()),
            }])
            .build();

        let body = Bytes::from(r#"{"model": "nvidia/test-model", "messages": [{"role": "user", "content": "Hi"}]}"#);
        let response = chat_completions(
            State(Arc::clone(&state)),
            HeaderMap::new(),
            body,
        )
        .await;

        assert_eq!(response.status(), 401);
    }

    #[tokio::test]
    async fn test_chat_completions_rate_limited() {
        use crate::mock_http::MockNvidiaAPI;
        use crate::test_utils::MockAppStateBuilder;
        use std::sync::Arc;

        let mut mock_api = MockNvidiaAPI::new().await;
        let mock_url = mock_api.url();
        mock_api.mock_rate_limited(60);

        let state = MockAppStateBuilder::new()
            .with_target(&mock_url)
            .with_custom_keys(vec![crate::KeyEntry {
                key: "test-key".to_string(),
                label: Some("test".to_string()),
            }])
            .build();

        let body = Bytes::from(r#"{"model": "nvidia/test-model", "messages": [{"role": "user", "content": "Hi"}]}"#);
        let response = chat_completions(
            State(Arc::clone(&state)),
            HeaderMap::new(),
            body,
        )
        .await;

        assert_eq!(response.status(), 429);
    }

    #[tokio::test]
    async fn test_chat_completions_server_error() {
        use crate::mock_http::MockNvidiaAPI;
        use crate::test_utils::MockAppStateBuilder;
        use std::sync::Arc;

        let mut mock_api = MockNvidiaAPI::new().await;
        let mock_url = mock_api.url();
        mock_api.mock_server_error("Internal server error");

        let state = MockAppStateBuilder::new()
            .with_target(&mock_url)
            .with_custom_keys(vec![crate::KeyEntry {
                key: "test-key".to_string(),
                label: Some("test".to_string()),
            }])
            .build();

        let body = Bytes::from(r#"{"model": "nvidia/test-model", "messages": [{"role": "user", "content": "Hi"}]}"#);
        let response = chat_completions(
            State(Arc::clone(&state)),
            HeaderMap::new(),
            body,
        )
        .await;

        assert_eq!(response.status(), 500);
    }

    #[tokio::test]
    async fn test_chat_completions_empty_messages() {
        use crate::mock_http::MockNvidiaAPI;
        use crate::test_utils::MockAppStateBuilder;
        use std::sync::Arc;

        let mut mock_api = MockNvidiaAPI::new().await;
        let mock_url = mock_api.url();
        mock_api.mock_chat_success("nvidia/test-model", "Response", 200);

        let state = MockAppStateBuilder::new()
            .with_target(&mock_url)
            .with_custom_keys(vec![crate::KeyEntry {
                key: "test-key".to_string(),
                label: Some("test".to_string()),
            }])
            .build();

        let body = Bytes::from(r#"{"model": "nvidia/test-model", "messages": []}"#);
        let response = chat_completions(
            State(Arc::clone(&state)),
            HeaderMap::new(),
            body,
        )
        .await;

        assert_eq!(response.status(), 200);
    }

    #[tokio::test]
    async fn test_chat_completions_model_routing() {
        use crate::mock_http::MockNvidiaAPI;
        use crate::test_utils::MockAppStateBuilder;
        use crate::model_router::{ModelRouter, Strategy};
        use std::sync::Arc;

        let mut mock_api = MockNvidiaAPI::new().await;
        let mock_url = mock_api.url();
        mock_api.mock_chat_success("routed-model", "Routed response", 200);

        let mut state = MockAppStateBuilder::new()
            .with_target(&mock_url)
            .with_custom_keys(vec![crate::KeyEntry {
                key: "test-key".to_string(),
                label: Some("test".to_string()),
            }])
            .build();

        // Use Arc::get_mut to modify the router
        if let Some(state_mut) = Arc::get_mut(&mut state) {
            state_mut.router = Some(ModelRouter::new(
                vec!["routed-model".to_string()],
                Strategy::RoundRobin
            ));
        }

        let body = Bytes::from(r#"{"model": "auto", "messages": [{"role": "user", "content": "Hi"}]}"#);
        let response = chat_completions(
            State(Arc::clone(&state)),
            HeaderMap::new(),
            body,
        )
        .await;

        assert_eq!(response.status(), 200);
    }

    #[tokio::test]
    async fn test_race_models_two_models() {
        use crate::mock_http::MockNvidiaAPI;
        use crate::test_utils::MockAppStateBuilder;
        use std::sync::Arc;

        let mut mock_api = MockNvidiaAPI::new().await;
        let mock_url = mock_api.url();
        mock_api.mock_chat_success("nvidia/model-1", "Model 1 wins", 200);

        let state = MockAppStateBuilder::new()
            .with_target(&mock_url)
            .with_custom_keys(vec![crate::KeyEntry {
                key: "test-key".to_string(),
                label: Some("test".to_string()),
            }])
            .with_custom_racing_models(vec![
                "nvidia/model-1".to_string(),
                "nvidia/model-2".to_string(),
            ])
            .with_racing_max_parallel(2)
            .with_racing_timeout_ms(5000)
            .build();

        let body = Bytes::from(r#"{"model": "auto", "messages": [{"role": "user", "content": "Hi"}]}"#);
        let response = chat_completions(
            State(Arc::clone(&state)),
            HeaderMap::new(),
            body,
        )
        .await;

        assert_eq!(response.status(), 200);
    }

    #[tokio::test]
    async fn test_race_models_all_fail() {
        use crate::mock_http::MockNvidiaAPI;
        use crate::test_utils::MockAppStateBuilder;
        use std::sync::Arc;

        let mut mock_api = MockNvidiaAPI::new().await;
        let mock_url = mock_api.url();
        mock_api.mock_server_error("Backend failure");

        let state = MockAppStateBuilder::new()
            .with_target(&mock_url)
            .with_custom_keys(vec![crate::KeyEntry {
                key: "test-key".to_string(),
                label: Some("test".to_string()),
            }])
            .with_custom_racing_models(vec![
                "nvidia/model-1".to_string(),
                "nvidia/model-2".to_string(),
            ])
            .with_racing_max_parallel(2)
            .with_racing_timeout_ms(5000)
            .build();

        let body = Bytes::from(r#"{"model": "auto", "messages": [{"role": "user", "content": "Hi"}]}"#);
        let response = chat_completions(
            State(Arc::clone(&state)),
            HeaderMap::new(),
            body,
        )
        .await;

        assert_eq!(response.status(), 500);
    }

    #[tokio::test]
    async fn test_race_models_timeout() {
        use crate::mock_http::MockNvidiaAPI;
        use crate::test_utils::MockAppStateBuilder;
        use std::sync::Arc;

        let mut mock_api = MockNvidiaAPI::new().await;
        let mock_url = mock_api.url();
        mock_api.mock_server_error("Timeout simulation");

        let state = MockAppStateBuilder::new()
            .with_target(&mock_url)
            .with_custom_keys(vec![crate::KeyEntry {
                key: "test-key".to_string(),
                label: Some("test".to_string()),
            }])
            .with_custom_racing_models(vec![
                "nvidia/slow-model".to_string(),
            ])
            .with_racing_max_parallel(1)
            .with_racing_timeout_ms(100)
            .build();

        let body = Bytes::from(r#"{"model": "auto", "messages": [{"role": "user", "content": "Hi"}]}"#);
        let response = chat_completions(
            State(Arc::clone(&state)),
            HeaderMap::new(),
            body,
        )
        .await;

        assert_eq!(response.status(), 500);
    }

    #[test]
    fn test_resolve_model_custom_params() {
        use std::collections::HashMap;
        use crate::ModelParams;

        let mut state = create_test_app_state();
        let mut model_params = HashMap::new();
        model_params.insert("nvidia/test".to_string(), ModelParams {
            temperature: Some(0.8),
            max_tokens: Some(2048),
            ..Default::default()
        });
        state.model_params = model_params;

        let body = Bytes::from(r#"{"model": "nvidia/test", "messages": [], "temperature": 0.5}"#);
        let (_, new_body) = resolve_model(body, &state);

        let parsed: Value = serde_json::from_slice(&new_body).unwrap();
        assert_eq!(parsed["temperature"], 0.8);
        assert_eq!(parsed["max_tokens"], 2048);
    }

    #[test]
    fn test_resolve_model_invalid_model() {
        let state = create_test_app_state();
        let body = Bytes::from(r#"{"model": 123, "messages": []}"#);
        let (model, _) = resolve_model(body, &state);

        assert_eq!(model, "unknown");
    }

    #[tokio::test]
    async fn test_streaming_response() {
        use crate::mock_http::MockNvidiaAPI;
        use crate::test_utils::MockAppStateBuilder;
        use std::sync::Arc;

        let mut mock_api = MockNvidiaAPI::new().await;
        let mock_url = mock_api.url();
        mock_api.mock_chat_stream("nvidia/stream-model", vec!["Chunk1", "Chunk2", "Chunk3"]);

        let state = MockAppStateBuilder::new()
            .with_target(&mock_url)
            .with_custom_keys(vec![crate::KeyEntry {
                key: "test-key".to_string(),
                label: Some("test".to_string()),
            }])
            .build();

        let body = Bytes::from(r#"{"model": "nvidia/stream-model", "messages": [{"role": "user", "content": "Stream it"}], "stream": true}"#);
        let response = chat_completions(
            State(Arc::clone(&state)),
            HeaderMap::new(),
            body,
        )
        .await;

        assert_eq!(response.status(), 200);
    }

    #[tokio::test]
    async fn test_error_handling_in_chain() {
        use crate::mock_http::MockNvidiaAPI;
        use crate::test_utils::MockAppStateBuilder;
        use std::sync::Arc;

        let mut mock_api = MockNvidiaAPI::new().await;
        let mock_url = mock_api.url();
        mock_api.mock_not_found();

        let state = MockAppStateBuilder::new()
            .with_target(&mock_url)
            .with_custom_keys(vec![crate::KeyEntry {
                key: "test-key".to_string(),
                label: Some("test".to_string()),
            }])
            .build();

        let body = Bytes::from(r#"{"model": "nonexistent-model", "messages": [{"role": "user", "content": "Error test"}]}"#);
        let response = chat_completions(
            State(Arc::clone(&state)),
            HeaderMap::new(),
            body,
        )
        .await;

        assert_eq!(response.status(), 404);
    }
    
    // ============ Edge case tests for coverage ============
    
    #[tokio::test]
    async fn test_chat_completions_model_not_found() {
        use crate::mock_http::MockNvidiaAPI;
        use crate::test_utils::MockAppStateBuilder;
        use std::sync::Arc;
    
        let mut mock_api = MockNvidiaAPI::new().await;
        let mock_url = mock_api.url();
        mock_api.mock_not_found();
    
        let state = MockAppStateBuilder::new()
            .with_target(&mock_url)
            .with_custom_keys(vec![crate::KeyEntry {
                key: "test-key".to_string(),
                label: Some("test".to_string()),
            }])
            .build();
    
        let body = Bytes::from(r#"{"model": "nonexistent-model", "messages": [{"role": "user", "content": "Hi"}]}"#);
        let response = chat_completions(
            State(Arc::clone(&state)),
            HeaderMap::new(),
            body,
        )
        .await;
    
        assert_eq!(response.status(), 404);
    }
    
    #[tokio::test]
    async fn test_chat_completions_bad_request() {
        use crate::mock_http::MockNvidiaAPI;
        use crate::test_utils::MockAppStateBuilder;
        use std::sync::Arc;
    
        let mut mock_api = MockNvidiaAPI::new().await;
        let mock_url = mock_api.url();
        mock_api.mock_bad_request("Invalid request body");
    
        let state = MockAppStateBuilder::new()
            .with_target(&mock_url)
            .with_custom_keys(vec![crate::KeyEntry {
                key: "test-key".to_string(),
                label: Some("test".to_string()),
            }])
            .build();
    
        let body = Bytes::from(r#"{"model": "nvidia/test-model", "messages": [{"role": "user", "content": "Hi"}]}"#);
        let response = chat_completions(
            State(Arc::clone(&state)),
            HeaderMap::new(),
            body,
        )
        .await;
    
        assert_eq!(response.status(), 400);
    }
    
    #[tokio::test]
    async fn test_chat_completions_network_timeout() {
        use crate::mock_http::MockNvidiaAPI;
        use crate::test_utils::MockAppStateBuilder;
        use std::sync::Arc;
    
        let mut mock_api = MockNvidiaAPI::new().await;
        let mock_url = mock_api.url();
        mock_api.mock_timeout();
    
        let state = MockAppStateBuilder::new()
            .with_target(&mock_url)
            .with_custom_keys(vec![crate::KeyEntry {
                key: "test-key".to_string(),
                label: Some("test".to_string()),
            }])
            .build();
    
        let body = Bytes::from(r#"{"model": "nvidia/test-model", "messages": [{"role": "user", "content": "Hi"}]}"#);
        let response = chat_completions(
            State(Arc::clone(&state)),
            HeaderMap::new(),
            body,
        )
        .await;
    
        // Timeout results in 504 Gateway Timeout
        assert_eq!(response.status(), 504);
    }
    
    #[tokio::test]
    async fn test_race_models_one_fails_one_succeeds() {
        use crate::mock_http::MockNvidiaAPI;
        use crate::test_utils::MockAppStateBuilder;
        use std::sync::Arc;
    
        let mut mock_api = MockNvidiaAPI::new().await;
        let mock_url = mock_api.url();
        mock_api.mock_chat_success("nvidia/model-success", "Success response", 200);
    
        let state = MockAppStateBuilder::new()
            .with_target(&mock_url)
            .with_custom_keys(vec![crate::KeyEntry {
                key: "test-key".to_string(),
                label: Some("test".to_string()),
            }])
            .with_custom_racing_models(vec![
                "nvidia/model-success".to_string(),
                "nvidia/model-fail".to_string(),
            ])
            .with_racing_max_parallel(2)
            .with_racing_timeout_ms(5000)
            .build();
    
        let body = Bytes::from(r#"{"model": "auto", "messages": [{"role": "user", "content": "Hi"}]}"#);
        let response = chat_completions(
            State(Arc::clone(&state)),
            HeaderMap::new(),
            body,
        )
        .await;
    
        assert_eq!(response.status(), 200);
    }
    
    #[tokio::test]
    async fn test_race_models_concurrent_execution() {
        use crate::mock_http::MockNvidiaAPI;
        use crate::test_utils::MockAppStateBuilder;
        use std::sync::Arc;
    
        let mut mock_api = MockNvidiaAPI::new().await;
        let mock_url = mock_api.url();
        mock_api.mock_chat_success("nvidia/model-a", "Model A response", 200);
    
        let state = MockAppStateBuilder::new()
            .with_target(&mock_url)
            .with_custom_keys(vec![crate::KeyEntry {
                key: "test-key".to_string(),
                label: Some("test".to_string()),
            }])
            .with_custom_racing_models(vec![
                "nvidia/model-a".to_string(),
                "nvidia/model-b".to_string(),
            ])
            .with_racing_max_parallel(2)
            .with_racing_timeout_ms(5000)
            .build();
    
        let body = Bytes::from(r#"{"model": "auto", "messages": [{"role": "user", "content": "Hi"}]}"#);
        let response = chat_completions(
            State(Arc::clone(&state)),
            HeaderMap::new(),
            body,
        )
        .await;
    
        assert_eq!(response.status(), 200);
    }
    
    #[test]
    fn test_resolve_model_with_all_params() {
        use std::collections::HashMap;
        use crate::ModelParams;
    
        let mut state = create_test_app_state();
        let mut model_params = HashMap::new();
        model_params.insert("nvidia/test".to_string(), ModelParams {
            temperature: Some(0.8),
            top_p: Some(0.9),
            top_k: Some(50),
            frequency_penalty: Some(0.5),
            presence_penalty: Some(0.3),
            max_tokens: Some(2048),
            min_p: None,
            reasoning_effort: Some("medium".to_string()),
            seed: Some(42),
            chat_template_kwargs: Some(HashMap::from([
                ("extra_param".to_string(), serde_json::Value::String("extra_value".to_string())),
            ])),
        });
        state.model_params = model_params;
    
        let body = Bytes::from(r#"{"model": "nvidia/test", "messages": []}"#);
        let (_, new_body) = resolve_model(body, &state);
    
        let parsed: Value = serde_json::from_slice(&new_body).unwrap();
        assert_eq!(parsed["temperature"], 0.8);
        assert_eq!(parsed["top_p"], 0.9);
        assert_eq!(parsed["top_k"], 50);
        assert_eq!(parsed["frequency_penalty"], 0.5);
        assert_eq!(parsed["presence_penalty"], 0.3);
        assert_eq!(parsed["max_tokens"], 2048);
        assert_eq!(parsed["reasoning_effort"], "medium");
        assert_eq!(parsed["seed"], 42);
        assert_eq!(parsed["extra_param"], "extra_value");
    }
    
    #[tokio::test]
    async fn test_stream_chunk_processing() {
        use crate::mock_http::MockNvidiaAPI;
        use crate::test_utils::MockAppStateBuilder;
        use std::sync::Arc;
    
        let mut mock_api = MockNvidiaAPI::new().await;
        let mock_url = mock_api.url();
        mock_api.mock_chat_stream("nvidia/stream-model", vec!["Hello", ", ", "world", "!"]);
    
        let state = MockAppStateBuilder::new()
            .with_target(&mock_url)
            .with_custom_keys(vec![crate::KeyEntry {
                key: "test-key".to_string(),
                label: Some("test".to_string()),
            }])
            .build();
    
        let body = Bytes::from(r#"{"model": "nvidia/stream-model", "messages": [{"role": "user", "content": "Hi"}], "stream": true}"#);
        let response = chat_completions(
            State(Arc::clone(&state)),
            HeaderMap::new(),
            body,
        )
        .await;
    
        assert_eq!(response.status(), 200);
    }
    
    #[tokio::test]
    async fn test_error_response_parsing() {
        use crate::mock_http::MockNvidiaAPI;
        use crate::test_utils::MockAppStateBuilder;
        use std::sync::Arc;
    
        let mut mock_api = MockNvidiaAPI::new().await;
        let mock_url = mock_api.url();
        mock_api.mock_server_error("Internal server error");
    
        let state = MockAppStateBuilder::new()
            .with_target(&mock_url)
            .with_custom_keys(vec![crate::KeyEntry {
                key: "test-key".to_string(),
                label: Some("test".to_string()),
            }])
            .build();
    
        let body = Bytes::from(r#"{"model": "nvidia/test-model", "messages": [{"role": "user", "content": "Hi"}]}"#);
        let response = chat_completions(
            State(Arc::clone(&state)),
            HeaderMap::new(),
            body,
        )
        .await;
    
        assert_eq!(response.status(), 500);
    }
    
    #[tokio::test]
    async fn test_retry_logic() {
        use crate::mock_http::MockNvidiaAPI;
        use crate::test_utils::MockAppStateBuilder;
        use std::sync::Arc;
    
        let mut mock_api = MockNvidiaAPI::new().await;
        let mock_url = mock_api.url();
        mock_api.mock_rate_limited(0);
    
        let state = MockAppStateBuilder::new()
            .with_target(&mock_url)
            .with_custom_keys(vec![
                crate::KeyEntry {
                    key: "test-key-1".to_string(),
                    label: Some("test1".to_string()),
                },
                crate::KeyEntry {
                    key: "test-key-2".to_string(),
                    label: Some("test2".to_string()),
                },
            ])
            .build();
    
        let body = Bytes::from(r#"{"model": "nvidia/test-model", "messages": [{"role": "user", "content": "Hi"}]}"#);
        let response = chat_completions(
            State(Arc::clone(&state)),
            HeaderMap::new(),
            body,
        )
        .await;
    
        assert_eq!(response.status(), 429);
    }
    
    #[tokio::test]
    async fn test_circuit_breaker_integration() {
        use crate::mock_http::MockNvidiaAPI;
        use crate::test_utils::MockAppStateBuilder;
        use std::sync::Arc;
    
        let mut mock_api = MockNvidiaAPI::new().await;
        let mock_url = mock_api.url();
        mock_api.mock_chat_success("nvidia/test-model", "{\"usage\": {\"completion_tokens\": 10}, \"choices\": []}", 200);
    
        let state = MockAppStateBuilder::new()
            .with_target(&mock_url)
            .with_custom_keys(vec![crate::KeyEntry {
                key: "test-key".to_string(),
                label: Some("test".to_string()),
            }])
            .build();
    
        let body = Bytes::from(r#"{"model": "nvidia/test-model", "messages": [{"role": "user", "content": "Hi"}]}"#);
        let response = chat_completions(
            State(Arc::clone(&state)),
            HeaderMap::new(),
            body,
        )
        .await;
    
        assert_eq!(response.status(), 200);
        
        let snapshots = state.model_stats.snapshot();
        assert!(!snapshots.is_empty());
}
    
    // ============ Endpoint tests (health, stats, models) ============
    
    #[tokio::test]
    async fn test_health_endpoint() {
        use crate::test_utils::MockAppStateBuilder;
        use std::sync::Arc;
    
        let state = MockAppStateBuilder::new()
            .with_custom_keys(vec![
                crate::KeyEntry {
                    key: "test-key-1".to_string(),
                    label: Some("test1".to_string()),
                },
                crate::KeyEntry {
                    key: "test-key-2".to_string(),
                    label: Some("test2".to_string()),
                },
            ])
            .build();
    
        let response = health(State(Arc::clone(&state))).await.into_response();
        assert_eq!(response.status(), 200);
    }
    
    #[tokio::test]
    async fn test_health_endpoint_degraded() {
        use crate::test_utils::MockAppStateBuilder;
        use std::sync::Arc;
    
        // Empty key pool = degraded
        let state = MockAppStateBuilder::new()
            .with_custom_keys(vec![])
            .build();
    
        let response = health(State(Arc::clone(&state))).await.into_response();
        assert_eq!(response.status(), 200);
    }
    
    #[tokio::test]
    async fn test_stats_endpoint() {
        use crate::test_utils::MockAppStateBuilder;
        use std::sync::Arc;
    
        let state = MockAppStateBuilder::new()
            .with_custom_keys(vec![crate::KeyEntry {
                key: "test-key".to_string(),
                label: Some("test".to_string()),
            }])
            .with_custom_racing_models(vec!["model1".to_string(), "model2".to_string()])
            .with_racing_max_parallel(2)
            .with_racing_timeout_ms(5000)
            .build();
    
        // Record some stats
        state.model_stats.record("test-model", 100.0, true);
        state.model_stats.record("test-model", 150.0, true);
    
        let response = stats(State(Arc::clone(&state))).await.into_response();
        assert_eq!(response.status(), 200);
    }
    
    #[tokio::test]
    async fn test_models_endpoint() {
        use crate::mock_http::MockNvidiaAPI;
        use crate::test_utils::MockAppStateBuilder;
        use std::sync::Arc;
    
        let mut mock_api = MockNvidiaAPI::new().await;
        let mock_url = mock_api.url();
        mock_api.mock_models_success(vec!["nvidia/test-model", "nvidia/another-model"]);
    
        let state = MockAppStateBuilder::new()
            .with_target(&mock_url)
            .with_custom_keys(vec![crate::KeyEntry {
                key: "test-key".to_string(),
                label: Some("test".to_string()),
            }])
            .build();
    
        let response = models(State(Arc::clone(&state))).await.into_response();
        assert_eq!(response.status(), 200);
    }
    
    #[tokio::test]
    async fn test_models_endpoint_no_keys() {
        use crate::test_utils::MockAppStateBuilder;
        use std::sync::Arc;
    
        let state = MockAppStateBuilder::new()
            .with_custom_keys(vec![])
            .build();
    
        let response = models(State(Arc::clone(&state))).await.into_response();
        assert_eq!(response.status(), 429);
    }
    
    // ============ Additional edge cases for coverage ============
    
    #[test]
    fn test_resolve_model_auto_with_params() {
        use crate::model_router::{ModelRouter, Strategy};
        use std::collections::HashMap;
        use crate::ModelParams;
    
        let mut state = create_test_app_state();
        state.router = Some(ModelRouter::new(
            vec!["model1".to_string()],
            Strategy::RoundRobin
        ));
        
        // Add model params
        let mut model_params = HashMap::new();
        model_params.insert("model1".to_string(), ModelParams {
            temperature: Some(0.7),
            top_p: Some(0.95),
            max_tokens: Some(1024),
            min_p: None,
            ..Default::default()
        });
        state.model_params = model_params;
    
        let body = Bytes::from(r#"{"model": "auto", "messages": []}"#);
        let (_, new_body) = resolve_model(body, &state);
    
        let parsed: Value = serde_json::from_slice(&new_body).unwrap();
        assert_eq!(parsed["model"], "model1");
        assert_eq!(parsed["temperature"], 0.7);
        assert_eq!(parsed["top_p"], 0.95);
        assert_eq!(parsed["max_tokens"], 1024);
    }
    
    #[test]
    fn test_resolve_model_invalid_json_returns_unknown() {
        let state = create_test_app_state();
        // Invalid JSON body
        let body = Bytes::from(b"not valid json".to_vec());
        let (model, _) = resolve_model(body, &state);
        assert_eq!(model, "unknown");
    }
    
    #[tokio::test]
    async fn test_chat_completions_no_keys_exhausted() {
        use crate::mock_http::MockNvidiaAPI;
        use crate::test_utils::MockAppStateBuilder;
        use std::sync::Arc;
    
        let mut mock_api = MockNvidiaAPI::new().await;
        let mock_url = mock_api.url();
        // All keys rate limited immediately
        mock_api.mock_rate_limited(60);
    
        let state = MockAppStateBuilder::new()
            .with_target(&mock_url)
            .with_custom_keys(vec![crate::KeyEntry {
                key: "test-key".to_string(),
                label: Some("test".to_string()),
            }])
            .build();
    
        let body = Bytes::from(r#"{"model": "nvidia/test-model", "messages": [{"role": "user", "content": "Hi"}]}"#);
        let response = chat_completions(
            State(Arc::clone(&state)),
            HeaderMap::new(),
            body,
        )
        .await;
    
        // When all keys are rate limited, should get 429
        assert_eq!(response.status(), 429);
}
// ============ Targeted coverage tests for uncovered lines ============

#[test]
fn test_count_repetitions_exactly_three_words() {
    assert_eq!(count_repetitions("one two three"), 0);
}

#[test]
fn test_count_repetitions_six_words_repeating() {
    let result = count_repetitions("one two three one two three");
    assert!(result > 0);
}

#[test]
fn test_extract_response_metrics_empty_usage() {
    let json = r#"{"usage": {}, "choices": []}"#;
    let (tokens, reps, tool) = extract_response_metrics(json);
    assert!(tokens > 0);
    assert_eq!(reps, 0);
    assert_eq!(tool, false);
}

#[test]
fn test_extract_response_metrics_non_numeric_tokens() {
    let json = r#"{"usage": {"completion_tokens": "not_a_number"}, "choices": []}"#;
    let (tokens, reps, tool) = extract_response_metrics(json);
    assert!(tokens > 0);
}

#[test]
fn test_validate_model_exists_in_racing_models_list() {
    let mut state = create_test_app_state();
    state.racing_models = vec!["test-model".to_string()];
    assert!(validate_model_exists("test-model", &state).is_ok());
}

#[test]
fn test_transform_message_roles_no_transformation_needed() {
    let state = create_test_app_state();
    let mut json = json!({
        "messages": [
            {"role": "user", "content": "Hello"},
            {"role": "assistant", "content": "Hi"}
        ]
    });
    transform_message_roles(&mut json, "openai/gpt-4", &state);
    assert_eq!(json["messages"][0]["role"], "user");
    assert_eq!(json["messages"][1]["role"], "assistant");
}

#[test]
fn test_transform_message_roles_transforms_tool_role() {
    let state = create_test_app_state();
    let mut json = json!({
        "messages": [
            {"role": "user", "content": "Hello"},
            {"role": "tool", "content": "Result"}
        ]
    });
    transform_message_roles(&mut json, "openai/gpt-4", &state);
    assert_eq!(json["messages"][1]["role"], "assistant");
}

#[test]
fn test_resolve_model_injects_top_k() {
    use std::collections::HashMap;
    use crate::ModelParams;

    let mut state = create_test_app_state();
    let mut model_params = HashMap::new();
    model_params.insert("test-model".to_string(), ModelParams {
        temperature: None,
        max_tokens: None,
        top_p: None,
        top_k: Some(50),
        frequency_penalty: None,
        presence_penalty: None,
        min_p: None,
        reasoning_effort: None,
        seed: None,
        chat_template_kwargs: None,
    });
    state.model_params = model_params;

    let body = Bytes::from(r#"{"model": "test-model", "messages": []}"#);
    let (_, new_body) = resolve_model(body, &state);
    let parsed: Value = serde_json::from_slice(&new_body).unwrap();
    assert_eq!(parsed["top_k"], 50);
}

#[test]
fn test_resolve_model_injects_frequency_penalty() {
    use std::collections::HashMap;
    use crate::ModelParams;

    let mut state = create_test_app_state();
    let mut model_params = HashMap::new();
    model_params.insert("test-model".to_string(), ModelParams {
        temperature: None,
        max_tokens: None,
        top_p: None,
        top_k: None,
        frequency_penalty: Some(0.5),
        presence_penalty: None,
        min_p: None,
        reasoning_effort: None,
        seed: None,
        chat_template_kwargs: None,
    });
    state.model_params = model_params;

    let body = Bytes::from(r#"{"model": "test-model", "messages": []}"#);
    let (_, new_body) = resolve_model(body, &state);
    let parsed: Value = serde_json::from_slice(&new_body).unwrap();
    assert_eq!(parsed["frequency_penalty"], 0.5);
}

#[test]
fn test_resolve_model_injects_presence_penalty() {
    use std::collections::HashMap;
    use crate::ModelParams;

    let mut state = create_test_app_state();
    let mut model_params = HashMap::new();
    model_params.insert("test-model".to_string(), ModelParams {
        temperature: None,
        max_tokens: None,
        top_p: None,
        top_k: None,
        frequency_penalty: None,
        presence_penalty: Some(0.3),
        min_p: None,
        reasoning_effort: None,
        seed: None,
        chat_template_kwargs: None,
    });
    state.model_params = model_params;

    let body = Bytes::from(r#"{"model": "test-model", "messages": []}"#);
    let (_, new_body) = resolve_model(body, &state);
    let parsed: Value = serde_json::from_slice(&new_body).unwrap();
    assert_eq!(parsed["presence_penalty"], 0.3);
}

#[test]
fn test_resolve_model_injects_seed() {
    use std::collections::HashMap;
    use crate::ModelParams;

    let mut state = create_test_app_state();
    let mut model_params = HashMap::new();
    model_params.insert("test-model".to_string(), ModelParams {
        temperature: None,
        max_tokens: None,
        top_p: None,
        top_k: None,
        frequency_penalty: None,
        presence_penalty: None,
        min_p: None,
        reasoning_effort: None,
        seed: Some(42),
        chat_template_kwargs: None,
    });
    state.model_params = model_params;

    let body = Bytes::from(r#"{"model": "test-model", "messages": []}"#);
    let (_, new_body) = resolve_model(body, &state);
    let parsed: Value = serde_json::from_slice(&new_body).unwrap();
    assert_eq!(parsed["seed"], 42);
}
}


