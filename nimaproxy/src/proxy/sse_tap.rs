//! Structural observation of a forwarded SSE stream.
//!
//! The proxy used to log "success" the instant the first SSE byte arrived and
//! then forward the stream without looking at it again. That made one failure
//! class invisible: an upstream that emits a few mid-word fragments and then
//! stops — `Comm`, `D1 i`, end of stream — is indistinguishable from a
//! completed generation as far as the log is concerned. The truncated assistant
//! message then lands in conversation history, and the next request imitates it,
//! so the failure compounds.
//!
//! The fix is structural, not lexical. This tap counts structural facts about
//! the stream — whether the upstream sent `[DONE]`, what `finish_reason` it
//! reported, how many content characters and tool calls it delivered — and
//! never reads the text it forwards. There is deliberately no content
//! inspection here: classifying a turn by looking for a phrase inside the body
//! is brittle by construction, and the moment the phrase changes the classifier
//! silently goes blind. `failure_mode` is derived only from the structural
//! signals, so an upstream that stops after `Comm` and one that stops after
//! four paragraphs are classified identically: both are truncated streams.
//!
//! The tap does not buffer anything for the client. It wraps the byte stream
//! that is already being forwarded and inspects each chunk as it passes
//! through, so the added work per chunk is an incremental line split plus one
//! `serde_json::from_str` on each `data:` frame's JSON payload.

use bytes::Bytes;
use futures::Stream;
use parking_lot::Mutex;
use serde::Deserialize;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

/// Bound on the trailing partial line held across chunk boundaries.
///
/// A well-formed SSE `data:` line is a few KB. The cap only exists so a
/// pathological upstream that never sends a newline cannot grow this buffer
/// without limit; once it is reached the line is treated as unsplittable and
/// parsing restarts at the next newline.
const MAX_PARTIAL_LINE: usize = 8 * 1024;

/// Structural facts observed about one forwarded SSE stream.
///
/// Cheap to clone (every field is behind an `Arc`), so a clone can be handed to
/// the code that logs the completion record while the original stays borrowed
/// by the stream.
#[derive(Debug, Default, Clone)]
pub struct SseTap {
    inner: Arc<SseTapInner>,
}

#[derive(Debug, Default)]
struct SseTapInner {
    /// Last non-null, non-empty `finish_reason` seen in `choices[0]`.
    finish_reason: Mutex<Option<String>>,
    /// Sum of characters in `choices[0].delta.content` across frames.
    content_chars: AtomicU64,
    /// Number of `choices[0].delta.tool_calls` entries seen across frames.
    tool_call_count: AtomicUsize,
    /// Whether the terminal `data: [DONE]` frame was observed.
    saw_done: AtomicBool,
    /// Number of `Ok` chunks passed through the tap.
    chunk_count: AtomicU64,
    /// Sum of the byte lengths of those chunks.
    body_bytes: AtomicU64,
    /// Whether the stream has already been finalized, so the completion record
    /// is written exactly once no matter how the stream ends.
    finalized: AtomicBool,
    /// Trailing partial SSE line, carried across chunk boundaries.
    partial: Mutex<String>,
}

