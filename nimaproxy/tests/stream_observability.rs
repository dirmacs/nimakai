//! Integration tests for structural SSE turn observability.
//!
//! These drive the real proxy handler against a `mockito` upstream and then
//! read back the turn log the proxy actually wrote. The point is to prove the
//! completion record is produced from what the stream DID — `[DONE]` or not,
//! `finish_reason` or not — and never from what the stream said.
//!
//! The upstreams here emit the degenerate shapes that caused the original bug:
//! a couple of mid-word fragments and then the stream just ends.

use axum::body::to_bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use mockito::Server;
use nimaproxy::config::{KeyEntry, ModelCompat};
use nimaproxy::model_stats::ModelStatsStore;
use nimaproxy::proxy::chat::chat_completions;
use nimaproxy::turn_log::TurnLog;
use nimaproxy::AppState;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const MODEL: &str = "test/model";

/// A mock upstream serving one canned SSE body.
///
/// Returns the server alongside the URL: the mock only listens while the
/// `Server` is alive, so the caller must hold it for the duration of the test.
/// Dropping it here would leave the proxy connecting to a dead port, which
/// surfaces as a 504 and a body with no first byte.
async fn mock_upstream(sse: &str) -> (mockito::ServerGuard, String) {
    let mut server = Server::new_async().await;
    server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(sse)
        .expect_at_least(1)
        .create();
    let url = server.url();
    (server, url)
}

fn make_state(api_url: String) -> Arc<AppState> {
    let keys = vec![KeyEntry {
        key: "test-key".to_string(),
        label: Some("test".to_string()),
        disabled: false,
    }];
    AppState::new(nimaproxy::AppStateParams {
        keys,
        target: api_url,
        router: None,
        model_stats: ModelStatsStore::new(100.0),
        racing_models: vec![],
        racing_max_parallel: 3,
        racing_timeout_ms: 20000,
        racing_strategy: "complete".to_string(),
        model_params: HashMap::new(),
        model_compat: ModelCompat::default(),
        loop_idle_timeout: nimaproxy::loop_idle_timeout(),
    })
}

/// A turn-log file the proxy can be pointed at.
///
/// The logger is a process-global, so it can only be initialized once per test
/// binary. Every test in this file therefore shares one log file and selects
/// records by request id rather than by file: a request id is unique per
/// record, so concurrent tests do not interfere even though they append to the
/// same file.
struct LogFile {
    path: std::path::PathBuf,
}

impl LogFile {
    fn new() -> Self {
        // A fixed path per test binary. The global logger can only be
        // initialized once, and the first test to run owns that path; later
        // tests append to the same file. That is safe because every record is
        // selected by its unique request id, never by position in the file.
        //
        // The file is NOT truncated here: a second test would wipe the records
        // the first test is still reading. Each run starts fresh because the
        // name carries the process id, and the pid is not reused while a test
        // binary is alive.
        let path =
            std::env::temp_dir().join(format!("nimaproxy-sse-tap-{}.jsonl", std::process::id()));
        let _ = nimaproxy::turn_log::init_logger(path.to_str().unwrap(), true);
        Self { path }
    }

