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

/// Resolve the model field, optionally rewriting the body for "auto" routing.
/// Returns (model_id_string, possibly_rewritten_body).
fn resolve_model(body: Bytes, state: &AppState) -> (String, Bytes) {
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
                    if let Some(ctk) = &params.chat_template_kwargs {
                        for (k, v) in ctk {
                            json[k] = v.clone();
                        }
                    }
                }
                let rewritten = Bytes::from(json.to_string());
                return (picked, rewritten);
            }
        }
    }

    let model_id = if requested.is_empty() {
        "unknown".to_string()
    } else {
        requested.clone()
    };

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
        if let Some(ctk) = &params.chat_template_kwargs {
            for (k, v) in ctk {
                json[k] = v.clone();
            }
        }
    }

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
