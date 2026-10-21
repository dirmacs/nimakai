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
use std::collections::HashSet;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

/// Bound on the trailing partial line held across chunk boundaries.
///
/// A well-formed SSE `data:` line is a few KB. The cap only exists so a
/// pathological upstream that never sends a newline cannot grow this buffer
/// without limit; once it is reached the line is treated as unsplittable and
/// parsing restarts at the next newline.
const MAX_PARTIAL_LINE: usize = 8 * 1024;

/// How a tapped stream ended, as observed by [`TappedStream`].
///
/// This answers the question `saw_done` cannot: a stream that ended without
/// `[DONE]` either stalled under the idle guard or broke upstream, and those
/// are different operator actions (widen the window vs suspect the network).
/// The tap sits *inside* the guard, so it sees the guard's abort error and
/// can tell the two apart by the same typed payload the assembly path
/// classifies with ([`crate::proxy::streaming::is_stall`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StreamEnd {
    /// The upstream closed the stream itself (`None`), or the tap never saw
    /// the stream end at all. A truncated stream can still be `Clean`: the
    /// upstream simply stopped sending without breaking the connection, and
    /// an unobserved stream says nothing worse than "no reported error".
    #[default]
    Clean,
    /// The idle guard fired: the stream yielded an error carrying the
    /// [`crate::proxy::streaming::StreamStalled`] payload.
    GuardStall,
    /// The stream yielded an error that is not a guard stall: the upstream
    /// broke the connection itself.
    UpstreamError,
}

impl StreamEnd {
    /// The value stored in the tap's atomic. Match arms, not a cast, so a
    /// future variant is a compile error here rather than a silent alias.
    fn discriminant(self) -> u8 {
        match self {
            StreamEnd::Clean => 0,
            StreamEnd::GuardStall => 1,
            StreamEnd::UpstreamError => 2,
        }
    }

    /// The inverse of [`Self::discriminant`]. An unknown value reads as
    /// `Clean`: the field is only ever written by this module, so anything
    /// else is impossible, and the conservative reading of an impossible
    /// value is "no reported error".
    fn from_discriminant(v: u8) -> Self {
        match v {
            1 => StreamEnd::GuardStall,
            2 => StreamEnd::UpstreamError,
            _ => StreamEnd::Clean,
        }
    }

