use crate::loop_tracker::SESSION_HEADER;
use crate::proxy::admission::acquire_gateway_permits;
use crate::proxy::chat_helpers::{accept_header_for_body, non_streaming_usage};
use crate::proxy::http::log_turn_request;
use crate::proxy::messages::{
    mistral_validation_error, normalize_requested_model, resolve_model, validate_model_exists,
};
use crate::proxy::racing::race_models;
use crate::proxy::retry::RetryPolicy;
use crate::proxy::timeouts::{
    conversation_identity, record_model_timeout, request_turn_summary, sanitize_session,
    timeout_for_model,
};
use crate::turn_log::{LoopKey, TurnRecord};
use crate::AppState;
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use chrono::Utc;
use futures::{StreamExt, TryStreamExt};
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::timeout;
use tracing::warn;
pub(crate) const MAX_RETRIES: usize = 8;

/// POST /v1/chat/completions
///
/// V1: injects key, retries on 429, streams SSE byte-for-byte.
/// V2: resolves `"model": "auto"` via router, records TTFC to model_stats.
/// V3: racing — fires N parallel requests to N models, returns first response.
pub async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
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
    let routing_model = normalize_requested_model(&original_model).to_string();
    // Racing owns per-model body rewriting. Passing the original auto request
    // avoids leaking router-picked model params into race candidates that do
    // not define the same fields.
    if routing_model == "auto" && !state.racing_models.is_empty() && state.racing_models.len() >= 2
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
    let (message_count, has_tool_calls, tool_call_count) = request_turn_summary(&body);
    // Session/loop tracking. The session id is client-supplied (x-nima-session)
    // or derived from the conversation's first user message; if neither is
    // available this request is not part of a tracked loop.
    let (last_role, fingerprint) = conversation_identity(&body);
    let force_new_turn = headers
        .get("x-nima-new-turn")
        .and_then(|v| v.to_str().ok())
        .map(|v| v != "0" && v != "false")
        .unwrap_or(false);
    // Resolve the loop this request belongs to. The session id is only ever
    // carried inside the key, so this yields just the key. A request with
    // neither the header nor a derivable fingerprint is not tracked at all,
    // rather than being given an invented session.
    let loop_key: Option<LoopKey> = match headers
        .get(SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(sanitize_session)
    {
        Some(session_id) => {
            let position = state.loop_tracker.begin(
                &session_id,
                last_role.as_deref(),
                message_count,
                force_new_turn,
                Utc::now(),
            );
            if let Some(closed) = position.closed {
                crate::proxy::http::log_loop_summary(&closed);
            }
            Some(LoopKey {
                session_id,
                loop_seq: position.loop_seq,
                round_index: position.round_index,
            })
        }
        None => match fingerprint {
            Some(fingerprint) => {
                let position = state.loop_tracker.begin(
                    &fingerprint,
                    last_role.as_deref(),
                    message_count,
                    force_new_turn,
                    Utc::now(),
                );
                if let Some(closed) = position.closed {
                    crate::proxy::http::log_loop_summary(&closed);
                }
                Some(LoopKey {
                    session_id: fingerprint,
                    loop_seq: position.loop_seq,
                    round_index: position.round_index,
                })
            }
            None => None,
        },
    };
    let n = state.pool.len().clamp(1, MAX_RETRIES);
    let mut last_auth_failure_response: Option<Response> = None;
    // Total wall-clock budget for this request's upstream attempts.
    //
    // Sized to admit exactly one retry: one window for the first attempt, one
    // for the retry, plus the admission wait that elapses before the first
    // permit, plus scheduling slack.
    //
    // Since the split, "the window" is two windows: the first-byte window
    // (`request_timeout_ms`) bounds the send and the wait for the first byte,
    // while the idle window (`mid_stream_idle_timeout_ms`) bounds each gap
    // between chunks once the body has started. The retry policy therefore
    // carries two budgets. A send timeout or a no-bytes stall consumed at
    // most one first-byte window, so its retry is admitted under the fast
    // budget (`2 * first_byte + admission + slack`). A mid-body stall
    // consumed a first-byte window AND ran into the idle rail, so its retry
    // is admitted under the body-stall budget, which is the fast budget
    // plus one idle window. Either way the retry is still a single extra
    // attempt — the budget admits exactly one window pair, never a chain.
    //
    // The admission allowance is load-bearing and was measured, not assumed:
    // `elapsed` is measured from the start of the REQUEST, which includes
    // waiting for a permit (up to `admission_wait_ms`, 5000 ms in production).
    // Observed on the live host, a single attempt measured 16546 ms against a
    // 15000 ms window, so a bare 2x budget (30500 ms) rejected the retry
    // because 16546 + 15000 > 30500.
    let retry_policy = RetryPolicy::new_split(
        n,
        timeout_for_model(&state, &model_id),
        state.mid_stream_idle_timeout_ms,
        state.admission_wait_ms,
    );
    let started = Instant::now();
    for attempt in 0..n {
        let (upstream_permit, key_lease) = match acquire_gateway_permits(&state).await {
            Ok(permits) => permits,
            Err(response) => return response,
        };
        let mut request_state = crate::proxy::request_state::RequestState::Admitted;
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
                record_model_timeout(&state, &model_id, key_label.as_deref(), request_timeout_ms);
                // Retry once on a different key while budget and attempts
                // remain. A timeout here is the request being slow on one
                // key, not the request being wrong; measured on this host,
                // 504s hit every key roughly evenly, so a different
                // key is plausibly able to finish the same prompt. Before
                // this retried, ~32% of admitted requests returned a 504
                // that no later attempt could recover.
                //
                // Binding conditions before retrying:
                //   - attempts remain, so the loop stays bounded
                //   - one more attempt fits inside the total budget, so
                //     the client is never asked to wait materially longer
                //     than a single timeout window
                //   - no response bytes were sent to the client yet; this
                //     branch runs before the body becomes a stream, so a
                //     retried request cannot double-send to the caller
                // The final attempt (or an exhausted budget) falls through
                // to the original 504, so failure paths are unchanged.
                let may_retry = request_state.on_failure(false).can_retry(
                    attempt,
                    n,
                    started,
                    std::time::Duration::from_millis(request_timeout_ms),
                    retry_policy.budget(),
                );
                if may_retry {
                    state.pool.note_upstream_stall(idx);
                    log_turn_request(TurnRecord {
                        requested_model: original_model.to_string(),
                        responding_model: model_id.to_string(),
                        latency_ms: t0.elapsed().as_millis() as u64,
                        success: false,
                        status_code: StatusCode::GATEWAY_TIMEOUT.as_u16(),
                        request_message_count: message_count,
                        loop_key: None,
                        response_message_count: 1,
                        has_tool_calls,
                        tool_call_count,
                        key_label: key_label.as_deref().map(str::to_string),
                        is_racing: false,
                        error: Some(format!(
                            "upstream timeout after {}ms; retrying on another key",
                            request_timeout_ms
                        )),
                        request_tokens: None,
                        response_tokens: None,
                    });
                    continue;
                }
                log_turn_request(TurnRecord {
                    requested_model: original_model.to_string(),
                    responding_model: model_id.to_string(),
                    latency_ms: request_timeout_ms as u128 as u64,
                    success: false,
                    status_code: StatusCode::GATEWAY_TIMEOUT.as_u16(),
                    request_message_count: message_count,
                    loop_key: None,
                    response_message_count: 1,
                    has_tool_calls,
                    tool_call_count,
                    key_label: key_label.as_deref().map(str::to_string),
                    is_racing: false,
                    error: Some(format!("upstream timeout after {}ms", request_timeout_ms)),
                    request_tokens: None,
                    response_tokens: None,
                });
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
                log_turn_request(TurnRecord {
                    requested_model: original_model.to_string(),
                    responding_model: model_id.to_string(),
                    latency_ms: t0.elapsed().as_millis() as u64,
                    success: false,
                    status_code: StatusCode::BAD_GATEWAY.as_u16(),
                    request_message_count: message_count,
                    loop_key: None,
                    response_message_count: 1,
                    has_tool_calls,
                    tool_call_count,
                    key_label: key_label.as_deref().map(str::to_string),
                    is_racing: false,
                    error: Some(e.to_string()),
                    request_tokens: None,
                    response_tokens: None,
                });
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
                // Auth failure: this key itself is bad (revoked/invalid), not the request.
                // Quarantine the key and retry with the next available key. Checked BEFORE
                // the body is converted to a stream so a streaming response never starts
                // sending bytes to the client on a key we're about to discard.
                if status == 401 || status == 403 {
                    state.pool.mark_auth_failed(
                        idx,
                        Duration::from_secs(state.auth_failure_cooldown_secs),
                    );
                    warn!(
                        key_idx = idx,
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
                    // A quarantined key IS a turn outcome: without this record the
                    // auth-failure path was the only failure mode with no line in
                    // turns.jsonl, so "check the logs" could not see it at all.
                    log_turn_request(TurnRecord {
                        requested_model: original_model.to_string(),
                        responding_model: model_id.to_string(),
                        latency_ms: t0.elapsed().as_millis() as u64,
                        success: false,
                        status_code: status.as_u16(),
                        request_message_count: message_count,
                        loop_key: None,
                        response_message_count: 1,
                        has_tool_calls,
                        tool_call_count,
                        key_label: key_label.as_deref().map(str::to_string),
                        is_racing: false,
                        error: Some(format!(
                            "upstream auth failure ({}); key quarantined",
                            status.as_u16()
                        )),
                        request_tokens: None,
                        response_tokens: None,
                    });
                    continue;
                }
                // Non-2xx bodies must be inspected BEFORE the response is
                // committed to the wire, because the DEGRADED and
                // invalid-assistant paths retry on the next key and a stream
                // that has already started cannot be retried.
                if status == 400 {
                    let upstream_content_type = resp
                        .headers()
                        .get("content-type")
                        .cloned()
                        .unwrap_or_else(|| HeaderValue::from_static("application/json"));
                    // Read the error body: a 400 is an error response, so it is
                    // short and bounded, unlike a generation body. This
                    // consumes `resp`, so it must happen before any streaming.
                    let err_body = match timeout(
                        Duration::from_millis(request_timeout_ms),
                        resp.bytes_stream().try_collect::<Vec<Bytes>>(),
                    )
                    .await
                    {
                        Ok(Ok(parts)) => Bytes::from(parts.concat()),
                        _ => Bytes::new(),
                    };
                    let body_str = std::str::from_utf8(&err_body).unwrap_or("");
                    let error_excerpt = body_str.chars().take(400).collect::<String>();
                    if body_str.contains("DEGRADED") || body_str.contains("degraded") {
                        eprintln!("[nimaproxy] SERVER-DEGRADED: model '{}' returned DEGRADED error from NVIDIA (server-side block)", model_id);
                        // Record as server-side degraded - this immediately marks the model as unavailable
                        state.model_stats.record_server_degraded(&model_id);
                        if routing_model == "auto" {
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
                    if body_str.contains("Invalid assistant message")
                        || body_str.contains("invalid assistant")
                    {
                        eprintln!("[nimaproxy] INVALID-ASSISTANT: model '{}' rejected message structure (400): {} — retrying with next key", model_id, &body_str[..body_str.len().min(200)]);
                        state.model_stats.record_hard_error(&model_id, body_str);
                        log_turn_request(TurnRecord {
                            requested_model: original_model.to_string(),
                            responding_model: model_id.to_string(),
                            latency_ms: t0.elapsed().as_millis() as u64,
                            success: false,
                            status_code: status.as_u16(),
                            request_message_count: message_count,
                            loop_key: None,
                            response_message_count: 1,
                            has_tool_calls,
                            tool_call_count,
                            key_label: key_label.as_deref().map(str::to_string),
                            is_racing: false,
                            error: Some(error_excerpt),
                            request_tokens: None,
                            response_tokens: None,
                        });
                        continue;
                    }
                    // An unrecognised 400 is forwarded verbatim so the caller
                    // sees exactly what the upstream said.
                    let mut response = Response::new(if err_body.is_empty() {
                        Body::empty()
                    } else {
                        Body::from(err_body)
                    });
                    *response.status_mut() = StatusCode::BAD_REQUEST;
                    response
                        .headers_mut()
                        .insert("content-type", upstream_content_type);
                    return response;
                }

                let ok = status.is_success();
                // Forward the response.
                //
                // Streaming is forwarded BYTE-FOR-BYTE as it arrives. It used
                // to be buffered to completion first (`try_collect`), which
                // silently turned a streaming response into a batch one: the
                // client got nothing until the LAST token, so a generation that
                // streamed its first token in 0.33 s and finished in 8 s was
                // instead held for the whole duration and then judged against
                // a 15 s window. Measured on the live gateway, 96% of failures
                // took this path, and the same request direct to the upstream
                // needed 80 s+ to produce a complete non-streaming body where
                // the stream had delivered 262 chunks in 7.9 s.
                //
                // The contract that must survive the change: a stall is still
                // detected. Two separate bounds do that now, and they are two
                // separate KNOBS, because they guard different events:
                //   1. the first byte must arrive within the first-byte window
                //      (`request_timeout_ms`: the "is the upstream alive"
                //      signal, and what the retry and the 504 are keyed on), and
                //   2. no single inter-chunk gap may exceed the idle window
                //      (`mid_stream_idle_timeout_ms`), which catches a
                //      mid-stream stall.
                // A response that merely takes a long time to finish is
                // therefore allowed, which is the whole point. And a live
                // generation that pauses to think between tokens is allowed a
                // longer pause than a silent upstream is allowed before its
                // first byte — that is why the idle window defaults wider
                // (60 s vs 25 s).
                let resp_status = axum::http::StatusCode::from_u16(status.as_u16())
                    .unwrap_or(StatusCode::BAD_GATEWAY);
                let content_type = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("application/json")
                    .to_string();
                let is_stream = content_type.contains("event-stream");

                let upstream = resp
                    .bytes_stream()
                    .map_err(|e| std::io::Error::other(e.to_string()));
                let window = Duration::from_millis(request_timeout_ms);
                let idle_window = Duration::from_millis(state.mid_stream_idle_timeout_ms);

                request_state = crate::proxy::request_state::RequestState::InFlight;
                // Bound the wait for the FIRST byte. This is the only part that
                // can still produce a 504, and it is the only part that may
                // trigger the retry, because nothing has been sent to the
                // client yet.
                let mut upstream = upstream;
                let first = match timeout(window, upstream.next()).await {
                    Ok(Some(Ok(chunk))) => Some(chunk),
                    Ok(Some(Err(e))) => {
                        return (StatusCode::BAD_GATEWAY, e.to_string()).into_response();
                    }
                    // A stream that ends without a single byte is a stall, not
                    // an empty success.
                    Ok(None) | Err(_) => None,
                };

                let Some(first) = first else {
                    state.gateway_metrics.record_timeout();
                    record_model_timeout(
                        &state,
                        &model_id,
                        key_label.as_deref(),
                        request_timeout_ms,
                    );
                    state.pool.note_upstream_stall(idx);

                    let may_retry = request_state.on_failure(false).can_retry(
                        attempt,
                        n,
                        started,
                        std::time::Duration::from_millis(request_timeout_ms),
                        retry_policy.budget(),
                    );
                    if may_retry {
                        log_turn_request(TurnRecord {
                            requested_model: original_model.to_string(),
                            responding_model: model_id.to_string(),
                            latency_ms: t0.elapsed().as_millis() as u64,
                            success: false,
                            status_code: StatusCode::GATEWAY_TIMEOUT.as_u16(),
                            request_message_count: message_count,
                            loop_key: None,
                            response_message_count: 1,
                            has_tool_calls,
                            tool_call_count,
                            key_label: key_label.as_deref().map(str::to_string),
                            is_racing: false,
                            error: Some(format!(
                                "upstream sent no bytes within {}ms; retrying on another key",
                                request_timeout_ms
                            )),
                            request_tokens: None,
                            response_tokens: None,
                        });
                        continue;
                    }
                    log_turn_request(TurnRecord {
                        requested_model: original_model.to_string(),
                        responding_model: model_id.to_string(),
                        latency_ms: t0.elapsed().as_millis() as u64,
                        success: false,
                        status_code: StatusCode::GATEWAY_TIMEOUT.as_u16(),
                        request_message_count: message_count,
                        loop_key: None,
                        response_message_count: 1,
                        has_tool_calls,
                        tool_call_count,
                        key_label: key_label.as_deref().map(str::to_string),
                        is_racing: false,
                        error: Some(format!(
                            "upstream sent no bytes within {}ms",
                            request_timeout_ms
                        )),
                        request_tokens: None,
                        response_tokens: None,
                    });
                    return (
                        StatusCode::GATEWAY_TIMEOUT,
                        format!("upstream sent no bytes within {}ms", request_timeout_ms),
                    )
                        .into_response();
                };

                // Headers and the first chunk are already in hand. Which state
                // that puts the request in depends on the response mode, because
                // only a stream actually hands those bytes to the client: a
                // non-streaming response is assembled below and the caller sees
                // nothing until it is complete, so a later failure there is
                // still retryable. The state machine asserts the invariant that
                // only Admitted/InFlight/Assembling may receive a first byte.
                request_state = request_state.on_first_byte(if is_stream {
                    crate::proxy::request_state::ResponseMode::Streaming
                } else {
                    crate::proxy::request_state::ResponseMode::Buffered
                });
                let ttfc_ms = t0.elapsed().as_millis() as f64;
                if ok {
                    state.pool.record_success(idx);
                }
                if let Some(label) = key_label.as_ref() {
                    state
                        .model_stats
                        .record_with_key(&model_id, label, ttfc_ms, ok);
                } else {
                    state.model_stats.record(&model_id, ttfc_ms, ok);
                }

                // Both branches forward the upstream stream as-is, with the
                // first chunk prepended (it was consumed while bounding the
                // time-to-first-byte).
                //
                // The two response modes differ in what they can still do once
                // the first byte is in hand, and that difference is the whole
                // point of this shape:
                //
                //   - streaming (SSE): the response is already on the wire, so
                //     it is forwarded chunk-by-chunk with only an inter-chunk
                //     idle guard. A mid-stream stall aborts the stream; it can
                //     no longer be retried, and there is no status code left to
                //     correct because the client already has a 200.
                //   - non-streaming: nothing has been sent yet, so the body is
                //     ASSEMBLED here and a mid-stream stall is still retryable.
                //     The assembly is bounded by the same idle guard rather
                //     than a whole-body deadline, which is what let a long
                //     generation be killed before it finished.
                let mapped = upstream.map(|chunk| match chunk {
                    Ok(bytes) => Ok(bytes),
                    Err(e) => Err(std::io::Error::other(e.to_string())),
                });
                let with_first = futures::stream::once(async move { Ok(first) }).chain(mapped);

                // Structural observation of the forwarded stream.
                //
                // The tap sits INSIDE the idle guard, so it sees the chunks
                // that were actually forwarded to the client — and it also
                // sees the guard's abort error, which is how a stalled stream
                // gets a completion record instead of vanishing. The first
                // chunk was already consumed by the TTFB bound, so it is
                // prepended back and observed like any other chunk.
                let tap = crate::proxy::sse_tap::SseTap::new();
                let tapped = crate::proxy::sse_tap::TappedStream::new(
                    tap.clone(),
                    crate::proxy::streaming::with_idle_timeout(with_first, idle_window),
                );

                // Usage tokens for this turn. Streaming paths leave them to the
                // tap, which reads the upstream's usage frame; the
                // non-streaming path parses them out of the assembled body
                // below. `None` until one of the two fills them in.
                let mut usage_tokens: (Option<u32>, Option<u32>) = (None, None);
                let body = if is_stream {
                    Body::from_stream(tapped)
                } else {
                    // Assemble the body. A stall here is retryable for this
                    // branch and only this branch: nothing has been written to
                    // the caller, so a retry cannot double-send. The streaming
                    // branch above is Committed once its first byte is seen, so
                    // its mid-stream stalls stay terminal.
                    //
                    // Either way the failure must be classified as a stall
                    // rather than an I/O error, because that is what produces
                    // the 504 and the stall metrics the retry and the circuit
                    // breaker read. Reading a stall as a plain upstream error
                    // is what used to return a 502 here and silently drop both.
                    let mut collected = Vec::new();
                    let mut stalled = false;
                    futures::pin_mut!(tapped);
                    loop {
                        // The idle window, not the first-byte window: the body
                        // has started, so this is the mid-stream rail now. The
                        // guard inside `tapped` fires first (same window), so
                        // in practice the stall arrives as a typed payload on
                        // the `Ok` arm below; this outer bound is the backstop
                        // for a guard that somehow never yields.
                        match timeout(idle_window, tapped.next()).await {
                            Ok(Some(Ok(chunk))) => collected.push(chunk),
                            // The idle guard reports a stall as an error on the
                            // stream, so a silent upstream arrives here and not
                            // on the `Err(_)` arm below. Classify it by its typed
                            // payload rather than by the message: a stall is
                            // retryable (nothing has reached the client yet),
                            // while a genuine upstream I/O failure is not.
                            // Reading both as a 502 is what silently disabled
                            // the mid-body retry and the 504.
                            Ok(Some(Err(e))) if crate::proxy::streaming::is_stall(&e) => {
                                stalled = true;
                                break;
                            }
                            Ok(Some(Err(e))) => {
                                return (StatusCode::BAD_GATEWAY, e.to_string()).into_response();
                            }
                            Ok(None) => break,
                            Err(_) => {
                                stalled = true;
                                break;
                            }
                        }
                    }

                    if stalled {
                        state.gateway_metrics.record_timeout();
                        record_model_timeout(
                            &state,
                            &model_id,
                            key_label.as_deref(),
                            request_timeout_ms,
                        );
                        state.pool.note_upstream_stall(idx);

                        // The body-stall budget: this attempt consumed a
                        // first-byte window AND ran into the idle rail, so
                        // its retry is admitted under the wider ceiling.
                        let may_retry = request_state.on_failure(false).can_retry(
                            attempt,
                            n,
                            started,
                            std::time::Duration::from_millis(request_timeout_ms),
                            retry_policy.body_stall_budget(),
                        );
                        if may_retry {
                            log_turn_request(TurnRecord {
                                requested_model: original_model.to_string(),
                                responding_model: model_id.to_string(),
                                latency_ms: t0.elapsed().as_millis() as u64,
                                success: false,
                                status_code: StatusCode::GATEWAY_TIMEOUT.as_u16(),
                                request_message_count: message_count,
                                loop_key: None,
                                response_message_count: 1,
                                has_tool_calls,
                                tool_call_count,
                                key_label: key_label.as_deref().map(str::to_string),
                                is_racing: false,
                                error: Some(format!(
                                    "upstream body stalled after {}ms; retrying on another key",
                                    state.mid_stream_idle_timeout_ms
                                )),
                                request_tokens: None,
                                response_tokens: None,
                            });
                            continue;
                        }
                        log_turn_request(TurnRecord {
                            requested_model: original_model.to_string(),
                            responding_model: model_id.to_string(),
                            latency_ms: t0.elapsed().as_millis() as u64,
                            success: false,
                            status_code: StatusCode::GATEWAY_TIMEOUT.as_u16(),
                            request_message_count: message_count,
                            loop_key: None,
                            response_message_count: 1,
                            has_tool_calls,
                            tool_call_count,
                            key_label: key_label.as_deref().map(str::to_string),
                            is_racing: false,
                            error: Some(format!(
                                "upstream body stalled after {}ms",
                                state.mid_stream_idle_timeout_ms
                            )),
                            request_tokens: None,
                            response_tokens: None,
                        });
                        return (
                            StatusCode::GATEWAY_TIMEOUT,
                            format!(
                                "upstream body stalled after {}ms",
                                state.mid_stream_idle_timeout_ms
                            ),
                        )
                            .into_response();
                    }

                    // Usage accounting lives in the assembled body, so it is
                    // read from the bytes the client is about to receive rather
                    // than from the stream. Absent means `None`, never 0.
                    usage_tokens = non_streaming_usage(&collected.concat());

                    Body::from(collected.concat())
                };

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
                if is_stream {
                    response
                        .headers_mut()
                        .insert("cache-control", HeaderValue::from_static("no-cache"));
                    response
                        .headers_mut()
                        .insert("x-accel-buffering", HeaderValue::from_static("no"));
                }
                // The first-byte record is written BEFORE the stream is
                // handed to axum, so its generated `request_id` is known and
                // can be threaded into the tap for the completion record.
                // Reading the id back out of the global logger is the only way
                // to get it: `TurnLog::new` generates it internally.
                //
                // The read MUST come after the write: `last_request_id` is
                // only updated inside `log`, once the record is on disk.
                // Reading it first returns the previous request's id (or
                // `None` on a fresh logger), which would silently join this
                // turn's completion record to the wrong turn — or to nothing.
                log_turn_request(TurnRecord {
                    requested_model: original_model.to_string(),
                    responding_model: model_id.to_string(),
                    latency_ms: ttfc_ms as u128 as u64,
                    success: ok,
                    status_code: status.as_u16(),
                    request_message_count: message_count,
                    // Cloned: this record is built inside the attempt loop, so
                    // it must not consume the key that later records also need.
                    loop_key: loop_key.clone(),
                    response_message_count: 1,
                    has_tool_calls,
                    tool_call_count,
                    key_label: key_label.as_deref().map(str::to_string),
                    is_racing: false,
                    error: None,
                    request_tokens: usage_tokens.0,
                    response_tokens: usage_tokens.1,
                });
                let request_id =
                    crate::turn_log::with_logger(|logger| logger.last_request_id()).flatten();
                let tap_for_completion = tap.clone();

                // Echo the generated request id back to the client.
                //
                // This is the join key the turn log is keyed on: the client
                // cannot correlate its own turn with the JSONL records without
                // it, and guessing by file position is not possible for anyone
                // outside this process. It is also what makes the id available
                // to the tests, which read it from the response instead of
                // racing other tests' writes to the shared log.
                response.headers_mut().insert(
                    "x-request-id",
                    HeaderValue::from_str(request_id.as_deref().unwrap_or("unknown"))
                        .unwrap_or_else(|_| HeaderValue::from_static("unknown")),
                );

                if is_stream {
                    // The stream is owned by axum once it is returned, so the
                    // completion record cannot be written here. Instead the tap
                    // is watched by a detached task: the record is written when
                    // the stream ends, including on the idle guard's abort,
                    // because that is the only point at which the structural
                    // outcome is known.
                    //
                    // The task holds a clone of the tap, whose counters are
                    // shared with the one inside the body, so it reads the same
                    // values axum's polls produced.
                    //
                    // The wait is bounded rather than open-ended. A client that
                    // goes away mid-stream can leave the body unpolled forever,
                    // and an unbounded wait would leak a task per such request.
                    // The bound scales with the IDLE window — the widest gap a
                    // healthy stream may legitimately show — plus a minute, so
                    // a slow-but-healthy generation is never cut off; if it
                    // expires, the stream is recorded as truncated, which is
                    // what a client that stopped reading actually produced.
                    let wait_bound = idle_window * 20 + Duration::from_secs(60);
                    let started_stream = Instant::now();
                    let turn_ctx = crate::proxy::http::TurnContext {
                        requested_model: original_model.clone(),
                        responding_model: model_id.clone(),
                        message_count,
                        has_tool_calls,
                        tool_call_count,
                        key_label: key_label.clone(),
                        is_racing: false,
                        loop_key: loop_key.clone(),
                    };
                    tokio::spawn(async move {
                        let deadline = Instant::now() + wait_bound;
                        while !tap_for_completion.is_finalized() {
                            if Instant::now() >= deadline {
                                // The body was never driven to the end. Record
                                // what was forwarded, as a truncation.
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(25)).await;
                        }
                        let outcome = tap_for_completion.outcome();
                        let abort_reason = if outcome.stream_ended_cleanly {
                            None
                        } else {
                            // A stream that ended without [DONE] either
                            // truncated or was aborted mid-stream; both are the
                            // same structural fact.
                            Some("stream ended without data: [DONE]".to_string())
                        };
                        crate::proxy::http::log_turn_completion(
                            &turn_ctx,
                            &crate::proxy::http::StreamCompletion {
                                request_id,
                                duration_ms: started_stream.elapsed().as_millis(),
                                finish_reason: outcome.finish_reason,
                                content_chars: outcome.content_chars,
                                stream_ended_cleanly: outcome.stream_ended_cleanly,
                                chunk_count: outcome.chunk_count,
                                body_bytes: outcome.body_bytes,
                                prompt_tokens: outcome.prompt_tokens,
                                completion_tokens: outcome.completion_tokens,
                                tool_call_count: outcome.tool_call_count as u64,
                                tool_call_distinct: outcome.tool_call_distinct as u64,
                                abort_reason,
                                max_gap_ms: outcome.max_gap_ms,
                                ended_by: outcome.ended_by.as_str().to_string(),
                            },
                            &state.loop_tracker,
                            &state.gateway_metrics,
                        );
                    });
                }

                return response;
            }
        }
    }
    // Every key was tried and none produced a usable response. If any of them
    // was an auth failure, forward that upstream response verbatim rather than
    // a generic 429: a revoked key is a different operator action from a busy
    // gateway, and the upstream's own status and body are what diagnose it.
    if let Some(response) = last_auth_failure_response {
        return response;
    }
    (
        StatusCode::TOO_MANY_REQUESTS,
        "all keys exhausted after retries",
    )
        .into_response()
}
