//! Structured turn logging for observability and analysis.
//! Logs request/response metadata to JSONL format. Message capture is available
//! in the data model, but the proxy path currently writes counts and status only.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// A single message in a conversation
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MessageLog {
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<String>, // JSON string for complex structure
}

/// Current size of `path`, or 0 if it cannot be determined.
///
/// A rotation decision must not fail because of this: the worst case is that
/// the counter starts low and the first file lives a little past its
/// threshold, which is a size overrun, not lost records.
fn path_len(path: &str) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.len())
}

/// Metadata about a completed turn (request/response pair)
///
/// `Deserialize` is derived alongside `Serialize` so the writer's own type can
/// be read back: a record's structural fields can then be verified against the
/// bytes that were actually written, rather than against a separate reader
/// struct that may not carry them.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TurnLog {
    /// ISO8601 timestamp
    pub timestamp: DateTime<Utc>,

    /// Request ID (UUID if provided, else generated)
    pub request_id: Option<String>,

    /// Model that was requested
    pub requested_model: String,

    /// Actual model that responded (after routing)
    pub responding_model: String,

    /// Time to first byte (ms)
    pub latency_ms: u64,

    /// Whether the request succeeded
    pub success: bool,

    /// HTTP status code
    pub status_code: u16,

    /// Number of messages in request
    pub request_message_count: usize,

    /// Number of messages in response  
    pub response_message_count: usize,

    /// Total tokens in request (if available)
    pub request_tokens: Option<u32>,

    /// Total tokens in response (if available)
    pub response_tokens: Option<u32>,

    /// Whether tool calls were present
    pub has_tool_calls: bool,

    /// Tool call count
    ///
    /// REQUEST-SIDE, and it OVERCOUNTS what the model did in this turn: this is
    /// [`request_turn_summary`](crate::proxy::timeouts::request_turn_summary)'s
    /// count of the tool history the request CARRIED — every `role == "tool"`
    /// message plus every entry of every message's `tool_calls` array. Measured
    /// on 2026-10-20 traffic, 27.5% of completions carried tool history while
    /// the response made zero tool calls, and the summed request-side count
    /// overstated the real call count by 63.9x. Any "tool calls per request"
    /// rate computed from this field is wrong by that factor.
    ///
    /// The value is kept exactly as it always was so historical queries stay
    /// comparable; the response-side reading is
    /// [`Self::response_tool_calls`].
    pub tool_call_count: usize,

    /// The REQUEST-side tool-call count under a name that says which side it
    /// is -- the same number as [`Self::tool_call_count`], carried alongside it
    /// so the two sides of the question can be told apart BY NAME rather than
    /// by reading the documentation of a field whose name says the other side.
    ///
    /// It is derived from `tool_call_count` in one place, so the two can never
    /// disagree. `None` on a line written before this field existed.
    ///
    /// **The two sides are not interchangeable on a pre-existing line.** A line
    /// written before this field carries only `tool_call_count`, which is the
    /// REQUEST-side count; its `response_tool_calls` is absent, so it correctly
    /// never satisfies a response-side filter. No pre-existing line is
    /// reinterpreted by this field, and none needs migrating.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_tool_call_count: Option<u64>,

    /// Distinct logical tool calls the RESPONSE made, de-duplicated by the SSE
    /// delta's `index`.
    ///
    /// This is the response-side reading: `Some(0)` when the response made no
    /// tool call, even when the request carried tool history — the "non-call
    /// class" that `tool_call_count` reports as non-zero. It is the only field
    /// from which a per-request tool-call rate can be computed honestly.
    ///
    /// `None` where the response was never observed: first-byte records, the
    /// failure paths, non-streaming turns, and records written before this
    /// field existed. So `Some(0)` is "the response made no tool call" and
    /// `None` is "not observed", which are different answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_tool_calls: Option<u64>,

    /// Error message if failed
    pub error: Option<String>,

    /// Key label used (if available)
    pub key_label: Option<String>,

    /// Whether this was a racing request
    pub is_racing: bool,

    /// Racing context: how many models were raced
    pub racing_models_count: Option<usize>,

    /// Racing context: did this model win the race?
    pub racing_winner: Option<bool>,

    /// Full request messages (redacted if needed)
    pub request_messages: Vec<MessageLog>,

    /// Response message (first choice only)
    pub response_message: Option<MessageLog>,

    /// Which phase of the request this record describes: `ttfb` or
    /// `completion` (see [`Self::RECORD_KIND_TTFB`]).
    ///
    /// Defaults to `ttfb` (not empty) when absent, because every record written
    /// before this field existed was a first-byte record.
    #[serde(default = "default_record_kind")]
    pub record_kind: String,

    // ---- Structural outcome of the forwarded SSE stream ----
    //
    // These fields describe what the stream DID, never what it said. They are
    // all optional so records written before streaming observation existed —
    // and non-streaming turns, which have no stream to observe — stay valid.
    /// Last `finish_reason` the upstream reported, if any.
    #[serde(default)]
    pub finish_reason: Option<String>,

    /// Characters of `delta.content` forwarded to the client.
    #[serde(default)]
    pub content_chars: Option<u64>,

    /// Characters of `delta.reasoning_content` forwarded to the client.
    /// `Some(0)` on a completion record whose stream carried no reasoning
    /// deltas; `None` where the tap never ran (first-byte records,
    /// non-streaming turns, records predating the field). What splits a
    /// zero-content clean EOF into reasoning-abandon
    /// (`content_chars: Some(0)`, `reasoning_chars: Some(n > 0)`) vs
    /// true-empty (content zero AND reasoning zero).
    #[serde(default)]
    pub reasoning_chars: Option<u64>,

    /// Whether the stream reached `data: [DONE]`. `false` means it ended
    /// without one, i.e. it was truncated.
    #[serde(default)]
    pub stream_ended_cleanly: Option<bool>,

    /// Number of chunks forwarded.
    #[serde(default)]
    pub chunk_count: Option<u64>,

    /// Bytes forwarded.
    #[serde(default)]
    pub body_bytes: Option<u64>,

    /// Largest gap between two consecutive forwarded chunks, in milliseconds.
    /// Present only on `completion` records whose stream forwarded at least
    /// two chunks. What lets the next truncation be diagnosed: a completion
    /// with `stream_ended_cleanly: false` and a small `max_inter_chunk_gap_ms`
    /// is a hung upstream (many small gaps, then silence), while one large
    /// gap is a single pathological pause. Absent otherwise — never zero.
    #[serde(default)]
    pub max_inter_chunk_gap_ms: Option<u64>,

    /// How the stream ended: `clean`, `guard_stall`, or `upstream_error`.
    /// Present only on `completion` records. `guard_stall` means the proxy's
    /// own idle guard aborted the stream; `upstream_error` means the upstream
    /// broke the connection itself. Absent on records written before the tap
    /// observed stream ends.
    #[serde(default)]
    pub stream_ended_by: Option<String>,

    /// Named structural classification of the turn, from [`classify`].
    ///
    /// Derived only from the fields above. It never inspects response text:
    /// a classifier that reads content is brittle the moment an upstream
    /// phrases something differently, and a classifier that reads structure
    /// keeps working.
    #[serde(default)]
    pub failure_mode: Option<String>,

    /// TOP-LEVEL `id` of the upstream's response body, present on `completion`
    /// records.
    ///
    /// This is the id the client echoes back as `responseId`, so it is the
    /// only key that joins a client-side TTFT measurement to the proxy's own
    /// completion record for the same turn. Without it the exact join is
    /// impossible: the client's per-message ids and the proxy's log lines have
    /// no shared column.
    ///
    /// Populated on BOTH paths: the streaming tap reads it off the upstream's
    /// SSE frames, and the non-streaming path reads it off the assembled JSON
    /// body. `None` means the upstream sent no top-level id (or the body was
    /// unparsable), which is not the same fact as an empty id.
    ///
    /// **Deliberately absent from the ttfb record.** A first-byte record is
    /// written before any response bytes exist, and on the non-streaming path
    /// the id is only knowable once the whole body has been assembled, so a
    /// ttfb-time field would be `None` on every non-streamed turn. The
    /// completion record is the one that always can carry it, and it shares
    /// `request_id` with its ttfb record, so the pair is still one joinable
    /// turn.
    ///
    /// This is the response-BODY id, never a tool-call id. A tool-call id
    /// (`choices[].delta.tool_calls[].id`, `call_...`) is a different object;
    /// recording it here would populate the field with a value no client ever
    /// echoes, which looks like success while joining to nothing.
    ///
    /// SCOPE: it is populated on the STREAMING completion record only. The
    /// non-streaming path writes a first-byte record but NO completion record
    /// at all, so there is no row there to carry the id - and its first-byte
    /// record is written BEFORE the body is assembled, which is when the id
    /// becomes knowable. The two facts are not interchangeable, so this field
    /// is absent on the non-streaming path rather than populated there.
    ///
    /// The join's population is therefore the STREAMING subset of attempts.
    /// Measured on the 2026-10-21 window, that subset is 3590 of 3603
    /// first-byte records (the other 13 are non-streaming), so the omitted
    /// fraction is 0.36 % - but a match rate must be published beside any
    /// ratio computed over this field, because "no rows" and "no match" are
    /// different facts.
    // Skipped when absent, like the other fields where "not observed" and
    // "observed as absent" are different answers: an absent
    // `response_body_id` says the id was not observed on this record, and
    // writing null would collapse that into the same shape as a field that
    // exists and is empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_body_id: Option<String>,
    /// Which agent loop this record belongs to, if any.
    ///
    /// `None` for ordinary per-request records and for records written before
    /// loop tracking existed. Loop summary records carry a real key so a
    /// reader can filter loop summaries by session.
    #[serde(default)]
    pub loop_key: Option<LoopKey>,

    /// When the loop started, if this record is or relates to a loop summary.
    /// `None` for per-request records.
    #[serde(default)]
    pub loop_started_at: Option<DateTime<Utc>>,

    /// When the loop ended, if this record is or relates to a loop summary.
    /// `None` for per-request records.
    #[serde(default)]
    pub loop_ended_at: Option<DateTime<Utc>>,

    /// Total rounds counted for the loop, if this record is or relates to a
    /// loop summary. `None` for per-request records.
    #[serde(default)]
    pub loop_rounds: Option<u64>,

    /// Rounds that produced a reported outcome, if this record is or relates
    /// to a loop summary. `None` for per-request records.
    #[serde(default)]
    pub loop_completed_rounds: Option<u64>,

    /// Summed upstream model time for the loop, if this record is or relates
    /// to a loop summary. `None` for per-request records.
    #[serde(default)]
    pub loop_model_ms: Option<u64>,

    /// The loop's own first-to-last wall-clock span (what the user waited),
    /// if this record is or relates to a loop summary. `None` for
    /// per-request records. `loop_model_ms` is summed upstream time (what the
    /// model cost); `loop_wall_ms - loop_model_ms` is tool-execution plus
    /// client time, and that difference is the whole reason both are stored.
    #[serde(default)]
    pub loop_wall_ms: Option<u64>,

    /// Rounds that ended in an error, if this record is or relates to a loop
    /// summary. `None` for per-request records.
    #[serde(default)]
    pub loop_error_rounds: Option<u64>,

    /// Tool call fragments forwarded across the loop, if this record is or
    /// relates to a loop summary. `None` for per-request records.
    #[serde(default)]
    pub loop_tool_call_fragments: Option<u64>,

    /// Distinct tool calls across the loop, if this record is or relates to a
    /// loop summary. `None` for per-request records.
    #[serde(default)]
    pub loop_tool_calls_distinct: Option<u64>,

    /// Last finish reason the upstream reported for the loop, if any.
    #[serde(default)]
    pub loop_last_finish_reason: Option<String>,

    /// How the loop ended: `"new_turn"` or `"idle"`, if this record is or
    /// relates to a loop summary. `None` for per-request records.
    #[serde(default)]
    pub loop_ended_by: Option<String>,

    /// Whether the request carried a non-empty top-level `tools` array.
    /// `None` when the key is absent or the body was unparseable, so "no
    /// tools mentioned" stays distinguishable from "explicitly no tools".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_has_tools: Option<bool>,

    /// Effective `max_tokens` for the request: the client value after
    /// model-default injection, NOT the raw client value. `None` when no
    /// integer cap applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_max_tokens: Option<i32>,
}