impl SseTap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe one forwarded chunk, then it is yielded to the client unchanged.
    ///
    /// Called on every `Ok` chunk of the wrapped stream. Parsing is
    /// incremental: a frame that splits across chunks is completed on the
    /// chunk that finishes it.
    pub fn observe(&self, chunk: &Bytes) {
        self.inner.chunk_count.fetch_add(1, Ordering::Relaxed);
        self.inner
            .body_bytes
            .fetch_add(chunk.len() as u64, Ordering::Relaxed);

        let mut partial = self.inner.partial.lock();
        // SSE lines are separated by LF; CRLF is stripped per line below, so a
        // body that uses either ending parses identically.
        let text = match std::str::from_utf8(chunk) {
            Ok(text) => text,
            // A frame that is not UTF-8 cannot be an SSE `data:` frame we can
            // read. It is still forwarded untouched; the tap just does not
            // learn anything from it.
            Err(_) => return,
        };
        let mut buf = std::mem::take(&mut *partial);
        buf.push_str(text);

        while let Some(pos) = buf.find('\n') {
            // +1 keeps the newline out of the line, so a CR at the end of a
            // CRLF pair is trimmed by `trim_end` rather than being left inside
            // the field name.
            let line = buf[..pos].trim_end_matches('\r').to_string();
            buf.drain(..pos + 1);
            drop(partial);
            self.observe_line(&line);
            partial = self.inner.partial.lock();
        }

        if buf.len() > MAX_PARTIAL_LINE {
            // Never a well-formed SSE frame; drop it rather than growing.
            buf.clear();
        }
        *partial = buf;
    }

    /// Parse one complete SSE line.
    fn observe_line(&self, line: &str) {
        let Some(payload) = line.strip_prefix("data:") else {
            // `event:`, `id:`, `:comment`, and blank separator lines carry no
            // structural signal for this tap.
            return;
        };
        let payload = payload.strip_prefix(' ').unwrap_or(payload);
        if payload == "[DONE]" {
            self.inner.saw_done.store(true, Ordering::Relaxed);
            return;
        }
        self.observe_data_frame(payload);
    }

    /// Extract the structural fields from one `data:` JSON payload.
    ///
    /// Best-effort by design: a payload this cannot parse contributes nothing,
    /// because a chat completion chunk we cannot read is not evidence of
    /// anything. Nothing here looks at the text of `content`.
    fn observe_data_frame(&self, payload: &str) {
        let Ok(frame) = serde_json::from_str::<SseFrame>(payload) else {
            return;
        };
        let Some(choice) = frame.choices.into_iter().next() else {
            return;
        };

        // `null` and `""` both mean "no finish reason on this frame"; only a
        // real value is recorded, and the last one wins.
        if let Some(reason) = choice.finish_reason.filter(|r| !r.is_empty()) {
            *self.inner.finish_reason.lock() = Some(reason);
        }

        if let Some(delta) = choice.delta {
            if let Some(content) = delta.content.as_ref() {
                self.inner
                    .content_chars
                    .fetch_add(content.chars().count() as u64, Ordering::Relaxed);
            }
            if let Some(tool_calls) = delta.tool_calls.as_ref() {
                self.inner
                    .tool_call_count
                    .fetch_add(tool_calls.len(), Ordering::Relaxed);
            }
        }
    }

    /// Last non-null `finish_reason` observed, if any.
    pub fn finish_reason(&self) -> Option<String> {
        self.inner.finish_reason.lock().clone()
    }

    pub fn content_chars(&self) -> u64 {
        self.inner.content_chars.load(Ordering::Relaxed)
    }

    pub fn tool_call_count(&self) -> usize {
        self.inner.tool_call_count.load(Ordering::Relaxed)
    }

    /// Whether the stream reached `data: [DONE]`.
    pub fn saw_done(&self) -> bool {
        self.inner.saw_done.load(Ordering::Relaxed)
    }

    pub fn chunk_count(&self) -> u64 {
        self.inner.chunk_count.load(Ordering::Relaxed)
    }

    pub fn body_bytes(&self) -> u64 {
        self.inner.body_bytes.load(Ordering::Relaxed)
    }

    /// Whether [`Self::finalize`] has already run.
    pub fn is_finalized(&self) -> bool {
        self.inner.finalized.load(Ordering::Relaxed)
    }

    /// Mark the stream finished. Returns `true` only for the call that
    /// finalized it, so the completion record is written exactly once whether
    /// the stream ended cleanly, errored, or was aborted by the idle guard.
    pub fn finalize(&self) -> bool {
        !self.inner.finalized.swap(true, Ordering::AcqRel)
    }

    /// Structural outcome of the stream, ready to be written to a `TurnLog`.
    pub fn outcome(&self) -> SseOutcome {
        SseOutcome {
            finish_reason: self.finish_reason(),
            content_chars: self.content_chars(),
            tool_call_count: self.tool_call_count(),
            stream_ended_cleanly: self.saw_done(),
            chunk_count: self.chunk_count(),
            body_bytes: self.body_bytes(),
        }
    }
}

