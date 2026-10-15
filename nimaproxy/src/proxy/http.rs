use crate::proxy::admission::acquire_gateway_permits;
use crate::proxy::chat::MAX_RETRIES;
use crate::proxy::messages::extract_response_metrics;
use crate::AppState;
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use serde_json::Value;
/// GET /v1/models — configured model list when routing/racing is enabled, otherwise passthrough.
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::warn;
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
                "configured_max_in_flight": s.configured_max_in_flight,
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
        "key_window_capacity": state.pool.window_capacity(),
        "key_available_permits": state.pool.available_permits(),
        "admission_wait_ms": state.admission_wait_ms,
        "routing_enabled": state.routing_enabled(),
        "racing_enabled": state.racing_enabled(),
        "routing_models": routing_models,
        "racing_models": racing_models,
        "racing_max_parallel": state.racing_max_parallel,
        "racing_timeout_ms": state.racing_timeout_ms,
        "racing_max_total_request_ms": state.racing_max_total_request_ms,
        "racing_adaptive": state.racing_adaptive,
        "racing_min_parallel": state.racing_min_parallel,
        "racing_pressure_parallel": state.racing_pressure_parallel,
        "racing_degraded_parallel": state.racing_degraded_parallel,
        "racing_large_prompt_char_threshold": state.racing_large_prompt_char_threshold,
        "racing_large_prompt_parallel": state.racing_large_prompt_parallel,
        "racing_solo_fallback": state.racing_solo_fallback,
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
                "configured_max_in_flight": s.configured_max_in_flight,
                "auth_failures": s.auth_failures,
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
            "key_window_capacity": state.pool.window_capacity(),
            "key_available_permits": state.pool.available_permits(),
            "admission_wait_ms": state.admission_wait_ms,
            "overload_rejects": metrics.overload_rejects,
            "no_key_rejects": metrics.no_key_rejects,
            "timeout_count": metrics.timeout_count,
            "rate_limit_count": metrics.rate_limit_count,
            "fanout_total": metrics.fanout_total,
            "fanout_samples": metrics.fanout_samples,
            "fanout_avg": metrics.fanout_avg,
            "solo_fallbacks": metrics.solo_fallbacks,
            "sequential_fallbacks": metrics.sequential_fallbacks,
            "racing_all_failed": metrics.racing_all_failed,
            "racing_deadline_exceeded": metrics.racing_deadline_exceeded,
            "racing_wins": metrics.racing_wins,
        },
        "racing_models": racing_models,
        "racing_enabled": state.racing_enabled(),
        "racing_max_parallel": state.racing_max_parallel,
        "racing_timeout_ms": state.racing_timeout_ms,
        "racing_max_total_request_ms": state.racing_max_total_request_ms,
        "racing_adaptive": state.racing_adaptive,
        "racing_min_parallel": state.racing_min_parallel,
        "racing_pressure_parallel": state.racing_pressure_parallel,
        "racing_degraded_parallel": state.racing_degraded_parallel,
        "racing_large_prompt_char_threshold": state.racing_large_prompt_char_threshold,
        "racing_large_prompt_parallel": state.racing_large_prompt_parallel,
        "racing_solo_fallback": state.racing_solo_fallback,
        "racing_fast_models": state.racing_fast_models.clone(),
        "racing_fallback_models": state.racing_fallback_models.clone(),
        "pruned_models": state.pruned_models.lock().map(|g| g.clone()).unwrap_or_default(),
        "auth_failure_cooldown_secs": state.auth_failure_cooldown_secs,
    });
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
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
    let mut last_auth_failure_response: Option<Response> = None;
    for _ in 0..n {
        let (upstream_permit, key_lease) = match acquire_gateway_permits(&state).await {
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
                if status == StatusCode::TOO_MANY_REQUESTS {
                    state.gateway_metrics.record_rate_limit();
                    state.pool.mark_rate_limited(key_lease.idx, 60);
                    continue;
                }
                if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                    state.pool.mark_auth_failed(
                        key_lease.idx,
                        Duration::from_secs(state.auth_failure_cooldown_secs),
                    );
                    warn!(
                        key_idx = key_lease.idx,
                        key_label = key_label.as_deref().unwrap_or("unknown"),
                        status = status.as_u16(),
                        cooldown_secs = state.auth_failure_cooldown_secs,
                        "auth failure (401/403) on leased key; quarantining and retrying with next key"
                    );
                    let auth_resp_status =
                        StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::FORBIDDEN);
                    let auth_content_type = resp
                        .headers()
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("application/json")
                        .to_string();
                    let auth_failure_body = resp.bytes().await.unwrap_or_default();
                    let mut auth_response = Response::new(Body::from(auth_failure_body));
                    *auth_response.status_mut() = auth_resp_status;
                    auth_response.headers_mut().insert(
                        "content-type",
                        HeaderValue::from_str(&auth_content_type)
                            .unwrap_or_else(|_| HeaderValue::from_static("application/json")),
                    );
                    last_auth_failure_response = Some(auth_response);
                    log_turn_request(
                        &model_id,
                        &model_id,
                        t0.elapsed().as_millis(),
                        false,
                        status.as_u16(),
                        1,
                        false,
                        0,
                        key_label.as_deref(),
                        false,
                        Some(format!(
                            "upstream auth failure ({}); key quarantined",
                            status.as_u16()
                        )),
                    );
                    continue;
                }
                if ok {
                    state.pool.record_success(key_lease.idx);
                }
                let ttfc_ms = t0.elapsed().as_millis() as f64;
                let resp_status =
                    StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
                let content_type = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("application/json")
                    .to_string();
                let stream = crate::proxy::streaming::map_upstream(resp.bytes_stream());
                // Assemble under an inter-chunk idle bound rather than a
                // whole-body deadline: these endpoints need the full body to
                // extract token metrics, but a long generation must not be
                // killed for taking a long time. See `proxy::streaming`.
                let window = std::time::Duration::from_millis(state.racing_timeout_ms);
                let full_body =
                    match crate::proxy::streaming::assemble_with_idle_bound(stream, window).await {
                        Ok(bytes) => bytes,
                        Err(e) => {
                            return (StatusCode::BAD_GATEWAY, e.to_string()).into_response();
                        }
                    };
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
    if let Some(response) = last_auth_failure_response {
        return response;
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
    let mut last_auth_failure_response: Option<Response> = None;
    for _ in 0..n {
        let (upstream_permit, key_lease) = match acquire_gateway_permits(&state).await {
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
                if status == StatusCode::TOO_MANY_REQUESTS {
                    state.gateway_metrics.record_rate_limit();
                    state.pool.mark_rate_limited(key_lease.idx, 60);
                    continue;
                }
                if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                    state.pool.mark_auth_failed(
                        key_lease.idx,
                        Duration::from_secs(state.auth_failure_cooldown_secs),
                    );
                    warn!(
                        key_idx = key_lease.idx,
                        key_label = key_label.as_deref().unwrap_or("unknown"),
                        status = status.as_u16(),
                        cooldown_secs = state.auth_failure_cooldown_secs,
                        "auth failure (401/403) on leased key; quarantining and retrying with next key"
                    );
                    let auth_resp_status =
                        StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::FORBIDDEN);
                    let auth_content_type = resp
                        .headers()
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("application/json")
                        .to_string();
                    let auth_failure_body = resp.bytes().await.unwrap_or_default();
                    let mut auth_response = Response::new(Body::from(auth_failure_body));
                    *auth_response.status_mut() = auth_resp_status;
                    auth_response.headers_mut().insert(
                        "content-type",
                        HeaderValue::from_str(&auth_content_type)
                            .unwrap_or_else(|_| HeaderValue::from_static("application/json")),
                    );
                    last_auth_failure_response = Some(auth_response);
                    log_turn_request(
                        &model_id,
                        &model_id,
                        t0.elapsed().as_millis(),
                        false,
                        status.as_u16(),
                        1,
                        false,
                        0,
                        key_label.as_deref(),
                        false,
                        Some(format!(
                            "upstream auth failure ({}); key quarantined",
                            status.as_u16()
                        )),
                    );
                    continue;
                }
                if ok {
                    state.pool.record_success(key_lease.idx);
                }
                let ttfc_ms = t0.elapsed().as_millis() as f64;
                let resp_status =
                    StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
                let content_type = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("application/json")
                    .to_string();
                let stream = crate::proxy::streaming::map_upstream(resp.bytes_stream());
                let window = std::time::Duration::from_millis(state.racing_timeout_ms);
                let full_body =
                    match crate::proxy::streaming::assemble_with_idle_bound(stream, window).await {
                        Ok(bytes) => bytes,
                        Err(e) => {
                            return (StatusCode::BAD_GATEWAY, e.to_string()).into_response();
                        }
                    };
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
    if let Some(response) = last_auth_failure_response {
        return response;
    }
    (
        StatusCode::TOO_MANY_REQUESTS,
        "all keys exhausted after retries",
    )
        .into_response()
}
/// Log a completed turn
pub(crate) fn log_turn_request(
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