    /// The string written to the turn log, so the record is readable without
    /// consulting this enum. Snake case matches `failure_mode`'s vocabulary.
    pub fn as_str(self) -> &'static str {
        match self {
            StreamEnd::Clean => "clean",
            StreamEnd::GuardStall => "guard_stall",
            StreamEnd::UpstreamError => "upstream_error",
        }
    }
}

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
    /// Sum of characters in `choices[0].delta.reasoning_content` across
    /// frames. What splits a zero-content clean EOF into reasoning-abandon
    /// (reasoning present, content absent) vs true-empty (neither): a
    /// thinking model that emitted its whole trace and then stopped without
    /// content is a different operator action from one that emitted nothing
    /// at all.
    ///
    /// A zero here does NOT mean "the model did not reason": NVIDIA serves
    /// step-5-preview traces in `reasoning_content` on BOTH paths (SSE deltas
    /// confirmed live 2026-10-21: 18/19 and 82/83 reasoning frames at 16k/32k
    /// caps), but the non-streaming client response carries the trace in the
    /// assembled JSON body, not in SSE deltas the tap can see. Zero on a
    /// non-streamed turn is "unobserved", not "absent".
    reasoning_chars: AtomicU64,
    /// Number of `choices[0].delta.tool_calls` entries seen across frames.
    ///
    /// Counts fragment entries, not logical calls: a single call whose
    /// `arguments` stream over ~26 frames counts ~26 here. Use
    /// `tool_call_distinct` for the de-duplicated reading.
    tool_call_count: AtomicUsize,
    /// Distinct logical tool calls, keyed so a call whose `arguments` stream
    /// over many frames counts once rather than once per fragment entry.
    ///
    /// `tool_call_count` counts fragment entries and overcounts logical calls;
    /// this set is the de-duplicated reading.
    tool_call_keys: Mutex<HashSet<String>>,
    /// Monotonic sequence for anonymous tool-call keys. An entry that carries
    /// neither `index` nor `id` cannot be joined to a previous call, so it
    /// gets a unique per-occurrence key — folding it into another call would
    /// undercount.
    anon_seq: AtomicUsize,
    /// Whether the terminal `data: [DONE]` frame was observed.
    saw_done: AtomicBool,
    /// Number of `Ok` chunks passed through the tap.
    chunk_count: AtomicU64,
    /// Sum of the byte lengths of those chunks.
    body_bytes: AtomicU64,
    /// `usage.prompt_tokens` from the terminal usage frame, if the upstream
    /// sent one. The stream is never modified to force it.
    prompt_tokens: Mutex<Option<u32>>,
    /// `usage.completion_tokens` from the terminal usage frame, if any.
    completion_tokens: Mutex<Option<u32>>,
    /// Whether the stream has already been finalized, so the completion record
    /// is written exactly once no matter how the stream ends.
    finalized: AtomicBool,
    /// Trailing partial SSE line, carried across chunk boundaries.
    partial: Mutex<String>,
    /// When the last `Ok` chunk was observed, if any. The clock behind the gap
    /// measurement: `None` until the first forwarded chunk arrives, so the
    /// wait for the first byte is never counted as a mid-stream gap.
    last_chunk_at: Mutex<Option<Instant>>,
    /// Largest observed gap between two consecutive forwarded chunks, in
    /// milliseconds. Updated with a compare-and-swap loop so a concurrent
    /// poll never loses a larger gap to a smaller one.
    max_gap_ms: AtomicU64,
    /// How the stream ended, as observed by the adapter: clean EOF, a
    /// guard-stall error, or an upstream error. Stored as the
    /// [`StreamEnd::discriminant`] so it fits an atomic; the default, `Clean`,
    /// is what a stream that was never observed must say — it ended with no
    /// reported error, so there is no evidence of an abort.
    ended_by: AtomicU8,
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

        // The gap clock: measured between consecutive forwarded chunks, at
        // observe time rather than poll time, so what is recorded is the
        // upstream's inter-chunk spacing. The first chunk only starts the
        // clock — the wait for it is the TTFB bound's business, not a gap.
        let now = Instant::now();
        if let Some(prev) = self.inner.last_chunk_at.lock().replace(now) {
            let gap_ms = now
                .duration_since(prev)
                .as_millis()
                .min(u128::from(u64::MAX)) as u64;
            let mut cur = self.inner.max_gap_ms.load(Ordering::Relaxed);
            while gap_ms > cur {
                match self.inner.max_gap_ms.compare_exchange_weak(
                    cur,
                    gap_ms,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(next) => cur = next,
                }
            }
        }

        // One lock acquisition per chunk: `partial` is borrowed for the whole
        // scan, so a chunk carrying many lines neither re-locks per line nor
        // allocates a `String` per line. `observe_line` and everything below it
        // only touch other fields, so holding this guard across them cannot
        // deadlock.
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
        // Appending in place keeps the buffer's capacity across chunks, so a
        // steady stream of small chunks does not reallocate.
        partial.push_str(text);

        // Walk the complete lines with a cursor instead of draining after each
        // one: `String::drain` shifts the remainder, so draining inside the
        // loop is quadratic in the number of lines in the chunk.
        let mut consumed = 0;
        while let Some(pos) = partial[consumed..].find('\n') {
            let end = consumed + pos;
            // `end` keeps the newline out of the line, so a CR at the end of a
            // CRLF pair is trimmed by `trim_end` rather than being left inside
            // the field name.
            let line = partial[consumed..end].trim_end_matches('\r');
            self.observe_line(line);
            consumed = end + 1;
        }

        if consumed > 0 {
            // Drop the consumed prefix exactly once per chunk.
            partial.drain(..consumed);
        }

        if partial.len() > MAX_PARTIAL_LINE {
            // Never a well-formed SSE frame; drop it rather than growing.
            partial.clear();
        }
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
        // The usage frame arrives with `choices: []` in most upstreams, so the
        // choice-only read below would drop it. `include_usage` frames carry
        // the whole request's accounting; record whatever the upstream chose
        // to send, and never synthesise counts when it did not.
        if let Some(usage) = frame.usage {
            if let Some(prompt) = usage.prompt_tokens {
                *self.inner.prompt_tokens.lock() = Some(prompt);
            }
            if let Some(completion) = usage.completion_tokens {
                *self.inner.completion_tokens.lock() = Some(completion);
            }
        }

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
            if let Some(reasoning) = delta.reasoning_content.as_ref() {
                self.inner
                    .reasoning_chars
                    .fetch_add(reasoning.chars().count() as u64, Ordering::Relaxed);
            }
            if let Some(tool_calls) = delta.tool_calls.as_ref() {
                self.inner
                    .tool_call_count
                    .fetch_add(tool_calls.len(), Ordering::Relaxed);
                for entry in tool_calls {
                    let key = self.tool_call_key(entry);
                    self.inner.tool_call_keys.lock().insert(key);
                }
            }
        }
    }
    /// Build a stable, joinable key for one `tool_calls` delta entry.
    ///
    /// Index is preferred over `id` because every fragment of a single logical
    /// call shares its array position, while `id` is only present on the first
    /// fragment. Keying on `id` would leave every fragment after the first
    /// without a key, and keying on a missing id folds that fragment into a
    /// previous call — undercounting. An entry that carries neither index nor
    /// id cannot be joined to anything, so it gets a unique per-occurrence
    /// anonymous key; merging it with any other call would also undercount.
    fn tool_call_key(&self, entry: &serde_json::Value) -> String {
        if let Some(idx) = entry.get("index").and_then(serde_json::Value::as_u64) {
            return format!("i:{idx}");
        }
        if let Some(id) = entry.get("id").and_then(serde_json::Value::as_str) {
            return format!("d:{id}");
        }
        let n = self.inner.anon_seq.fetch_add(1, Ordering::Relaxed);
        format!("a:{n}")
    }

    /// Distinct logical tool calls, de-duplicated across fragment frames.
    ///
    /// Unlike [`tool_call_count`](Self::tool_call_count), which counts every
    /// `tool_calls` delta entry (so a call whose `arguments` stream over many
    /// frames counts once per frame), this counts each logical call once.
    pub fn tool_call_distinct(&self) -> usize {
        self.inner.tool_call_keys.lock().len()
    }

    /// Last non-null `finish_reason` observed, if any.
    pub fn finish_reason(&self) -> Option<String> {
        self.inner.finish_reason.lock().clone()
    }

    pub fn content_chars(&self) -> u64 {
        self.inner.content_chars.load(Ordering::Relaxed)
    }

    /// Sum of characters in `choices[0].delta.reasoning_content` across
    /// frames. 0 when the upstream emitted no reasoning deltas.
    pub fn reasoning_chars(&self) -> u64 {
        self.inner.reasoning_chars.load(Ordering::Relaxed)
    }

    /// Number of `choices[0].delta.tool_calls` entries seen across frames.
    ///
    /// Counts fragment entries, not logical calls: a single call whose
    /// `arguments` stream over ~26 frames counts ~26 here. This is the pinned
    /// fragment contract. Use [`tool_call_distinct`](Self::tool_call_distinct)
    /// for the de-duplicated logical-call count.
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

    /// Largest gap between two consecutive forwarded chunks, in milliseconds.
    ///
    /// This is what lets the next truncation be diagnosed instead of argued
    /// over: without it the log cannot discriminate a single 26 s gap from a
    /// hung upstream, because both look like "stream ended without [DONE]".
    /// `None` when fewer than two chunks were forwarded, which is "no gap
    /// was observed", never "the gap was zero".
    pub fn max_gap_ms(&self) -> Option<u64> {
        if self.inner.last_chunk_at.lock().is_some() && self.chunk_count() >= 2 {
            Some(self.inner.max_gap_ms.load(Ordering::Relaxed))
        } else {
            None
        }
    }

    /// How the stream ended, as recorded by the adapter. `Clean` when the
    /// upstream closed the stream itself — or when the end was never observed,
    /// which says nothing worse than "no reported error".
    pub fn ended_by(&self) -> StreamEnd {
        StreamEnd::from_discriminant(self.inner.ended_by.load(Ordering::Relaxed))
    }

    /// Record how the stream ended. Called exactly once, by the adapter, on
    /// the terminal poll; a compare-and-swap keeps a late second terminal
    /// (the `Drop` finalize racing a final `poll_next`) from overwriting the
    /// first reading.
    pub(crate) fn record_end(&self, end: StreamEnd) {
        let _ = self.inner.ended_by.compare_exchange(
            StreamEnd::Clean.discriminant(),
            end.discriminant(),
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }

    /// `usage.prompt_tokens` from the upstream's usage frame, if it sent one.
    ///
    /// `None` means the upstream did not report it, which is not a zero: a
    /// caller must not turn "no usage frame" into "0 tokens".
    pub fn prompt_tokens(&self) -> Option<u32> {
        *self.inner.prompt_tokens.lock()
    }

    /// `usage.completion_tokens` from the upstream's usage frame, if any.
    pub fn completion_tokens(&self) -> Option<u32> {
        *self.inner.completion_tokens.lock()
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
            reasoning_chars: self.reasoning_chars(),
            tool_call_count: self.tool_call_count(),
            tool_call_distinct: self.tool_call_distinct(),
            stream_ended_cleanly: self.saw_done(),
            chunk_count: self.chunk_count(),
            body_bytes: self.body_bytes(),
            prompt_tokens: self.prompt_tokens(),
            completion_tokens: self.completion_tokens(),
            max_gap_ms: self.max_gap_ms(),
            ended_by: self.ended_by(),
        }
    }
}