/// Default `record_kind` for records written before the field existed.
///
/// Those were all first-byte records, so the honest default is `"ttfb"`, not
/// an empty string — an empty kind would make a historical record
/// indistinguishable from a bug.
fn default_record_kind() -> String {
    "ttfb".to_string()
}

/// Structural input for building a `TurnLog` record: the fields the proxy
/// path already knows at first-byte time, passed as named fields instead of
/// an 11-positional-argument list.
///
/// `TurnLog::new` and `log_turn_request` both take this, so the shape is
/// fixed once and every call site reads the same way.
/// A loop's position within a session: which conversation, which agent loop
/// (user turn), and which round within that loop.
///
/// All fields are explicit: there is no `Default`, so every caller must name
/// the session, loop sequence, and round it is addressing. A loop summary
/// record carries `round_index: 0` because it is not a round, and any
/// consumer grouping by round must filter `record_kind=loop` out first.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct LoopKey {
    pub session_id: String,
    pub loop_seq: u64,
    pub round_index: u64,
}
#[derive(Debug, Clone)]
pub struct TurnRecord {
    pub requested_model: String,
    pub responding_model: String,
    pub latency_ms: u64,
    pub success: bool,
    pub status_code: u16,
    pub request_message_count: usize,
    pub response_message_count: usize,
    pub has_tool_calls: bool,
    pub tool_call_count: usize,
    pub key_label: Option<String>,
    pub is_racing: bool,
    pub error: Option<String>,
    pub request_tokens: Option<u32>,
    pub response_tokens: Option<u32>,
    /// Which agent loop this request belongs to, if a session could be
    /// established. `None` means no session id was available for this request,
    /// so it is not part of a tracked loop.
    ///
    /// Failure-path records carry the key too. A failed attempt is part of the
    /// same turn as the attempt that follows it, and this key is what makes
    /// that joinable: `request_id` is unique per RECORD, not per turn, so
    /// without it a stalled attempt could not be attributed to the turn that
    /// recovered from it.
    ///
    /// This does not change how rounds are counted. The tracker's counters
    /// come from [`LoopTracker::begin`] and from the completion path, never
    /// from these records, so a keyed failure record leaves `rounds`,
    /// `completed_rounds` and `error_rounds` exactly as they were.
    pub loop_key: Option<LoopKey>,
    /// Whether the request carried a non-empty top-level `tools` array.
    pub request_has_tools: Option<bool>,
    /// Effective `max_tokens` for the request: the client value after
    /// model-default injection, NOT the raw client value.
    pub request_max_tokens: Option<i32>,
    /// Distinct logical tool calls the RESPONSE made, de-duplicated by the SSE
    /// delta's `index`.
    ///
    /// `None` everywhere the response has not been observed yet: first-byte
    /// records, the failure paths, and non-streaming turns. This is the
    /// response-side reading; `tool_call_count` above is the request-side one.
    pub response_tool_calls: Option<u64>,
    /// TOP-LEVEL id of the upstream's response body - what the client echoes
    /// back as `responseId`. Only ever set on completion records; see
    /// [`TurnLog::response_body_id`] for why the ttfb half cannot carry it.
    pub response_body_id: Option<String>,
}

