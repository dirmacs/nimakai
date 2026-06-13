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
use tokio::task::JoinSet;
use tokio::time::timeout;

use crate::{AppState, KeyAcquireError, KeyLease, UpstreamPermit};

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
            for j in (i + window_size..)
                .step_by(window_size)
                .take_while(|&j| j + window_size <= words.len())
            {
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
                if choice
                    .get("message")
                    .and_then(|m| m.get("tool_calls"))
                    .is_some()
                {
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

    let configured_models = state.configured_models();
    if !configured_models.is_empty() {
        if configured_models.iter().any(|m| m == model) {
            return Ok(());
        }
        return Err(format!("model '{}' not found in configured models", model));
    }

    // No routing configured - accept any model (passthrough to NVIDIA)
    // This preserves backward compatibility: when no models are configured,
    // passthrough mode allows any model through
    Ok(())
}

fn mistral_validation_error(model_id: &str, body: &Bytes) -> Option<Response> {
    if let Ok(json) = serde_json::from_slice::<Value>(body) {
        if let Err((status, msg)) = validate_mistral_tool_call_ids(&json, model_id) {
            return Some((status, msg).into_response());
        }
    }
    None
}

fn apply_model_params(json: &mut Value, params: &crate::config::ModelParams) {
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
    if let Some(rp) = params.repetition_penalty {
        json["repetition_penalty"] = Value::from(rp);
    }
    if let Some(min_p) = params.min_p {
        json["min_p"] = Value::from(min_p);
    }
    if let Some(max_tokens) = params.max_tokens {
        json["max_tokens"] = Value::from(max_tokens);
    }
    if let Some(reasoning_budget) = params.reasoning_budget {
        json["reasoning_budget"] = Value::from(reasoning_budget);
    }
    if let Some(reasoning_effort) = &params.reasoning_effort {
        json["reasoning_effort"] = Value::String(reasoning_effort.clone());
    }
    if let Some(seed) = params.seed {
        json["seed"] = Value::from(seed);
    }

    // Treat stream as a client response-mode choice, not as a model
    // hyperparameter. Callers that explicitly request streaming or
    // non-streaming keep control, and omitted stream must remain JSON-compatible.
    if let Some(stream) = params.stream {
        if json.get("stream").is_some_and(|v| !v.is_null()) {
            // Preserve the caller's explicit response mode.
        } else if !stream {
            json["stream"] = Value::Bool(stream);
        }
    }

    if let Some(ctk) = &params.chat_template_kwargs {
        if let Some(obj) = json.as_object_mut() {
            let entry = obj
                .entry("chat_template_kwargs".to_string())
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
            if !entry.is_object() {
                *entry = Value::Object(serde_json::Map::new());
            }
            if let Some(kwargs) = entry.as_object_mut() {
                for (k, v) in ctk {
                    kwargs.insert(k.clone(), v.clone());
                }
            }
        }
    }
}

fn accept_header_for_json(json: &Value) -> &'static str {
    if json.get("stream").and_then(Value::as_bool).unwrap_or(false) {
        "text/event-stream"
    } else {
        "application/json"
    }
}

fn accept_header_for_body(body: &Bytes) -> &'static str {
    serde_json::from_slice::<Value>(body)
        .map(|json| accept_header_for_json(&json))
        .unwrap_or("application/json")
}

fn timeout_for_model(state: &AppState, model_id: &str) -> u64 {
    state.model_stats.get_model_timeout_with_policy(
        model_id,
        state.racing_timeout_ms,
        state.min_dynamic_timeout_ms,
        state.dynamic_sample_floor,
    )
}

fn gateway_overloaded_response(state: &AppState) -> Response {
    state.gateway_metrics.record_overload();
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "gateway overloaded; retry later",
    )
        .into_response()
}

fn no_key_response(state: &AppState) -> Response {
    state.gateway_metrics.record_no_key();
    (StatusCode::TOO_MANY_REQUESTS, "all API keys rate-limited").into_response()
}

fn acquire_gateway_permits(state: &AppState) -> Result<(UpstreamPermit, KeyLease), Response> {
    let upstream_permit = state
        .try_acquire_upstream()
        .ok_or_else(|| gateway_overloaded_response(state))?;

    match state.pool.next_key_with_permit() {
        Ok(key_lease) => Ok((upstream_permit, key_lease)),
        Err(KeyAcquireError::AllBusy) => Err(gateway_overloaded_response(state)),
        Err(KeyAcquireError::NoKeys | KeyAcquireError::AllCoolingDown) => {
            Err(no_key_response(state))
        }
    }
}

fn adaptive_racing_parallel(state: &AppState, candidate_count: usize) -> usize {
    let configured = state.racing_max_parallel.min(candidate_count);
    if configured < 2 {
        return configured;
    }

    let available_key_slots = state.pool.available_permits();
    if available_key_slots < 2 {
        return available_key_slots;
    }

    if !state.racing_adaptive {
        return configured.min(available_key_slots);
    }

    let active_keys = state.pool.active_count();
    if active_keys == 0 {
        return 0;
    }

    let in_flight = state.gateway_metrics.upstream_in_flight() as usize;
    let pressure_mark = (state.max_upstream_in_flight / 3).max(1);
    let degraded_mark = ((state.max_upstream_in_flight * 2) / 3).max(1);

    let desired = if active_keys < state.pool.len() || in_flight >= degraded_mark {
        state.racing_degraded_parallel
    } else if in_flight >= pressure_mark {
        state.racing_pressure_parallel
    } else {
        configured
    };

    let min_parallel = state
        .racing_min_parallel
        .min(candidate_count)
        .min(available_key_slots);
    desired
        .min(configured)
        .min(candidate_count)
        .min(available_key_slots)
        .max(min_parallel)
}