/// The structural outcome of one SSE stream, as observed by an [`SseTap`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SseOutcome {
    pub finish_reason: Option<String>,
    pub content_chars: u64,
    /// Sum of characters in `choices[0].delta.reasoning_content` across
    /// frames. 0 when the upstream emitted no reasoning deltas.
    pub reasoning_chars: u64,
    /// Number of `choices[0].delta.tool_calls` entries seen across frames —
    /// fragment entries, not logical calls. A single call whose `arguments`
    /// stream over many frames counts once per frame here; see
    /// `tool_call_distinct` for the de-duplicated logical-call count.
    pub tool_call_count: usize,
    /// Distinct logical tool calls, de-duplicated across fragment frames.
    ///
    /// Keyed per call so a call whose `arguments` stream over many frames
    /// counts once rather than once per fragment entry. See
    /// [`SseTap::tool_call_distinct`] for the keying rule.
    pub tool_call_distinct: usize,
    /// Defaults to `false`: a stream that was never observed says nothing
    /// about whether it ended cleanly, and the conservative reading is "not
    /// clean".
    pub stream_ended_cleanly: bool,
    pub chunk_count: u64,
    pub body_bytes: u64,
    /// Usage tokens the upstream reported. `None` when it reported none.
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    /// Largest gap between two consecutive forwarded chunks, in milliseconds.
    /// `None` when fewer than two chunks were forwarded: no gap observed.
    pub max_gap_ms: Option<u64>,
    /// How the stream ended: clean EOF, a guard stall, or an upstream error.
    /// Defaults to `Clean`, which for an unobserved stream means "no reported
    /// error" — never evidence of an abort.
    pub ended_by: StreamEnd,
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
                //
                // The cause is classified by the typed stall payload, not by
                // the error kind: an upstream may time out on its own, and
                // only the payload proves the *guard* fired. This is what
                // lets the next truncation answer "guard abort or upstream
                // close" from the log alone, with no packet capture.
                if crate::proxy::streaming::is_stall(&e) {
                    tap.record_end(StreamEnd::GuardStall);
                } else {
                    tap.record_end(StreamEnd::UpstreamError);
                }
                tap.finalize();
                this.ended = true;
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                tap.record_end(StreamEnd::Clean);
                tap.finalize();
                this.ended = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
