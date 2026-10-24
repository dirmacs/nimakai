use crate::key_pool::BenchReason;
use crate::proxy::admission::acquire_gateway_permits;
use crate::proxy::chat::MAX_RETRIES;
use crate::proxy::messages::extract_response_metrics;
use crate::proxy::retry::upstream_retry_after_secs;
use crate::turn_log::TurnRecord;
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
    // The shared client carries no timeout (see `AppState::new_with_controls`),
    // and this passthrough has no per-phase bounds of its own, so the request
    // sets its own whole-request cap: a catalog fetch is short, and a hung
    // upstream must not hold this handler (and its key) open forever.
    match state
        .client
        .get(format!("{}/v1/models", state.target))
        .header("Authorization", format!("Bearer {}", key))
        .timeout(Duration::from_secs(120))
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
                "bench_reason": s.bench_reason,
                "stalls": s.stalls,
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
    let key_statuses = state.pool.status();
    let keys_benched_rate_limit = key_statuses
        .iter()
        .filter(|s| s.bench_reason == Some(BenchReason::RateLimit))
        .count();
    let keys_json: Vec<Value> = key_statuses
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
                "rate_limit_ratio": s.rate_limit_ratio,
                "bench_reason": s.bench_reason,
                "stalls": s.stalls,
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
            "aimd_pressure_adaptive": state.aimd_pressure_adaptive,
            "aimd_max_success_bar": state.aimd_max_success_bar,
            "rate_limit_bench_cap_secs": state.rate_limit_bench_cap_secs,
            "key_window_capacity": state.pool.window_capacity(),
            "key_available_permits": state.pool.available_permits(),
            "admission_wait_ms": state.admission_wait_ms,
            "overload_rejects": metrics.overload_rejects,
            "no_key_rejects": metrics.no_key_rejects,
            "timeout_count": metrics.timeout_count,
            "truncated_streams": metrics.truncated_streams,
            "rate_limit_count": metrics.rate_limit_count,
            "keys_benched_rate_limit": keys_benched_rate_limit,
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
        "pruned_models": (*state.pruned_models.load_full()).clone(),
        "auth_failure_cooldown_secs": state.auth_failure_cooldown_secs,
    });
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
}
/// GET /loops — live agent-loop depth aggregates over the recent ring.
pub async fn loops(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // Read-only aggregates: counts only, never message content, key material, or session ids.
    let recent = state.loop_tracker.recent_loops();
    let mut rounds: Vec<u64> = recent.iter().map(|l| l.rounds).collect();
    let mut wall_ms: Vec<u64> = recent.iter().map(loop_wall_ms).collect();
    let mut model_ms: Vec<u64> = recent.iter().map(|l| l.model_ms).collect();
    let mut tool_calls: Vec<u64> = recent.iter().map(|l| l.tool_calls_distinct).collect();
    // Counted in a BTreeMap so iteration is ascending by round count.
    let mut histogram_counts = std::collections::BTreeMap::<u64, u64>::new();
    for l in &recent {
        *histogram_counts.entry(l.rounds).or_insert(0) += 1;
    }
    let histogram: serde_json::Map<String, Value> = histogram_counts
        .into_iter()
        .map(|(r, c)| (format!("{r}"), serde_json::json!(c)))
        .collect();
    let mut new_turn = 0u64;
    let mut idle = 0u64;
    for l in &recent {
        match l.ended_by {
            crate::loop_tracker::LoopEnd::NewTurn => new_turn += 1,
            crate::loop_tracker::LoopEnd::Idle => idle += 1,
        }
    }
    let body = serde_json::json!({
        "open_loops": state.loop_tracker.open_loops(),
        "recent_loops": recent.len(),
        "recent_capacity": state.loop_tracker.recent_capacity(),
        "idle_timeout_secs": state.loop_tracker.idle_timeout().as_secs(),
        "rounds": loop_dist_summary(&mut rounds),
        "wall_ms": loop_dist_summary(&mut wall_ms),
        "model_ms": loop_dist_summary(&mut model_ms),
        "tool_calls_distinct": loop_dist_summary(&mut tool_calls),
        "rounds_histogram": histogram,
        "ended_by": {
            "new_turn": new_turn,
            "idle": idle,
        },
    });
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
}

