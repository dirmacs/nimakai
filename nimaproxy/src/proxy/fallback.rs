use crate::proxy::admission::{acquire_gateway_permits, no_key_response};
use crate::proxy::chat::MAX_RETRIES;
use crate::proxy::chat_helpers::accept_header_for_body;
use crate::proxy::chat_helpers::apply_model_params;
use crate::proxy::http::log_turn_request;
use crate::turn_log::TurnRecord;
use crate::proxy::messages::{
    fix_message_ordering, inject_minimax_system_message, inject_mistral_tool_params,
    mistral_validation_error, normalize_assistant_messages, sanitize_tool_calls,
    transform_message_roles,
};
use crate::proxy::racing::{racing_deadline_response, timeout_before_deadline};
use crate::proxy::timeouts::request_turn_summary;
use crate::proxy::timeouts::{record_model_timeout, timeout_for_model};
use crate::proxy::upstream_errors::{is_hard_model_error, sse_body_error};
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
use tokio::time::timeout;
use tracing::warn;
fn prepare_model_body(state: &AppState, body: &Bytes, model_id: &str) -> Option<Bytes> {
    let mut json: Value = serde_json::from_slice(body).ok()?;
    json["model"] = Value::String(model_id.to_string());
    inject_mistral_tool_params(&mut json, model_id);
    inject_minimax_system_message(&mut json, model_id);
    sanitize_tool_calls(&mut json);
    transform_message_roles(&mut json, model_id, state);
    fix_message_ordering(&mut json);
    normalize_assistant_messages(&mut json);
    if let Some(params) = state.model_params.get(model_id) {
        apply_model_params(&mut json, params);
    }
    serde_json::to_vec(&json).ok().map(Bytes::from)
}
pub(crate) async fn solo_model_fallback(
    state: Arc<AppState>,
    body: Bytes,
    model_ids: Vec<String>,
    deadline: Option<Instant>,
) -> Response {
    state.gateway_metrics.record_solo_fallback();
    if model_ids.len() > 1 {
        state.gateway_metrics.record_sequential_fallback();
    }
    let mut last_error: Option<(StatusCode, String)> = None;
    for model_id in model_ids {
        if timeout_before_deadline(deadline, state.racing_timeout_ms).is_none() {
            return racing_deadline_response(&state);
        }
        let Some(req_body) = prepare_model_body(&state, &body, &model_id) else {
            return (StatusCode::BAD_REQUEST, "invalid JSON body").into_response();
        };
        if let Some(response) = mistral_validation_error(&model_id, &req_body) {
            return response;
        }
        let n = state.pool.len().clamp(1, MAX_RETRIES);
        for _ in 0..n {
            let (upstream_permit, key_lease) = match acquire_gateway_permits(&state).await {
                Ok(permits) => permits,
                Err(response) => return response,
            };
            let _upstream_permit = upstream_permit;
            let key_idx = key_lease.idx;
            let key_label = key_lease.label.clone();
            let request_timeout_ms = timeout_for_model(&state, &model_id);
            let Some(send_timeout_ms) = timeout_before_deadline(deadline, request_timeout_ms)
            else {
                return racing_deadline_response(&state);
            };
            let (message_count, has_tool_calls, tool_call_count) = request_turn_summary(&req_body);
            let t0 = Instant::now();
            let result = timeout(
                Duration::from_millis(send_timeout_ms),
                state
                    .client
                    .post(format!("{}/v1/chat/completions", state.target))
                    .header("Authorization", format!("Bearer {}", key_lease.key))
                    .header("Content-Type", "application/json")
                    .header("Accept", accept_header_for_body(&req_body))
                    .body(req_body.clone())
                    .send(),
            )
            .await;
            let resp = match result {
                Ok(Ok(resp)) => resp,
                Ok(Err(e)) => {
                    let msg = e.to_string();
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
                    log_turn_request(TurnRecord {
                        requested_model: "auto".to_string(),
                        responding_model: model_id.clone(),
                        latency_ms: t0.elapsed().as_millis() as u64,
                        success: false,
                        status_code: StatusCode::BAD_GATEWAY.as_u16(),
                        request_message_count: message_count,
                        response_message_count: 1,
                        has_tool_calls,
                        tool_call_count,
                        key_label: key_label.as_deref().map(|s| s.to_string()),
                        is_racing: true,
                        error: Some(msg.clone()),
                        request_tokens: None,
                        response_tokens: None,
                    });
                    last_error = Some((StatusCode::BAD_GATEWAY, msg));
                    break;
                }
                Err(_) => {
                    state.gateway_metrics.record_timeout();
                    let msg = if send_timeout_ms < request_timeout_ms {
                        format!(
                            "racing deadline exceeded after {}ms",
                            state.racing_max_total_request_ms
                        )
                    } else {
                        format!("upstream timeout after {}ms", request_timeout_ms)
                    };
                    record_model_timeout(&state, &model_id, key_label.as_deref(), send_timeout_ms);
                    log_turn_request(TurnRecord {
                        requested_model: "auto".to_string(),
                        responding_model: model_id.clone(),
                        latency_ms: send_timeout_ms as u128 as u64,
                        success: false,
                        status_code: StatusCode::GATEWAY_TIMEOUT.as_u16(),
                        request_message_count: message_count,
                        response_message_count: 1,
                        has_tool_calls,
                        tool_call_count,
                        key_label: key_label.as_deref().map(|s| s.to_string()),
                        is_racing: true,
                        error: Some(msg.clone()),
                        request_tokens: None,
                        response_tokens: None,
                    });
                    last_error = Some((StatusCode::GATEWAY_TIMEOUT, msg));
                    break;
                }
            };
            let status = resp.status();
            if status == StatusCode::TOO_MANY_REQUESTS {
                state.gateway_metrics.record_rate_limit();
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(60);
                state.pool.mark_rate_limited(key_idx, retry_after);
                last_error = Some((
                    StatusCode::TOO_MANY_REQUESTS,
                    "all API keys rate-limited".to_string(),
                ));
                continue;
            }
            let ttfc_ms = t0.elapsed().as_millis() as f64;
            let ok = status.is_success();
            let resp_status =
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let content_type = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/json")
                .to_string();
            let Some(body_timeout_ms) = timeout_before_deadline(deadline, request_timeout_ms)
            else {
                return racing_deadline_response(&state);
            };
            let body_bytes = match timeout(Duration::from_millis(body_timeout_ms), resp.bytes())
                .await
            {
                Ok(Ok(bytes)) => bytes,
                Ok(Err(e)) => {
                    let msg = e.to_string();
                    log_turn_request(TurnRecord {
                        requested_model: "auto".to_string(),
                        responding_model: model_id.clone(),
                        latency_ms: t0.elapsed().as_millis() as u64,
                        success: false,
                        status_code: StatusCode::BAD_GATEWAY.as_u16(),
                        request_message_count: message_count,
                        response_message_count: 1,
                        has_tool_calls,
                        tool_call_count,
                        key_label: key_label.as_deref().map(|s| s.to_string()),
                        is_racing: true,
                        error: Some(msg.clone()),
                        request_tokens: None,
                        response_tokens: None,
                    });
                    last_error = Some((StatusCode::BAD_GATEWAY, msg));
                    break;
                }
                Err(_) => {
                    state.gateway_metrics.record_timeout();
                    let msg = if body_timeout_ms < request_timeout_ms {
                        format!(
                            "racing deadline exceeded after {}ms",
                            state.racing_max_total_request_ms
                        )
                    } else {
                        format!("upstream body timeout after {}ms", request_timeout_ms)
                    };
                    record_model_timeout(&state, &model_id, key_label.as_deref(), body_timeout_ms);
                    log_turn_request(TurnRecord {
                        requested_model: "auto".to_string(),
                        responding_model: model_id.clone(),
                        latency_ms: body_timeout_ms as u128 as u64,
                        success: false,
                        status_code: StatusCode::GATEWAY_TIMEOUT.as_u16(),
                        request_message_count: message_count,
                        response_message_count: 1,
                        has_tool_calls,
                        tool_call_count,
                        key_label: key_label.as_deref().map(|s| s.to_string()),
                        is_racing: true,
                        error: Some(msg.clone()),
                        request_tokens: None,
                        response_tokens: None,
                    });
                    last_error = Some((StatusCode::GATEWAY_TIMEOUT, msg));
                    break;
                }
            };
            let body_str = std::str::from_utf8(&body_bytes).unwrap_or("");
            let error_excerpt = body_str.chars().take(400).collect::<String>();
            // Same in-stream error detection as the racing leg: an HTTP 2xx with an
            // embedded upstream error body (e.g. NVIDIA NIM's overloaded-model SSE frame)
            // must not be treated as a real success. Move on to the next key/model instead
            // of returning the fake-success response to the client.
            if status.is_success() {
                if let Some((embedded_code, embedded_msg)) =
                    sse_body_error(&body_bytes, Some(&content_type))
                {
                    if let Some(label) = key_label.as_ref() {
                        state
                            .model_stats
                            .record_with_key(&model_id, label, ttfc_ms, false);
                    } else {
                        state.model_stats.record(&model_id, ttfc_ms, false);
                    }
                    log_turn_request(TurnRecord {
                        requested_model: "auto".to_string(),
                        responding_model: model_id.clone(),
                        latency_ms: ttfc_ms as u128 as u64,
                        success: false,
                        status_code: embedded_code,
                        request_message_count: message_count,
                        response_message_count: 1,
                        has_tool_calls,
                        tool_call_count,
                        key_label: key_label.as_deref().map(|s| s.to_string()),
                        is_racing: true,
                        error: Some(embedded_msg.clone()),
                        request_tokens: None,
                        response_tokens: None,
                    });
                    last_error = Some((
                        StatusCode::from_u16(embedded_code).unwrap_or(StatusCode::BAD_GATEWAY),
                        embedded_msg,
                    ));
                    break;
                }
            }
            // Auth failure: this key itself is bad, not the model. Quarantine the key and
            // retry with the next available key for the same model — do NOT touch
            // model_stats/hard-error tracking for `model_id`.
            if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                state.pool.mark_auth_failed(
                    key_idx,
                    Duration::from_secs(state.auth_failure_cooldown_secs),
                );
                warn!(
                    key_idx = key_idx,
                    key_label = key_label.as_deref().unwrap_or("unknown"),
                    model = %model_id,
                    status = status.as_u16(),
                    cooldown_secs = state.auth_failure_cooldown_secs,
                    "auth failure (401/403) on leased key; quarantining and retrying with next key"
                );
                last_error = Some((resp_status, error_excerpt));
                continue;
            }
            let hard_model_error = status == StatusCode::BAD_REQUEST
                && is_hard_model_error(&format!("HTTP 400 {body_str}"));
            if hard_model_error {
                state.model_stats.record_hard_error(&model_id, body_str);
            }
            if ok {
                state.pool.record_success(key_idx);
            }
            if let Some(label) = key_label.as_ref() {
                state
                    .model_stats
                    .record_with_key(&model_id, label, ttfc_ms, ok);
            } else {
                state.model_stats.record(&model_id, ttfc_ms, ok);
            }
            log_turn_request(TurnRecord {
                requested_model: "auto".to_string(),
                responding_model: model_id.clone(),
                latency_ms: ttfc_ms as u128 as u64,
                success: ok,
                status_code: status.as_u16(),
                request_message_count: message_count,
                response_message_count: 1,
                has_tool_calls,
                tool_call_count,
                key_label: key_label.as_deref().map(|s| s.to_string()),
                is_racing: true,
                error: if ok {
                    None
                } else {
                    Some(error_excerpt.clone())
                },
                request_tokens: None,
                response_tokens: None,
            });
            if ok {
                state.gateway_metrics.record_racing_win(&model_id);
                let mut response = Response::new(Body::from(body_bytes));
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
            if resp_status.is_server_error() || hard_model_error {
                last_error = Some((resp_status, error_excerpt));
                break;
            }
            let mut response = Response::new(Body::from(body_bytes));
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
    if let Some((status, msg)) = last_error {
        return (status, msg).into_response();
    }
    no_key_response(&state)
}
