//! Session and agent-loop identity.
//!
//! The turn log records one line per HTTP request. An *agent loop* — one user
//! turn — is served by MANY sequential requests: the model asks for tools, the
//! harness runs them, the model asks again, and only the last request in the
//! sequence produces the user-visible answer. Nothing in a per-request record
//! groups those requests, so "how many rounds deep did this turn go, and how
//! long did it take" cannot be answered from the log at all.
//!
//! This module is that grouping. A session is a conversation; a loop is one
//! user turn inside it; a round is one request inside that loop.
//!
//! # How a turn boundary is detected
//!
//! Structurally, never by reading content. A conversation's history grows as
//! the turn proceeds — `user`, then `assistant`/`tool` pairs — so a request
//! whose FINAL message is a `user` message is one that no assistant has
//! answered yet: the beginning of a turn. Every round after the first ends with
//! the client sending back a `tool` result, so only round 1 of a turn carries a
//! trailing user message. That is the whole rule, and it needs no content
//! inspection, no lexical matching, and no model cooperation.
//!
//! # Why rounds are counted at `begin`
//!
//! The proxy writes its error records (504 timeouts, auth failures) from sites
//! that carry no session, so a failed round cannot report itself here. If
//! `rounds` were incremented by [`LoopTracker::record_round`], every failed
//! round would vanish from the depth metric — and with 8.4% of live traffic
//! returning 504, that is the exact number this feature exists to measure.
//! So a round is counted when its request BEGINS, and `record_round` only
//! enriches it. The gap between `rounds` and `completed_rounds` is then, by
//! construction, "rounds that started and never reported".
//!
//! # Why `ended_at` is `last_seen`
//!
//! A loop closed by the idle sweeper sets `ended_at` to the loop's own last
//! activity, not to the moment the sweeper noticed. Stamping the sweep clock
//! would inflate the loop's wall time by up to a full sweep period and corrupt
//! the very number this exists to report.

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::time::Duration;

/// Header a client may set to name its own conversation.
///
/// Preferred over the derived fingerprint because the client knows the real
/// boundary between conversations and the proxy can only infer it.
pub const SESSION_HEADER: &str = "x-nima-session";

/// How a loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopEnd {
    /// The client began another turn in the same session.
    NewTurn,
    /// The idle sweeper closed it after silence; this is the normal end for the
    /// last turn of a session, which has no successor request to close it.
    Idle,
}

/// What one finished round contributed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoundOutcome {
    /// Upstream time for the round, in milliseconds.
    pub model_ms: u64,
    /// Whether the round produced a complete response. A truncated stream is
    /// not a success.
    pub success: bool,
    pub finish_reason: Option<String>,
    /// SSE entries carrying tool-call deltas. Overcounts logical calls.
    pub tool_call_fragments: u64,
    /// Distinct logical tool calls.
    pub tool_calls_distinct: u64,
}

/// A loop that has ended, ready to be written as a `loop` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosedLoop {
    pub session_id: String,
    pub loop_seq: u64,
    pub started_at: DateTime<Utc>,
    /// The loop's own last activity, never the sweep clock. See the module doc.
    pub ended_at: DateTime<Utc>,
    /// Rounds the loop STARTED, counted at entry so that a round which failed
    /// before reporting an outcome is still counted.
    pub rounds: u64,
    /// Rounds that reported an outcome.
    pub completed_rounds: u64,
    /// Summed upstream time across COMPLETED rounds only.
    ///
    /// A loop with `rounds > completed_rounds` hit 504s or hard upstream
    /// failures, and this sum understates its true model cost. That is
    /// deliberate: an unrecorded round's duration is unknown, and inventing one
    /// to make the total look complete would corrupt the metric.
    pub model_ms: u64,
    pub error_rounds: u64,
    pub tool_call_fragments: u64,
    pub tool_calls_distinct: u64,
    pub last_finish_reason: Option<String>,
    pub ended_by: LoopEnd,
}

/// Where a request sits in the session's loop structure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopPosition {
    pub session_id: String,
    pub loop_seq: u64,
    /// 1-based position of the request about to be served, within its loop.
    pub round_index: u64,
    /// Set when this request BEGAN a new loop, carrying the loop that just
    /// ended. The caller should write it immediately: a loop that has ended
    /// must not depend on the NEXT request succeeding in order to be recorded.
    pub closed: Option<ClosedLoop>,
}