    fn records(&self) -> Vec<TurnLog> {
        let body = std::fs::read_to_string(&self.path).unwrap_or_default();
        body.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("every line is a TurnLog record"))
            .collect()
    }

    /// This turn's request id, read from the `x-request-id` response header the
    /// handler returns.
    ///
    /// The logger is process-global, so every test in this binary appends to
    /// one file and the default parallel runner interleaves those writes.
    /// Locating a turn by position in that file therefore cannot be correct:
    /// "the Nth first-byte record" can be a sibling test's. The response header
    /// is per-request and independent of the file, so it identifies this turn
    /// no matter what the other tests are doing.
    fn request_id_from(response: &Response) -> String {
        response
            .headers()
            .get("x-request-id")
            .unwrap_or_else(|| {
                panic!("the handler echoes x-request-id on every turn response");
            })
            .to_str()
            .expect("a request id is ASCII")
            .to_string()
    }

    /// Run one turn through the handler and return its first-byte request id.
    ///
    /// The id comes from the response header, so it is this turn's regardless of
    /// what sibling tests are writing to the shared log concurrently.
    async fn turn(&self, state: Arc<AppState>, body: Value) -> (Vec<u8>, String) {
        let (response, request_id) = self.turn_response(state, body).await;
        let bytes = to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap()
            .to_vec();
        (bytes, request_id)
    }

    /// Run one turn and return the handler's response plus its request id.
    ///
    /// The id is read from the response header, so it is this turn's even when
    /// sibling tests are writing to the shared log. The response is kept so a
    /// test can assert on headers and status before draining the body.
    async fn turn_response(&self, state: Arc<AppState>, body: Value) -> (Response, String) {
        let response = chat_completions(
            State(state),
            HeaderMap::new(),
            Bytes::from(body.to_string()),
        )
        .await
        .into_response();
        let request_id = Self::request_id_from(&response);
        (response, request_id)
    }

    /// The completion record for a request id, waiting for it to land.
    ///
    /// The stream is finished by the time the client has the body, but the
    /// record is written by a detached task, so it can arrive a few
    /// milliseconds later.
    async fn completion_for(&self, request_id: &str) -> TurnLog {
        for _ in 0..400 {
            if let Some(record) = self.records().into_iter().find(|r| {
                r.request_id.as_deref() == Some(request_id)
                    && r.record_kind == TurnLog::RECORD_KIND_COMPLETION
            }) {
                return record;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("no completion record for request {request_id}");
    }
}

/// A `data:` frame with the given content and finish reason.
fn frame(content: &str, finish: &str) -> String {
    format!(
        "data: {{\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"{MODEL}\",\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":\"{content}\"}},\"finish_reason\":\"{finish}\"}}]}}\n\n"
    )
}

fn turn_body() -> Value {
    json!({"model": MODEL, "messages": [{"role": "user", "content": "hi"}], "stream": true})
}

/// A clean streamed turn: content, a final `finish_reason: "stop"`, and
/// `data: [DONE]`.
#[tokio::test]
async fn clean_stream_writes_a_completion_record() {
    let (_server, url) = mock_upstream(&format!(
        "{}{}data: [DONE]\n\n",
        frame("Hello", ""),
        frame(" world", "stop")
    ))
    .await;
    let log = LogFile::new();
    let state = make_state(url);

    let (body, request_id) = log.turn(state, turn_body()).await;

    let text = String::from_utf8(body).unwrap();
    assert!(text.contains("Hello"), "client must see the stream: {text}");
    assert!(text.contains("data: [DONE]"), "clean stream: {text}");

    let completion = log.completion_for(&request_id).await;
    assert_eq!(completion.finish_reason.as_deref(), Some("stop"));
    assert_eq!(completion.stream_ended_cleanly, Some(true));
    assert_eq!(completion.failure_mode.as_deref(), Some("clean_stop"));
    // "Hello" + " world" = 11 characters.
    assert_eq!(completion.content_chars, Some(11));
    // The whole body arrives as one chunk from the mock upstream, so
    // `chunk_count` counts forwarded chunks, not SSE frames.
    assert_eq!(completion.chunk_count, Some(1), "one forwarded chunk");
    assert!(completion.body_bytes.unwrap() > 0);
    assert_eq!(completion.responding_model, MODEL);
    assert_eq!(completion.key_label.as_deref(), Some("test"));
    assert!(completion.error.is_none(), "a clean turn records no error");
    assert_eq!(
        completion.request_id.as_deref(),
        Some(request_id.as_str()),
        "the completion record must join to the first-byte record"
    );
}

/// A stream that stops mid-word with no `[DONE]` — the failure class that was
/// invisible before. The client still gets a 200 and the bytes that arrived,
/// but the log must now say the stream was truncated.
#[tokio::test]
async fn truncated_stream_is_recorded_as_truncated() {
    // The exact shape that produced the self-reinforcing loop: a fragment, the
    // start of a second one, and then the stream just ends.
    let (_server, url) =
        mock_upstream(&format!("{}{}", frame("Comm", ""), frame("D1 i", ""))).await;
    let log = LogFile::new();
    let state = make_state(url);

    let (body, request_id) = log.turn(state, turn_body()).await;

    let text = String::from_utf8(body).unwrap();
    assert!(
        !text.contains("[DONE]"),
        "the upstream deliberately omits [DONE]: {text}"
    );

    let completion = log.completion_for(&request_id).await;
    assert_eq!(completion.stream_ended_cleanly, Some(false));
    assert_eq!(
        completion.failure_mode.as_deref(),
        Some("truncated_stream"),
        "a stream that ended without [DONE] is a truncation, whatever it said"
    );
    assert_eq!(completion.content_chars, Some(8), "\"Comm\" + \"D1 i\"");
    assert!(!completion.success, "a truncated turn is not a success");
    assert!(
        completion
            .error
            .as_deref()
            .is_some_and(|e| e.contains("[DONE]")),
        "the truncation must be visible in the error field too: {:?}",
        completion.error
    );
    assert_eq!(
        completion.request_id.as_deref(),
        Some(request_id.as_str()),
        "the completion record must join to the first-byte record"
    );
}

/// A turn that ends in tool calls is a success, not a failure, and is named as
/// such.
#[tokio::test]
async fn tool_call_stream_is_classified_as_tool_calls() {
    let sse = concat!(
        "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"f\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"\"}]}\n\n",
        "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (_server, url) = mock_upstream(sse).await;
    let log = LogFile::new();
    let state = make_state(url);

    let (_, request_id) = log.turn(state, turn_body()).await;

    let completion = log.completion_for(&request_id).await;
    assert_eq!(completion.finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!(
        completion.failure_mode.as_deref(),
        Some("tool_calls"),
        "a tool-call turn is a success, not a failure"
    );
    assert_eq!(completion.stream_ended_cleanly, Some(true));
    assert!(completion.success);
}

/// A stream that reaches `[DONE]` but never reports a finish reason has no
/// classification of its own beyond "the upstream did not say".
#[tokio::test]
async fn missing_finish_reason_is_classified_as_missing() {
    let sse = concat!(
        "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"partial\"},\"finish_reason\":\"\"}]}\n\n",
        "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"\"},\"finish_reason\":null}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (_server, url) = mock_upstream(sse).await;
    let log = LogFile::new();
    let state = make_state(url);

    let (_, request_id) = log.turn(state, turn_body()).await;

    let completion = log.completion_for(&request_id).await;
    assert_eq!(completion.finish_reason, None);
    assert_eq!(
        completion.failure_mode.as_deref(),
        Some("missing_finish_reason")
    );
    assert_eq!(completion.content_chars, Some(7));
    assert_eq!(completion.stream_ended_cleanly, Some(true));
}

/// The two records of one turn share a request id, and only the completion
/// record carries the structural fields. This is the join the whole feature
/// exists to make possible.
#[tokio::test]
async fn ttfb_and_completion_records_share_a_request_id() {
    let (_server, url) = mock_upstream(&format!(
        "{}{}data: [DONE]\n\n",
        frame("one", ""),
        frame(" two", "stop")
    ))
    .await;
    let log = LogFile::new();
    let state = make_state(url);

    let (_, request_id) = log.turn(state, turn_body()).await;

    let completion = log.completion_for(&request_id).await;
    assert_eq!(
        completion.request_id.as_deref(),
        Some(request_id.as_str()),
        "the completion record must join to the first-byte record"
    );
    // Exactly one completion record for this turn: written once, not once per
    // poll of the body.
    let completions = log
        .records()
        .into_iter()
        .filter(|r| {
            r.request_id.as_deref() == Some(request_id.as_str())
                && r.record_kind == TurnLog::RECORD_KIND_COMPLETION
        })
        .count();
    assert_eq!(
        completions, 1,
        "the completion record is written exactly once"
    );
    // The first-byte record carries no structural fields: at that instant
    // nothing about the stream is known yet.
    let ttfb = log
        .records()
        .into_iter()
        .find(|r| {
            r.request_id.as_deref() == Some(request_id.as_str())
                && r.record_kind == TurnLog::RECORD_KIND_TTFB
        })
        .expect("the first-byte record exists");
    assert_eq!(ttfb.failure_mode, None);
    assert_eq!(ttfb.finish_reason, None);
    assert_eq!(ttfb.stream_ended_cleanly, None);
}

/// The tap must not change what the client receives: the forwarded bytes are
/// identical to the upstream's, and the response is still a 200 SSE stream.
#[tokio::test]
async fn the_tap_does_not_alter_the_forwarded_bytes() {
    let (_server, url) = mock_upstream(&format!(
        "{}{}{}data: [DONE]\n\n",
        frame("alpha", ""),
        frame("beta", ""),
        frame("gamma", "stop")
    ))
    .await;
    let log = LogFile::new();
    let state = make_state(url);

    let (response, request_id) = log.turn_response(state, turn_body()).await;

    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream"),
    );
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    for fragment in ["alpha", "beta", "gamma"] {
        assert!(text.contains(fragment), "forwarded bytes must be intact");
    }
    assert!(text.contains("data: [DONE]"));

    let completion = log.completion_for(&request_id).await;
    assert_eq!(completion.failure_mode.as_deref(), Some("clean_stop"));
    assert_eq!(completion.content_chars, Some(14));
}

/// An upstream that emits `finish_reason: ""` and `null` (which is what real
/// upstreams send on non-final frames) must not be classified from those: they
/// mean "no reason yet", not "stop".
#[tokio::test]
async fn empty_and_null_finish_reason_are_absent() {
    let sse = concat!(
        "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"only chunk\"},\"finish_reason\":\"\"}]}\n\n",
        "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"\"},\"finish_reason\":null}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (_server, url) = mock_upstream(sse).await;
    let log = LogFile::new();
    let state = make_state(url);

    let (_, request_id) = log.turn(state, turn_body()).await;

    let completion = log.completion_for(&request_id).await;
    assert_eq!(completion.finish_reason, None);
    assert_eq!(
        completion.failure_mode.as_deref(),
        Some("missing_finish_reason"),
        "an empty or null finish_reason is absent, not a classification"
    );
    assert_eq!(completion.content_chars, Some(10));
}

/// A non-streaming turn must not write a completion record: there is no SSE
/// stream to observe, and inventing one would put a `truncated_stream` on every
/// batch request.
#[tokio::test]
async fn non_streaming_turn_writes_no_completion_record() {
    let mut server = Server::new_async().await;
    server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            json!({
                "id": "c",
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "a complete answer"}, "finish_reason": "stop"}]
            })
            .to_string(),
        )
        .expect_at_least(1)
        .create();
    let log = LogFile::new();
    let state = make_state(server.url());

    // Scoped to this turn's request id rather than to the file: the logger is
    // process-global, so a detached completion task from an earlier test can
    // land here and inflate an absolute count. Those records carry a different
    // request id, so filtering on this turn's id makes the assertion immune to
    // both concurrent tests and late strays.
    let (_, request_id) = log
        .turn(
            state,
            json!({"model": MODEL, "messages": [{"role": "user", "content": "hi"}], "stream": false}),
        )
        .await;

    // Give any stray task a chance to land, then check nothing was added for
    // this turn.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    let mine = log
        .records()
        .into_iter()
        .filter(|r| r.request_id.as_deref() == Some(request_id.as_str()))
        .collect::<Vec<_>>();
    let ttfb_for_this_id = mine
        .iter()
        .filter(|r| r.record_kind == TurnLog::RECORD_KIND_TTFB)
        .count();
    let completions_for_this_id = mine
        .iter()
        .filter(|r| r.record_kind == TurnLog::RECORD_KIND_COMPLETION)
        .count();
    assert_eq!(
        ttfb_for_this_id, 1,
        "exactly one first-byte record for this request id"
    );
    assert_eq!(
        completions_for_this_id, 0,
        "a non-streaming turn writes no completion record; got {}",
        completions_for_this_id
    );
    assert_eq!(
        mine.len(),
        1,
        "only the first-byte record exists for this id"
    );
    assert_eq!(
        mine[0].record_kind,
        TurnLog::RECORD_KIND_TTFB,
        "the single record is the first-byte record"
    );
    assert_eq!(mine[0].failure_mode, None);
    assert_eq!(mine[0].finish_reason, None);
    assert_eq!(mine[0].stream_ended_cleanly, None);
}