/// Finalize the tap when the wrapped stream is dropped without being driven to
/// its end — for example when the client disconnects mid-stream or the handler
/// returns without ever polling the body. `finalize` is a single compare-and-
/// swap, so a tap already finalized by `poll_next` is a no-op: the completion
/// record is still written exactly once, regardless of which path ends the
/// stream.
impl Drop for TappedStream {
    fn drop(&mut self) {
        if !self.ended {
            self.tap.finalize();
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
    /// Terminal usage accounting, sent by upstreams the client asked for
    /// `include_usage`. Optional: an upstream that does not report usage is
    /// normal and leaves the token fields unset rather than zeroed.
    #[serde(default)]
    usage: Option<SseUsage>,
}

#[derive(Debug, Deserialize)]
struct SseUsage {
    #[serde(default)]
    prompt_tokens: Option<u32>,
    #[serde(default)]
    completion_tokens: Option<u32>,
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
    /// The key the live NIM SSE frames carry for thinking-model traces (the
    /// proxy's own request sanitizer strips exactly this key from assistant
    /// messages in `messages.rs`, and no other reasoning key appears anywhere
    /// in the repo's frames or fixtures). No `reasoning` alias: a second key
    /// would double-count a frame that carries both.
    #[serde(default)]
    reasoning_content: Option<String>,
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

    fn reasoning_frame(reasoning: &str, finish: &str) -> String {
        format!(
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"reasoning_content\":\"{reasoning}\"}},\"finish_reason\":\"{finish}\"}}]}}\n\n"
        )
    }

    /// A stream that emits reasoning but no content must show up as exactly
    /// that: reasoning chars counted, content chars zero. This is the split
    /// that separates reasoning-abandon (a thinking model that emitted its
    /// whole trace and then stopped) from true-empty (content zero AND
    /// reasoning zero) on a zero-content clean EOF.
    #[tokio::test]
    async fn reasoning_only_stream_counts_reasoning_not_content() {
        let body = format!(
            "{}{}{}data: [DONE]\n\n",
            reasoning_frame("Let me think", ""),
            reasoning_frame(" step by step", ""),
            reasoning_frame("", "stop")
        );
        let outcome = feed(&[&body]).await;
        // "Let me think" + " step by step" = 25 characters; the empty final
        // frame contributes nothing.
        assert_eq!(outcome.reasoning_chars, 25);
        assert_eq!(outcome.content_chars, 0);
        assert_eq!(outcome.finish_reason.as_deref(), Some("stop"));
        assert!(outcome.stream_ended_cleanly);
    }

    /// Content and reasoning accumulate independently: a mixed stream counts
    /// each delta key into its own counter, never cross-contaminating.
    #[tokio::test]
    async fn mixed_content_and_reasoning_accumulate_independently() {
        let body = format!(
            "{}{}{}data: [DONE]\n\n",
            reasoning_frame("thinking", ""),
            frame("answer", ""),
            frame("", "stop")
        );
        let outcome = feed(&[&body]).await;
        assert_eq!(outcome.reasoning_chars, 8);
        assert_eq!(outcome.content_chars, 6);
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
        assert_eq!(outcome.tool_call_distinct, 2);
        assert_eq!(outcome.content_chars, 6);
        assert_eq!(outcome.finish_reason.as_deref(), Some("tool_calls"));
    }

    /// A stream split at a chunk boundary must be observed exactly like the
    /// same stream arriving in one chunk: the incremental split is a buffering
    /// detail, so the tap's state must not depend on how the upstream framed
    /// the bytes.
    #[tokio::test]
    async fn split_feed_matches_single_call_feed() {
        // A frame whose payload is cut mid-line by the first chunk, then a
        // second chunk carrying several complete lines plus a usage frame.
        let first = "data: {\"choices\":[{\"delta\":{\"content\":\"He";
        let rest = "llo\"},\"finish_reason\":\"stop\"}]}\n\n"
            .to_string()
            + "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0}]},\"finish_reason\":\"\"}]}\n\n"
            + "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":11,\"total_tokens\":16}}\n\n"
            + "data: [DONE]\n\n";

        let split = feed(&[first, rest.as_str()]).await;
        let joined = format!("{first}{rest}");
        let whole = feed(&[joined.as_str()]).await;

        assert_eq!(split.prompt_tokens, Some(5));
        assert_eq!(split.completion_tokens, Some(11));
        assert_eq!(split.finish_reason.as_deref(), Some("stop"));
        assert_eq!(split.content_chars, 5);
        assert_eq!(split.tool_call_count, 1);
        assert!(split.stream_ended_cleanly);

        // Chunk counting is the only field the split is allowed to move, so
        // compare every other structural field for identity.
        assert_eq!(split.prompt_tokens, whole.prompt_tokens);
        assert_eq!(split.completion_tokens, whole.completion_tokens);
        assert_eq!(split.finish_reason, whole.finish_reason);
        assert_eq!(split.content_chars, whole.content_chars);
        assert_eq!(split.tool_call_count, whole.tool_call_count);
        assert_eq!(split.stream_ended_cleanly, whole.stream_ended_cleanly);
        assert_eq!(split.body_bytes, whole.body_bytes);
        assert_eq!(split.chunk_count, 2);
        assert_eq!(whole.chunk_count, 1);
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

    /// The terminal `include_usage` frame carries the whole request's token
    /// accounting and an empty `choices`, so it must be read for usage even
    /// though it contributes no content.
    #[tokio::test]
    async fn usage_frame_populates_tokens() {
        let body = format!(
            "{}data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":5,\"completion_tokens\":11,\"total_tokens\":16}}}}\n\ndata: [DONE]\n\n",
            frame("Hello", "stop")
        );
        let outcome = feed(&[&body]).await;
        assert_eq!(outcome.prompt_tokens, Some(5));
        assert_eq!(outcome.completion_tokens, Some(11));
        // Reading the usage frame must not disturb the structural fields.
        assert_eq!(outcome.finish_reason.as_deref(), Some("stop"));
        assert_eq!(outcome.content_chars, 5);
        assert!(outcome.stream_ended_cleanly);
    }

    /// A usage frame split across chunk boundaries is still captured: the
    /// incremental line split is what makes this true, not a special case.
    #[tokio::test]
    async fn usage_frame_split_across_chunks_is_captured() {
        let usage = "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":13,\"total_tokens\":20}}\n\n";
        let bytes = usage.as_bytes();
        for split in 1..usage.len() {
            let a = std::str::from_utf8(&bytes[..split]).unwrap();
            let b = std::str::from_utf8(&bytes[split..]).unwrap();
            let outcome = feed(&[a, b]).await;
            assert_eq!(outcome.prompt_tokens, Some(7), "split at {split}");
            assert_eq!(outcome.completion_tokens, Some(13), "split at {split}");
        }
    }

    /// An upstream that sends no usage is the common case: the fields stay
    /// `None`, never zero, because "not reported" is not "0 tokens".
    #[tokio::test]
    async fn no_usage_frame_leaves_tokens_unset() {
        let outcome = feed(&[&frame("Hello", "stop"), "data: [DONE]\n\n"]).await;
        assert_eq!(outcome.prompt_tokens, None);
        assert_eq!(outcome.completion_tokens, None);
    }

    /// A partial usage object (only `completion_tokens`, which is what some
    /// upstreams send) populates only the field it reported.
    #[tokio::test]
    async fn partial_usage_populates_only_what_was_reported() {
        let body = "data: {\"choices\":[],\"usage\":{\"completion_tokens\":9}}\n\ndata: [DONE]\n\n";
        let outcome = feed(&[body]).await;
        assert_eq!(outcome.prompt_tokens, None);
        assert_eq!(outcome.completion_tokens, Some(9));
    }

    /// A frame that carries usage alongside a real choice (some upstreams put
    /// usage on the final content frame) is read for both.
    #[tokio::test]
    async fn usage_on_a_content_frame_is_also_captured() {
        let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":4,\"total_tokens\":7}}\n\ndata: [DONE]\n\n";
        let outcome = feed(&[body]).await;
        assert_eq!(outcome.prompt_tokens, Some(3));
        assert_eq!(outcome.completion_tokens, Some(4));
        assert_eq!(outcome.content_chars, 4);
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

    /// The gap clock measures what the upstream did between chunks: a 60 ms
    /// pause between two forwarded chunks is recorded as (about) 60 ms.
    ///
    /// The lower bound is generous (half the sleep): a loaded CI host can
    /// only stretch the observed gap, never shrink it below what the
    /// upstream waited, so the assertion that matters is "at least the
    /// pause, modulo scheduling", not an exact value.
    #[tokio::test]
    async fn max_gap_records_the_largest_inter_chunk_pause() {
        let tap = SseTap::new();
        tap.observe(&Bytes::from_static(b"data: {\"choices\":[]}\n\n"));
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        tap.observe(&Bytes::from_static(b"data: {\"choices\":[]}\n\n"));
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        tap.observe(&Bytes::from_static(b"data: [DONE]\n\n"));
        let gap = tap
            .max_gap_ms()
            .expect("three chunks must produce a gap reading");
        assert!(
            (30..500).contains(&gap),
            "the 60 ms pause must dominate the reading, got {gap} ms"
        );
        assert_eq!(tap.outcome().max_gap_ms, Some(gap));
    }

    /// A single chunk has no gap: the wait for the first byte is the TTFB
    /// bound's business, not a mid-stream gap, so it must not be recorded.
    #[tokio::test]
    async fn single_chunk_stream_has_no_gap_reading() {
        let tap = SseTap::new();
        tap.observe(&Bytes::from_static(b"data: [DONE]\n\n"));
        assert_eq!(tap.max_gap_ms(), None);
        assert_eq!(tap.outcome().max_gap_ms, None);
    }

    /// A guard stall and an upstream break are different ends, classified by
    /// the typed payload — not by the error kind, which both share. This is
    /// the record-first half of the RST question: "guard abort or upstream
    /// close" answered from the log alone.
    #[tokio::test]
    async fn tapped_stream_records_guard_stall_vs_upstream_error() {
        use crate::proxy::streaming::{is_stall, stalled_error};

        // Guard stall: the idle guard's error carries the typed payload.
        let tap = SseTap::new();
        let stream = futures::stream::iter(vec![
            Ok(Bytes::from_static(b"data: {\"choices\":[]}\n\n")),
            Err(stalled_error(std::time::Duration::from_millis(100))),
        ]);
        let mut tapped = TappedStream::new(tap.clone(), stream);
        assert!(tapped.next().await.is_some());
        let err = tapped.next().await.unwrap().unwrap_err();
        assert!(is_stall(&err), "test setup must yield a real stall");
        assert_eq!(tap.ended_by(), StreamEnd::GuardStall);
        assert_eq!(tap.outcome().ended_by, StreamEnd::GuardStall);

        // Upstream break: same TimedOut kind, no payload — a different end.
        let tap = SseTap::new();
        let stream = futures::stream::iter(vec![
            Ok(Bytes::from_static(b"data: {\"choices\":[]}\n\n")),
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "upstream timed out on its own",
            )),
        ]);
        let mut tapped = TappedStream::new(tap.clone(), stream);
        assert!(tapped.next().await.is_some());
        let err = tapped.next().await.unwrap().unwrap_err();
        assert!(!is_stall(&err), "test setup must not yield a stall");
        assert_eq!(tap.ended_by(), StreamEnd::UpstreamError);

        // Clean EOF stays clean — and a truncated stream (no [DONE]) can
        // still be clean-ended: the upstream simply stopped sending without
        // breaking the connection.
        let tap = SseTap::new();
        let stream = futures::stream::iter(vec![Ok(Bytes::from_static(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"Comm\"}}]}\n\n",
        ))]);
        let mut tapped = TappedStream::new(tap.clone(), stream);
        assert!(tapped.next().await.is_some());
        assert!(tapped.next().await.is_none());
        assert_eq!(tap.ended_by(), StreamEnd::Clean);
        assert!(!tap.saw_done());
    }
    /// One logical call whose `arguments` stream over three fragment frames
    /// counts as three entries under the pinned `tool_call_count` contract,
    /// but only one distinct logical call under `tool_call_distinct`. This
    /// test fails if the distinct counter is removed.
    #[tokio::test]
    async fn tool_call_fragments_overcount_one_logical_call() {
        let f1 = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"function\":{\"name\":\"f\",\"arguments\":\"{\\\"k\\\":\\\"v\\\"\"}}]}}]}\n\n";
        let f2 = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"function\":{\"arguments\":\"{\\\"k\\\":\\\"w\\\"}\"}}]}}]}\n\n";
        let f3 = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"function\":{\"arguments\":\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n";
        let outcome = feed(&[f1, f2, f3, "data: [DONE]\n\n"]).await;
        assert_eq!(outcome.tool_call_count, 3);
        assert_eq!(outcome.tool_call_distinct, 1);
    }

    /// Two separate calls (index 0 and index 1) with fragments each count
    /// as two distinct calls.
    #[tokio::test]
    async fn distinct_tool_calls_counts_separate_calls() {
        let c0 = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_0\",\"function\":{\"name\":\"f\",\"arguments\":\"{\"}}]}}]}\n\n";
        let c0b = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_0\",\"function\":{\"arguments\":\"}\"}}]}}]}\n\n";
        let c1 = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_1\",\"function\":{\"name\":\"g\",\"arguments\":\"{\"}}]}}]}\n\n";
        let c1b = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_1\",\"function\":{\"arguments\":\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n";
        let outcome = feed(&[c0, c0b, c1, c1b, "data: [DONE]\n\n"]).await;
        assert_eq!(outcome.tool_call_count, 4);
        assert_eq!(outcome.tool_call_distinct, 2);
    }

    /// Two entries with neither `index` nor `id` cannot be joined, so each gets
    /// a unique anonymous key and distinct == 2 — the fallback must never fold
    /// distinct calls together.
    #[tokio::test]
    async fn tool_call_without_joinable_key_is_not_folded() {
        let e1 = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"function\":{\"name\":\"f\",\"arguments\":\"{\"}}]}}]}\n\n";
        let e2 = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"function\":{\"name\":\"g\",\"arguments\":\"{\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n";
        let outcome = feed(&[e1, e2, "data: [DONE]\n\n"]).await;
        assert_eq!(outcome.tool_call_count, 2);
        assert_eq!(outcome.tool_call_distinct, 2);
    }

    /// The exact frame shape a real upstream sends, captured from live traffic:
    /// `id` appears ONLY on the first fragment, and every continuation carries
    /// `index` alone.
    ///
    /// This is what pins the key ordering to `index` before `id`. An id-first
    /// key scores frame 1 as `d:chatcmpl-tool-…` and frames 2..4 — which have no
    /// `id` — as `i:0`, counting ONE logical call as TWO. Neither of the other
    /// tests catches that: one feeds frames that carry no `id` at all (so the
    /// fallback still finds `index`), and the other carries `id` on every frame
    /// (so id-first happens to agree). This is the shape that discriminates, and
    /// it is the shape production actually uses.
    #[tokio::test]
    async fn an_id_only_on_the_first_fragment_does_not_split_the_call() {
        let f1 = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"chatcmpl-tool-82e4ebc410e18260\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"\"}}]}}]}\n\n";
        let f2 = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"command\\\": \\\"echo\"}}]}}]}\n\n";
        let f3 = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\" hello\"}}]}}]}\n\n";
        let f4 = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n";
        let outcome = feed(&[f1, f2, f3, f4, "data: [DONE]\n\n"]).await;
        assert_eq!(
            outcome.tool_call_count, 4,
            "four fragment entries, which is the overcounting figure"
        );
        assert_eq!(
            outcome.tool_call_distinct, 1,
            "one logical call: the continuations must join the first fragment by index"
        );
    }
}