impl TurnLog {
    /// Which kind of record this is: [`Self::RECORD_KIND_TTFB`],
    /// [`Self::RECORD_KIND_COMPLETION`], or [`Self::RECORD_KIND_LOOP`].
    ///
    /// A streamed turn now produces two records: `ttfb` when the first byte
    /// arrives (what the proxy knew at that instant) and `completion` when the
    /// stream ends (what the stream actually turned out to be). They share a
    /// `request_id`, so a truncated generation can be traced back to the
    /// attempt that produced it.
    ///
    /// # Denominator rule — read this before computing a rate from the log
    ///
    /// The three kinds do not share a denominator, and only one of them can be
    /// one. [`Self::is_request_record`] answers this in code; the reasons are
    /// worth stating, because each mistake yields a plausible number rather
    /// than an error:
    ///
    /// - `ttfb` is written exactly **once per request**, before the outcome is
    ///   known, so it is the only valid denominator for a success or failure
    ///   rate.
    /// - `completion` is written only when a stream *ends*, so it is absent for
    ///   every request that failed. A rate over completions reports 0% failures
    ///   by construction, however many requests failed.
    /// - `loop` is a loop summary written when a user turn ends (see [`LoopKey`],
    ///   and the `loops`/`depth` queries in `log_query`). It is deliberate,
    ///   queryable instrumentation, not accidental output — but it is not a
    ///   model call: empty model, zero messages, zero latency. It is a
    ///   non-denominator, which is a different thing from a defect.
    ///
    /// Because all three share one file, a count over "every record in
    /// turns.jsonl" mixes one denominator with two non-denominators. Measured on
    /// 2026-10-19 traffic that is a ~2x error from the ttfb/completion pair
    /// alone, before any loop records are counted.
    pub const RECORD_KIND_TTFB: &'static str = "ttfb";
    pub const RECORD_KIND_COMPLETION: &'static str = "completion";
    pub const RECORD_KIND_LOOP: &'static str = "loop";

    /// Whether this record was written exactly once per request, and so is a
    /// valid denominator for a success or failure rate.
    ///
    /// True only for `ttfb`. Prefer this to comparing `record_kind` against a
    /// literal in an ad-hoc script, so that a future non-request kind cannot
    /// silently enter a rate computation that was already written.
    pub fn is_request_record(&self) -> bool {
        self.record_kind == Self::RECORD_KIND_TTFB
    }

    pub fn new(record: TurnRecord) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        // A stable identifier for this request, so the turn log can be
        // replayed and a request followed across a retry. Before this the
        // field was always None, so there was no way to ask "what did request
        // X do?": the only joinable column was the timestamp, which collides
        // on a busy gateway. The id is timestamp + pid + a process-local
        // counter, so it is unique per record, sortable by time, and carries
        // no user content and no key material.
        let seq = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let request_id = Some(format!(
            "{}.{}.{}",
            Utc::now().format("%Y%m%d%H%M%S"),
            std::process::id(),
            seq
        ));
        Self {
            timestamp: Utc::now(),
            request_id,
            requested_model: record.requested_model,
            responding_model: record.responding_model,
            latency_ms: record.latency_ms,
            success: record.success,
            status_code: record.status_code,
            request_message_count: record.request_message_count,
            response_message_count: record.response_message_count,
            request_tokens: record.request_tokens,
            response_tokens: record.response_tokens,
            has_tool_calls: record.has_tool_calls,
            tool_call_count: record.tool_call_count,
            error: record.error,
            // Derived from the same source as `tool_call_count`, so the two
            // can never disagree; see the field's documentation.
            request_tool_call_count: Some(record.tool_call_count as u64),
            key_label: record.key_label,
            is_racing: record.is_racing,
            racing_models_count: None,
            racing_winner: None,
            request_messages: Vec::new(),
            response_message: None,
            record_kind: Self::RECORD_KIND_TTFB.to_string(),
            finish_reason: None,
            content_chars: None,
            reasoning_chars: None,
            stream_ended_cleanly: None,
            chunk_count: None,
            body_bytes: None,
            max_inter_chunk_gap_ms: None,
            stream_ended_by: None,
            failure_mode: None,
            loop_key: record.loop_key,
            loop_started_at: None,
            loop_ended_at: None,
            loop_rounds: None,
            loop_completed_rounds: None,
            loop_model_ms: None,
            loop_wall_ms: None,
            loop_error_rounds: None,
            loop_tool_call_fragments: None,
            loop_tool_calls_distinct: None,
            loop_last_finish_reason: None,
            loop_ended_by: None,
            request_has_tools: record.request_has_tools,
            request_max_tokens: record.request_max_tokens,
            response_tool_calls: record.response_tool_calls,
            // Unknowable at first-byte time: on the non-streaming path the id
            // exists only once the body is assembled, and on the streaming
            // path not before the first frame is parsed. `TurnLog::new` builds
            // the ttfb record, so the field starts `None` here and only the
            // completion path fills it in.
            response_body_id: None,
        }
    }
}

/// Named structural classification of a streamed turn.
///
/// Every input is a structural fact about the stream, never its text. The
/// function is pure so the taxonomy can be unit-tested without a proxy, a
/// socket, or an upstream.
///
/// Precedence, most severe first:
///
/// | order | mode | meaning |
/// |---|---|---|
/// | 1 | `truncated_stream` | the stream ended without `[DONE]` |
/// | 2 | `empty_completion` | the upstream said "stop" but sent no content |
/// | 3 | `length_cutoff` | the upstream hit its token limit |
/// | 4 | `missing_finish_reason` | the stream ended but never said why |
/// | 5 | `tool_calls` | the turn ended in tool calls |
/// | 6 | `clean_stop` | a normal completion |
///
/// A stream can satisfy several conditions at once — a truncated stream also
/// has no finish reason — so the precedence above decides which name is
/// reported. The more severe name wins, because it is the one an operator has
/// to act on.
pub fn classify(
    finish_reason: Option<&str>,
    content_chars: u64,
    stream_ended_cleanly: bool,
) -> &'static str {
    if !stream_ended_cleanly {
        // No `[DONE]` means the upstream stopped on its own. This is the
        // failure class that was invisible before: a few mid-word fragments
        // and then silence looked exactly like success.
        return "truncated_stream";
    }
    match finish_reason {
        Some("stop") if content_chars == 0 => "empty_completion",
        Some("length") => "length_cutoff",
        None => "missing_finish_reason",
        Some("tool_calls") => "tool_calls",
        Some(_) => "clean_stop",
    }
}

/// Turn logger with file output
pub struct TurnLogger {
    writer: Mutex<Option<BufWriter<File>>>,
    path: String,
    enabled: bool,
    /// Bytes written to the current file since it was opened, and the
    /// threshold that triggers a rotation.
    ///
    /// Both live behind the writer mutex so the check and the write see the
    /// same value: a rotation decided from a stale byte count can be lost
    /// between concurrent `log` calls. The counter is reset by `rotate`,
    /// which also swaps in a fresh file, so the two can never disagree.
    bytes_since_rotate: Mutex<u64>,
    rotate_at_bytes: u64,
}

