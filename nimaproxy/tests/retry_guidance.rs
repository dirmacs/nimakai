//! Same-prompt-as-fresh retry: the proxy-side half of `docs/RETRY-GUIDANCE.md`.
//!
//! The rule: where the proxy CAN retry, it re-issues the IDENTICAL payload on
//! another key — never a shrunk or reshaped one. Where it CANNOT (a streaming
//! response already on the wire), it must not send a second request at all.
//!
//! These tests pin both halves against a fake upstream that records every
//! request body it receives:
//!
//! * a stalled non-streaming request is retried with a byte-identical body on
//!   a different key, and the client sees the retry's success;
//! * a streaming stall sends exactly one upstream request (the no-double-send
//!   invariant: the first response is already committed, so a retry would
//!   double-send);
//! * when every attempt stalls, the client sees the original 504 — the retry
//!   is bounded and exhaustion falls through to the terminal error.
//!
//! A raw TCP listener stands in for the upstream rather than a mock, because
//! the assertions are about exact request bytes and exact request counts, and
//! a mock's chunked-body buffering hides the inter-chunk gaps the idle guard
//! reads (see `test_stalled_stream_still_times_out`).

use axum::http::HeaderMap;
use axum::response::IntoResponse;
use bytes::Bytes;
use nimaproxy::config::{KeyEntry, ModelCompat};
use nimaproxy::model_stats::ModelStatsStore;
use nimaproxy::proxy::chat::chat_completions;
use nimaproxy::{AppState, AppStateParams, RuntimeControls};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One upstream request, as the fake upstream saw it.
#[derive(Debug, Clone)]
struct ObservedRequest {
    authorization: String,
    body: Vec<u8>,
}

/// Read one HTTP/1.1 request (headers + exact Content-Length body) off `sock`.
async fn read_request(sock: &mut tokio::net::TcpStream) -> Option<ObservedRequest> {
    // Headers first, byte-at-a-time until the blank line: request bodies are
    // small here and correctness beats throughput in a test fake.
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if sock.read_exact(&mut byte).await.is_err() {
            return None;
        }
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
        if head.len() > 16384 {
            return None;
        }
    }
    let head_text = String::from_utf8_lossy(&head);
    let authorization = head_text
        .lines()
        .find_map(|line| line.strip_prefix("authorization: "))
        .or_else(|| {
            head_text
                .lines()
                .find_map(|line| line.strip_prefix("Authorization: "))
        })
        .unwrap_or("")
        .trim()
        .to_string();
    let content_length: usize = head_text
        .lines()
        .find_map(|line| {
            line.strip_prefix("content-length: ")
                .or_else(|| line.strip_prefix("Content-Length: "))
        })
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; content_length];
    if content_length > 0 && sock.read_exact(&mut body).await.is_err() {
        return None;
    }
    Some(ObservedRequest {
        authorization,
        body,
    })
}

/// Write one HTTP/1.1 200 with `content_type` and `body`, then close.
async fn write_response(sock: &mut tokio::net::TcpStream, content_type: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        content_type,
        body.len()
    );
    let _ = sock.write_all(head.as_bytes()).await;
    let _ = sock.write_all(body).await;
    let _ = sock.flush().await;
}

fn retry_test_state(target: String) -> Arc<AppState> {
    AppState::new_with_controls(
        AppStateParams {
            keys: vec![
                KeyEntry {
                    key: "retry-key-0".to_string(),
                    label: Some("retry-0".to_string()),
                    disabled: false,
                },
                KeyEntry {
                    key: "retry-key-1".to_string(),
                    label: Some("retry-1".to_string()),
                    disabled: false,
                },
            ],
            target,
            router: None,
            model_stats: ModelStatsStore::new(100.0),
            racing_models: vec![],
            racing_max_parallel: 1,
            racing_timeout_ms: 250,
            racing_strategy: "complete".to_string(),
            model_params: HashMap::new(),
            model_compat: ModelCompat::default(),
            loop_idle_timeout: nimaproxy::loop_idle_timeout(),
        },
        RuntimeControls {
            max_upstream_in_flight: 8,
            max_in_flight_per_key: 2,
            admission_wait_ms: 0,
            min_dynamic_timeout_ms: 250,
            // The stall these tests inject is 30 s of silence; pin the idle
            // window to the same 250 ms the first-byte rail uses here so the
            // injected silence still reads as a stall after the #21 split
            // (production default is 180 s, which would swallow it).
            mid_stream_idle_timeout_ms: 250,
            dynamic_sample_floor: 10,
            auth_failure_cooldown_secs: 900,
            ..RuntimeControls::default()
        },
    )
}