/// A loop still in progress.
#[derive(Debug, Clone)]
struct OpenLoop {
    session_id: String,
    loop_seq: u64,
    started_at: DateTime<Utc>,
    /// Last activity. This, not the clock at sweep time, becomes `ended_at`.
    last_seen: DateTime<Utc>,
    rounds: u64,
    completed_rounds: u64,
    model_ms: u64,
    error_rounds: u64,
    tool_call_fragments: u64,
    tool_calls_distinct: u64,
    last_finish_reason: Option<String>,
}

#[derive(Debug)]
struct SessionState {
    /// Highest loop number issued for this session. Monotonic, so a loop id is
    /// never reused even after the sweeper drops the session entry.
    loop_seq: u64,
    open: Option<OpenLoop>,
    /// Last activity of the SESSION, used to drop abandoned entries so the map
    /// cannot grow without bound as clients come and go.
    last_touched: DateTime<Utc>,
}

impl SessionState {
    fn new() -> Self {
        Self {
            loop_seq: 0,
            open: None,
            // "Never touched" rather than now(): the value is overwritten by
            // the `begin` that creates the entry, and a clock read here would
            // make construction non-deterministic under test.
            last_touched: DateTime::<Utc>::MIN_UTC,
        }
    }
}

/// Registry of sessions and their in-progress loops.
pub struct LoopTracker {
    sessions: Mutex<HashMap<String, SessionState>>,
    idle_timeout: Duration,
}

impl LoopTracker {
    pub fn new(idle_timeout: Duration) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            idle_timeout,
        }
    }

    /// Register a request and return its position, opening a new loop when this
    /// request begins a new user turn.
    ///
    /// `last_role` is the role of the request's FINAL message, or `None` when
    /// the body could not be read. `force_new_turn` lets a client that knows
    /// the real boundary assert it.
    ///
    /// This INCREMENTS the loop's round count, because the round has begun. See
    /// the module doc for why that is not deferred to
    /// [`Self::record_round`].
    pub fn begin(
        &self,
        session_id: &str,
        last_role: Option<&str>,
        force_new_turn: bool,
        now: DateTime<Utc>,
    ) -> LoopPosition {
        let mut sessions = self.sessions.lock();
        let state = sessions
            .entry(session_id.to_string())
            .or_insert_with(SessionState::new);

        // A trailing `user` message means no assistant reply to it exists yet,
        // so this request opens a fresh turn.
        //
        // The `completed_rounds > 0` guard is what keeps a duplicate of a
        // still-unanswered first request from closing a loop that has nothing
        // in it. `rounds` cannot serve here: a round is counted at entry, so it
        // is already 1 after the first attempt. The same guard means a turn
        // whose rounds all failed keeps accumulating rather than being closed,
        // which is the honest reading — a failed round produced nothing the
        // user could react to, so their next message is a retry of that turn,
        // not a new one.
        let trailing_user = last_role == Some("user");
        let starts_new_turn = force_new_turn
            || (trailing_user && state.open.as_ref().is_some_and(|l| l.completed_rounds > 0));

        let mut closed = None;
        if starts_new_turn {
            if let Some(finished) = state.open.take() {
                closed = Some(finalize(finished, LoopEnd::NewTurn));
            }
        }

        if state.open.is_none() {
            state.loop_seq += 1;
            let seq = state.loop_seq;
            state.open = Some(OpenLoop {
                session_id: session_id.to_string(),
                loop_seq: seq,
                started_at: now,
                last_seen: now,
                rounds: 0,
                completed_rounds: 0,
                model_ms: 0,
                error_rounds: 0,
                tool_call_fragments: 0,
                tool_calls_distinct: 0,
                last_finish_reason: None,
            });
        }

        state.last_touched = now;
        let open = state
            .open
            .as_mut()
            .expect("an open loop was just ensured above");
        open.rounds += 1;
        open.last_seen = now;

        LoopPosition {
            session_id: session_id.to_string(),
            loop_seq: open.loop_seq,
            round_index: open.rounds,
            closed,
        }
    }

    /// Fold a finished round's outcome into its open loop.
    ///
    /// A no-op when the session has no open loop: that request either belonged
    /// to a session the sweeper has already dropped, or was never tracked. It
    /// must not create one, because a round with no recorded `begin` has no
    /// loop to belong to and inventing one would misreport the depth.
    pub fn record_round(&self, session_id: &str, round: RoundOutcome, now: DateTime<Utc>) {
        let mut sessions = self.sessions.lock();
        let Some(state) = sessions.get_mut(session_id) else {
            return;
        };
        let Some(open) = state.open.as_mut() else {
            return;
        };

        open.completed_rounds += 1;
        open.model_ms = open.model_ms.saturating_add(round.model_ms);
        open.tool_call_fragments = open
            .tool_call_fragments
            .saturating_add(round.tool_call_fragments);
        open.tool_calls_distinct = open
            .tool_calls_distinct
            .saturating_add(round.tool_calls_distinct);
        if !round.success {
            open.error_rounds += 1;
        }
        // Keep the last reason a round actually reported. A round that reported
        // none does not erase the previous one: `None` here means "this round
        // did not say", not "the loop has no reason".
        if round.finish_reason.is_some() {
            open.last_finish_reason = round.finish_reason;
        }
        open.last_seen = now;
        state.last_touched = now;
    }

    /// Close every loop that has been silent for longer than the idle timeout,
    /// and forget sessions that are finished and equally stale.
    ///
    /// Returned order is deterministic (session id, then loop number) so that a
    /// caller writing these records produces a stable log.
    pub fn sweep(&self, now: DateTime<Utc>) -> Vec<ClosedLoop> {
        let timeout = self.idle_timeout;
        // `now` before `t` is clock skew (or a test passing an earlier `now`).
        // Treat it as NOT idle: reading skew as "infinitely old" would close
        // every loop at once.
        let is_idle = |t: DateTime<Utc>| match (now - t).to_std() {
            Ok(elapsed) => elapsed >= timeout,
            Err(_) => false,
        };

        let mut sessions = self.sessions.lock();
        let mut closed = Vec::new();
        let mut abandon = Vec::new();

        for (id, state) in sessions.iter_mut() {
            if state
                .open
                .as_ref()
                .is_some_and(|open| is_idle(open.last_seen))
            {
                if let Some(finished) = state.open.take() {
                    closed.push(finalize(finished, LoopEnd::Idle));
                }
            }
            // A session with nothing open that has been silent beyond the
            // timeout is gone. Dropping it is what keeps the map bounded on a
            // gateway that sees many short-lived clients.
            if state.open.is_none() && is_idle(state.last_touched) {
                abandon.push(id.clone());
            }
        }

        for id in abandon {
            sessions.remove(&id);
        }

        closed.sort_by(|a, b| {
            a.session_id
                .cmp(&b.session_id)
                .then(a.loop_seq.cmp(&b.loop_seq))
        });
        closed
    }

    /// Number of sessions with a loop in progress.
    pub fn open_loops(&self) -> usize {
        self.sessions
            .lock()
            .values()
            .filter(|s| s.open.is_some())
            .count()
    }

    pub fn idle_timeout(&self) -> Duration {
        self.idle_timeout
    }
}