/// A stalled stream must still produce a completion record rather than
/// vanishing: the idle guard aborts the body, the tap finalizes on that error,
/// and the record says the stream did not end cleanly.
#[tokio::test]
async fn stalled_stream_still_produces_a_completion_record() {
    use mockito::Matcher;
    use std::thread;
    use std::time::Duration;

    let mut server = Server::new_async().await;
    // One frame, then a long silence past the inter-chunk window.
    let stalled = server
        .mock("POST", "/v1/chat/completions")
        .match_body(Matcher::PartialJson(json!({"model": MODEL})))
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_chunked_body(|w| {
            // Deliberately NO `data: [DONE]`: the stall is what truncates the
            // stream, and an upstream that already sent its terminal frame is
            // not a truncation no matter how long it then stays quiet.
            std::io::Write::write_all(w, frame("partial answer", "").as_bytes()).unwrap();
            thread::sleep(Duration::from_millis(600));
            Ok(())
        })
        .expect_at_least(1)
        .create();
    let _ = stalled;

    let log = LogFile::new();
    let keys = vec![KeyEntry {
        key: "test-key".to_string(),
        label: Some("test".to_string()),
        disabled: false,
    }];
    // A short inter-chunk window so the guard fires inside the test.
    let state = AppState::new(nimaproxy::AppStateParams {
        keys,
        target: server.url(),
        router: None,
        model_stats: ModelStatsStore::new(100.0),
        racing_models: vec![],
        racing_max_parallel: 3,
        racing_timeout_ms: 150,
        racing_strategy: "complete".to_string(),
        model_params: HashMap::new(),
        model_compat: ModelCompat::default(),
        loop_idle_timeout: nimaproxy::loop_idle_timeout(),
    });

    let (response, request_id) = log.turn_response(state, turn_body()).await;
    assert_eq!(response.status(), 200);
    // The client gets whatever arrived; the connection is then aborted by the
    // idle guard, which `to_bytes` surfaces as a truncated body.
    let _ = to_bytes(response.into_body(), 1 << 20).await;
    let completion = log.completion_for(&request_id).await;
    assert_eq!(
        completion.failure_mode.as_deref(),
        Some("truncated_stream"),
        "a stalled stream is a truncation, not a silent success"
    );
    assert_eq!(completion.stream_ended_cleanly, Some(false));
    assert_eq!(completion.content_chars, Some(14));
}