/// A stalled non-streaming request must be retried with the IDENTICAL body on
/// a DIFFERENT key, and the client must see the retry's success.
///
/// The first upstream request gets one chunk and then silence past the idle
/// window; the second gets a complete JSON body. The test then proves three
/// things the stats-based retry tests cannot: the retry carried the same
/// bytes (same-prompt-as-fresh, no payload shrink), it went out on the other
/// key's credential, and the client received the recovered body.
#[tokio::test]
async fn test_stalled_request_retries_with_identical_body_on_another_key() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let observed: Arc<Mutex<Vec<ObservedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let observed_task = Arc::clone(&observed);
    // Attempts are counted across connections; each connection is served on
    // its own task so a stalled first attempt never blocks the retry's
    // accept — otherwise the retry's send times out in the backlog and the
    // test measures the fake instead of the proxy.
    let attempts = Arc::new(Mutex::new(0usize));
    let attempts_task = Arc::clone(&attempts);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let observed_task = Arc::clone(&observed_task);
            let attempts_task = Arc::clone(&attempts_task);
            tokio::spawn(async move {
                let Some(req) = read_request(&mut sock).await else {
                    return;
                };
                observed_task.lock().unwrap().push(req);
                let attempt = {
                    let mut guard = attempts_task.lock().unwrap();
                    *guard += 1;
                    *guard
                };
                if attempt == 1 {
                    // One chunk, then silence far past the 250 ms window. The
                    // proxy's assembly must read this as a stall and retry.
                    let head = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";
                    let _ = sock.write_all(head.as_bytes()).await;
                    let chunk = br#"{"id":"partial","#;
                    let framed = format!("{:x}\r\n", chunk.len());
                    let _ = sock.write_all(framed.as_bytes()).await;
                    let _ = sock.write_all(chunk).await;
                    let _ = sock.write_all(b"\r\n").await;
                    let _ = sock.flush().await;
                    tokio::time::sleep(Duration::from_secs(30)).await;
                } else {
                    let body = br#"{"id":"recovered","choices":[{"message":{"content":"fresh attempt"}}]}"#;
                    write_response(&mut sock, "application/json", body).await;
                }
            });
        }
    });

    let state = retry_test_state(format!("http://{addr}"));
    let body = serde_json::json!({
        "model": "retry-model",
        "messages": [{"role": "user", "content": "recover me"}],
        "max_tokens": 32,
    });

    let resp = chat_completions(
        axum::extract::State(state.clone()),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    )
    .await
    .into_response();

    assert_eq!(
        resp.status(),
        axum::http::StatusCode::OK,
        "the retry must recover the request: the client sees the fresh attempt's success"
    );
    let response_body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(
        response_body
            .windows(b"fresh attempt".len())
            .any(|w| w == b"fresh attempt"),
        "the client must receive the retry's body, got: {}",
        String::from_utf8_lossy(&response_body)
    );

    let observed = observed.lock().unwrap();
    assert_eq!(
        observed.len(),
        2,
        "exactly two upstream requests: the stalled first attempt and its retry"
    );
    assert_eq!(
        observed[0].body, observed[1].body,
        "the retry must carry the identical payload — same prompt as fresh, never a reshaped one"
    );
    // The exclusion at admission is what forces the retry onto the other
    // key: without it, round-robin happens to advance here too, so this
    // assertion pins the behavior while the unit test pins the mechanism.
    assert_ne!(
        observed[0].authorization, observed[1].authorization,
        "the retry must go out on a different key than the stalled attempt"
    );
    assert!(
        observed[0].authorization.contains("retry-key-")
            && observed[1].authorization.contains("retry-key-"),
        "both attempts must carry a pool credential, got {:?} and {:?}",
        observed[0].authorization,
        observed[1].authorization
    );
}