/// The structural outcome of one SSE stream, as observed by an [`SseTap`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SseOutcome {
    pub finish_reason: Option<String>,
    pub content_chars: u64,
    pub tool_call_count: usize,
    /// Defaults to `false`: a stream that was never observed says nothing
    /// about whether it ended cleanly, and the conservative reading is "not
    /// clean".
    pub stream_ended_cleanly: bool,
    pub chunk_count: u64,
    pub body_bytes: u64,
}

/// A stream adapter that observes chunks and finalizes the tap exactly once
/// when the stream ends — cleanly, on error, or because it was aborted.
///
/// It adds no buffering: each chunk is inspected and immediately forwarded, so
/// the client sees the same bytes at the same time it would have without the
/// tap.
///
/// The finalize is a single compare-and-swap, so wrapping a stream with this is
/// idempotent even if the adapter is polled again after the stream has ended.
///
/// The inner stream is boxed so the adapter is usable both with
/// `Body::from_stream` (which needs `Send + 'static`, not `Unpin`) and with
/// `StreamExt::next` from the non-streaming assembly path (which needs
/// `Unpin`). One type therefore serves both arms of the proxy's response path.
pub struct TappedStream {
    tap: SseTap,
    stream: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>,
    ended: bool,
}

impl TappedStream {
    pub fn new<S>(tap: SseTap, stream: S) -> Self
    where
        S: Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
    {
        Self {
            tap,
            stream: Box::pin(stream),
            ended: false,
        }
    }

    pub fn tap(&self) -> &SseTap {
        &self.tap
    }
}