/// A stream whose upstream emits a terminal usage frame must record the token
/// counts it reported.
///
/// Upstreams that honour `include_usage` send the accounting in a final frame
/// with `choices: []`, which is the frame a choice-only read would drop. The
/// counts are facts about the turn the upstream stated, and they belong on the
/// completion record so a turn can be costed without re-deriving them.
#[tokio::test]
async fn usage_frame_populates_token_counts_on_the_completion_record() {
    let sse = concat!(
        "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"finish_reason\":\"\"}]}\n\n",
        "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":11,\"total_tokens\":16}}\n\n",
        "data: [DONE]\n\n",
    );
    let (_server, url) = mock_upstream(sse).await;
    let log = LogFile::new();
    let state = make_state(url);

    let (_, request_id) = log.turn(state, turn_body()).await;

    let completion = log.completion_for(&request_id).await;
    assert_eq!(
        completion.response_tokens,
        Some(11),
        "completion_tokens from the usage frame"
    );
    assert_eq!(
        completion.request_tokens,
        Some(5),
        "prompt_tokens from the usage frame"
    );
    assert_eq!(
        completion.failure_mode.as_deref(),
        Some("clean_stop"),
        "the usage frame must not disturb the structural classification"
    );
}

/// A non-streaming turn carries its usage in the assembled JSON body, and that
/// is where the first-byte record must read it from.
#[tokio::test]
async fn non_streaming_usage_body_populates_token_counts() {
    let mut server = Server::new_async().await;
    server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            json!({
                "id": "c",
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "a complete answer"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 5, "completion_tokens": 11, "total_tokens": 16}
            })
            .to_string(),
        )
        .expect_at_least(1)
        .create();
    let log = LogFile::new();
    let state = make_state(server.url());

    let (_, request_id) = log
        .turn(
            state,
            json!({"model": MODEL, "messages": [{"role": "user", "content": "hi"}], "stream": false}),
        )
        .await;

    // A non-streaming turn writes exactly one record, the first-byte one, and
    // it is the record that carries the body's usage.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    let mine = log
        .records()
        .into_iter()
        .filter(|r| r.request_id.as_deref() == Some(request_id.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(mine.len(), 1, "one record for a non-streaming turn");
    assert_eq!(mine[0].request_tokens, Some(5));
    assert_eq!(mine[0].response_tokens, Some(11));
}

/// A turn whose upstream reports no usage must record no token counts.
///
/// "The upstream said nothing" and "the upstream said 0" are different facts;
/// recording 0 would understate every turn from a non-reporting upstream and
/// make the field unusable for cost questions.
#[tokio::test]
async fn absent_usage_records_no_token_counts() {
    let (_server, url) = mock_upstream(&format!(
        "{}{}data: [DONE]\n\n",
        frame("Hello", ""),
        frame(" world", "stop")
    ))
    .await;
    let log = LogFile::new();
    let state = make_state(url);

    let (_, request_id) = log.turn(state, turn_body()).await;

    let completion = log.completion_for(&request_id).await;
    assert_eq!(
        completion.request_tokens, None,
        "no usage frame means no prompt count, never 0"
    );
    assert_eq!(
        completion.response_tokens, None,
        "no usage frame means no completion count, never 0"
    );
}

/// The completion record carries the largest observed inter-chunk gap.
///
/// A mock upstream delivers its whole body as one chunk, so the gap clock
/// never starts here: the record must say "no gap observed", not 0. The
/// positive arm — a real gap recorded in milliseconds — lives in the tap's
/// unit tests, where the chunk timing is under the test's control; what
/// this proves is the end-to-end field path for the degenerate shape, plus
/// the clean end cause on the same record.
#[tokio::test]
async fn single_chunk_stream_records_no_gap_and_clean_end() {
    let (_server, url) = mock_upstream(&format!(
        "{}{}data: [DONE]\n\n",
        frame("Hello", ""),
        frame(" world", "stop")
    ))
    .await;
    let log = LogFile::new();
    let state = make_state(url);

    let (_, request_id) = log.turn(state, turn_body()).await;

    let completion = log.completion_for(&request_id).await;
    assert_eq!(
        completion.max_inter_chunk_gap_ms, None,
        "one forwarded chunk means no gap was observed, never a 0 ms gap"
    );
    assert_eq!(
        completion.stream_ended_by.as_deref(),
        Some("clean"),
        "an upstream that closed its own stream ended cleanly"
    );
}

/// A stream the idle guard aborts must record `guard_stall` as its end
/// cause — not `upstream_error`, and not `clean`. This is the field that
/// answers "guard abort or upstream close" on the next truncation without
/// any capture infra.
#[tokio::test]
async fn guard_aborted_stream_records_guard_stall_end_cause() {
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    // A real TCP listener, not a mock: a mock buffers its whole chunked
    // body into one fast write, so the stream ends cleanly before the
    // guard can fire and the record says `clean`. Only a real
    // inter-chunk gap exercises the guard.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";
            let _ = sock.write_all(head.as_bytes()).await;
            // One frame, then silence past the idle window: the guard —
            // not the upstream — is what ends this stream.
            let chunk = frame("partial answer", "");
            let framed = format!("{:x}\r\n{}\r\n", chunk.len(), chunk);
            let _ = sock.write_all(framed.as_bytes()).await;
            let _ = sock.flush().await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });

    let log = LogFile::new();
    // A short idle window so the guard fires inside the test; the
    // first-byte window is equally short, but the first byte arrives
    // immediately, so only the mid-stream rail is exercised.
    let state = nimaproxy::AppState::new_with_controls(
        nimaproxy::AppStateParams {
            keys: vec![KeyEntry {
                key: "test-key".to_string(),
                label: Some("test".to_string()),
                disabled: false,
            }],
            target: format!("http://{}", addr),
            router: None,
            model_stats: ModelStatsStore::new(100.0),
            racing_models: vec![],
            racing_max_parallel: 3,
            racing_timeout_ms: 150,
            racing_strategy: "complete".to_string(),
            model_params: HashMap::new(),
            model_compat: ModelCompat::default(),
            loop_idle_timeout: nimaproxy::loop_idle_timeout(),
        },
        nimaproxy::RuntimeControls {
            mid_stream_idle_timeout_ms: 150,
            ..nimaproxy::RuntimeControls::default()
        },
    );

    let (response, request_id) = log.turn_response(state, turn_body()).await;
    assert_eq!(response.status(), 200);
    let _ = to_bytes(response.into_body(), 1 << 20).await;
    let completion = log.completion_for(&request_id).await;
    assert_eq!(completion.stream_ended_cleanly, Some(false));
    assert_eq!(
        completion.stream_ended_by.as_deref(),
        Some("guard_stall"),
        "the guard aborted this stream, so the record must say so"
    );
}