impl TurnLogger {
    /// Default threshold: 64 MiB.
    ///
    /// The turn log is the operator's only record of what the proxy did, so
    /// rotation must not discard anything that has not been accounted for —
    /// it moves the file aside, it never truncates. A large threshold keeps
    /// the number of rotated files (and therefore inode churn) low on a
    /// busy host while still bounding disk use.
    pub const DEFAULT_ROTATE_AT_BYTES: u64 = 64 * 1024 * 1024;

    pub fn new(path: &str, enabled: bool) -> io::Result<Self> {
        Self::with_rotate_at(path, enabled, Self::DEFAULT_ROTATE_AT_BYTES)
    }

    /// Build a logger that rotates once the current file reaches
    /// `rotate_at_bytes`. `rotate_at_bytes == 0` disables rotation.
    pub fn with_rotate_at(path: &str, enabled: bool, rotate_at_bytes: u64) -> io::Result<Self> {
        let logger = Self {
            writer: Mutex::new(None),
            path: path.to_string(),
            enabled,
            bytes_since_rotate: Mutex::new(0),
            rotate_at_bytes,
        };

        if enabled {
            logger.open_file()?;
        }

        Ok(logger)
    }

    /// Bytes written to the current file so far.
    pub fn bytes_since_rotate(&self) -> u64 {
        *self.bytes_since_rotate.lock().unwrap()
    }

    /// Whether this logger rotates, and at what size.
    pub fn rotate_threshold(&self) -> Option<u64> {
        (self.rotate_at_bytes > 0).then_some(self.rotate_at_bytes)
    }

    fn open_file(&self) -> io::Result<()> {
        // Create parent directory if needed
        if let Some(parent) = Path::new(&self.path).parent() {
            std::fs::create_dir_all(parent)?;
        }

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;

        // Seed the counter from the file's existing size so a restart does not
        // postpone rotation by a whole threshold: the log is append-only across
        // restarts, so the file on disk already holds everything the counter
        // would otherwise consider unseen.
        let existing = path_len(&self.path).unwrap_or(0);
        *self.bytes_since_rotate.lock().unwrap() = existing;

        *self.writer.lock().unwrap() = Some(BufWriter::new(file));
        Ok(())
    }

    /// Append `turn` to the log and return the request id that was written.
    ///
    /// The id is the one `TurnLog::new` generated for THIS record, returned by
    /// the very write that produced it. A caller that needs to join a later
    /// record to this one — the completion record joining to the first-byte
    /// record — uses the returned value rather than reading a shared slot: a
    /// process-global "last written id" is the last id written by ANY caller,
    /// so two concurrent requests can each read the other's id, which joins a
    /// turn's completion to another turn's first byte and echoes a foreign id
    /// back to the client.
    ///
    /// `None` means no record was written, because logging is disabled.
    pub fn log(&self, turn: &TurnLog) -> io::Result<Option<String>> {
        if !self.enabled {
            return Ok(None);
        }

        // Serialize first so the byte count is exact rather than estimated:
        // the counter is what decides rotation, so it must match what the
        // file actually contains.
        let mut buf = Vec::new();
        {
            serde_json::to_writer(&mut buf, turn)?;
            buf.push(b'\n');
        }

        // The writer mutex must NOT be held while calling `open_file`.
        //
        // Holding it used to deadlock: `log` locked `self.writer`, found it
        // `None` (e.g. after `rotate`), and called `open_file` which locks
        // `self.writer` again — `std::sync::Mutex` is not reentrant, so the
        // second lock blocked forever on the first one's guard. The re-lock on
        // the line after was unreachable.
        //
        // The consequence was not a crash but a wedged thread: after any
        // rotation the next `log` call hung the caller instead of writing, so
        // turn logging stopped with no error at all — the same silent shape as
        // a permission failure.
        let needs_open = {
            let guard = self.writer.lock().unwrap();
            guard.is_none()
        };
        if needs_open {
            self.open_file()?;
        }

        let mut writer_guard = self.writer.lock().unwrap();
        if let Some(writer) = writer_guard.as_mut() {
            writer.write_all(&buf)?;
            writer.flush()?;
        }
        // The id is handed back only once its record is on disk. An id
        // returned for a record that was never written would be worse than no
        // id at all: the completion record would claim a join that does not
        // exist.

        // Rotate once this file is over the threshold. The check is after the
        // write (not before) so the file that triggered it keeps the record
        // that pushed it over rather than losing it on the next call.
        //
        // `rotate` swaps the writer and renames the file, and it is called
        // without the writer guarded here — it takes the lock itself. This is
        // the same non-reentrancy discipline as `open_file` above: holding the
        // writer across `rotate` would deadlock.
        if self.rotate_at_bytes > 0 {
            let mut bytes = self.bytes_since_rotate.lock().unwrap();
            *bytes += buf.len() as u64;
            if *bytes >= self.rotate_at_bytes {
                drop(bytes);
                drop(writer_guard);
                self.rotate()?;
            }
        }

        Ok(turn.request_id.clone())
    }

    pub fn rotate(&self) -> io::Result<()> {
        // Close current file
        {
            let mut writer_guard = self.writer.lock().unwrap();
            *writer_guard = None;
        }

        // Move the current file aside, taking care not to clobber a file that
        // is already there.
        //
        // A plain `rename` onto a second-granularity timestamp silently
        // destroys the earlier file whenever two rotations land in the same
        // second — which is exactly what happens on a fast sequence of small
        // rotations. The turn log is the operator's only record of what the
        // proxy did, so "deleted the previous rotation's records" is not an
        // acceptable cost. Instead, walk a counter until an unused name is
        // found; the first rotation of a second still gets the clean name.
        let path = Path::new(&self.path);
        let timestamp = Utc::now().format("%Y%m%d_%H%M%S");
        if path.exists() {
            let base = format!("{}.{}", path.display(), timestamp);
            let mut candidate = base.clone();
            let mut n = 1u32;
            while Path::new(&candidate).exists() {
                candidate = format!("{base}.{n}");
                n += 1;
            }
            std::fs::rename(path, &candidate)?;
        }

        // Reset the counter: the file that just moved aside is accounted for
        // separately from now on. Without this the fresh file inherits the
        // old file's byte count and rotates again on the very next record.
        *self.bytes_since_rotate.lock().unwrap() = 0;

        // Open fresh file. `open_file` re-seeds the counter from the new
        // file's size, which is 0 because the rotation just moved the old
        // content aside — so the reset above and the seed agree.
        self.open_file()
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

/// Global turn logger instance
static GLOBAL_LOGGER: OnceLock<TurnLogger> = OnceLock::new();

/// Initialize the global turn logger
pub fn init_logger(path: &str, enabled: bool) -> io::Result<()> {
    let logger = TurnLogger::new(path, enabled)?;
    GLOBAL_LOGGER.set(logger).map_err(|_| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "turn logger is already initialized",
        )
    })
}