impl Stream for TappedStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.ended {
            return Poll::Ready(None);
        }
        let this = self.get_mut();
        let tap = this.tap.clone();
        match this.stream.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                tap.observe(&chunk);
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(e))) => {
                // The stream failed, so it did not end cleanly. Finalize here
                // so the completion record is still written for an aborted
                // stream (an idle-guard timeout produces exactly this shape).
                tap.finalize();
                this.ended = true;
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                tap.finalize();
                this.ended = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Minimal shape of an OpenAI-compatible SSE chunk.
///
/// Every field is optional and unknown fields are ignored, so frames from
/// upstreams that carry extra keys (or that omit `delta` entirely) parse the
/// same as the ones this proxy emits itself.
#[derive(Debug, Deserialize)]
struct SseFrame {
    #[serde(default)]
    choices: Vec<SseChoice>,
}

#[derive(Debug, Deserialize)]
struct SseChoice {
    #[serde(default)]
    delta: Option<SseDelta>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SseDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<serde_json::Value>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    /// Feed whole strings as chunks and return the resulting outcome.
    async fn feed(parts: &[&str]) -> SseOutcome {
        let tap = SseTap::new();
        for part in parts {
            tap.observe(&Bytes::copy_from_slice(part.as_bytes()));
        }
        tap.outcome()
    }

    fn frame(content: &str, finish: &str) -> String {
        format!(
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":\"{content}\"}},\"finish_reason\":\"{finish}\"}}]}}\n\n"
        )
    }

    /// A frame split across two chunk boundaries must still parse, so a
    /// network-level split can never hide a finish reason or a `[DONE]`.
    #[tokio::test]
    async fn frame_split_across_chunks_is_observed() {
        let whole = format!("{}data: [DONE]\n\n", frame("Hello", "stop"));
        // Split at arbitrary byte positions, including inside `data:` and
        // inside the JSON.
        let bytes = whole.as_bytes();
        for split in 1..whole.len() {
            let a = std::str::from_utf8(&bytes[..split]).unwrap();
            let b = std::str::from_utf8(&bytes[split..]).unwrap();
            let outcome = feed(&[a, b]).await;
            assert_eq!(
                outcome.finish_reason.as_deref(),
                Some("stop"),
                "split at {split}"
            );
            assert!(outcome.stream_ended_cleanly, "split at {split}");
            assert_eq!(outcome.content_chars, 5, "split at {split}");
        }
    }

    /// Several frames arriving in one chunk are all counted.
    #[tokio::test]
    async fn multiple_frames_per_chunk_are_all_observed() {
        let body = format!(
            "{}{}{}data: [DONE]\n\n",
            frame("Hel", ""),
            frame("lo ", ""),
            frame("world", "stop")
        );
        let outcome = feed(&[&body]).await;
        assert_eq!(outcome.content_chars, 11);
        assert_eq!(outcome.finish_reason.as_deref(), Some("stop"));
        assert!(outcome.stream_ended_cleanly);
        assert_eq!(outcome.chunk_count, 1);
    }

    /// CRLF is the other legal SSE line ending and must parse identically.
    #[tokio::test]
    async fn crlf_line_endings_are_accepted() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\r\n\r\ndata: [DONE]\r\n\r\n";
        let outcome = feed(&[body]).await;
        assert_eq!(outcome.finish_reason.as_deref(), Some("stop"));
        assert_eq!(outcome.content_chars, 2);
        assert!(outcome.stream_ended_cleanly);
    }

    /// The terminal frame is what separates a completed generation from a
    /// truncated one, so detecting it is the tap's core job.
    #[tokio::test]
    async fn done_frame_is_detected() {
        let outcome = feed(&[&frame("x", "stop"), "data: [DONE]\n\n"]).await;
        assert!(outcome.stream_ended_cleanly);
        assert!(outcome.chunk_count == 2);
    }

    #[tokio::test]
    async fn missing_done_means_not_clean() {
        let outcome = feed(&[&frame("Comm", "stop")]).await;
        assert!(!outcome.stream_ended_cleanly);
    }

    /// A stream that stops mid-word with no finish reason at all is the exact
    /// failure this tap exists to make visible.
    #[tokio::test]
    async fn truncated_mid_word_stream_has_no_finish_reason() {
        let outcome = feed(&[&frame("Comm", "")]).await;
        assert_eq!(outcome.finish_reason, None);
        assert_eq!(outcome.content_chars, 4);
        assert!(!outcome.stream_ended_cleanly);
    }

    /// `null` and `""` both mean "no finish reason on this frame"; treating
    /// either as a value would manufacture a classification that never
    /// happened.
    #[tokio::test]
    async fn null_and_empty_finish_reason_are_absent() {
        let null_frame =
            "data: {\"choices\":[{\"delta\":{\"content\":\"a\"},\"finish_reason\":null}]}\n\n";
        let empty_frame =
            "data: {\"choices\":[{\"delta\":{\"content\":\"b\"},\"finish_reason\":\"\"}]}\n\n";
        let real = frame("c", "length");
        let outcome = feed(&[null_frame, empty_frame, &real]).await;
        assert_eq!(outcome.finish_reason.as_deref(), Some("length"));
        assert_eq!(outcome.content_chars, 3);

        let outcome = feed(&[null_frame, empty_frame]).await;
        assert_eq!(outcome.finish_reason, None);
    }

    /// The last non-empty finish reason wins, which is how real upstreams
    /// signal a late state change.
    #[tokio::test]
    async fn last_finish_reason_wins() {
        let body = format!("{}{}", frame("a", ""), frame("b", "tool_calls"));
        let outcome = feed(&[&body]).await;
        assert_eq!(outcome.finish_reason.as_deref(), Some("tool_calls"));
    }

    /// Tool call fragments are counted per entry, and content is counted in
    /// characters rather than bytes so a multi-byte body is not overcounted.
    ///
    /// The terminal `finish_reason` is `tool_calls`, carried on the final data
    /// frame the way real upstreams emit it; the content frame earlier in the
    /// stream carries no reason. This respects the tap's last-wins rule.
    #[tokio::test]
    async fn tool_calls_are_counted_and_content_is_chars() {
        let unicode =
            "data: {\"choices\":[{\"delta\":{\"content\":\"héllo→\"},\"finish_reason\":\"\"}]}\n\n";
        let tc = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"f\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"\"}]}\n\n";
        let tc2 = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_2\",\"function\":{\"name\":\"g\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n";
        let outcome = feed(&[unicode, tc, tc2, "data: [DONE]\n\n"]).await;
        assert_eq!(outcome.tool_call_count, 2);
        assert_eq!(outcome.content_chars, 6);
        assert_eq!(outcome.finish_reason.as_deref(), Some("tool_calls"));
    }

    /// Byte and chunk counters let the completion record say how much was
    /// actually forwarded.
    #[tokio::test]
    async fn chunk_and_byte_counts_are_exact() {
        let outcome = feed(&["data: [DONE]\n\n", "ignored trailing"]).await;
        assert_eq!(outcome.chunk_count, 2);
        // "data: [DONE]\n\n" is 14 bytes; "ignored trailing" is 16 bytes.
        assert_eq!(outcome.body_bytes, 14 + 16);
    }

    /// A non-UTF-8 chunk is forwarded and counted but contributes no frame.
    #[tokio::test]
    async fn non_utf8_chunk_is_counted_but_not_parsed() {
        let tap = SseTap::new();
        let bad = Bytes::from_static(&[0xff, 0xfe, 0x00]);
        tap.observe(&bad);
        assert_eq!(tap.chunk_count(), 1);
        assert_eq!(tap.body_bytes(), 3);
        assert_eq!(tap.finish_reason(), None);
    }

    /// Unparseable `data:` payloads contribute nothing rather than inventing a
    /// classification.
    #[tokio::test]
    async fn unparseable_payloads_are_ignored() {
        let outcome = feed(&["data: not json\n\n", "data: {\"choices\":[]}\n\n"]).await;
        assert_eq!(outcome.finish_reason, None);
        assert_eq!(outcome.content_chars, 0);
    }

    /// `event:`/`id:`/comment lines and blank separators carry no signal.
    #[tokio::test]
    async fn non_data_lines_are_ignored() {
        let body = ": keep-alive\nevent: message\nid: 7\n\n";
        let outcome = feed(&[body]).await;
        assert_eq!(outcome.chunk_count, 1);
        assert_eq!(outcome.content_chars, 0);
    }

    /// A pathological upstream that never sends a newline must not grow the
    /// partial-line buffer without bound.
    #[tokio::test]
    async fn unbounded_partial_line_is_bounded() {
        let tap = SseTap::new();
        let big = "x".repeat(MAX_PARTIAL_LINE * 3);
        tap.observe(&Bytes::from(big));
        assert!(tap.inner.partial.lock().len() <= MAX_PARTIAL_LINE);
    }

    /// The tap must forward the bytes it was given, unchanged.
    #[tokio::test]
    async fn wrapped_stream_yields_the_same_chunks() {
        let tap = SseTap::new();
        let parts = vec![
            Bytes::from_static(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n"),
            Bytes::from_static(b"\ndata: [DONE]\n\n"),
        ];
        let stream = futures::stream::iter(parts.clone().into_iter().map(Ok));
        let collected: Vec<Bytes> = TappedStream::new(tap.clone(), stream)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(|c| c.unwrap())
            .collect();
        assert_eq!(collected, parts);
        assert!(tap.saw_done());
        assert_eq!(tap.finish_reason().as_deref(), Some("stop"));
        assert_eq!(tap.content_chars(), 2);
    }

    /// `TappedStream` finalizes once when the upstream ends cleanly.
    #[tokio::test]
    async fn tapped_stream_finalizes_once_on_clean_end() {
        let tap = SseTap::new();
        let stream = futures::stream::iter(vec![Ok(Bytes::from_static(b"data: [DONE]\n\n"))]);
        let mut tapped = TappedStream::new(tap.clone(), stream);
        assert!(!tap.is_finalized());
        let chunk = tapped.next().await.unwrap().unwrap();
        assert_eq!(&chunk[..], b"data: [DONE]\n\n");
        assert!(tapped.next().await.is_none());
        assert!(tap.is_finalized());
        // A second end-of-stream poll must not resurrect the flag.
        assert!(!tap.finalize());
        assert!(tapped.next().await.is_none());
    }

    /// An aborted stream (the idle guard's timeout error) must still finalize,
    /// so the completion record is written rather than lost.
    #[tokio::test]
    async fn tapped_stream_finalizes_once_on_error() {
        let tap = SseTap::new();
        let err = || Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "stalled"));
        let stream = futures::stream::iter(vec![
            Ok(Bytes::from_static(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"Comm\"}}]}\n\n",
            )),
            err(),
        ]);
        let mut tapped = TappedStream::new(tap.clone(), stream);
        assert!(tapped.next().await.is_some());
        let second = tapped.next().await.unwrap();
        assert!(second.is_err());
        assert!(tap.is_finalized());
        assert_eq!(tap.content_chars(), 4);
        assert!(!tap.saw_done());
    }
}