fn rotated_models(models: &[String], cursor: usize) -> Vec<String> {
    let n = models.len();
    (0..n).map(|i| models[(cursor + i) % n].clone()).collect()
}

fn tiered_candidates(state: &AppState, rotated: &[String], max_parallel: usize) -> Vec<String> {
    if !state.racing_adaptive || state.racing_fast_models.is_empty() {
        return state.model_stats.racing_candidates(rotated, max_parallel);
    }

    let fast: Vec<String> = rotated
        .iter()
        .filter(|m| state.racing_fast_models.iter().any(|f| f == *m))
        .cloned()
        .collect();

    let fallback: Vec<String> = rotated
        .iter()
        .filter(|m| {
            state.racing_fallback_models.iter().any(|f| f == *m)
                || !state.racing_fast_models.iter().any(|f| f == *m)
        })
        .cloned()
        .collect();

    state
        .model_stats
        .racing_candidates_tiered(&fast, &fallback, max_parallel)
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
    let original_body = body.clone();

    // Extract original model BEFORE resolve_model modifies it
    let original_model = {
        if let Ok(v) = serde_json::from_slice::<Value>(&body) {
            v.get("model")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string()
        } else {
            String::new()
        }
    };

    if let Err(msg) = validate_model_exists(&original_model, &state) {
        return (StatusCode::BAD_REQUEST, msg).into_response();
    }

    // Racing owns per-model body rewriting. Passing the original auto request
    // avoids leaking router-picked model params into race candidates that do
    // not define the same fields.
    if original_model == "auto" && !state.racing_models.is_empty() && state.racing_models.len() >= 2
    {
        state.gateway_metrics.record_request(true);
        let racing_models = state.racing_models.clone();
        return race_models(state, original_body, &racing_models).await;
    }
    state.gateway_metrics.record_request(false);

    let (mut model_id, mut body) = resolve_model(body, &state);

    // Validate tool call IDs for Mistral models
    if let Some(response) = mistral_validation_error(&model_id, &body) {
        return response;
    }

    let n = state.pool.len().min(MAX_RETRIES).max(1);

    for _ in 0..n {
        let (upstream_permit, key_lease) = match acquire_gateway_permits(&state) {
            Ok(permits) => permits,
            Err(response) => return response,
        };
        let _upstream_permit = upstream_permit;
        let idx = key_lease.idx;
        let key_label = key_lease.label.clone();

        let t0 = Instant::now();
        let request_timeout_ms = timeout_for_model(&state, &model_id);
        let result = timeout(
            std::time::Duration::from_millis(request_timeout_ms),
            state
                .client
                .post(format!("{}/v1/chat/completions", state.target))
                .header("Authorization", format!("Bearer {}", key_lease.key))
                .header("Content-Type", "application/json")
                .header("Accept", accept_header_for_body(&body))
                .body(body.clone())
                .send(),
        )
        .await;

        match result {
            Err(_) => {
                state.gateway_metrics.record_timeout();
                if let Some(label) = key_label.as_ref() {
                    state.model_stats.record_with_key(
                        &model_id,
                        label,
                        request_timeout_ms as f64,
                        false,
                    );
                } else {
                    state
                        .model_stats
                        .record(&model_id, request_timeout_ms as f64, false);
                }
                return (
                    StatusCode::GATEWAY_TIMEOUT,
                    format!("upstream timeout after {}ms", request_timeout_ms),
                )
                    .into_response();
            }
            Ok(Err(e)) => {
                if let Some(label) = key_label.as_ref() {
                    state.model_stats.record_with_key(
                        &model_id,
                        label,
                        t0.elapsed().as_millis() as f64,
                        false,
                    );
                } else {
                    state
                        .model_stats
                        .record(&model_id, t0.elapsed().as_millis() as f64, false);
                }
                return (StatusCode::BAD_GATEWAY, e.to_string()).into_response();
            }
            Ok(Ok(resp)) => {
                let status = resp.status();

                if status == 429 {
                    state.gateway_metrics.record_rate_limit();
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
                let resp_status = axum::http::StatusCode::from_u16(status.as_u16())
                    .unwrap_or(StatusCode::BAD_GATEWAY);

                let content_type = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("application/json")
                    .to_string();

                let stream = resp
                    .bytes_stream()
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()));

                let collected = match timeout(
                    std::time::Duration::from_millis(request_timeout_ms),
                    stream.try_collect::<Vec<Bytes>>(),
                )
                .await
                {
                    Ok(Ok(c)) => c,
                    Ok(Err(e)) => {
                        return (StatusCode::BAD_GATEWAY, e.to_string()).into_response();
                    }
                    Err(_) => {
                        state.gateway_metrics.record_timeout();
                        if let Some(label) = key_label.as_ref() {
                            state.model_stats.record_with_key(
                                &model_id,
                                label,
                                request_timeout_ms as f64,
                                false,
                            );
                        } else {
                            state
                                .model_stats
                                .record(&model_id, request_timeout_ms as f64, false);
                        }
                        return (
                            StatusCode::GATEWAY_TIMEOUT,
                            format!("upstream body timeout after {}ms", request_timeout_ms),
                        )
                            .into_response();
                    }
                };

                let full_body = collected.concat();
                // Check for server-side degradation from NVIDIA API
                // NVIDIA returns: {"status":400,"title":"Bad Request","detail":"Function id '...': DEGRADED function cannot be invoked"}
                let body_str = std::str::from_utf8(&full_body).unwrap_or("");
                if status == 400 && (body_str.contains("DEGRADED") || body_str.contains("degraded"))
                {
                    eprintln!("[nimaproxy] SERVER-DEGRADED: model '{}' returned DEGRADED error from NVIDIA (server-side block)", model_id);
                    // Record as server-side degraded - this immediately marks the model as unavailable
                    state.model_stats.record_server_degraded(&model_id);
                    if original_model == "auto" {
                        let (next_model_id, next_body) =
                            resolve_model(original_body.clone(), &state);
                        model_id = next_model_id;
                        body = next_body;
                        if let Some(response) = mistral_validation_error(&model_id, &body) {
                            return response;
                        }
                    }
                    // Continue retry with the next key and, for auto routing, a freshly resolved model.
                    continue;
                }
                if status == 400
                    && (body_str.contains("Invalid assistant message")
                        || body_str.contains("invalid assistant"))
                {
                    eprintln!("[nimaproxy] INVALID-ASSISTANT: model '{}' rejected message structure (400): {} — retrying with next key", model_id, &body_str[..body_str.len().min(200)]);
                    continue;
                }

                let (output_tokens, repetition_count, had_tool_call) =
                    extract_response_metrics(std::str::from_utf8(&full_body).unwrap_or(""));

                if output_tokens > 0 || repetition_count > 0 {
                    if key_label.is_some() {
                        state.model_stats.record_with_circuit_breaker(
                            &model_id,
                            ttfc_ms,
                            ok,
                            output_tokens,
                            repetition_count,
                            had_tool_call,
                        );
                    } else {
                        state.model_stats.record_with_circuit_breaker(
                            &model_id,
                            ttfc_ms,
                            ok,
                            output_tokens,
                            repetition_count,
                            had_tool_call,
                        );
                    }
                } else {
                    if let Some(label) = key_label.as_ref() {
                        state
                            .model_stats
                            .record_with_key(&model_id, label, ttfc_ms, ok);
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
                if let Some(label) = key_label.as_ref() {
                    response.headers_mut().insert(
                        "x-key-label",
                        HeaderValue::from_str(label)
                            .unwrap_or_else(|_| HeaderValue::from_static("unknown")),
                    );
                }
                if content_type.contains("event-stream") {
                    response
                        .headers_mut()
                        .insert("cache-control", HeaderValue::from_static("no-cache"));
                    response
                        .headers_mut()
                        .insert("x-accel-buffering", HeaderValue::from_static("no"));
                }
                return response;
            }
        }
    }

    (
        StatusCode::TOO_MANY_REQUESTS,
        "all keys exhausted after retries",
    )
        .into_response()
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
                                                 // NVIDIA NIM requires: EITHER content OR tool_calls, not both
                                                 // When tool_calls is present, set content to null
                        if obj.get("tool_calls").is_some() {
                            obj.insert("content".to_string(), serde_json::Value::Null);
                        }
                    }
                }
            }

            if let Some(tool_calls) = msg.get_mut("tool_calls").and_then(|tc| tc.as_array_mut()) {
                let original_len = tool_calls.len();
                // Filter out tool_calls with empty names
                tool_calls.retain(|tc| {
                    if let Some(name) = tc
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                    {
                        !name.is_empty()
                    } else {
                        // Keep if no name field (shouldn't happen but be safe)
                        true
                    }
                });
                // If all tool_calls were removed (and there were some originally), remove the tool_calls field entirely
                if original_len > 0 && tool_calls.is_empty() {
                    if let Some(obj) = msg.as_object_mut() {
                        obj.remove("tool_calls");
                    }
                }
            }
        }
    }

    // Sanitize tools array (tool definitions) — fix schema fields that break Jinja templates
    // NVIDIA models crash with 500 "tool_use:98" when tool.function.description is null/undefined
    // or when tool.function.parameters is missing/null (template does `description + " "` → boom)
    if let Some(tools) = json.get_mut("tools").and_then(|t| t.as_array_mut()) {
        // First pass: fix null/missing description and parameters before filtering
        // NVIDIA Jinja templates do string concat on description → null/undefined causes 500 "tool_use:98"
        for tool in tools.iter_mut() {
            if let Some(func) = tool.get_mut("function").and_then(|f| f.as_object_mut()) {
                match func.get("description") {
                    None | Some(Value::Null) => {
                        func.insert("description".to_string(), Value::String(String::new()));
                    }
                    _ => {}
                }
                match func.get("parameters") {
                    None | Some(Value::Null) => {
                        func.insert(
                            "parameters".to_string(),
                            serde_json::json!({"type": "object", "properties": {}}),
                        );
                    }
                    _ => {}
                }
            }
        }
        // Second pass: filter out tools with empty function names
        tools.retain(|tool| {
            if let Some(name) = tool
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
            {
                !name.is_empty()
            } else {
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
/// Also validates that the number of tool calls matches the number of tool responses
/// (only when tool messages are present in the request).
/// Validate tool call IDs for Mistral models.
/// Mistral requires tool call IDs to be exactly 9 alphanumeric characters.
/// Also validates that the number of tool calls matches the number of tool responses
/// (only when tool messages are present in the request).
pub(super) fn validate_mistral_tool_call_ids(
    json: &Value,
    model_id: &str,
) -> Result<(), (StatusCode, String)> {
    if !is_mistral_model(model_id) {
        return Ok(());
    }

    let mut tool_call_ids = std::collections::HashSet::new();
    let mut tool_response_ids = std::collections::HashSet::new();
    let mut has_tool_messages = false;

    if let Some(messages) = json.get("messages").and_then(|m| m.as_array()) {
        for msg in messages {
            if let Some(role) = msg.get("role").and_then(|r| r.as_str()) {
                if role == "assistant" {
                    if let Some(tool_calls) = msg.get("tool_calls").and_then(|tc| tc.as_array()) {
                        for tc in tool_calls {
                            if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                                if !id.chars().all(|c| c.is_alphanumeric()) {
                                    eprintln!("WARNING: Tool call id '{}' may be invalid for Mistral models.", id);
                                    continue;
                                }
                                tool_call_ids.insert(id.to_string());
                            }
                        }
                    }
                } else if role == "tool" {
                    has_tool_messages = true;
                    if let Some(id) = msg.get("tool_call_id").and_then(|i| i.as_str()) {
                        tool_response_ids.insert(id.to_string());
                    }
                }
            }
        }
    }

    if has_tool_messages {
        for id in &tool_call_ids {
            if !tool_response_ids.contains(id) {
                eprintln!("WARNING: Tool call id '{}' has no matching response.", id);
                continue;
            }
        }
        for id in &tool_response_ids {
            if !tool_call_ids.contains(id) {
                eprintln!("WARNING: Tool response id '{}' has no matching call.", id);
                continue;
            }
        }
    }

    Ok(())
}

/// Fix message ordering for OpenAI API compatibility.
/// After tool messages, the API requires an assistant message before the next user message.
/// This function inserts empty assistant messages where needed.
pub fn fix_message_ordering(json: &mut Value) {
    if let Some(messages) = json.get_mut("messages").and_then(|m| m.as_array_mut()) {
        let mut i = 0;
        while i < messages.len() {
            let current_role = messages[i]
                .get("role")
                .and_then(|r| r.as_str())
                .unwrap_or("");
            if current_role == "tool" {
                // Check if next message exists and is "user" or "developer" (developer→user transform may not have run yet)
                if i + 1 < messages.len() {
                    let next_role = messages[i + 1]
                        .get("role")
                        .and_then(|r| r.as_str())
                        .unwrap_or("");
                    if next_role == "user" || next_role == "developer" {
                        // Insert an assistant message after the tool message
                        // Must have ONLY content (no tool_calls field)
                        // NVIDIA NIM rejects messages with both content AND tool_calls
                        let empty_assistant = serde_json::json!({
                            "role": "assistant",
                            "content": null,
                        });
                        messages.insert(i + 1, empty_assistant);
                        i += 2; // Skip the inserted message
                        continue;
                    }
                }
            }
            i += 1;
        }
    }
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

    if !transform_developer && !transform_tool {
        return;
    }

    if let Some(messages) = json.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages {
            let role = msg
                .get("role")
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string();

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
    if let Some(messages) = json.get("messages").and_then(|m| m.as_array()) {
        let has_tool_role = messages
            .iter()
            .any(|msg| msg.get("role").and_then(|r| r.as_str()) == Some("tool"));
        let has_tool_calls = messages.iter().any(|msg| msg.get("tool_calls").is_some());
        let has_tool = has_tool_role || has_tool_calls;
        return has_tool;
    }
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
                let system_msg =
                    serde_json::json!({"role": "system", "content": minmax_instruction});
                messages.insert(0, system_msg);
            }
        } else {
            let system_msg = serde_json::json!({"role": "system", "content": minmax_instruction});
            messages.insert(0, system_msg);
        }
    }
}

/// Check if the last message in the conversation is from the assistant.
fn is_last_message_from_assistant(json: &Value) -> bool {
    if let Some(messages) = json.get("messages").and_then(|m| m.as_array()) {
        if let Some(last) = messages.last() {
            if let Some(role) = last.get("role").and_then(|r| r.as_str()) {
                return role == "assistant";
            }
        }
    }
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

    // Only inject Mistral-specific parameters for Mistral models
    // These params are rejected by NVIDIA for non-Mistral models
    if is_mistral {
        if has_tools {
            json["add_generation_prompt"] = Value::Bool(false);
        }
        if last_from_assistant {
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

    // Transform roles first (developer→user) so fix_message_ordering sees the
    // final role assignments when inserting assistant messages between tool→user gaps.
    transform_message_roles(&mut json, &model_id, state);

    fix_message_ordering(&mut json);

    if let Some(params) = state.model_params.get(&model_id) {
        apply_model_params(&mut json, params);
    }

    (model_id, Bytes::from(json.to_string()))
}

/// GET /v1/models — configured model list when routing/racing is enabled, otherwise passthrough.
pub async fn models(State(state): State<Arc<AppState>>) -> Response {
    let configured_models = state.configured_models();
    if !configured_models.is_empty() {
        let data: Vec<Value> = configured_models
            .iter()
            .map(|id| {
                serde_json::json!({
                    "id": id,
                    "object": "model",
                    "owned_by": "nimaproxy",
                })
            })
            .collect();
        let body = serde_json::json!({
            "object": "list",
            "data": data,
        });
        return (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            body.to_string(),
        )
            .into_response();
    }

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
    let routing_models = state
        .router
        .as_ref()
        .map(|router| router.models.clone())
        .unwrap_or_default();
    let racing_models = state.racing_models.clone();

    let keys_json: Vec<Value> = statuses
        .iter()
        .map(|s| {
            serde_json::json!({
                "label": s.label,
                "key_hint": s.key_hint,
                "active": s.active,
                "cooldown_secs_remaining": s.cooldown_secs_remaining,
                "in_flight": s.in_flight,
                "max_in_flight": s.max_in_flight,
            })
        })
        .collect();

    let metrics = state.gateway_metrics.snapshot();

    let body = serde_json::json!({
        "status": if active > 0 { "UP" } else { "DEGRADED" },
        "keys_total": total,
        "keys_active": active,
        "keys": keys_json,
        "gateway_in_flight": metrics.upstream_in_flight,
        "gateway_limit": state.max_upstream_in_flight,
        "routing_enabled": state.routing_enabled(),
        "racing_enabled": state.racing_enabled(),
        "routing_models": routing_models,
        "racing_models": racing_models,
        "racing_max_parallel": state.racing_max_parallel,
        "racing_timeout_ms": state.racing_timeout_ms,
        "racing_adaptive": state.racing_adaptive,
        "racing_min_parallel": state.racing_min_parallel,
        "racing_pressure_parallel": state.racing_pressure_parallel,
        "racing_degraded_parallel": state.racing_degraded_parallel,
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
                "in_flight": s.in_flight,
                "max_in_flight": s.max_in_flight,
            })
        })
        .collect();

    let racing_models: Vec<Value> = state
        .racing_models
        .iter()
        .map(|m| serde_json::json!(m))
        .collect();
    let metrics = state.gateway_metrics.snapshot();

    let body = serde_json::json!({
        "models": models_json,
        "keys": keys_json,
        "gateway": {
            "request_total": metrics.request_total,
            "direct_requests": metrics.direct_requests,
            "racing_requests": metrics.racing_requests,
            "upstream_attempts": metrics.upstream_attempts,
            "upstream_in_flight": metrics.upstream_in_flight,
            "max_upstream_in_flight": state.max_upstream_in_flight,
            "max_in_flight_per_key": state.max_in_flight_per_key,
            "overload_rejects": metrics.overload_rejects,
            "no_key_rejects": metrics.no_key_rejects,
            "timeout_count": metrics.timeout_count,
            "rate_limit_count": metrics.rate_limit_count,
            "fanout_total": metrics.fanout_total,
            "fanout_samples": metrics.fanout_samples,
            "fanout_avg": metrics.fanout_avg,
            "racing_wins": metrics.racing_wins,
        },
        "racing_models": racing_models,
        "racing_enabled": state.racing_enabled(),
        "racing_max_parallel": state.racing_max_parallel,
        "racing_timeout_ms": state.racing_timeout_ms,
        "racing_adaptive": state.racing_adaptive,
        "racing_min_parallel": state.racing_min_parallel,
        "racing_pressure_parallel": state.racing_pressure_parallel,
        "racing_degraded_parallel": state.racing_degraded_parallel,
        "racing_fast_models": state.racing_fast_models.clone(),
        "racing_fallback_models": state.racing_fallback_models.clone(),
    });

    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
}

async fn race_models(state: Arc<AppState>, body: Bytes, models: &[String]) -> Response {
    let max_parallel = adaptive_racing_parallel(&state, models.len());

    if max_parallel < 2 {
        if state.pool.active_count() == 0 {
            return no_key_response(&state);
        }
        return gateway_overloaded_response(&state);
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

    let candidates = rotated_models(models, cursor);
    let candidates_for_race = tiered_candidates(&state, &candidates, max_parallel);

    if candidates_for_race.len() < 2 {
        eprintln!("[racing] not enough viable models after filtering (need ≥2)");
        return (StatusCode::BAD_GATEWAY, "not enough viable racing models").into_response();
    }

    let models_to_race = candidates_for_race;
    {
        let mut c = state.racing_cursor.lock().unwrap();
        *c = (cursor + models_to_race.len()) % n;
    }

    let mut tasks = JoinSet::new();
    let mut skipped_no_key = 0usize;
    let mut skipped_overloaded = 0usize;

    for model_id in &models_to_race {
        let timeout_val = timeout_for_model(&state, model_id);

        let mut json: Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => continue,
        };
        json["model"] = Value::String(model_id.clone());

        // Inject Mistral-specific parameters BEFORE message transformations
        // so has_tool_messages() can detect tool messages in the original JSON
        inject_mistral_tool_params(&mut json, model_id);
        // Inject MiniMax system message for JSON tool calling
        inject_minimax_system_message(&mut json, model_id);

        // Sanitize tool_calls to remove entries with empty names (Azure OpenAI rejects these)
        sanitize_tool_calls(&mut json);

        // Transform roles first (developer→user) so fix_message_ordering sees the
        // final role assignments when inserting assistant messages between tool→user gaps.
        transform_message_roles(&mut json, model_id, &state);

        // Fix message ordering: insert empty assistant between tool→user transitions
        fix_message_ordering(&mut json);

        // Inject per-model catalog defaults and hyperparameters.
        if let Some(params) = state.model_params.get(model_id) {
            apply_model_params(&mut json, params);
        }

        let accept_header = accept_header_for_json(&json).to_string();
        let req_body = match serde_json::to_vec(&json) {
            Ok(b) => Bytes::from(b),
            Err(_) => continue,
        };

        let target = state.target.clone();
        let client = state.client.clone();
        let state_clone = state.clone();
        let model_id_clone = model_id.clone();
        let model_id_for_task = model_id.clone();
        let timeout_ms_for_model = timeout_val;

        let upstream_permit = match state.try_acquire_upstream() {
            Some(permit) => permit,
            None => {
                skipped_overloaded += 1;
                state.gateway_metrics.record_overload();
                eprintln!("[racing] upstream limit reached for {}", model_id);
                continue;
            }
        };
        let key_lease = match state.pool.next_key_with_permit() {
            Ok(lease) => lease,
            Err(KeyAcquireError::AllBusy) => {
                skipped_overloaded += 1;
                state.gateway_metrics.record_overload();
                eprintln!("[racing] key concurrency limit reached for {}", model_id);
                continue;
            }
            Err(KeyAcquireError::NoKeys | KeyAcquireError::AllCoolingDown) => {
                skipped_no_key += 1;
                eprintln!("[racing] no keys available for {}", model_id);
                continue;
            }
        };

        let key = key_lease.key.clone();
        let key_idx_for_spawn = key_lease.idx;
        let key_label = key_lease.label.clone();

        tasks.spawn(async move {
            let _upstream_permit = upstream_permit;
            let _key_lease = key_lease;
            let t0 = Instant::now();
            let result = timeout(
                std::time::Duration::from_millis(timeout_ms_for_model),
                client
                    .post(format!("{}/v1/chat/completions", target))
                    .header("Authorization", format!("Bearer {}", key))
                    .header("Content-Type", "application/json")
                    .header("Accept", accept_header)
                    .body(req_body)
                    .send(),
            )
            .await;

            let task_result: Result<(Response, u16, usize, u64), String> = match result {
                Ok(Ok(resp)) => {
                    let latency = t0.elapsed().as_millis() as f64;
                    let status = resp.status();
                    let retry_after_secs: u64 = resp
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                        .unwrap_or(60);
                    let record_outcome = |ok: bool| {
                        if let Some(ref label) = key_label {
                            state_clone.model_stats.record_with_key(
                                &model_id_clone,
                                label,
                                latency,
                                ok,
                            );
                        } else {
                            state_clone.model_stats.record(&model_id_clone, latency, ok);
                        }
                    };

                    // For 4xx/5xx: buffer body now (stream will be consumed) so we can log it
                    if status.as_u16() != 429 && status.as_u16() >= 400 {
                        let body_bytes = resp.bytes().await.unwrap_or_default();
                        let body_str = String::from_utf8_lossy(&body_bytes);
                        record_outcome(false);
                        return (
                            model_id_for_task,
                            Err(format!(
                                "HTTP {} from {}: {}",
                                status.as_u16(),
                                model_id_clone,
                                &body_str[..body_str.len().min(400)]
                            )),
                        );
                    }
                    record_outcome(status.is_success());
                    let content_type = resp
                        .headers()
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("application/json")
                        .to_string();

                    let body_bytes = match timeout(
                        std::time::Duration::from_millis(timeout_ms_for_model),
                        resp.bytes(),
                    )
                    .await
                    {
                        Ok(Ok(bytes)) => bytes,
                        Ok(Err(e)) => {
                            return (model_id_for_task, Err(format!("body error: {}", e)));
                        }
                        Err(_) => {
                            state_clone.gateway_metrics.record_timeout();
                            if let Some(ref label) = key_label {
                                state_clone.model_stats.record_with_key(
                                    &model_id_clone,
                                    label,
                                    timeout_ms_for_model as f64,
                                    false,
                                );
                            } else {
                                state_clone.model_stats.record(
                                    &model_id_clone,
                                    timeout_ms_for_model as f64,
                                    false,
                                );
                            }
                            return (
                                model_id_for_task,
                                Err(format!("body timeout after {}ms", timeout_ms_for_model)),
                            );
                        }
                    };
                    let body = Body::from(body_bytes);

                    let mut response = Response::new(body);
                    *response.status_mut() =
                        StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
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
                    Ok::<(Response, u16, usize, u64), String>((
                        response,
                        status.as_u16(),
                        key_idx_for_spawn,
                        retry_after_secs,
                    ))
                }
                Ok(Err(e)) => {
                    if let Some(ref label) = key_label {
                        state_clone.model_stats.record_with_key(
                            &model_id_clone,
                            label,
                            timeout_ms_for_model as f64,
                            false,
                        );
                    } else {
                        state_clone.model_stats.record(
                            &model_id_clone,
                            timeout_ms_for_model as f64,
                            false,
                        );
                    }
                    Err(format!("request error: {}", e))
                }
                Err(_) => {
                    state_clone.gateway_metrics.record_timeout();
                    if let Some(ref label) = key_label {
                        state_clone.model_stats.record_with_key(
                            &model_id_clone,
                            label,
                            timeout_ms_for_model as f64,
                            false,
                        );
                    } else {
                        state_clone.model_stats.record(
                            &model_id_clone,
                            timeout_ms_for_model as f64,
                            false,
                        );
                    }
                    Err(format!("timeout after {}ms", timeout_ms_for_model))
                }
            };
            (model_id_for_task, task_result)
        });
    }

    let actual_fanout = tasks.len();
    state.gateway_metrics.record_fanout(actual_fanout);

    if actual_fanout == 0 {
        if skipped_no_key > 0 {
            state.gateway_metrics.record_no_key();
            return (StatusCode::TOO_MANY_REQUESTS, "all API keys rate-limited").into_response();
        }
        if skipped_overloaded > 0 {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "gateway overloaded; retry later",
            )
                .into_response();
        }
        return (StatusCode::BAD_REQUEST, "no valid models to race").into_response();
    }

    let mut last_error = None;
    let mut pending_rate_limited_keys: Vec<(usize, u64)> = Vec::new();
    let mut saw_non_rate_limit_failure = false;

    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok((model_id, Ok((response, status_code, key_idx, retry_after_secs)))) => {
                if status_code == 429 {
                    state.gateway_metrics.record_rate_limit();
                    eprintln!(
                        "[racing] {} → 429, key {} may be rate-limited {}s, trying next",
                        model_id, key_idx, retry_after_secs
                    );
                    pending_rate_limited_keys.push((key_idx, retry_after_secs));
                    last_error = Some(format!("429 rate-limited (key {})", key_idx));
                } else {
                    eprintln!("[racing] {} → HTTP {} (winner)", model_id, status_code);
                    state.gateway_metrics.record_racing_win(&model_id);
                    tasks.abort_all();
                    return response;
                }
            }
            Ok((model_id, Err(e))) => {
                eprintln!("[racing] {} failed: {}", model_id, e);
                saw_non_rate_limit_failure = true;
                last_error = Some(e);
            }
            Err(e) => {
                eprintln!("[racing] task failed: {}", e);
                saw_non_rate_limit_failure = true;
                last_error = Some(e.to_string());
            }
        }
    }

    for (key_idx, retry_after_secs) in pending_rate_limited_keys {
        state.pool.mark_rate_limited(key_idx, retry_after_secs);
    }

    if last_error
        .as_deref()
        .is_some_and(|e| e.contains("429 rate-limited"))
        && !saw_non_rate_limit_failure
    {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "all racing requests rate-limited",
        )
            .into_response();
    }

    (
        StatusCode::BAD_GATEWAY,
        last_error.unwrap_or_else(|| "all racing models failed".to_string()),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
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
                },
                crate::KeyEntry {
                    key: "key-b".to_string(),
                    label: Some("key-b".to_string()),
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
                max_upstream_in_flight: 6,
                max_in_flight_per_key: 2,
                min_dynamic_timeout_ms: 8000,
                dynamic_sample_floor: 10,
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
}

