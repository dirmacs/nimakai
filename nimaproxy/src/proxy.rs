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
    // Check for racing mode
    let racing_models = state.racing_models.clone();
    if !racing_models.is_empty() && racing_models.len() >= 2 {
        return race_models(state, body, &racing_models).await;
    }

    // Standard single-model proxy
    let (model_id, body) = resolve_model(body, &state);

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
                state.model_stats.record(&model_id, t0.elapsed().as_millis() as f64, false);
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
                state.model_stats.record(&model_id, ttfc_ms, ok);

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
                let body = Body::from_stream(stream);

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
                let rewritten = Bytes::from(json.to_string());
                return (picked, rewritten);
            }
        }
    }

    let model_id = if requested.is_empty() {
        "unknown".to_string()
    } else {
        requested
    };
    (model_id, body)
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
        "status": if active > 0 { "ok" } else { "degraded" },
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
                "id": s.id,
                "avg_ms": s.avg_ms,
                "p95_ms": s.p95_ms,
                "total_requests": s.total,
                "success_requests": s.success,
                "success_rate_pct": s.success_rate,
                "sample_count": s.sample_count,
                "consecutive_failures": s.consecutive_failures,
                "degraded": s.degraded,
            })
        })
        .collect();

    let body = serde_json::json!({
        "models": models_json,
        "count": models_json.len(),
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
    let models_to_race: Vec<String> = models.iter().take(max_parallel).cloned().collect();

    if models_to_race.len() < 2 {
        return (StatusCode::BAD_REQUEST, "racing requires at least 2 models").into_response();
    }

    let mut handles = Vec::new();

    for model_id in &models_to_race {
        let mut json: Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => continue,
        };
        json["model"] = Value::String(model_id.clone());
        let req_body = match serde_json::to_vec(&json) {
            Ok(b) => Bytes::from(b),
            Err(_) => continue,
        };

        let target = state.target.clone();
        let client = state.client.clone();
        let state_clone = state.clone();
        let model_id_clone = model_id.clone();
        let timeout_val = timeout_ms;

        let handle = tokio::spawn(async move {
            let Some((key, key_idx)) = state_clone.pool.next_key() else {
                return Err(format!("no keys"));
            };
            let key_label = state_clone.pool.get_key_label(key_idx);

            let t0 = Instant::now();
            let result = timeout(
                std::time::Duration::from_millis(timeout_val),
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
                    state_clone.model_stats.record(&model_id_clone, latency, true);

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
                    state_clone.model_stats.record(&model_id_clone, timeout_val as f64, false);
                    Err(e.to_string())
                }
                Err(_) => {
                    state_clone.model_stats.record(&model_id_clone, timeout_val as f64, false);
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