/// Nearest-rank percentile over an ascending sample: the smallest value at or
/// above which `p` percent of the sample lies. An empty sample reports 0
/// rather than panicking, so a dashboard can read the endpoint's shape before
/// any loop has closed. Mirrors the offline `nimaproxy-query depth` math so
/// the live endpoint and the log-derived report agree.
fn loop_percentile(sorted: &[u64], p: u64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (p as f64 / 100.0 * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

/// `{"n","p50","p90","p99","max"}` over a sample, sorted in place. An empty
/// sample reports all zeros so the response shape is stable from boot.
fn loop_dist_summary(values: &mut [u64]) -> Value {
    values.sort_unstable();
    serde_json::json!({
        "n": values.len(),
        "p50": loop_percentile(values, 50),
        "p90": loop_percentile(values, 90),
        "p99": loop_percentile(values, 99),
        "max": values.last().copied().unwrap_or(0),
    })
}

/// First-to-last span of a closed loop in milliseconds, from the loop's own
/// timestamps. A loop that ended the instant it began reports 0.
fn loop_wall_ms(summary: &crate::loop_tracker::ClosedLoop) -> u64 {
    (summary.ended_at - summary.started_at)
        .to_std()
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
    let n = state.pool.len().clamp(1, MAX_RETRIES);
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
        // Bound the send (headers) the way the chat path does: the shared
        // client carries no timeout, and a whole-request cap here would
        // reintroduce exactly the defect the client builder removed — the
        // body below is assembled under the idle rail instead. A per-request
        // `.timeout(...)` would cover the body too, so it is NOT equivalent.
        let send_window =
            Duration::from_millis(crate::proxy::timeouts::timeout_for_model(&state, &model_id));
        let result = tokio::time::timeout(
            send_window,
            state
                .client
                .post(format!("{}/v1/completions", state.target))
                .header("Authorization", format!("Bearer {}", key_lease.key))
                .header("Content-Type", "application/json")
                .body(body.clone())
                .send(),
        )
        .await;
        match result {
            // A send timeout is an admitted upstream stall, so it is counted
            // for the key. A send failure is an I/O error, not a stall.
            Err(_) => {
                state.pool.note_upstream_stall(key_lease.idx);
                let msg = format!(
                    "upstream send timed out after {}ms",
                    send_window.as_millis()
                );
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
                return (StatusCode::BAD_GATEWAY, msg).into_response();
            }
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
                return (StatusCode::BAD_GATEWAY, msg).into_response();
            }
            Ok(Ok(resp)) => {
                let status = resp.status();
                let ok = status.is_success();
                if status == StatusCode::TOO_MANY_REQUESTS {
                    state.gateway_metrics.record_rate_limit();
                    let retry_after = upstream_retry_after_secs(resp.headers());
                    state.pool.mark_rate_limited(key_lease.idx, retry_after);
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
                    log_turn_request(TurnRecord {
                        requested_model: model_id.clone(),
                        responding_model: model_id.clone(),
                        latency_ms: t0.elapsed().as_millis() as u64,
                        success: false,
                        status_code: status.as_u16(),
                        request_message_count: 1,
                        loop_key: None,
                        response_message_count: 1,
                        has_tool_calls: false,
                        tool_call_count: 0,
                        // Embeddings requests carry no tools or max_tokens.
                        request_has_tools: None,
                        request_max_tokens: None,
                        key_label: key_label.as_deref().map(|s| s.to_string()),
                        is_racing: false,
                        error: Some(format!(
                            "upstream auth failure ({}); key quarantined",
                            status.as_u16()
                        )),
                        request_tokens: None,
                        response_tokens: None,
                        // The response side is not known here: this record is
                        // written before any stream is observed, and the
                        // request failed anyway. `None` keeps "no tool calls"
                        // and "not observed" distinguishable.
                        response_tool_calls: None,
                        // Not known on any path that has not observed the response body:
                        // ttfb and failure records, and the non-streaming body id is filled
                        // in where the body is assembled.
                        response_body_id: None,
                    });
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
                            if crate::proxy::streaming::is_stall(&e) {
                                state.pool.note_upstream_stall(key_lease.idx);
                            }
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
    let n = state.pool.len().clamp(1, MAX_RETRIES);
    let mut last_auth_failure_response: Option<Response> = None;
    for _ in 0..n {
        let (upstream_permit, key_lease) = match acquire_gateway_permits(&state).await {
            Ok(permits) => permits,
            Err(response) => return response,
        };
        let _upstream_permit = upstream_permit;
        let key_label = key_lease.label.clone();
        let t0 = Instant::now();
        // As in `completions` above: bound the send (headers) with the
        // first-byte window. A per-request `.timeout(...)` is NOT equivalent
        // — it would cover the body too and reintroduce the defect the
        // client builder removed; the body is assembled under the idle rail.
        let send_window =
            Duration::from_millis(crate::proxy::timeouts::timeout_for_model(&state, &model_id));
        let result = tokio::time::timeout(
            send_window,
            state
                .client
                .post(format!("{}/v1/embeddings", state.target))
                .header("Authorization", format!("Bearer {}", key_lease.key))
                .header("Content-Type", "application/json")
                .body(body.clone())
                .send(),
        )
        .await;
        match result {
            // A send timeout is an admitted upstream stall, so it is counted
            // for the key. A send failure is an I/O error, not a stall.
            Err(_) => {
                state.pool.note_upstream_stall(key_lease.idx);
                let msg = format!(
                    "upstream send timed out after {}ms",
                    send_window.as_millis()
                );
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
                return (StatusCode::BAD_GATEWAY, msg).into_response();
            }
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
                return (StatusCode::BAD_GATEWAY, msg).into_response();
            }
            Ok(Ok(resp)) => {
                let status = resp.status();
                let ok = status.is_success();
                if status == StatusCode::TOO_MANY_REQUESTS {
                    state.gateway_metrics.record_rate_limit();
                    let retry_after = upstream_retry_after_secs(resp.headers());
                    state.pool.mark_rate_limited(key_lease.idx, retry_after);
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
                    log_turn_request(TurnRecord {
                        requested_model: model_id.clone(),
                        responding_model: model_id.clone(),
                        latency_ms: t0.elapsed().as_millis() as u64,
                        success: false,
                        status_code: status.as_u16(),
                        request_message_count: 1,
                        loop_key: None,
                        response_message_count: 1,
                        has_tool_calls: false,
                        tool_call_count: 0,
                        // Embeddings requests carry no tools or max_tokens.
                        request_has_tools: None,
                        request_max_tokens: None,
                        key_label: key_label.as_deref().map(|s| s.to_string()),
                        is_racing: false,
                        error: Some(format!(
                            "upstream auth failure ({}); key quarantined",
                            status.as_u16()
                        )),
                        request_tokens: None,
                        response_tokens: None,
                        // The response side is not known here: this record is
                        // written before any stream is observed, and the
                        // request failed anyway. `None` keeps "no tool calls"
                        // and "not observed" distinguishable.
                        response_tool_calls: None,
                        // Not known on any path that has not observed the response body:
                        // ttfb and failure records, and the non-streaming body id is filled
                        // in where the body is assembled.
                        response_body_id: None,
                    });
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
                            if crate::proxy::streaming::is_stall(&e) {
                                state.pool.note_upstream_stall(key_lease.idx);
                            }
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
/// Log a first-byte (or failure) record and return the request id it wrote.
///
/// The id is `TurnLog::new`'s, generated inside the write, so it is handed
/// back to the caller rather than read out of a shared slot: the caller needs
/// the id of ITS OWN record to thread into the completion record and the
/// response header, and under concurrency a shared "last written id" is
/// another request's.
pub(crate) fn log_turn_request(record: TurnRecord) -> Option<String> {
    use crate::turn_log::{log_turn as log_turn_event, TurnLog};
    log_turn_event(&TurnLog::new(record))
}

/// Identity of the turn a completion record belongs to.
///
/// These are the fields the first-byte record already carried; the completion
/// record repeats them so it can be read on its own, and reuses the first-byte
/// record's `request_id` so the two can be joined.
#[derive(Debug, Clone)]
pub struct TurnContext {
    pub requested_model: String,
    pub responding_model: String,
    pub message_count: usize,
    pub has_tool_calls: bool,
    pub tool_call_count: usize,
    pub key_label: Option<String>,
    pub is_racing: bool,
    pub loop_key: Option<crate::turn_log::LoopKey>,
    /// Whether the request carried a non-empty top-level `tools` array.
    pub request_has_tools: Option<bool>,
    /// Effective `max_tokens` for the request: the client value after
    /// model-default injection, NOT the raw client value.
    pub request_max_tokens: Option<i32>,
}

/// Structural outcome of a forwarded SSE stream, as carried into a turn record.
///
/// Every field is a fact about the stream, never about its text.
#[derive(Debug, Clone, Default)]
pub struct StreamCompletion {
    /// `request_id` of the first-byte record for this turn, so the two records
    /// can be joined.
    pub request_id: Option<String>,
    /// Total duration of the stream, first byte to last (ms).
    pub duration_ms: u128,
    pub finish_reason: Option<String>,
    pub content_chars: u64,
    /// Sum of `choices[0].delta.reasoning_content` characters across frames.
    /// 0 when the upstream emitted no reasoning deltas.
    pub reasoning_chars: u64,
    pub stream_ended_cleanly: bool,
    pub chunk_count: u64,
    pub body_bytes: u64,
    /// Usage tokens the upstream reported, if it reported any. `None` means
    /// the upstream sent no usage object — never "0 tokens".
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    /// Tool-call FRAGMENT entries: `choices[0].delta.tool_calls` entries summed
    /// across frames. This OVERCOUNTS logical calls — one call whose
    /// `arguments` stream over N frames counts N here. `tool_call_distinct` is
    /// the count of logical calls.
    pub tool_call_count: u64,
    /// Distinct logical tool calls, de-duplicated by the delta's `index`.
    pub tool_call_distinct: u64,
    /// Set when the stream was aborted rather than ended (idle-guard timeout,
    /// upstream error), which is also a truncation.
    pub abort_reason: Option<String>,
    /// Largest gap between two consecutive forwarded chunks, in milliseconds.
    /// `None` when fewer than two chunks were forwarded: no gap observed.
    pub max_gap_ms: Option<u64>,
    /// How the stream ended, in the tap's vocabulary (`clean`,
    /// `guard_stall`, `upstream_error`). What answers "guard abort or
    /// upstream close" on the next truncation without any capture infra.
    pub ended_by: String,
    /// TOP-LEVEL `id` of the upstream's response body - the id the client
    /// echoes back as `responseId`. The tap reads it off the SSE frames; the
    /// non-streaming path fills the same field from the assembled body.
    /// `None` when the upstream sent no top-level id.
    pub response_body_id: Option<String>,
}

/// Log the completion of a streamed turn.
///
/// This is the record that carries what the stream actually turned out to be.
/// The first-byte record is written the moment the first SSE byte arrives and
/// can only say "a response started"; this one is written when the stream ends
/// and says whether it finished, what finish reason the upstream gave, and how
/// much was forwarded. Without it a truncated generation is logged as a
/// success.
///
/// The record shares the first-byte record's `request_id`, so the two halves of
/// one turn can be read together.
pub(crate) fn log_turn_completion(
    turn: &TurnContext,
    completion: &StreamCompletion,
    loop_tracker: &crate::loop_tracker::LoopTracker,
    metrics: &crate::GatewayMetrics,
) {
    use crate::turn_log::log_turn as log_turn_event;
    let record = build_completion_record(turn, completion);
    if !completion.stream_ended_cleanly {
        // The stall counter (`timeout_count`) is the no-bytes path in the
        // request handler; this is the mid-stream cutoff, which arrives here
        // as a 200 with success=false. Separate mechanisms, separate numbers.
        metrics.record_truncated_stream();
    }
    if let Some(loop_key) = &turn.loop_key {
        // Enrich the round this completion belongs to with the real outcome.
        // `begin` already counted the round (even if it 504'd and never
        // reported), so `record_round` only enriches and bumps
        // `completed_rounds`.
        loop_tracker.record_round(
            &loop_key.session_id,
            crate::loop_tracker::RoundOutcome {
                model_ms: completion.duration_ms as u64,
                success: completion.stream_ended_cleanly,
                finish_reason: completion.finish_reason.clone(),
                tool_call_fragments: completion.tool_call_count,
                tool_calls_distinct: completion.tool_call_distinct,
            },
            chrono::Utc::now(),
        );
    }
    log_turn_event(&record);
}

/// Build the `completion` record for a finished stream.
///
/// Split out from [`log_turn_completion`] so the record's CONTENT can be
/// asserted without the process-wide turn logger: `init_logger` is a
/// `OnceLock`, so two tests that each initialize it collide (the second gets
/// `AlreadyExists`) and a test that writes through the global cannot be run
/// beside any other test that does. This is the same split
/// [`build_loop_record`] already uses for the loop summary.
///
/// The response-side tool-call count is the point of the split: it is known
/// only here, where the stream has ended.
pub(crate) fn build_completion_record(
    turn: &TurnContext,
    completion: &StreamCompletion,
) -> crate::turn_log::TurnLog {
    use crate::turn_log::{classify, TurnLog};
    let mut record = TurnLog::new(TurnRecord {
        requested_model: turn.requested_model.clone(),
        responding_model: turn.responding_model.clone(),
        latency_ms: completion.duration_ms as u64,
        // A truncated stream is not a success: the client was promised a
        // completion and did not get one.
        success: completion.stream_ended_cleanly,
        status_code: 200,
        request_message_count: turn.message_count,
        response_message_count: 1,
        has_tool_calls: turn.has_tool_calls,
        tool_call_count: turn.tool_call_count,
        request_has_tools: turn.request_has_tools,
        request_max_tokens: turn.request_max_tokens,
        key_label: turn.key_label.clone(),
        is_racing: turn.is_racing,
        loop_key: turn.loop_key.clone(),
        error: completion.abort_reason.clone(),
        request_tokens: completion.prompt_tokens,
        response_tokens: completion.completion_tokens,
        // The only place the response side is known: the stream has ended, so
        // the tap's de-duplicated logical-call count is final. `None` would
        // mean "not observed", which is not true here. The paired first-byte
        // record keeps `None` because the response has not happened yet at
        // first-byte time; the two share `request_id`, so a query reads the
        // count off either half of the pair.
        response_tool_calls: Some(completion.tool_call_distinct),
        // The response body's top-level id - what the client echoes as its
        // `responseId`. This is the join key the client-side TTFT
        // measurement needs: client.responseId == this field -> the record's
        // request_id -> the first-byte record's latency.
        response_body_id: completion.response_body_id.clone(),
    });
    record.record_kind = TurnLog::RECORD_KIND_COMPLETION.to_string();
    record.request_id = completion.request_id.clone();
    record.finish_reason = completion.finish_reason.clone();
    record.content_chars = Some(completion.content_chars);
    record.reasoning_chars = Some(completion.reasoning_chars);
    record.stream_ended_cleanly = Some(completion.stream_ended_cleanly);
    record.chunk_count = Some(completion.chunk_count);
    record.body_bytes = Some(completion.body_bytes);
    record.max_inter_chunk_gap_ms = completion.max_gap_ms;
    record.stream_ended_by = Some(completion.ended_by.clone());
    // The response-body id, so this completion can be joined to the client's
    // own per-message `responseId`. Only the completion path fills it in: the
    // paired ttfb record keeps `None` because a first byte says nothing about
    // which response body it belongs to, and on the non-streaming path the id
    // does not exist until the body is assembled.
    record.response_body_id = completion.response_body_id.clone();
    // Only when the upstream actually reported usage. A completion record with
    // no usage frame keeps the fields `None`, so a query can tell "0 tokens"
    // from "unknown".
    record.request_tokens = completion.prompt_tokens;
    record.response_tokens = completion.completion_tokens;
    record.failure_mode = Some(
        classify(
            completion.finish_reason.as_deref(),
            completion.content_chars,
            completion.stream_ended_cleanly,
        )
        .to_string(),
    );
    if let Some(reason) = completion.abort_reason.as_ref() {
        record.error = Some(reason.clone());
    }
    record
}

/// Log a closed loop as a `loop` record.
///
/// A loop record is not a request: its request-level fields are honest
/// placeholders (`requested_model`/`responding_model` empty, `latency_ms` 0,
/// `status_code` 200, `success` true, `error` None) so that a request-level
/// filter never decides whether a loop appears. Consumers must select on
/// `record_kind=loop` rather than reuse request filters, because a loop record
/// is not a request outcome.
///
/// `round_index: 0` on its `loop_key` is deliberate: a loop summary is not a
/// round, and any consumer grouping by round must filter `record_kind=loop`
/// out first.
pub(crate) fn build_loop_record(
    summary: &crate::loop_tracker::ClosedLoop,
) -> crate::turn_log::TurnLog {
    use crate::turn_log::{TurnLog, TurnRecord};
    let wall_ms = loop_wall_ms(summary);
    let ended_by = match summary.ended_by {
        crate::loop_tracker::LoopEnd::NewTurn => "new_turn",
        crate::loop_tracker::LoopEnd::Idle => "idle",
    }
    .to_string();
    let mut record = TurnLog::new(TurnRecord {
        requested_model: String::new(),
        responding_model: String::new(),
        latency_ms: 0,
        success: true,
        status_code: 200,
        request_message_count: 0,
        response_message_count: 0,
        has_tool_calls: false,
        tool_call_count: 0,
        request_has_tools: None,
        request_max_tokens: None,
        key_label: None,
        is_racing: false,
        loop_key: Some(crate::turn_log::LoopKey {
            session_id: summary.session_id.clone(),
            loop_seq: summary.loop_seq,
            round_index: 0,
        }),
        error: None,
        request_tokens: None,
        response_tokens: None,
        // A loop summary is not a request and has no response of its own. The
        // response-side counts already live in the dedicated `loop_*` fields.
        response_tool_calls: None,
        // Not known on any path that has not observed the response body:
        // ttfb and failure records, and the non-streaming body id is filled
        // in where the body is assembled.
        response_body_id: None,
    });
    record.record_kind = TurnLog::RECORD_KIND_LOOP.to_string();
    record.loop_started_at = Some(summary.started_at);
    record.loop_ended_at = Some(summary.ended_at);
    record.loop_rounds = Some(summary.rounds);
    record.loop_completed_rounds = Some(summary.completed_rounds);
    record.loop_model_ms = Some(summary.model_ms);
    record.loop_wall_ms = Some(wall_ms);
    record.loop_error_rounds = Some(summary.error_rounds);
    record.loop_tool_call_fragments = Some(summary.tool_call_fragments);
    record.loop_tool_calls_distinct = Some(summary.tool_calls_distinct);
    record.loop_last_finish_reason = summary.last_finish_reason.clone();
    record.loop_ended_by = Some(ended_by);
    record
}

/// Write a closed loop to the turn log as a `loop` record.
pub(crate) fn log_loop_summary(summary: &crate::loop_tracker::ClosedLoop) {
    use crate::turn_log::log_turn as log_turn_event;
    log_turn_event(&build_loop_record(summary));
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::loop_tracker::{ClosedLoop, LoopEnd};
    use crate::turn_log::TurnLog;
    use chrono::Utc;

    /// `build_loop_record` maps a closed loop onto a `loop` record whose
    /// request-level fields are honest placeholders and whose `loop_*` fields
    /// mirror the loop, including `loop_wall_ms` as the first-to-last span.
    ///
    /// This calls the production builder. An earlier version of this test
    /// hand-built a `TurnLog`, assigned the same fields to it, and asserted
    /// them back — a tautology that would have passed with the mapping deleted.
    #[test]
    fn test_build_loop_record_maps_a_closed_loop() {
        let started = Utc::now();
        let ended = started + chrono::Duration::seconds(5);
        let summary = ClosedLoop {
            session_id: "sess-1".to_string(),
            loop_seq: 2,
            started_at: started,
            ended_at: ended,
            rounds: 4,
            completed_rounds: 3,
            model_ms: 1200,
            error_rounds: 1,
            tool_call_fragments: 7,
            tool_calls_distinct: 3,
            last_finish_reason: Some("stop".to_string()),
            ended_by: LoopEnd::Idle,
        };

        let record = build_loop_record(&summary);

        assert_eq!(record.record_kind, TurnLog::RECORD_KIND_LOOP);
        assert_eq!(record.loop_rounds, Some(4));
        assert_eq!(record.loop_completed_rounds, Some(3));
        assert_eq!(record.loop_model_ms, Some(1200));
        // The span the user actually waited, derived from the loop's own
        // timestamps rather than from any clock read at write time.
        assert_eq!(record.loop_wall_ms, Some(5000));
        assert_eq!(record.loop_error_rounds, Some(1));
        assert_eq!(record.loop_tool_call_fragments, Some(7));
        assert_eq!(record.loop_tool_calls_distinct, Some(3));
        assert_eq!(record.loop_last_finish_reason.as_deref(), Some("stop"));
        assert_eq!(record.loop_ended_by.as_deref(), Some("idle"));
        assert_eq!(record.loop_started_at, Some(started));
        assert_eq!(record.loop_ended_at, Some(ended));
        // A summary is not a round, so it carries round_index 0: any consumer
        // grouping by round must filter `record_kind=loop` out first.
        assert_eq!(
            record.loop_key,
            Some(crate::turn_log::LoopKey {
                session_id: "sess-1".to_string(),
                loop_seq: 2,
                round_index: 0,
            })
        );
    }

    /// A loop that ended the same instant it began reports a zero span rather
    /// than panicking, and a loop with no reported finish reason keeps `None`
    /// rather than being given a placeholder.
    #[test]
    fn test_build_loop_record_handles_a_zero_span_and_no_reason() {
        let t = Utc::now();
        let summary = ClosedLoop {
            session_id: "s".to_string(),
            loop_seq: 1,
            started_at: t,
            ended_at: t,
            rounds: 1,
            completed_rounds: 1,
            model_ms: 0,
            error_rounds: 0,
            tool_call_fragments: 0,
            tool_calls_distinct: 0,
            last_finish_reason: None,
            ended_by: LoopEnd::NewTurn,
        };

        let record = build_loop_record(&summary);
        assert_eq!(record.loop_wall_ms, Some(0));
        assert_eq!(record.loop_ended_by.as_deref(), Some("new_turn"));
        assert_eq!(record.loop_last_finish_reason, None);
    }

    /// `loop_percentile` is nearest-rank: empty samples report 0 instead of
    /// panicking, a single element is every percentile, and a known sample
    /// yields the rank `ceil(p/100 * n)` can predict by hand.
    #[test]
    fn test_loop_percentile_is_nearest_rank() {
        assert_eq!(loop_percentile(&[], 50), 0);
        assert_eq!(loop_percentile(&[], 99), 0);
        assert_eq!(loop_percentile(&[7], 50), 7);
        assert_eq!(loop_percentile(&[7], 99), 7);
        // n=10: p50 -> rank ceil(5)=5 -> 50; p90 -> rank ceil(9)=9 -> 90;
        // p99 -> rank ceil(9.9)=10 -> 100.
        let v = [10u64, 20, 30, 40, 50, 60, 70, 80, 90, 100];
        assert_eq!(loop_percentile(&v, 50), 50);
        assert_eq!(loop_percentile(&v, 90), 90);
        assert_eq!(loop_percentile(&v, 99), 100);
        // n=4: p50 -> rank ceil(2)=2 -> 20, the smaller middle value.
        assert_eq!(loop_percentile(&[10u64, 20, 30, 40], 50), 20);
    }

    /// An empty sample still produces the full distribution object with zero
    /// fields, so the endpoint's shape is readable before any loop closes.
    #[test]
    fn test_loop_dist_summary_reports_zeros_when_empty() {
        let summary = loop_dist_summary(&mut []);
        assert_eq!(summary["n"], serde_json::json!(0));
        assert_eq!(summary["p50"], serde_json::json!(0));
        assert_eq!(summary["p90"], serde_json::json!(0));
        assert_eq!(summary["p99"], serde_json::json!(0));
        assert_eq!(summary["max"], serde_json::json!(0));
    }

    fn test_turn_context() -> TurnContext {
        TurnContext {
            requested_model: "test-model".to_string(),
            responding_model: "test-model".to_string(),
            message_count: 1,
            has_tool_calls: false,
            tool_call_count: 0,
            request_has_tools: None,
            request_max_tokens: None,
            key_label: None,
            is_racing: false,
            loop_key: None,
        }
    }

    fn test_completion(cleanly: bool) -> StreamCompletion {
        StreamCompletion {
            request_id: None,
            duration_ms: 100,
            finish_reason: None,
            content_chars: 10,
            reasoning_chars: 0,
            stream_ended_cleanly: cleanly,
            chunk_count: 2,
            body_bytes: 20,
            prompt_tokens: None,
            completion_tokens: None,
            tool_call_count: 0,
            tool_call_distinct: 0,
            abort_reason: if cleanly {
                None
            } else {
                Some("stream ended without data: [DONE]".to_string())
            },
            max_gap_ms: Some(12),
            ended_by: "clean".to_string(),
            response_body_id: None,
        }
    }

    /// A completion record carries the RESPONSE-side distinct tool-call count,
    /// and only the completion record does. MEASURED DEFECT this fixes: on
    /// 2026-10-20 traffic, 27.5% of completion rows carried request-side tool
    /// history while the response made zero tool calls, and the only
    /// per-request field available counted the request side — 63.9x
    /// overstatement in the summed total, so every per-request tool-call rate
    /// was wrong by that factor.
    ///
    /// The first-byte record keeps the field `None`: at first-byte time the
    /// response has not happened, so a value there would be fabricated. The
    /// two records share a `request_id`, so a query reads the count off either
    /// half of the pair.
    #[test]
    fn test_completion_record_carries_the_response_side_tool_call_count() {
        use crate::turn_log::TurnLog;

        // The non-call class: the request carried tool history (request-side
        // count 4 from two tool_calls entries plus two tool-role messages) and
        // the response made no tool call.
        let turn = TurnContext {
            requested_model: "auto".to_string(),
            responding_model: "m".to_string(),
            message_count: 5,
            has_tool_calls: true,
            tool_call_count: 4,
            request_has_tools: Some(true),
            request_max_tokens: None,
            key_label: None,
            is_racing: false,
            loop_key: None,
        };
        // A stream that made no tool call: zero distinct logical calls, and
        // also zero fragments (nothing arrived at all).
        let completion = StreamCompletion {
            tool_call_distinct: 0,
            tool_call_count: 0,
            ..test_completion(true)
        };

        // The record is the evidence, and it is built by the pure builder so
        // this test does not need the process-wide turn logger.
        let written = build_completion_record(&turn, &completion);
        assert_eq!(written.record_kind, TurnLog::RECORD_KIND_COMPLETION);
        assert_eq!(
            written.response_tool_calls,
            Some(0),
            "the non-call class must record 0 response-side calls"
        );
        assert_eq!(
            written.tool_call_count, 4,
            "the request-side field is unchanged and still overcounts"
        );
        // The serialized form must carry the key, so a reader sees 0 rather
        // than an absent field it has to guess about.
        let body = serde_json::to_string(&written).unwrap();
        assert!(body.contains("\"response_tool_calls\":0"), "{body}");

        // The first-byte record for the same turn keeps `None`: at first-byte
        // time the response has not happened, so a value there would be
        // fabricated.
        let first_byte = TurnLog::new(crate::turn_log::TurnRecord {
            requested_model: turn.requested_model.clone(),
            responding_model: turn.responding_model.clone(),
            latency_ms: 1,
            success: true,
            status_code: 200,
            request_message_count: turn.message_count,
            response_message_count: 1,
            has_tool_calls: turn.has_tool_calls,
            tool_call_count: turn.tool_call_count,
            key_label: None,
            is_racing: false,
            error: None,
            request_tokens: None,
            response_tokens: None,
            loop_key: None,
            request_has_tools: Some(true),
            request_max_tokens: None,
            response_tool_calls: None,
            // Not known on any path that has not observed the response body:
            // ttfb and failure records, and the non-streaming body id is filled
            // in where the body is assembled.
            response_body_id: None,
        });
        assert_eq!(
            first_byte.response_tool_calls, None,
            "the first-byte record must not fabricate a response-side count"
        );
        assert_eq!(
            first_byte.tool_call_count, 4,
            "the request-side field keeps its historical value on the first-byte record"
        );
        let ttfb_body = serde_json::to_string(&first_byte).unwrap();
        assert!(
            !ttfb_body.contains("response_tool_calls"),
            "an unobserved response must omit the key entirely, got {ttfb_body}"
        );
    }

    /// A completion whose response made N distinct tool calls records N on the
    /// per-request record — not the fragment count, which is larger by the
    /// number of SSE frames each call's `arguments` arrived in.
    #[test]
    fn test_completion_record_carries_the_distinct_count_not_the_fragment_count() {
        let turn = test_turn_context();
        let completion = StreamCompletion {
            // Three logical calls streamed over eleven fragment entries.
            tool_call_count: 11,
            tool_call_distinct: 3,
            ..test_completion(true)
        };

        let written = build_completion_record(&turn, &completion);
        assert_eq!(written.response_tool_calls, Some(3));
        let body = serde_json::to_string(&written).unwrap();
        assert!(
            body.contains("\"response_tool_calls\":3"),
            "the distinct count must be on the record, got {body}"
        );
    }

    /// A truncated stream increments `truncated_streams` and NOT
    /// `timeout_count`: the mid-stream cutoff (200, success=false) is a
    /// different mechanism from the no-bytes stall, and the numbers must say so.
    #[test]
    fn test_truncated_completion_counts_truncation_not_timeout() {
        let metrics = crate::GatewayMetrics::new();
        let tracker = crate::loop_tracker::LoopTracker::new(std::time::Duration::from_secs(300));
        log_turn_completion(
            &test_turn_context(),
            &test_completion(false),
            &tracker,
            &metrics,
        );
        let snap = metrics.snapshot();
        assert_eq!(snap.truncated_streams, 1);
        assert_eq!(snap.timeout_count, 0);
    }

    /// A clean stream increments neither counter: the truncation metric counts
    /// the truncated class specifically, not all completions.
    #[test]
    fn test_clean_completion_counts_neither_counter() {
        let metrics = crate::GatewayMetrics::new();
        let tracker = crate::loop_tracker::LoopTracker::new(std::time::Duration::from_secs(300));
        log_turn_completion(
            &test_turn_context(),
            &test_completion(true),
            &tracker,
            &metrics,
        );
        let snap = metrics.snapshot();
        assert_eq!(snap.truncated_streams, 0);
        assert_eq!(snap.timeout_count, 0);
    }

    /// The request-shape fields survive the `TurnRecord` -> `TurnLog`
    /// boundary: `Some` values are written, `None` values are omitted from
    /// the JSON (so old readers keep parsing), and JSON lacking both keys
    /// — as written before the fields existed — reads back as `None`.
    #[test]
    fn test_request_shape_fields_round_trip_through_turn_log() {
        use crate::turn_log::{TurnLog, TurnRecord};
        fn record(has_tools: Option<bool>, max_tokens: Option<i32>) -> TurnRecord {
            TurnRecord {
                requested_model: "m".to_string(),
                responding_model: "m".to_string(),
                latency_ms: 1,
                success: true,
                status_code: 200,
                request_message_count: 1,
                response_message_count: 1,
                has_tool_calls: false,
                tool_call_count: 0,
                key_label: None,
                is_racing: false,
                error: None,
                request_tokens: None,
                response_tokens: None,
                loop_key: None,
                request_has_tools: has_tools,
                request_max_tokens: max_tokens,
                response_tool_calls: None,
                // Not known on any path that has not observed the response body:
                // ttfb and failure records, and the non-streaming body id is filled
                // in where the body is assembled.
                response_body_id: None,
            }
        }
        let json = serde_json::to_string(&TurnLog::new(record(Some(true), Some(32000)))).unwrap();
        assert!(json.contains("\"request_has_tools\":true"), "{json}");
        assert!(json.contains("\"request_max_tokens\":32000"), "{json}");
        let json = serde_json::to_string(&TurnLog::new(record(None, None))).unwrap();
        assert!(!json.contains("request_has_tools"), "{json}");
        assert!(!json.contains("request_max_tokens"), "{json}");
        // A blob as written before the fields existed: no shape keys at all.
        // Built from a live record so the fixture carries every field a real
        // old blob had; only the two new keys are stripped.
        let old_value: serde_json::Value =
            serde_json::to_value(TurnLog::new(record(None, None))).unwrap();
        for key in ["request_has_tools", "request_max_tokens"] {
            assert!(
                old_value.get(key).is_none(),
                "None fields must serialize absent"
            );
        }
        let old_json = serde_json::to_string(&old_value).unwrap();
        let old: TurnLog = serde_json::from_str(&old_json).unwrap();
        assert_eq!(old.request_has_tools, None);
        assert_eq!(old.request_max_tokens, None);
    }
    /// The completion record carries the upstream's response-body id under the
    /// exact key the client join needs, and the paired first-byte record does
    /// not carry it at all.
    ///
    /// The join is `client.responseId == log.response_body_id`, so the key
    /// name is part of the contract, and the value must be the id verbatim.
    /// The ttfb half must OMIT the key, not write null: a first byte says
    /// nothing about which response body it belongs to, and on the
    /// non-streaming path the id does not exist until the body is assembled.
    #[test]
    fn test_completion_record_carries_response_body_id_and_ttfb_does_not() {
        use crate::turn_log::TurnLog;

        let turn = test_turn_context();
        let completion = StreamCompletion {
            response_body_id: Some("chatcmpl-8e3aa1b2c3".to_string()),
            ..test_completion(true)
        };

        let written = build_completion_record(&turn, &completion);
        assert_eq!(written.record_kind, TurnLog::RECORD_KIND_COMPLETION);
        assert_eq!(
            written.response_body_id.as_deref(),
            Some("chatcmpl-8e3aa1b2c3"),
            "the completion record must carry the upstream's response-body id"
        );

        // The serialized key is the join contract, so it is asserted on the
        // bytes rather than on the struct.
        let body = serde_json::to_string(&written).unwrap();
        assert!(
            body.contains("\"response_body_id\":\"chatcmpl-8e3aa1b2c3\""),
            "the field must serialize under the exact key response_body_id, got {body}"
        );
        // And it must round-trip, so the writer's own type reads back what it
        // wrote.
        let read_back: TurnLog = serde_json::from_str(&body).unwrap();
        assert_eq!(
            read_back.response_body_id.as_deref(),
            Some("chatcmpl-8e3aa1b2c3"),
            "the field must round-trip through the log's serde format"
        );

        // The first-byte record for the same turn omits the key entirely.
        let first_byte = TurnLog::new(crate::turn_log::TurnRecord {
            requested_model: turn.requested_model.clone(),
            responding_model: turn.responding_model.clone(),
            latency_ms: 1,
            success: true,
            status_code: 200,
            request_message_count: turn.message_count,
            response_message_count: 1,
            has_tool_calls: turn.has_tool_calls,
            tool_call_count: turn.tool_call_count,
            key_label: None,
            is_racing: false,
            error: None,
            request_tokens: None,
            response_tokens: None,
            loop_key: None,
            request_has_tools: None,
            request_max_tokens: None,
            response_tool_calls: None,
            response_body_id: None,
        });
        assert_eq!(
            first_byte.record_kind,
            TurnLog::RECORD_KIND_TTFB,
            "TurnLog::new builds the first-byte record"
        );
        assert_eq!(
            first_byte.response_body_id, None,
            "a first byte says nothing about which response body it belongs to"
        );
        let ttfb_body = serde_json::to_string(&first_byte).unwrap();
        assert!(
            !ttfb_body.contains("response_body_id"),
            "the ttfb record must omit the key rather than write null, got {ttfb_body}"
        );

        // A record written before the field existed stays valid: the key is
        // absent, and it deserializes to None rather than failing.
        let old_value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&written).unwrap()).unwrap();
        let mut old_map = old_value.as_object().unwrap().clone();
        old_map.remove("response_body_id");
        let old: TurnLog = serde_json::from_value(serde_json::Value::Object(old_map)).unwrap();
        assert_eq!(
            old.response_body_id, None,
            "a pre-existing record must read back as None, not fail"
        );
    }

    /// A completion whose upstream sent no top-level id records `None` rather
    /// than an empty string, so a reader can tell "unlabelled response" from
    /// "labelled with nothing".
    #[test]
    fn test_completion_record_without_a_body_id_stays_none() {
        let written = build_completion_record(&test_turn_context(), &test_completion(true));
        assert_eq!(written.response_body_id, None);
        let body = serde_json::to_string(&written).unwrap();
        assert!(
            !body.contains("response_body_id"),
            "None must serialize absent, got {body}"
        );
    }
}