/// Log a turn to the global logger.
///
/// Returns the request id the global logger wrote, so a caller that needs to
/// join a later record to this one can obtain its OWN id rather than reading a
/// shared "last written" slot (which under concurrency is another request's).
/// `None` means nothing was written: no global logger, or logging is disabled.
pub fn log_turn(turn: &TurnLog) -> Option<String> {
    if let Some(logger) = GLOBAL_LOGGER.get() {
        logger.log(turn).ok().flatten()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_turn_log_creation() {
        let turn = TurnLog::new(TurnRecord {
            requested_model: "auto".to_string(),
            responding_model: "mistralai/mistral-medium-3.5-128b".to_string(),
            latency_ms: 742,
            success: true,
            status_code: 200,
            request_message_count: 5,
            response_message_count: 1,
            has_tool_calls: false,
            tool_call_count: 0,
            key_label: Some("test".to_string()),
            is_racing: true,
            error: None,
            request_tokens: None,
            loop_key: None,
            response_tokens: None,
            request_has_tools: None,
            request_max_tokens: None,
            response_tool_calls: None,
            // Not known on any path that has not observed the response body:
            // ttfb and failure records, and the non-streaming body id is filled
            // in where the body is assembled.
            response_body_id: None,
        });

        assert_eq!(turn.requested_model, "auto");
        assert_eq!(turn.responding_model, "mistralai/mistral-medium-3.5-128b");
        assert_eq!(turn.latency_ms, 742);
        assert!(turn.success);
        assert!(!turn.has_tool_calls);
    }

    /// Only `ttfb` may serve as a rate denominator: it is the one kind written
    /// exactly once per request. `completion` is absent on failure and `loop` is
    /// not a model call, so a rate computed over either is wrong in a direction
    /// that looks plausible rather than failing loudly. This is pinned because
    /// adding a future record kind without regard to the rule is exactly how a
    /// measurement drifts while continuing to look well-formed.
    #[test]
    fn test_only_ttfb_may_be_a_rate_denominator() {
        fn record() -> TurnRecord {
            TurnRecord {
                requested_model: "auto".to_string(),
                responding_model: "m".to_string(),
                latency_ms: 10,
                success: true,
                status_code: 200,
                request_message_count: 1,
                response_message_count: 1,
                has_tool_calls: false,
                tool_call_count: 0,
                key_label: Some("test".to_string()),
                is_racing: false,
                error: None,
                request_tokens: None,
                loop_key: None,
                response_tokens: None,
                request_has_tools: None,
                request_max_tokens: None,
                response_tool_calls: None,
                // Not known on any path that has not observed the response body:
                // ttfb and failure records, and the non-streaming body id is filled
                // in where the body is assembled.
                response_body_id: None,
            }
        }

        // A fresh record is a request record: it is written before the outcome
        // is known, which is precisely what makes it usable as a denominator.
        let ttfb = TurnLog::new(record());
        assert_eq!(ttfb.record_kind, TurnLog::RECORD_KIND_TTFB);
        assert!(ttfb.is_request_record());

        // A completion exists only once a stream ends, so it is missing for
        // every failed request and cannot be a denominator.
        let mut completion = TurnLog::new(record());
        completion.record_kind = TurnLog::RECORD_KIND_COMPLETION.to_string();
        assert!(
            !completion.is_request_record(),
            "completion is absent on failure and must not be a denominator"
        );

        // A loop summary is deliberate instrumentation, but it is not a call.
        let mut loop_summary = TurnLog::new(record());
        loop_summary.record_kind = TurnLog::RECORD_KIND_LOOP.to_string();
        assert!(
            !loop_summary.is_request_record(),
            "a loop summary is not a model call and must not be a denominator"
        );

        // Records written before the field existed deserialize as ttfb and are
        // genuine request records, so the legacy path must stay a denominator.
        let fresh = TurnLog::new(record());
        let mut value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&fresh).unwrap()).unwrap();
        value.as_object_mut().unwrap().remove("record_kind");
        let legacy: TurnLog = serde_json::from_value(value).unwrap();
        assert_eq!(legacy.record_kind, TurnLog::RECORD_KIND_TTFB);
        assert!(legacy.is_request_record());
    }

    /// The request id must be populated and unique, otherwise the log cannot
    /// be replayed and a retry cannot be followed across attempts. Before this
    /// change the field was always None.
    #[test]
    fn test_request_id_is_populated_and_unique() {
        let a = TurnLog::new(TurnRecord {
            requested_model: "auto".to_string(),
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
            loop_key: None,
            response_tokens: None,
            request_has_tools: None,
            request_max_tokens: None,
            response_tool_calls: None,
            // Not known on any path that has not observed the response body:
            // ttfb and failure records, and the non-streaming body id is filled
            // in where the body is assembled.
            response_body_id: None,
        });
        let b = TurnLog::new(TurnRecord {
            requested_model: "auto".to_string(),
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
            loop_key: None,
            response_tokens: None,
            request_has_tools: None,
            request_max_tokens: None,
            response_tool_calls: None,
            // Not known on any path that has not observed the response body:
            // ttfb and failure records, and the non-streaming body id is filled
            // in where the body is assembled.
            response_body_id: None,
        });
        assert!(a.request_id.is_some(), "request_id must be populated");
        assert!(b.request_id.is_some(), "request_id must be populated");
        assert_ne!(
            a.request_id, b.request_id,
            "two turns must not share an id, or a retry cannot be told apart"
        );
    }

    #[test]
    fn test_turn_log_with_messages() {
        let mut turn = TurnLog::new(TurnRecord {
            requested_model: "auto".to_string(),
            responding_model: "qwen/qwen3.5".to_string(),
            latency_ms: 891,
            success: true,
            status_code: 200,
            request_message_count: 3,
            response_message_count: 1,
            has_tool_calls: true,
            tool_call_count: 2,
            key_label: None,
            is_racing: false,
            error: None,
            request_tokens: None,
            loop_key: None,
            response_tokens: None,
            request_has_tools: None,
            request_max_tokens: None,
            response_tool_calls: None,
            // Not known on any path that has not observed the response body:
            // ttfb and failure records, and the non-streaming body id is filled
            // in where the body is assembled.
            response_body_id: None,
        });

        turn.request_messages = vec![
            MessageLog {
                role: "user".to_string(),
                content: "Hello".to_string(),
                tool_call_id: None,
                tool_calls: None,
            },
            MessageLog {
                role: "assistant".to_string(),
                content: "Hi there!".to_string(),
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        turn.response_message = Some(MessageLog {
            role: "assistant".to_string(),
            content: "How can I help?".to_string(),
            tool_call_id: None,
            tool_calls: None,
        });

        assert_eq!(turn.request_messages.len(), 2);
        assert_eq!(turn.request_messages[0].role, "user");
        assert!(turn.response_message.is_some());
    }

    #[test]
    fn test_logger_initially_disabled() {
        let logger = TurnLogger::new("/tmp/test_turns.jsonl", false).unwrap();
        assert!(!logger.is_enabled());
    }

    #[test]
    fn test_turn_log_serialization() {
        let turn = TurnLog::new(TurnRecord {
            requested_model: "auto".to_string(),
            responding_model: "test-model".to_string(),
            latency_ms: 100,
            success: true,
            status_code: 200,
            request_message_count: 2,
            response_message_count: 1,
            has_tool_calls: false,
            tool_call_count: 0,
            key_label: Some("key1".to_string()),
            is_racing: false,
            error: None,
            request_tokens: None,
            loop_key: None,
            response_tokens: None,
            request_has_tools: None,
            request_max_tokens: None,
            response_tool_calls: None,
            // Not known on any path that has not observed the response body:
            // ttfb and failure records, and the non-streaming body id is filled
            // in where the body is assembled.
            response_body_id: None,
        });

        let json = serde_json::to_string(&turn).unwrap();
        assert!(json.contains("test-model"));
        assert!(json.contains("requested_model"));
    }
    /// A `TurnRecord` with `loop_key: None` yields a `TurnLog` whose
    /// `loop_key` is `None`: the legacy path stays valid and old records keep
    /// reading back without a loop association.
    #[test]
    fn test_loop_key_none_is_legacy_path() {
        let turn = TurnLog::new(TurnRecord {
            requested_model: "auto".to_string(),
            responding_model: "test-model".to_string(),
            latency_ms: 100,
            success: true,
            status_code: 200,
            request_message_count: 2,
            response_message_count: 1,
            has_tool_calls: false,
            tool_call_count: 0,
            key_label: Some("key1".to_string()),
            is_racing: false,
            error: None,
            request_tokens: None,
            loop_key: None,
            response_tokens: None,
            request_has_tools: None,
            request_max_tokens: None,
            response_tool_calls: None,
            // Not known on any path that has not observed the response body:
            // ttfb and failure records, and the non-streaming body id is filled
            // in where the body is assembled.
            response_body_id: None,
        });
        assert_eq!(turn.loop_key, None);
    }

    /// The three tool-call fields name three different things, and a
    /// pre-existing line must not be reinterpreted by any of them.
    ///
    /// `tool_call_count` is the REQUEST-side count and keeps its historical
    /// meaning on every line, old and new. `request_tool_call_count` is the
    /// same number under a name that says which side it is.
    /// `response_tool_calls` is the RESPONSE-side count, and it is absent
    /// wherever the response was never observed.
    #[test]
    fn the_tool_call_fields_name_different_sides_and_keep_history_readable() {
        let record = TurnRecord {
            requested_model: "auto".to_string(),
            responding_model: "m".to_string(),
            latency_ms: 1,
            success: true,
            status_code: 200,
            request_message_count: 1,
            response_message_count: 1,
            has_tool_calls: true,
            tool_call_count: 6,
            key_label: None,
            is_racing: false,
            error: None,
            request_tokens: None,
            response_tokens: None,
            loop_key: None,
            request_has_tools: None,
            request_max_tokens: None,
            response_tool_calls: None,
            // Not known on any path that has not observed the response body:
            // ttfb and failure records, and the non-streaming body id is filled
            // in where the body is assembled.
            response_body_id: None,
        };
        let turn = TurnLog::new(record);
        // The legacy field keeps its meaning and its type: it is the request's
        // tool history, and a new line carries it exactly as an old one did.
        assert_eq!(turn.tool_call_count, 6);
        // The alias is the same number, and it cannot disagree with the field
        // it mirrors because it is derived from it.
        assert_eq!(turn.request_tool_call_count, Some(6));
        // The response side is unknown until the response is observed.
        assert_eq!(turn.response_tool_calls, None);

        // Reading a line written before this change is the reader's job, and
        // the reader is the query binary's own copy of this schema -- not this
        // struct, which is the writer's and requires every field. The
        // compatibility guarantee is pinned there, in log_query.rs.
    }

    /// A `TurnRecord` carrying a real `LoopKey` round-trips through
    /// serialization: serialization keeps `session_id`, `loop_seq`, and
    /// `round_index`, and deserialization reads them back.
    #[test]
    fn test_loop_key_round_trips() {
        let key = LoopKey {
            session_id: "sess-1".to_string(),
            loop_seq: 3,
            round_index: 2,
        };
        let turn = TurnLog::new(TurnRecord {
            requested_model: "auto".to_string(),
            responding_model: "test-model".to_string(),
            latency_ms: 100,
            success: true,
            status_code: 200,
            request_message_count: 2,
            response_message_count: 1,
            has_tool_calls: false,
            tool_call_count: 0,
            key_label: None,
            is_racing: false,
            error: None,
            request_tokens: None,
            loop_key: Some(key.clone()),
            response_tokens: None,
            request_has_tools: None,
            request_max_tokens: None,
            response_tool_calls: None,
            // Not known on any path that has not observed the response body:
            // ttfb and failure records, and the non-streaming body id is filled
            // in where the body is assembled.
            response_body_id: None,
        });
        let json = serde_json::to_string(&turn).unwrap();
        let back: TurnLog = serde_json::from_str(&json).unwrap();
        assert_eq!(back.loop_key, Some(key));
    }

    /// The write path itself, previously 0% covered.
    ///
    /// Turn logging is the operator's only view of what the proxy did, and it
    /// has been silently off in the past (permission errors on the container
    /// user). These tests pin the behaviour that must hold: an enabled logger
    /// writes a complete JSON line, a disabled logger writes nothing at all,
    /// and a logger whose writer was rotated away reopens and continues.
    fn sample_turn() -> TurnLog {
        TurnLog::new(TurnRecord {
            requested_model: "auto".to_string(),
            responding_model: "test/model".to_string(),
            latency_ms: 1234,
            success: true,
            status_code: 200,
            request_message_count: 3,
            response_message_count: 1,
            has_tool_calls: false,
            tool_call_count: 0,
            key_label: Some("key-a".to_string()),
            is_racing: false,
            error: None,
            request_tokens: None,
            loop_key: None,
            response_tokens: None,
            request_has_tools: None,
            request_max_tokens: None,
            response_tool_calls: None,
            // Not known on any path that has not observed the response body:
            // ttfb and failure records, and the non-streaming body id is filled
            // in where the body is assembled.
            response_body_id: None,
        })
    }

    #[test]
    fn test_enabled_logger_writes_one_json_line_per_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("turns.jsonl");
        let logger = TurnLogger::new(path.to_str().unwrap(), true).unwrap();

        logger.log(&sample_turn()).unwrap();
        logger.log(&sample_turn()).unwrap();
        drop(logger);

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "one line per record, got {lines:?}");
        // Every line must be a complete, parseable JSON object.
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["requested_model"], "auto");
            assert_eq!(v["status_code"], 200);
        }
    }

    #[test]
    fn test_disabled_logger_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("turns.jsonl");
        let logger = TurnLogger::new(path.to_str().unwrap(), false).unwrap();

        // Must succeed silently rather than erroring, and must not create a file.
        logger.log(&sample_turn()).unwrap();
        assert!(
            !path.exists(),
            "a disabled logger must not create any file, found {:?}",
            path
        );
    }

    #[test]
    fn test_logger_reopens_after_writer_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("turns.jsonl");
        let logger = TurnLogger::new(path.to_str().unwrap(), true).unwrap();
        logger.log(&sample_turn()).unwrap();

        // Simulate rotation tearing down the writer: the next log must reopen
        // rather than silently dropping the record.
        *logger.writer.lock().unwrap() = None;
        logger.log(&sample_turn()).unwrap();
        drop(logger);

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            content.lines().count(),
            2,
            "record after reopen must be persisted, not lost"
        );
    }

    #[test]
    fn test_logger_appends_never_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("turns.jsonl");
        // Pre-existing content must survive.
        std::fs::write(&path, b"existing line\n").unwrap();

        let logger = TurnLogger::new(path.to_str().unwrap(), true).unwrap();
        logger.log(&sample_turn()).unwrap();
        drop(logger);

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.starts_with("existing line\n"),
            "append mode must preserve prior content, got {content:?}"
        );
        assert_eq!(content.lines().count(), 2);
    }

    #[test]
    fn test_logger_survives_unwritable_directory_and_reports_it() {
        // A logger pointed at an impossible path must FAIL LOUDLY rather than
        // silently disabling itself — the failure mode that hid a 12-day
        // logging outage in the deployed container.
        let bad = "/proc/definitely/not/writable/turns.jsonl";
        let result = TurnLogger::new(bad, true);
        assert!(
            result.is_err(),
            "an unwritable log path must return an error, not a silent no-op"
        );
    }

    /// Every `turns.jsonl*` file in `dir`, with its line count.
    ///
    /// This is the assertion surface for rotation: the invariant is that the
    /// records are still somewhere in the directory, not in which file.
    fn records_in_dir(dir: &Path) -> Vec<(String, usize)> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("turns") {
                continue;
            }
            let content = std::fs::read_to_string(entry.path()).unwrap_or_default();
            out.push((name, content.lines().count()));
        }
        out.sort();
        out
    }

    #[test]
    fn test_rotation_fires_at_threshold_and_preserves_every_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("turns.jsonl");

        // A tiny threshold so a handful of records crosses it several times.
        let logger = TurnLogger::with_rotate_at(path.to_str().unwrap(), true, 600).unwrap();

        const RECORDS: usize = 25;
        for _ in 0..RECORDS {
            logger.log(&sample_turn()).unwrap();
        }
        drop(logger);

        let files = records_in_dir(dir.path());
        // One rotated file per crossing, plus the current live file.
        assert!(
            files.len() > 1,
            "a 25-record run at a 600-byte threshold must rotate, got {files:?}"
        );

        let total: usize = files.iter().map(|(_, n)| n).sum();
        assert_eq!(
            total, RECORDS,
            "rotation moves files aside, it must never drop a record: {files:?}"
        );

        // Every surviving line must still be a complete, parseable record.
        for (name, _) in &files {
            let content = std::fs::read_to_string(dir.path().join(name)).unwrap();
            for line in content.lines() {
                let v: serde_json::Value = serde_json::from_str(line)
                    .unwrap_or_else(|e| panic!("{name} has a corrupt line after rotation: {e}"));
                assert_eq!(v["requested_model"], "auto");
            }
        }
    }

    #[test]
    fn test_record_that_crosses_threshold_is_not_lost() {
        // The boundary case: the write that pushes the file over the limit
        // must land in the file it belongs to, not be deferred into the next
        // one and dropped.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("turns.jsonl");

        let one = serde_json::to_vec(&sample_turn()).unwrap().len() as u64 + 1;
        // Threshold exactly one record wide: every log call rotates.
        let logger = TurnLogger::with_rotate_at(path.to_str().unwrap(), true, one).unwrap();
        const RECORDS: usize = 3;
        for _ in 0..RECORDS {
            logger.log(&sample_turn()).unwrap();
        }
        drop(logger);

        let total: usize = records_in_dir(dir.path()).iter().map(|(_, n)| n).sum();
        assert_eq!(
            total, RECORDS,
            "the record that crosses the threshold must be written, not lost"
        );
    }

    #[test]
    fn test_rotation_can_be_disabled() {
        // `rotate_at_bytes == 0` must mean "never rotate", not "rotate
        // immediately" (which would be the reading if 0 were compared
        // directly against the byte counter, since 0 >= 0).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("turns.jsonl");

        let logger = TurnLogger::with_rotate_at(path.to_str().unwrap(), true, 0).unwrap();
        for _ in 0..25 {
            logger.log(&sample_turn()).unwrap();
        }
        assert_eq!(logger.rotate_threshold(), None);
        drop(logger);

        let files = records_in_dir(dir.path());
        assert_eq!(
            files,
            vec![("turns.jsonl".to_string(), 25)],
            "rotation disabled must keep every record in the live file, got {files:?}"
        );
    }

    #[test]
    fn test_preexisting_log_does_not_postpone_rotation() {
        // A restart into a log that is already over the threshold must rotate
        // on the first record rather than granting a fresh threshold. The file
        // is append-only across restarts, so its size is the honest starting
        // byte count.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("turns.jsonl");

        // 800 bytes of pre-existing content, threshold 600.
        std::fs::write(&path, vec![b'x'; 800]).unwrap();
        let logger = TurnLogger::with_rotate_at(path.to_str().unwrap(), true, 600).unwrap();
        assert!(
            logger.bytes_since_rotate() >= 600,
            "the existing file's size must seed the counter, got {}",
            logger.bytes_since_rotate()
        );

        logger.log(&sample_turn()).unwrap();
        drop(logger);

        // The pre-existing content must have been moved aside, not overwritten.
        let files = records_in_dir(dir.path());
        let rotated = files.iter().find(|(n, _)| n != "turns.jsonl");
        assert!(
            rotated.is_some(),
            "an over-threshold file must rotate on first write, got {files:?}"
        );
        let moved = std::fs::read_to_string(dir.path().join(&rotated.unwrap().0)).unwrap();
        // The rotated file holds the original 800 bytes plus the record that
        // triggered the rotation: `log` writes first and rotates after, so the
        // record lands in the file it was written to rather than being lost.
        let original = &moved[..800];
        assert!(
            original.chars().all(|c| c == 'x'),
            "rotation must move the existing content aside intact, got {original:?}"
        );
        assert!(
            moved.contains("requested_model"),
            "the triggering record must be in the rotated file too, got {moved:?}"
        );
    }

    /// Every arm of the failure taxonomy, and the precedence rules that decide
    /// which name wins when a stream satisfies more than one.
    #[test]
    fn test_classify_every_arm_and_precedence() {
        // A normal completion.
        assert_eq!(classify(Some("stop"), 40, true), "clean_stop");
        // An upstream that reported "stop" but forwarded nothing.
        assert_eq!(classify(Some("stop"), 0, true), "empty_completion");
        // An upstream that hit its token limit.
        assert_eq!(classify(Some("length"), 500, true), "length_cutoff");
        // A turn that ended in tool calls.
        assert_eq!(classify(Some("tool_calls"), 0, true), "tool_calls");
        // A stream that ended but never said why.
        assert_eq!(classify(None, 40, true), "missing_finish_reason");
        // A stream that ended without [DONE]: the failure class that was
        // invisible before this record existed.
        assert_eq!(classify(Some("stop"), 4, false), "truncated_stream");
        assert_eq!(classify(None, 0, false), "truncated_stream");

        // Precedence: truncation beats every other classification, including a
        // finish reason the upstream did manage to send.
        assert_eq!(classify(Some("stop"), 0, false), "truncated_stream");
        assert_eq!(classify(Some("length"), 999, false), "truncated_stream");
        assert_eq!(classify(Some("tool_calls"), 0, false), "truncated_stream");
        // A clean end with an unusual finish reason is a completion, not a
        // failure: only the named conditions are failures.
        assert_eq!(classify(Some("content_filter"), 12, true), "clean_stop");
        // The `stop` + empty-content arm outranks a length cutoff only when
        // the reason really is `stop`; with `length` the cutoff is named.
        assert_eq!(classify(Some("length"), 0, true), "length_cutoff");
    }

    /// The first-byte record and the completion record must share a request id,
    /// or the two halves of a turn cannot be read together.
    ///
    /// The id is obtained from the write's own return value, which is what
    /// makes the join safe under concurrency: each caller gets the id of the
    /// record IT wrote, never another caller's.
    #[test]
    fn test_request_id_is_shared_between_record_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("turns.jsonl");
        let logger = TurnLogger::new(path.to_str().unwrap(), true).unwrap();

        let mut ttfb = sample_turn();
        assert_eq!(ttfb.record_kind, TurnLog::RECORD_KIND_TTFB);
        let shared_id = logger
            .log(&ttfb)
            .unwrap()
            .expect("a written record hands back its id");
        ttfb.request_id = Some(shared_id.clone());

        let mut completion = sample_turn();
        completion.record_kind = TurnLog::RECORD_KIND_COMPLETION.to_string();
        completion.request_id = Some(shared_id.clone());
        completion.finish_reason = Some("stop".to_string());
        completion.content_chars = Some(40);
        completion.stream_ended_cleanly = Some(true);
        completion.chunk_count = Some(12);
        completion.body_bytes = Some(900);
        completion.failure_mode = Some(classify(Some("stop"), 40, true).to_string());
        logger.log(&completion).unwrap();
        drop(logger);

        let body = std::fs::read_to_string(&path).unwrap();
        let records: Vec<TurnLog> = body
            .lines()
            .map(|line| serde_json::from_str(line).expect("every line must deserialize"))
            .collect();
        assert_eq!(records.len(), 2, "a streamed turn writes two records");

        let ttfb_record = records
            .iter()
            .find(|r| r.record_kind == TurnLog::RECORD_KIND_TTFB)
            .expect("the first-byte record must exist");
        let completion_record = records
            .iter()
            .find(|r| r.record_kind == TurnLog::RECORD_KIND_COMPLETION)
            .expect("the completion record must exist");
        assert_eq!(ttfb_record.request_id, completion_record.request_id);
        assert!(ttfb_record.request_id.is_some());

        assert_eq!(completion_record.finish_reason.as_deref(), Some("stop"));
        assert_eq!(completion_record.content_chars, Some(40));
        assert_eq!(completion_record.stream_ended_cleanly, Some(true));
        assert_eq!(completion_record.chunk_count, Some(12));
        assert_eq!(completion_record.body_bytes, Some(900));
        assert_eq!(
            completion_record.failure_mode.as_deref(),
            Some("clean_stop")
        );
    }

    /// A record written before these fields existed — one that simply omits
    /// them — must still deserialize, or adding a column would break every
    /// historical log file.
    #[test]
    fn test_records_without_structural_fields_still_deserialize() {
        let json = r#"{"timestamp":"2026-10-16T00:00:00Z","requested_model":"auto",
            "responding_model":"m","latency_ms":10,"success":true,"status_code":200,
            "has_tool_calls":false,"tool_call_count":0,"error":null,"is_racing":false,
            "request_message_count":1,"response_message_count":1,"request_tokens":null,
            "response_tokens":null,"racing_models_count":null,"racing_winner":null,
            "request_messages":[],"response_message":null}"#;
        let record: TurnLog = serde_json::from_str(json).expect("an old record must still parse");
        assert_eq!(record.finish_reason, None);
        assert_eq!(record.content_chars, None);
        assert_eq!(record.stream_ended_cleanly, None);
        assert_eq!(record.chunk_count, None);
        assert_eq!(record.body_bytes, None);
        assert_eq!(record.failure_mode, None);
        assert_eq!(record.record_kind, TurnLog::RECORD_KIND_TTFB);
    }

    /// The regression test for the turn-log tool-call defect.
    ///
    /// MEASURED DEFECT (2026-10-20, 2345 completion rows): 27.5% of turns
    /// carried tool history in the REQUEST while the RESPONSE made zero tool
    /// calls, and the only per-request field that existed counted the request
    /// side. Those 645 rows logged 58,344 tool calls that did not happen in
    /// their turn, and the summed per-request count overstated the real call
    /// count by 63.9x — so every "tool calls per request" rate derived from
    /// `tool_call_count` was wrong by that factor.
    ///
    /// This test pins the property that was missing: a turn whose history
    /// contains tool messages records 0 response-side calls while still
    /// recording its request-side count.
    #[test]
    fn test_non_call_class_is_zero_on_the_response_side() {
        use crate::proxy::timeouts::request_turn_summary;

        // The agent-loop shape that produced the defect: an earlier turn
        // called tools, so the conversation carries a `tool`-role result
        // message AND an assistant message with a `tool_calls` array.
        let body = br#"{"messages":[
            {"role":"user","content":"what is the weather"},
            {"role":"assistant","content":null,"tool_calls":[
                {"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Oslo\"}"}},
                {"id":"call_2","type":"function","function":{"name":"get_time","arguments":"{}"}}
            ]},
            {"role":"tool","tool_call_id":"call_1","content":"4C and raining"},
            {"role":"tool","tool_call_id":"call_2","content":"14:05"},
            {"role":"user","content":"thanks, summarise that"}
        ]}"#;

        let (message_count, has_tool_calls, tool_call_count) =
            request_turn_summary(&bytes::Bytes::from_static(body));
        // The request side is exactly the 2x-pairing signature measured on
        // live traffic (one assistant `tool_calls` entry plus one `tool`-role
        // message per logical call), and it is non-zero.
        assert_eq!(message_count, 5);
        assert!(has_tool_calls);
        assert_eq!(tool_call_count, 4);

        // The response made no tool call at all, so the completion record's
        // response-side count is 0 — not 4, and not 8.
        let turn = TurnLog::new(TurnRecord {
            requested_model: "auto".to_string(),
            responding_model: "m".to_string(),
            latency_ms: 800,
            success: true,
            status_code: 200,
            request_message_count: message_count,
            response_message_count: 1,
            has_tool_calls,
            tool_call_count,
            key_label: None,
            is_racing: false,
            error: None,
            request_tokens: None,
            response_tokens: None,
            loop_key: None,
            request_has_tools: Some(true),
            request_max_tokens: None,
            response_tool_calls: Some(0),
            // Not known on any path that has not observed the response body:
            // ttfb and failure records, and the non-streaming body id is filled
            // in where the body is assembled.
            response_body_id: None,
        });
        assert_eq!(
            turn.response_tool_calls,
            Some(0),
            "a response that made no tool call must record 0 response-side calls"
        );
        assert!(
            turn.tool_call_count > 0,
            "the request-side field keeps its historical (overcounting) value"
        );

        // And the field is written on the wire, so a log consumer can read it.
        let json = serde_json::to_string(&turn).unwrap();
        assert!(
            json.contains("\"response_tool_calls\":0"),
            "the response-side count must be on the record, got {json}"
        );
    }

    /// A response that made N distinct tool calls records N — not the fragment
    /// count, which overcounts by the number of SSE frames each call's
    /// `arguments` arrived in, and not 2N from any request-side pairing.
    #[test]
    fn test_response_side_records_the_distinct_call_count() {
        // Three logical calls whose arguments stream over 11 fragment frames
        // in total: the tap de-duplicates them by the delta's `index`.
        let distinct = 3u64;
        let fragments = 11u64;

        let turn = TurnLog::new(TurnRecord {
            requested_model: "auto".to_string(),
            responding_model: "m".to_string(),
            latency_ms: 900,
            success: true,
            status_code: 200,
            request_message_count: 1,
            response_message_count: 1,
            has_tool_calls: true,
            tool_call_count: 0,
            key_label: None,
            is_racing: false,
            error: None,
            request_tokens: None,
            response_tokens: None,
            loop_key: None,
            request_has_tools: Some(true),
            request_max_tokens: None,
            response_tool_calls: Some(distinct),
            response_body_id: None,
        });
        assert_eq!(turn.response_tool_calls, Some(3));
        assert_ne!(
            turn.response_tool_calls,
            Some(fragments),
            "the fragment count overcounts logical calls and must not be logged here"
        );
        let json = serde_json::to_string(&turn).unwrap();
        assert!(json.contains("\"response_tool_calls\":3"), "{json}");
    }

    /// A log line written before this field existed — one with no
    /// `response_tool_calls` key — must still deserialize, with the field
    /// `None` rather than defaulted to a value that reads as a real zero.
    #[test]
    fn test_a_line_without_response_tool_calls_deserializes_as_none() {
        let json = r#"{"timestamp":"2026-10-16T00:00:00Z","requested_model":"auto",
            "responding_model":"m","latency_ms":10,"success":true,"status_code":200,
            "has_tool_calls":true,"tool_call_count":6,"error":null,"is_racing":false,
            "request_message_count":3,"response_message_count":1,"request_tokens":null,
            "response_tokens":null,"racing_models_count":null,"racing_winner":null,
            "request_messages":[],"response_message":null,"record_kind":"completion"}"#;
        let record: TurnLog = serde_json::from_str(json).expect("an old line must still parse");
        assert_eq!(
            record.tool_call_count, 6,
            "the request-side field is unchanged, so historical queries still work"
        );
        assert_eq!(
            record.response_tool_calls, None,
            "an absent key means 'not observed', never 0"
        );
    }
}