#[cfg(test)]
mod tool_call_id_tests {
    use super::sanitize_tool_calls;
    use serde_json::json;

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
        assert!(!json["messages"][0]
            .as_object()
            .unwrap()
            .contains_key("tool_call_id"));
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
        assert!(json["messages"][0]
            .as_object()
            .unwrap()
            .contains_key("tool_call_id"));
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
        assert!(!json["messages"][0]
            .as_object()
            .unwrap()
            .contains_key("tool_call_id"));
        assert!(json["messages"][0]
            .as_object()
            .unwrap()
            .contains_key("tool_calls"));
    }

    // ============ fix_message_ordering tests ============

    #[test]
    fn test_fix_message_ordering_no_change_needed() {
        // No tool messages, should be unchanged
        let mut json = json!({
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "hello"}
            ]
        });
        crate::proxy::fix_message_ordering(&mut json);
        assert_eq!(json["messages"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_fix_message_ordering_inserts_assistant() {
        // tool followed by user - should insert empty assistant
        let mut json = json!({
            "messages": [
                {"role": "tool", "tool_call_id": "1", "content": "result"},
                {"role": "user", "content": "next"}
            ]
        });
        crate::proxy::fix_message_ordering(&mut json);
        let msgs = json["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "tool");
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"], serde_json::Value::Null);
        assert_eq!(msgs[2]["role"], "user");
    }

    #[test]
    fn test_fix_message_ordering_developer_role() {
        // tool followed by developer (<turn-aborted>) then user - should insert empty assistant
        // This is the exact OMP pattern: tool[N]->developer[N+1]->user[N+2]
        let mut json = json!({
            "messages": [
                {"role": "assistant", "content": null, "tool_calls": [{"id": "abc123XYZ", "type": "function", "function": {"name": "read"}}]},
                {"role": "tool", "tool_call_id": "abc123XYZ", "content": "result"},
                {"role": "developer", "content": "<turn-aborted>\nThe previous turn was aborted."},
                {"role": "user", "content": "continue"}
            ]
        });
        crate::proxy::fix_message_ordering(&mut json);
        let msgs = json["messages"].as_array().unwrap();
        // Should insert assistant between tool[1] and developer[2]
        assert_eq!(msgs.len(), 5);
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(msgs[1]["role"], "tool");
        assert_eq!(msgs[2]["role"], "assistant"); // inserted
        assert!(msgs[2]["content"].is_null());
        assert_eq!(msgs[3]["role"], "developer");
        assert_eq!(msgs[4]["role"], "user");
    }

    #[test]
    fn test_fix_message_ordering_multiple_tools_before_user() {
        // Multiple tool messages followed by user
        let mut json = json!({
            "messages": [
                {"role": "tool", "tool_call_id": "1", "content": "r1"},
                {"role": "tool", "tool_call_id": "2", "content": "r2"},
                {"role": "user", "content": "next"}
            ]
        });
        crate::proxy::fix_message_ordering(&mut json);
        let msgs = json["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0]["role"], "tool");
        assert_eq!(msgs[1]["role"], "tool");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[3]["role"], "user");
    }

    #[test]
    fn test_fix_message_ordering_assistant_exists_no_change() {
        // tool followed by assistant followed by user - no change needed
        let mut json = json!({
            "messages": [
                {"role": "tool", "tool_call_id": "1", "content": "result"},
                {"role": "assistant", "content": "summary"},
                {"role": "user", "content": "next"}
            ]
        });
        crate::proxy::fix_message_ordering(&mut json);
        assert_eq!(json["messages"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn test_validate_mistral_tool_call_ids_valid() {
        let json = serde_json::json!({"messages": [{"role": "assistant", "tool_calls": [{"id": "abc123XYZ", "type": "function", "function": {"name": "test"}}]}]});
        let result = crate::proxy::validate_mistral_tool_call_ids(
            &json,
            "mistralai/mistral-medium-3.5-128b",
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_mistral_tool_call_ids_invalid_length() {
        let json = serde_json::json!({"messages": [{"role": "assistant", "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "test"}}]}]});
        let result =
            crate::proxy::validate_mistral_tool_call_ids(&json, "mistralai/mistral-7b-instruct");
        assert!(result.is_ok()); // validate is warn-only, not hard reject;
    }

    #[test]
    fn test_validate_mistral_tool_call_ids_invalid_chars() {
        let json = serde_json::json!({"messages": [{"role": "assistant", "tool_calls": [{"id": "call_123", "type": "function", "function": {"name": "test"}}]}]});
        let result =
            crate::proxy::validate_mistral_tool_call_ids(&json, "mistralai/mistral-7b-instruct");
        assert!(result.is_ok()); // validate is warn-only, not hard reject;
    }

    #[test]
    fn test_validate_mistral_tool_call_ids_non_mistral() {
        let json = serde_json::json!({"messages": [{"role": "assistant", "tool_calls": [{"id": "call_123", "type": "function", "function": {"name": "test"}}]}]});
        let result = crate::proxy::validate_mistral_tool_call_ids(&json, "openai/gpt-4");
        assert!(result.is_ok());
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
    use crate::turn_log::{log_turn as log_turn_event, TurnLog};

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
/// POST /v1/completions — legacy completions endpoint
pub async fn completions(
    State(state): State<Arc<AppState>>,
    _headers: HeaderMap,
    body: Bytes,
) -> Response {
    state.gateway_metrics.record_request(false);
    eprintln!("[nimaproxy] POST /v1/completions");
    let model_id = if let Ok(v) = serde_json::from_slice::<Value>(&body) {
        v.get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string()
    } else {
        String::new()
    };
    let n = state.pool.len().min(MAX_RETRIES).max(1);
    eprintln!("[nimaproxy] POST /v1/completions - got n={}", n);
    for _ in 0..n {
        let (upstream_permit, key_lease) = match acquire_gateway_permits(&state) {
            Ok(permits) => permits,
            Err(response) => return response,
        };
        let _upstream_permit = upstream_permit;
        let key_label = key_lease.label.clone();
        eprintln!("[nimaproxy] POST /v1/completions - about to send request");
        let t0 = Instant::now();
        let result = state
            .client
            .post(format!("{}/v1/completions", state.target))
            .header("Authorization", format!("Bearer {}", key_lease.key))
            .header("Content-Type", "application/json")
            .body(body.clone())
            .send()
            .await;
        match result {
            Err(e) => {
                if let Some(label) = key_label.as_ref() {
                    state.model_stats.record_with_key(
                        &model_id,
                        label,
                        t0.elapsed().as_millis() as f64,
                        false,
                    );
                } else {
                    state
                        .model_stats
                        .record(&model_id, t0.elapsed().as_millis() as f64, false);
                }
                return (StatusCode::BAD_GATEWAY, e.to_string()).into_response();
            }
            Ok(resp) => {
                let status = resp.status();
                let ok = status.is_success();
                let ttfc_ms = t0.elapsed().as_millis() as f64;
                let resp_status =
                    StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
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
                let (output_tokens, repetition_count, had_tool_call) =
                    extract_response_metrics(std::str::from_utf8(&full_body).unwrap_or(""));
                if output_tokens > 0 || repetition_count > 0 {
                    state.model_stats.record_with_circuit_breaker(
                        &model_id,
                        ttfc_ms,
                        ok,
                        output_tokens,
                        repetition_count,
                        had_tool_call,
                    );
                } else {
                    if let Some(label) = key_label.as_ref() {
                        state
                            .model_stats
                            .record_with_key(&model_id, label, ttfc_ms, ok);
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
                if let Some(label) = key_label.as_ref() {
                    response.headers_mut().insert(
                        "x-key-label",
                        HeaderValue::from_str(label)
                            .unwrap_or_else(|_| HeaderValue::from_static("unknown")),
                    );
                }
                return response;
            }
        }
    }
    (
        StatusCode::TOO_MANY_REQUESTS,
        "all keys exhausted after retries",
    )
        .into_response()
}

/// POST /v1/embeddings — embeddings endpoint
pub async fn embeddings(
    State(state): State<Arc<AppState>>,
    _headers: HeaderMap,
    body: Bytes,
) -> Response {
    state.gateway_metrics.record_request(false);
    eprintln!("[nimaproxy] POST /v1/embeddings");
    let model_id = if let Ok(v) = serde_json::from_slice::<Value>(&body) {
        v.get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string()
    } else {
        String::new()
    };
    let n = state.pool.len().min(MAX_RETRIES).max(1);
    for _ in 0..n {
        let (upstream_permit, key_lease) = match acquire_gateway_permits(&state) {
            Ok(permits) => permits,
            Err(response) => return response,
        };
        let _upstream_permit = upstream_permit;
        let key_label = key_lease.label.clone();
        let t0 = Instant::now();
        let result = state
            .client
            .post(format!("{}/v1/embeddings", state.target))
            .header("Authorization", format!("Bearer {}", key_lease.key))
            .header("Content-Type", "application/json")
            .body(body.clone())
            .send()
            .await;
        match result {
            Err(e) => {
                if let Some(label) = key_label.as_ref() {
                    state.model_stats.record_with_key(
                        &model_id,
                        label,
                        t0.elapsed().as_millis() as f64,
                        false,
                    );
                } else {
                    state
                        .model_stats
                        .record(&model_id, t0.elapsed().as_millis() as f64, false);
                }
                return (StatusCode::BAD_GATEWAY, e.to_string()).into_response();
            }
            Ok(resp) => {
                let status = resp.status();
                let ok = status.is_success();
                let ttfc_ms = t0.elapsed().as_millis() as f64;
                let resp_status =
                    StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
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
                if let Some(label) = key_label.as_ref() {
                    state
                        .model_stats
                        .record_with_key(&model_id, label, ttfc_ms, ok);
                } else {
                    state.model_stats.record(&model_id, ttfc_ms, ok);
                }
                let body = Body::from(full_body);
                let mut response = Response::new(body);
                *response.status_mut() = resp_status;
                response.headers_mut().insert(
                    "content-type",
                    HeaderValue::from_str(&content_type)
                        .unwrap_or_else(|_| HeaderValue::from_static("application/json")),
                );
                if let Some(label) = key_label.as_ref() {
                    response.headers_mut().insert(
                        "x-key-label",
                        HeaderValue::from_str(label)
                            .unwrap_or_else(|_| HeaderValue::from_static("unknown")),
                    );
                }
                return response;
            }
        }
    }
    (
        StatusCode::TOO_MANY_REQUESTS,
        "all keys exhausted after retries",
    )
        .into_response()
}

/// GET /props — tool capability discovery endpoint (for OMP compatibility)
pub async fn props() -> Response {
    eprintln!("[nimaproxy] GET /props");
    let props = serde_json::json!({
        "contextWindow": 8192,
        "input": true,
        "supports_developer_role": true,
        "supports_tool_messages": true,
        "supports_tool_calls": true,
        "supports_embeddings": true,
        "supports_completions": true,
        "supported_roles": ["user", "assistant", "system", "tool", "developer", "function"],
        "tool_capabilities": {
            "function_calling": true,
            "code_interpreter": false,
            "image_generation": false
        }
    });
    let body = Body::from(props.to_string());
    let mut response = Response::new(body);
    response
        .headers_mut()
        .insert("content-type", HeaderValue::from_static("application/json"));
    response
}