fn finalize(open: OpenLoop, ended_by: LoopEnd) -> ClosedLoop {
    ClosedLoop {
        session_id: open.session_id,
        loop_seq: open.loop_seq,
        started_at: open.started_at,
        // `last_seen`, never the caller's clock: see the module doc.
        ended_at: open.last_seen,
        rounds: open.rounds,
        completed_rounds: open.completed_rounds,
        model_ms: open.model_ms,
        error_rounds: open.error_rounds,
        tool_call_fragments: open.tool_call_fragments,
        tool_calls_distinct: open.tool_calls_distinct,
        last_finish_reason: open.last_finish_reason,
        ended_by,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn t0() -> DateTime<Utc> {
        at("2026-10-18T10:00:00Z")
    }

    fn round(ms: u64, ok: bool) -> RoundOutcome {
        RoundOutcome {
            model_ms: ms,
            success: ok,
            finish_reason: Some(if ok { "stop" } else { "error" }.to_string()),
            tool_call_fragments: 0,
            tool_calls_distinct: 0,
        }
    }

    fn tracker() -> LoopTracker {
        LoopTracker::new(Duration::from_secs(300))
    }

    /// The first request of a conversation opens loop 1 as round 1, and there
    /// is no previous loop to report.
    #[test]
    fn first_request_opens_loop_one() {
        let t = tracker();
        let pos = t.begin("s1", Some("user"), false, t0());
        assert_eq!(pos.loop_seq, 1);
        assert_eq!(pos.round_index, 1);
        assert!(pos.closed.is_none());
    }

    /// A round whose trailing message is a tool result continues the SAME loop:
    /// this is what makes a long agent turn one loop rather than N.
    #[test]
    fn a_tool_result_continues_the_same_loop() {
        let t = tracker();
        t.begin("s1", Some("user"), false, t0());
        let pos = t.begin("s1", Some("tool"), false, t0());
        assert_eq!(pos.loop_seq, 1);
        assert_eq!(pos.round_index, 2);
        assert!(pos.closed.is_none());
    }

    /// A later trailing-user request closes the previous loop and opens the
    /// next, reporting the finished one so the caller can write it.
    #[test]
    fn a_trailing_user_message_closes_the_previous_loop() {
        let t = tracker();
        t.begin("s1", Some("user"), false, t0());
        t.record_round("s1", round(1000, true), t0());
        let pos = t.begin("s1", Some("user"), false, at("2026-10-18T10:05:00Z"));

        assert_eq!(pos.loop_seq, 2);
        assert_eq!(pos.round_index, 1);
        let closed = pos.closed.expect("the finished loop must be reported");
        assert_eq!(closed.loop_seq, 1);
        assert_eq!(closed.rounds, 1);
        assert_eq!(closed.ended_by, LoopEnd::NewTurn);
    }

    /// A client that knows the real turn boundary can assert it even when the
    /// trailing message is not a user message.
    #[test]
    fn force_new_turn_closes_regardless_of_trailing_role() {
        let t = tracker();
        t.begin("s1", Some("user"), false, t0());
        t.record_round("s1", round(10, true), t0());
        let pos = t.begin("s1", Some("tool"), true, t0());
        assert_eq!(pos.loop_seq, 2);
        assert_eq!(
            pos.closed.map(|c| c.loop_seq),
            Some(1),
            "force_new_turn must close the open loop"
        );
    }

    /// A duplicate of the first request (no round completed yet) must not emit
    /// an empty loop, which would inflate the loop count and deflate the mean
    /// depth.
    #[test]
    fn a_duplicate_first_request_does_not_emit_an_empty_loop() {
        let t = tracker();
        t.begin("s1", Some("user"), false, t0());
        let pos = t.begin("s1", Some("user"), false, t0());
        assert!(pos.closed.is_none());
        assert_eq!(pos.loop_seq, 1);
        assert_eq!(pos.round_index, 2);
    }

    /// `record_round` accumulates every counter and never touches `rounds`,
    /// which `begin` owns.
    #[test]
    fn record_round_enriches_without_recounting_rounds() {
        let t = tracker();
        t.begin("s1", Some("user"), false, t0());
        t.record_round(
            "s1",
            RoundOutcome {
                model_ms: 500,
                success: true,
                finish_reason: Some("tool_calls".to_string()),
                tool_call_fragments: 80,
                tool_calls_distinct: 3,
            },
            t0(),
        );
        let closed = t
            .begin("s1", Some("user"), false, t0())
            .closed
            .expect("loop closed");
        assert_eq!(closed.rounds, 1, "begin counts the round, not record_round");
        assert_eq!(closed.completed_rounds, 1);
        assert_eq!(closed.model_ms, 500);
        assert_eq!(closed.tool_call_fragments, 80);
        assert_eq!(closed.tool_calls_distinct, 3);
        assert_eq!(closed.last_finish_reason.as_deref(), Some("tool_calls"));
    }

    /// A round that begins and never reports — the 504 shape — is still
    /// counted, and shows as the gap between the two counters.
    #[test]
    fn a_round_that_never_reported_is_still_counted() {
        let t = tracker();
        t.begin("s1", Some("user"), false, t0());
        t.begin("s1", Some("tool"), false, t0());
        t.record_round("s1", round(200, true), t0());
        let closed = t.begin("s1", Some("user"), false, t0()).closed.unwrap();
        assert_eq!(closed.rounds, 2);
        assert_eq!(closed.completed_rounds, 1);
        assert_eq!(
            closed.model_ms, 200,
            "an unknown duration must not be guessed at"
        );
    }

    /// A failed round is counted as an error round.
    #[test]
    fn a_failed_round_counts_as_an_error_round() {
        let t = tracker();
        t.begin("s1", Some("user"), false, t0());
        t.record_round("s1", round(100, false), t0());
        t.record_round("s1", round(100, true), t0());
        let closed = t.begin("s1", Some("user"), false, t0()).closed.unwrap();
        assert_eq!(closed.error_rounds, 1);
    }

    /// The swept loop reports when it ACTUALLY stopped, not when the sweeper
    /// happened to look, or its wall time would be inflated by the sweep period.
    #[test]
    fn sweep_stamps_the_loops_own_last_activity() {
        let t = tracker();
        t.begin("s1", Some("user"), false, at("2026-10-18T10:00:00Z"));
        t.record_round("s1", round(1000, true), at("2026-10-18T10:00:30Z"));

        // Sweep an hour later: the loop's end must remain 10:00:30.
        let swept = t.sweep(at("2026-10-18T11:00:00Z"));
        assert_eq!(swept.len(), 1);
        assert_eq!(swept[0].ended_by, LoopEnd::Idle);
        assert_eq!(swept[0].ended_at, at("2026-10-18T10:00:30Z"));
        assert_eq!(swept[0].started_at, at("2026-10-18T10:00:00Z"));
    }

    /// A loop still inside the timeout stays open: sweeping early would split
    /// one turn, which is the failure this design most needs to avoid.
    #[test]
    fn sweep_leaves_a_recent_loop_open() {
        let t = tracker();
        t.begin("s1", Some("user"), false, at("2026-10-18T10:00:00Z"));
        let swept = t.sweep(at("2026-10-18T10:04:59Z"));
        assert!(swept.is_empty());
        assert_eq!(t.open_loops(), 1);
    }

    /// Two conversations never share a loop, whatever their interleaving.
    #[test]
    fn sessions_do_not_share_loops() {
        let t = tracker();
        let a = t.begin("s1", Some("user"), false, t0());
        let b = t.begin("s2", Some("user"), false, t0());
        assert_eq!(a.loop_seq, 1);
        assert_eq!(b.loop_seq, 1);
        assert_eq!(t.open_loops(), 2);

        let a2 = t.begin("s1", Some("tool"), false, t0());
        assert_eq!(a2.round_index, 2, "s2's traffic must not advance s1");
        assert_eq!(a2.session_id, "s1");
    }

    /// Sweep output order is stable, so the written log does not depend on
    /// HashMap iteration order.
    #[test]
    fn sweep_order_is_deterministic() {
        let t = tracker();
        for s in ["s3", "s1", "s2"] {
            t.begin(s, Some("user"), false, at("2026-10-18T10:00:00Z"));
        }
        let first = t.sweep(at("2026-10-18T11:00:00Z"));
        let ids: Vec<_> = first.iter().map(|c| c.session_id.as_str()).collect();
        assert_eq!(ids, vec!["s1", "s2", "s3"]);

        // And the same tracker swept again yields nothing, not a repeat.
        assert!(t.sweep(at("2026-10-18T11:00:00Z")).is_empty());
    }

    /// An abandoned session is forgotten, so the map cannot grow without bound.
    #[test]
    fn sweep_forgets_an_abandoned_session() {
        let t = tracker();
        t.begin("s1", Some("user"), false, at("2026-10-18T10:00:00Z"));
        t.sweep(at("2026-10-18T11:00:00Z"));
        // The entry must be gone: a later request starts a fresh loop at 1
        // rather than resuming a conversation the sweeper already closed.
        let pos = t.begin("s1", Some("user"), false, at("2026-10-18T12:00:00Z"));
        assert_eq!(pos.loop_seq, 1);
    }

    /// `record_round` for a session that was never begun is a no-op rather than
    /// a panic or an invented loop.
    #[test]
    fn record_round_without_a_session_is_a_noop() {
        let t = tracker();
        t.record_round("never-seen", round(500, true), t0());
        assert_eq!(t.open_loops(), 0);
    }

    /// A round reporting no finish reason must not erase the previous one.
    #[test]
    fn a_missing_finish_reason_does_not_erase_the_last_one() {
        let t = tracker();
        t.begin("s1", Some("user"), false, t0());
        t.record_round("s1", round(10, true), t0()); // finish_reason: "stop"
        t.record_round(
            "s1",
            RoundOutcome {
                model_ms: 10,
                success: true,
                finish_reason: None,
                tool_call_fragments: 0,
                tool_calls_distinct: 0,
            },
            t0(),
        );
        let closed = t.begin("s1", Some("user"), false, t0()).closed.unwrap();
        assert_eq!(closed.last_finish_reason.as_deref(), Some("stop"));
    }

    /// Clock skew must not read as "infinitely old" and close every loop.
    #[test]
    fn a_sweep_clock_behind_the_loop_does_not_close_it() {
        let t = tracker();
        t.begin("s1", Some("user"), false, at("2026-10-18T10:00:00Z"));
        let swept = t.sweep(at("2026-10-18T09:00:00Z"));
        assert!(swept.is_empty());
    }

    /// The idle timeout the tracker was built with is readable, since the
    /// sweeper's interval is derived from it at startup.
    #[test]
    fn idle_timeout_is_readable() {
        let t = LoopTracker::new(Duration::from_secs(120));
        assert_eq!(t.idle_timeout(), Duration::from_secs(120));
    }
}
