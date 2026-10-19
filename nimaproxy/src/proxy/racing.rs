use crate::key_pool::KeyAcquireError;
use crate::proxy::admission::{gateway_overloaded_response, no_key_response};
use crate::proxy::chat_helpers::{accept_header_for_json, apply_model_params};
use crate::proxy::fallback::solo_model_fallback;
use crate::proxy::http::log_turn_request;
use crate::proxy::messages::{
    fix_message_ordering, inject_minimax_system_message, inject_mistral_tool_params,
    normalize_assistant_messages, sanitize_tool_calls, transform_message_roles,
};
use crate::proxy::timeouts::{
    prompt_chars, record_model_timeout, request_turn_summary, timeout_for_model,
};
use crate::proxy::upstream_errors::{is_hard_model_error, is_transient_race_error, sse_body_error};
use crate::turn_log::TurnRecord;
use crate::AppState;
use axum::{
    body::Body,
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinSet;
use tokio::time::timeout;
use tracing::warn;
pub(crate) fn racing_deadline(state: &AppState) -> Option<Instant> {
    if state.racing_max_total_request_ms == 0 {
        return None;
    }
    Instant::now().checked_add(Duration::from_millis(state.racing_max_total_request_ms))
}
pub(crate) fn timeout_before_deadline(
    deadline: Option<Instant>,
    per_attempt_ms: u64,
) -> Option<u64> {
    let Some(deadline) = deadline else {
        return Some(per_attempt_ms);
    };
    let remaining = deadline.checked_duration_since(Instant::now())?;
    let remaining_ms = remaining.as_millis().try_into().unwrap_or(u64::MAX);
    if remaining_ms == 0 {
        None
    } else {
        Some(per_attempt_ms.min(remaining_ms))
    }
}
pub(crate) fn racing_deadline_response(state: &AppState) -> Response {
    state.gateway_metrics.record_timeout();
    state.gateway_metrics.record_deadline_exceeded();
    (
        StatusCode::GATEWAY_TIMEOUT,
        format!(
            "racing deadline exceeded after {}ms",
            state.racing_max_total_request_ms
        ),
    )
        .into_response()
}
pub(crate) fn adaptive_racing_parallel(state: &AppState, candidate_count: usize) -> usize {
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
    let key_window_capacity = state.pool.window_capacity().max(1);
    let in_flight = key_window_capacity.saturating_sub(available_key_slots);
    let pressure_mark = (key_window_capacity / 3).max(1);
    let degraded_mark = ((key_window_capacity * 2) / 3).max(1);
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
pub(crate) fn rotated_models(models: &[String], cursor: usize) -> Vec<String> {
    let n = models.len();
    (0..n).map(|i| models[(cursor + i) % n].clone()).collect()
}
pub(crate) fn tiered_candidates(
    state: &AppState,
    rotated: &[String],
    max_parallel: usize,
) -> Vec<String> {
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
pub(crate) fn solo_candidate_models(state: &AppState, models: &[String]) -> Vec<String> {
    tiered_candidates(state, models, models.len().max(1))
}
#[cfg(test)]
pub(crate) fn solo_candidate_model(state: &AppState, models: &[String]) -> Option<String> {
    solo_candidate_models(state, models).into_iter().next()
}
pub(crate) fn prompt_parallel_cap(state: &AppState, body: &Bytes) -> Option<usize> {
    let threshold = state.racing_large_prompt_char_threshold;
    if threshold == 0 {
        return None;
    }
    if prompt_chars(body) >= threshold {
        Some(state.racing_large_prompt_parallel.max(1))
    } else {
        None
    }
}
pub(crate) async fn race_models(state: Arc<AppState>, body: Bytes, models: &[String]) -> Response {
    let deadline = racing_deadline(&state);
    let mut max_parallel = adaptive_racing_parallel(&state, models.len());
    if let Some(prompt_cap) = prompt_parallel_cap(&state, &body) {
        max_parallel = max_parallel.min(prompt_cap);
    }
    if max_parallel < 2 {
        if state.racing_solo_fallback {
            let solo_models = solo_candidate_models(&state, models);
            if !solo_models.is_empty() {
                state.gateway_metrics.record_fanout(1);
                return solo_model_fallback(state, body, solo_models, deadline).await;
            }
        }
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
        if state.racing_solo_fallback {
            let mut solo_models = candidates_for_race.clone();
            for model_id in solo_candidate_models(&state, models) {
                if !solo_models.iter().any(|m| m == &model_id) {
                    solo_models.push(model_id);
                }
            }
            if !solo_models.is_empty() {
                state.gateway_metrics.record_fanout(1);
                return solo_model_fallback(state, body, solo_models, deadline).await;
            }
        }
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
        let per_model_timeout_ms = timeout_for_model(&state, model_id);
        let Some(send_timeout_ms) = timeout_before_deadline(deadline, per_model_timeout_ms) else {
            return racing_deadline_response(&state);
        };
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
        normalize_assistant_messages(&mut json);
        // Inject per-model catalog defaults and hyperparameters.
        if let Some(params) = state.model_params.get(model_id) {
            apply_model_params(&mut json, params);
        }
        let accept_header = accept_header_for_json(&json).to_string();
        let req_body = match serde_json::to_vec(&json) {
            Ok(b) => Bytes::from(b),
            Err(_) => continue,
        };
        let (message_count, has_tool_calls, tool_call_count) = request_turn_summary(&req_body);
        let target = state.target.clone();
        let client = state.client.clone();
        let state_clone = state.clone();
        let model_id_clone = model_id.clone();
        let model_id_for_task = model_id.clone();
        let timeout_ms_for_model = per_model_timeout_ms;
        let send_timeout_ms_for_model = send_timeout_ms;
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
                std::time::Duration::from_millis(send_timeout_ms_for_model),
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
                        // Bounded like the success-path body below: the shared
                        // client carries no timeout, so an unbounded read here
                        // would let a hung error body stall this leg past the
                        // racing deadline. An unreadable error body degrades to
                        // an empty one — the status is already in hand.
                        let error_body_window = timeout_before_deadline(deadline, timeout_ms_for_model)
                            .unwrap_or(0);
                        let body_bytes = match timeout(
                            Duration::from_millis(error_body_window),
                            resp.bytes(),
                        )
                        .await
                        {
                            Ok(Ok(bytes)) => bytes,
                            Ok(Err(_)) | Err(_) => Bytes::new(),
                        };
                        let body_str = String::from_utf8_lossy(&body_bytes);
                        // Auth failure (401/403): this racing leg's KEY is bad, not the
                        // model. Quarantine the key only — do NOT call `record_outcome`
                        // (model_stats degradation) or the hard-error quarantine hook for
                        // `model_id_clone`. The error string still carries "HTTP 401/403" so
                        // the join loop's `is_transient_race_error` check lets a healthy
                        // solo-fallback retry (with a different key) take over.
                        if status.as_u16() == 401 || status.as_u16() == 403 {
                            state_clone.pool.mark_auth_failed(
                                key_idx_for_spawn,
                                Duration::from_secs(state_clone.auth_failure_cooldown_secs),
                            );
                            warn!(
                                key_idx = key_idx_for_spawn,
                                key_label = key_label.as_deref().unwrap_or("unknown"),
                                model = %model_id_clone,
                                status = status.as_u16(),
                                cooldown_secs = state_clone.auth_failure_cooldown_secs,
                                "racing leg: auth failure (401/403) on leased key; quarantining key, not the model"
                            );
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
                        record_outcome(false);
                        if status.as_u16() == 400 {
                            let err = format!("HTTP 400 from {}: {}", model_id_clone, body_str);
                            if is_hard_model_error(&err) {
                                state_clone
                                    .model_stats
                                    .record_hard_error(&model_id_clone, &err);
                            }
                        }
                        log_turn_request(TurnRecord {
                            requested_model: "auto".to_string(),
                            responding_model: model_id_clone.clone(),
                            latency_ms: latency as u128 as u64,
                            success: false,
                            status_code: status.as_u16(),
                            request_message_count: message_count,
                            loop_key: None,
                            response_message_count: 1,
                            has_tool_calls,
                            tool_call_count,
                            key_label: key_label.as_deref().map(|s| s.to_string()),
                            is_racing: true,
                            error: Some(body_str[..body_str.len().min(400)].to_string()),
                            request_tokens: None,
                            response_tokens: None,
                        });
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
                    if status.as_u16() == 429 {
                        state_clone.pool.record_rate_limited(key_idx_for_spawn);
                    } else if status.is_success() {
                        state_clone.pool.record_success(key_idx_for_spawn);
                    }
                    let content_type = resp
                        .headers()
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("application/json")
                        .to_string();
                    let body_timeout_ms =
                        match timeout_before_deadline(deadline, timeout_ms_for_model) {
                            Some(ms) => ms,
                            None => {
                                let msg = format!(
                                    "racing deadline exceeded after {}ms",
                                    state_clone.racing_max_total_request_ms
                                );
                                state_clone.gateway_metrics.record_timeout();
                                record_model_timeout(
                                    &state_clone,
                                    &model_id_clone,
                                    key_label.as_deref(),
                                    timeout_ms_for_model,
                                );
                                log_turn_request(TurnRecord {
                                    requested_model: "auto".to_string(),
                                    responding_model: model_id_clone.clone(),
                                    latency_ms: t0.elapsed().as_millis() as u64,
                                    success: false,
                                    status_code: StatusCode::GATEWAY_TIMEOUT.as_u16(),
                                    request_message_count: message_count,
                                    loop_key: None,
                                    response_message_count: 1,
                                    has_tool_calls,
                                    tool_call_count,
                                    key_label: key_label.as_deref().map(|s| s.to_string()),
                                    is_racing: true,
                                    error: Some(msg.clone()),
                                    request_tokens: None,
                                    response_tokens: None,
                                });
                                return (model_id_for_task, Err(msg));
                            }
                        };
                    let body_bytes = match timeout(
                        std::time::Duration::from_millis(body_timeout_ms),
                        resp.bytes(),
                    )
                    .await
                    {
                        Ok(Ok(bytes)) => bytes,
                        Ok(Err(e)) => {
                            log_turn_request(TurnRecord {
                                requested_model: "auto".to_string(),
                                responding_model: model_id_clone.clone(),
                                latency_ms: t0.elapsed().as_millis() as u64,
                                success: false,
                                status_code: StatusCode::BAD_GATEWAY.as_u16(),
                                request_message_count: message_count,
                                loop_key: None,
                                response_message_count: 1,
                                has_tool_calls,
                                tool_call_count,
                                key_label: key_label.as_deref().map(|s| s.to_string()),
                                is_racing: true,
                                error: Some(e.to_string()),
                                request_tokens: None,
                                response_tokens: None,
                            });
                            return (model_id_for_task, Err(format!("body error: {}", e)));
                        }
                        Err(_) => {
                            state_clone.gateway_metrics.record_timeout();
                            let timeout_error = if body_timeout_ms < timeout_ms_for_model {
                                format!(
                                    "racing deadline exceeded after {}ms",
                                    state_clone.racing_max_total_request_ms
                                )
                            } else {
                                format!("body timeout after {}ms", timeout_ms_for_model)
                            };
                            record_model_timeout(
                                &state_clone,
                                &model_id_clone,
                                key_label.as_deref(),
                                body_timeout_ms,
                            );
                            log_turn_request(TurnRecord {
                                requested_model: "auto".to_string(),
                                responding_model: model_id_clone.clone(),
                                latency_ms: body_timeout_ms as u128 as u64,
                                success: false,
                                status_code: StatusCode::GATEWAY_TIMEOUT.as_u16(),
                                request_message_count: message_count,
                                loop_key: None,
                                response_message_count: 1,
                                has_tool_calls,
                                tool_call_count,
                                key_label: key_label.as_deref().map(|s| s.to_string()),
                                is_racing: true,
                                error: Some(timeout_error.clone()),
                                request_tokens: None,
                                response_tokens: None,
                            });
                            return (model_id_for_task, Err(timeout_error));
                        }
                    };
                    // NVIDIA NIM (and similar providers) can return HTTP 200 and then emit
                    // an error payload inside the body instead of a real error status. Racing
                    // scores a leg by HTTP status alone, so without this check that leg
                    // "wins" the race and forwards a fake success to the client.
                    if status.is_success() {
                        if let Some((embedded_code, embedded_msg)) =
                            sse_body_error(&body_bytes, Some(&content_type))
                        {
                            log_turn_request(TurnRecord {
                                requested_model: "auto".to_string(),
                                responding_model: model_id_clone.clone(),
                                latency_ms: t0.elapsed().as_millis() as u64,
                                success: false,
                                status_code: embedded_code,
                                request_message_count: message_count,
                                loop_key: None,
                                response_message_count: 1,
                                has_tool_calls,
                                tool_call_count,
                                key_label: key_label.as_deref().map(|s| s.to_string()),
                                is_racing: true,
                                error: Some(embedded_msg.clone()),
                                request_tokens: None,
                                response_tokens: None,
                            });
                            return (
                                model_id_for_task,
                                Err(format!(
                                    "HTTP {} (in-stream) from {}: {}",
                                    embedded_code, model_id_clone, embedded_msg
                                )),
                            );
                        }
                    }
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
                    log_turn_request(TurnRecord {
                        requested_model: "auto".to_string(),
                        responding_model: model_id_clone.clone(),
                        latency_ms: latency as u128 as u64,
                        success: status.is_success(),
                        status_code: status.as_u16(),
                        request_message_count: message_count,
                        loop_key: None,
                        response_message_count: 1,
                        has_tool_calls,
                        tool_call_count,
                        key_label: key_label.as_deref().map(|s| s.to_string()),
                        is_racing: true,
                        error: if status.is_success() {
                            None
                        } else {
                            Some(format!("HTTP {}", status.as_u16()))
                        },
                        request_tokens: None,
                        response_tokens: None,
                    });
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
                    log_turn_request(TurnRecord {
                        requested_model: "auto".to_string(),
                        responding_model: model_id_clone.clone(),
                        latency_ms: t0.elapsed().as_millis() as u64,
                        success: false,
                        status_code: StatusCode::BAD_GATEWAY.as_u16(),
                        request_message_count: message_count,
                        loop_key: None,
                        response_message_count: 1,
                        has_tool_calls,
                        tool_call_count,
                        key_label: key_label.as_deref().map(|s| s.to_string()),
                        is_racing: true,
                        error: Some(e.to_string()),
                        request_tokens: None,
                        response_tokens: None,
                    });
                    Err(format!("request error: {}", e))
                }
                Err(_) => {
                    state_clone.gateway_metrics.record_timeout();
                    let timeout_error = if send_timeout_ms_for_model < timeout_ms_for_model {
                        format!(
                            "racing deadline exceeded after {}ms",
                            state_clone.racing_max_total_request_ms
                        )
                    } else {
                        format!("timeout after {}ms", timeout_ms_for_model)
                    };
                    record_model_timeout(
                        &state_clone,
                        &model_id_clone,
                        key_label.as_deref(),
                        send_timeout_ms_for_model,
                    );
                    log_turn_request(TurnRecord {
                        requested_model: "auto".to_string(),
                        responding_model: model_id_clone.clone(),
                        latency_ms: timeout_ms_for_model as u128 as u64,
                        success: false,
                        status_code: StatusCode::GATEWAY_TIMEOUT.as_u16(),
                        request_message_count: message_count,
                        loop_key: None,
                        response_message_count: 1,
                        has_tool_calls,
                        tool_call_count,
                        key_label: key_label.as_deref().map(|s| s.to_string()),
                        is_racing: true,
                        error: Some(timeout_error.clone()),
                        request_tokens: None,
                        response_tokens: None,
                    });
                    Err(timeout_error)
                }
            };
            (model_id_for_task, task_result)
        });
    }
    let actual_fanout = tasks.len();
    state.gateway_metrics.record_fanout(actual_fanout);
    if actual_fanout == 0 {
        if state.racing_solo_fallback {
            let solo_models = solo_candidate_models(&state, models);
            if !solo_models.is_empty() {
                state.gateway_metrics.record_fanout(1);
                return solo_model_fallback(state, body, solo_models, deadline).await;
            }
        }
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
    let mut saw_transient_failure = false;
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
                if is_transient_race_error(&e) {
                    saw_transient_failure = true;
                }
                last_error = Some(e);
            }
            Err(e) => {
                eprintln!("[racing] task failed: {}", e);
                saw_non_rate_limit_failure = true;
                saw_transient_failure = true;
                last_error = Some(e.to_string());
            }
        }
    }
    for (key_idx, retry_after_secs) in pending_rate_limited_keys {
        state.pool.mark_rate_limited(key_idx, retry_after_secs);
    }
    if saw_non_rate_limit_failure {
        state.gateway_metrics.record_all_racers_failed();
    }
    if state.racing_solo_fallback && saw_transient_failure {
        let mut solo_models: Vec<String> = solo_candidate_models(&state, models)
            .into_iter()
            .filter(|model| !models_to_race.iter().any(|raced| raced == model))
            .collect();
        if solo_models.is_empty() {
            solo_models = solo_candidate_models(&state, models);
        }
        if !solo_models.is_empty() {
            state.gateway_metrics.record_fanout(1);
            return solo_model_fallback(state, body, solo_models, deadline).await;
        }
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