/// A streaming stall must NOT retry: exactly one upstream request, no second
/// send.
///
/// The client already holds a 200 with the first chunk when the stall lands,
/// so a retry would double-send — the proxy would issue a second generation
/// the client never asked for while the first response is still on the wire.
/// The stall surfaces as a body error on the committed stream instead.
#[tokio::test]
async fn test_streaming_stall_does_not_retry() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let observed: Arc<Mutex<Vec<ObservedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let observed_task = Arc::clone(&observed);
    tokio::spawn(async move {
        // Per-connection tasks: a second accept (a wrongful retry) must be
        // observable even while the first connection sleeps through its
        // stall, or the count below would pass by never listening.
        while let Ok((mut sock, _)) = listener.accept().await {
            let observed_task = Arc::clone(&observed_task);
            tokio::spawn(async move {
                let Some(req) = read_request(&mut sock).await else {
                    return;
                };
                observed_task.lock().unwrap().push(req);
                // Commit a 200, send one SSE frame, then go quiet past the window.
                let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";
                let _ = sock.write_all(head.as_bytes()).await;
                let chunk = "data: {\"choices\":[{\"delta\":{\"content\":\"first\"}}]}\n\n";
                let framed = format!("{:x}\r\n{}\r\n", chunk.len(), chunk);
                let _ = sock.write_all(framed.as_bytes()).await;
                let _ = sock.flush().await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            });
        }
    });

    let state = retry_test_state(format!("http://{addr}"));
    let body = serde_json::json!({
        "model": "retry-model",
        "messages": [{"role": "user", "content": "stream me"}],
        "stream": true,
    });

    let resp = chat_completions(
        axum::extract::State(state.clone()),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    )
    .await
    .into_response();

    assert_eq!(
        resp.status(),
        axum::http::StatusCode::OK,
        "the committed stream keeps its 200; the stall arrives as a body error, not a status"
    );
    // Drain the body: the stall is only observed while the caller reads, and
    // the idle guard must end the stream promptly rather than holding it for
    // the upstream's 30 s silence. The drain is the guard's typed stall
    // error (`StreamStalled`'s message), which proves the abort came from
    // the idle guard and not from a connection break or a clean end.
    let drained = tokio::time::timeout(
        Duration::from_secs(5),
        axum::body::to_bytes(resp.into_body(), usize::MAX),
    )
    .await
    .expect("a stalled stream must end promptly");
    let err =
        drained.expect_err("draining a stalled stream must fail with the guard's stall error");
    assert!(
        err.to_string().contains("upstream stalled mid-stream"),
        "the drain error must be the idle guard's stall, got: {err}"
    );

    // Give any wrongful retry time to arrive before counting: without the
    // pause, the assertion could pass by racing the retry it is meant to
    // forbid.
    tokio::time::sleep(Duration::from_millis(600)).await;
    let observed = observed.lock().unwrap();
    assert_eq!(
        observed.len(),
        1,
        "a streaming stall must send exactly one upstream request — a retry would double-send"
    );
}

/// When every attempt stalls, the client sees the ORIGINAL 504: the retry is
/// bounded and exhaustion falls through to the terminal error rather than
/// hanging, inventing a success, or retrying forever.
#[tokio::test]
async fn test_retry_exhaustion_falls_through_to_original_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let observed: Arc<Mutex<Vec<ObservedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let observed_task = Arc::clone(&observed);
    tokio::spawn(async move {
        // Per-connection tasks, as above: the retry's accept must not wait
        // behind the first attempt's 30 s stall.
        while let Ok((mut sock, _)) = listener.accept().await {
            let observed_task = Arc::clone(&observed_task);
            tokio::spawn(async move {
                let Some(req) = read_request(&mut sock).await else {
                    return;
                };
                observed_task.lock().unwrap().push(req);
                // Every attempt stalls the same way: one chunk, then silence.
                let head = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";
                let _ = sock.write_all(head.as_bytes()).await;
                let chunk = br#"{"id":"partial","#;
                let framed = format!("{:x}\r\n", chunk.len());
                let _ = sock.write_all(framed.as_bytes()).await;
                let _ = sock.write_all(chunk).await;
                let _ = sock.write_all(b"\r\n").await;
                let _ = sock.flush().await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            });
        }
    });

    let state = retry_test_state(format!("http://{addr}"));
    let body = serde_json::json!({
        "model": "retry-model",
        "messages": [{"role": "user", "content": "stall forever"}],
    });

    let started = std::time::Instant::now();
    let resp = chat_completions(
        axum::extract::State(state.clone()),
        HeaderMap::new(),
        Bytes::from(body.to_string()),
    )
    .await
    .into_response();
    let elapsed = started.elapsed();

    assert_eq!(
        resp.status(),
        axum::http::StatusCode::GATEWAY_TIMEOUT,
        "exhaustion must fall through to the original 504"
    );
    let text = String::from_utf8_lossy(
        &axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .into_owned();
    assert!(
        text.contains("stalled"),
        "the terminal error must name the stall, got: {text}"
    );

    let observed = observed.lock().unwrap();
    assert!(
        observed.len() >= 2,
        "a stalled first attempt must have been retried, saw {} upstream requests",
        observed.len()
    );
    assert!(
        observed.len() <= 2,
        "the retry must be bounded by the budget, saw {} upstream requests",
        observed.len()
    );
    // The boundedness in wall-clock terms: two 250 ms windows plus slack,
    // not one window per key and not a hang.
    assert!(
        elapsed < Duration::from_millis(2500),
        "exhaustion must arrive within the budget, took {elapsed:?}"
    );
    // Every attempt carried the same prompt: bounded does not mean degraded.
    for req in observed.iter().skip(1) {
        assert_eq!(
            req.body, observed[0].body,
            "every retry attempt must carry the identical payload"
        );
    }
}
