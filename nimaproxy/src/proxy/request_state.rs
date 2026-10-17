//! The request lifecycle as an explicit state machine.
//!
//! The FSM doc (`docs/FORMAL-DESIGN.md`) names five implicit state machines;
//! this is the one that runs per request, and it is the class of defect this
//! session kept shipping: a state that nothing modelled, so nothing guarded
//! its transitions.
//!
//! The transitions that must hold, and what breaks if they do not:
//!
//! ```text
//!   Queued ──permit──> Admitted ──send──> InFlight ──first byte──> Committed (streaming)
//!     │                   │                  │                    on the wire
//!     │                   │                  └──timeout──> Retryable ──┐
//!     │                   │                                            │
//!     │                   └──429/401/403──────────────> Retryable ─────┤
//!     │                   │                                            │
//!     │                   └──timeout──────────────> Retryable ─────────┘
//!     │                                                            │
//!     └──overload/no-key──> Rejected (terminal)                    │
//!                                                                  │
//!                             InFlight ──first byte (buffered)──> Assembling
//!                                                                    │  nothing sent yet
//!                             Assembling ──stall──> Retryable ────────┘  still retryable
//!                             Assembling ──done──> Responded (terminal)
//!
//!                                               Retryable ──retry──> Admitted
//!                                               Retryable ──give up──> Failed
//!
//!                                               Committed ──body──> Responded (terminal)
//!                                               Committed ──stall──> Failed (terminal, no retry)
//! ```
//!
//! The invariants the type guards:
//!
//! - **S3**: a request receives at most one response body. Guarded by the
//!   `Committed` transition: only `Admitted`/`InFlight` may become `Committed`,
//!   and `Committed` may never go back to `Admitted`. A retried request that
//!   had already started streaming would violate this by double-sending.
//! - **L3**: a retry happens only from `Retryable`, and only when the budget
//!   allows. Guarded by `can_retry`.
//! - **L1**: every admitted request reaches a terminal state. Guarded by
//!   `is_terminal` on every transition path.

use std::time::{Duration, Instant};

/// Whether the first byte can reach the client as it arrives.
///
/// This is the fact the state machine needs and used to lack. A streaming
/// response forwards each chunk as it arrives, so once the first byte is seen
/// the client already has part of a response and a retry would double-send.
/// A non-streaming response is *assembled here* and handed to the client only
/// when it is complete, so the same first byte is internal buffering and a
/// retry remains safe.
///
/// Conflating the two is what made a stalled non-streaming body terminal: the
/// FSM committed on a byte the client had never seen, and S3 then refused a
/// retry that could not have double-sent anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseMode {
    /// Chunks are forwarded to the client as they arrive.
    Streaming,
    /// The body is assembled in full before the client sees anything.
    Buffered,
}

/// Where a request is in its lifecycle.
///
/// This is a value, not a control-flow position, so a transition is a typed
/// function call with a pre- and postcondition, not a `continue` or an early
/// `return` that cannot be checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestState {
    /// Waiting for a permit and a key lease.
    Queued,
    /// Holds a permit and a key; not yet sent.
    Admitted,
    /// Sent; waiting for the first byte. Still retryable.
    InFlight,
    /// First byte received and buffered in memory; the client has seen
    /// nothing, so a failure here is still retryable. This is the state a
    /// [`ResponseMode::Buffered`] response occupies while its body is being
    /// assembled.
    Assembling,
    /// First byte received and forwarded to the client; the response is on the
    /// wire. **Never** retryable, and only [`ResponseMode::Streaming`] may
    /// reach this state.
    Committed,
    /// A terminal state reached by rejection at admission.
    Rejected,
    /// A failure before any byte reached the client; may retry.
    Retryable,
    /// A terminal failure. No more attempts.
    Failed,
    /// A terminal success: the response completed.
    Responded,
}

impl RequestState {
    /// Whether this state may receive a first byte.
    ///
    /// S3: a request that has not yet put bytes on the wire may still advance.
    /// `Assembling` qualifies because its bytes are buffered and the client has
    /// seen nothing; only `Committed` — a streaming response already on the
    /// wire — must refuse, because a second commit would double-send.
    pub fn may_commit(&self) -> bool {
        matches!(
            self,
            RequestState::Admitted | RequestState::InFlight | RequestState::Assembling
        )
    }

    /// Whether this state may begin another attempt.
    ///
    /// L3: a retry is only legal from `Retryable`, and only while the retry
    /// budget has room for one more full window. A retry from `Committed` is
    /// exactly the bug that would double-send a response body.
    pub fn can_retry(
        &self,
        attempt: usize,
        n: usize,
        started: Instant,
        window: Duration,
        budget: Duration,
    ) -> bool {
        if !matches!(self, RequestState::Retryable) {
            return false;
        }
        attempt + 1 < n && started.elapsed() + window <= budget
    }

    /// Whether the request has reached a terminal state.
    ///
    /// L1: every admitted request must eventually reach one of these.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            RequestState::Rejected | RequestState::Failed | RequestState::Responded
        )
    }

    /// The transition on an upstream failure, given whether bytes were sent.
    ///
    /// This is the single place that decides retryability. A failure is
    /// `Failed` when the client already has a response — either because bytes
    /// were actually sent, or because the request is `Committed`, which is
    /// only reachable for a streaming response. A failure while `Assembling`
    /// is `Retryable`: the client has seen nothing, so retrying cannot
    /// double-send a body.
    pub fn on_failure(&self, bytes_sent: bool) -> RequestState {
        if bytes_sent || matches!(self, RequestState::Committed) {
            RequestState::Failed
        } else {
            RequestState::Retryable
        }
    }

    /// The transition on the first byte arriving, given the response mode.
    ///
    /// `Streaming` means the byte is already on the wire, so the request
    /// becomes `Committed` and can never be retried (S3). `Buffered` means the
    /// byte is only in memory, so the request becomes `Assembling` and a
    /// later failure is still retryable — nothing has reached the client, so
    /// a retry cannot double-send.
    pub fn on_first_byte(&self, mode: ResponseMode) -> RequestState {
        debug_assert!(
            self.may_commit(),
            "a request in {self:?} cannot receive a first byte; S3 would be violated"
        );
        match mode {
            ResponseMode::Streaming => RequestState::Committed,
            ResponseMode::Buffered => RequestState::Assembling,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// S3: a committed request must never become retryable, or it would
    /// double-send a response body to the client.
    #[test]
    fn test_committed_is_never_retryable() {
        let committed = RequestState::Committed;
        // Any failure after commit is terminal, not retryable.
        assert_eq!(committed.on_failure(true), RequestState::Failed);
        assert_eq!(committed.on_failure(false), RequestState::Failed);
        // And it cannot retry.
        let budget = Duration::from_secs(60);
        assert!(
            !committed.can_retry(0, 8, Instant::now(), Duration::from_secs(15), budget),
            "a committed request that retries would double-send the response body"
        );
    }

    /// L3: a pre-commit failure is retryable only while the budget has room.
    #[test]
    fn test_retryable_respects_the_budget() {
        let r = RequestState::Retryable;
        let window = Duration::from_secs(15);
        let started = Instant::now();
        // Budget covers exactly one more window.
        let budget = window + Duration::from_millis(100);
        assert!(
            r.can_retry(0, 8, started, window, budget),
            "one window fits"
        );
        // Attempts exhausted.
        assert!(
            !r.can_retry(7, 8, started, window, budget),
            "no attempts remain"
        );
    }

    /// S3 must be ENFORCED: a first byte arriving in a non-committable state
    /// (e.g. a retry that already started streaming) is a bug, and the guard
    /// must catch it rather than silently committing again.
    #[test]
    #[should_panic]
    fn test_first_byte_in_a_committed_state_is_a_panic() {
        let committed = RequestState::Committed;
        // A second first byte means a retry double-sent the response body. The
        // mode is irrelevant here: no mode may re-commit a committed request.
        let _ = committed.on_first_byte(ResponseMode::Streaming);
    }

    /// L1: the terminal states are the ones every request must end in.
    #[test]
    fn test_terminal_states_are_the_only_end_states() {
        assert!(RequestState::Rejected.is_terminal());
        assert!(RequestState::Failed.is_terminal());
        assert!(RequestState::Responded.is_terminal());
        assert!(!RequestState::Admitted.is_terminal());
        assert!(!RequestState::InFlight.is_terminal());
        assert!(!RequestState::Committed.is_terminal());
        assert!(!RequestState::Retryable.is_terminal());
        assert!(!RequestState::Assembling.is_terminal());
        assert!(!RequestState::Queued.is_terminal());
    }

    /// The mode decides which state the first byte produces, and that is the
    /// whole point of it: streaming means the byte is already on the wire so
    /// the request is committed; a buffered body means the client has seen
    /// nothing so it is still assembling.
    #[test]
    fn test_first_byte_mode_decides_commit_or_assemble() {
        let inflight = RequestState::InFlight;

        assert_eq!(
            inflight.on_first_byte(ResponseMode::Streaming),
            RequestState::Committed,
            "a streamed byte is on the wire, so the request is committed"
        );
        assert_eq!(
            inflight.on_first_byte(ResponseMode::Buffered),
            RequestState::Assembling,
            "a buffered byte is internal, so the request is still assembling"
        );

        // Same input, different mode, different outcome. Without this the mode
        // parameter could be decoration and this test would not notice.
        assert_ne!(
            inflight.on_first_byte(ResponseMode::Streaming),
            inflight.on_first_byte(ResponseMode::Buffered),
            "the two modes must land in different states"
        );
    }

    /// The regression this state exists for: a stalled non-streaming body was
    /// terminal because the FSM committed on a byte the client never saw.
    #[test]
    fn test_assembling_failure_is_retryable() {
        let assembling = RequestState::Assembling;
        assert_eq!(
            assembling.on_failure(false),
            RequestState::Retryable,
            "a buffered body that stalls has sent the client nothing, so it must be retryable"
        );

        // Model the real call shape from chat.rs: the caller evaluates
        // `on_failure(..).can_retry(..)`, so the retry decision is taken from
        // the state the transition produced, not from `Assembling` itself.
        let budget = Duration::from_secs(60);
        let retryable = assembling.on_failure(false);
        assert!(
            retryable.can_retry(0, 8, Instant::now(), Duration::from_secs(15), budget),
            "and the retry must actually be permitted while the budget allows"
        );

        // The negative arm: what makes the stall terminal is bytes genuinely
        // on the wire, not merely having seen a first byte.
        assert_eq!(
            assembling.on_failure(true),
            RequestState::Failed,
            "once bytes reach the client it is no longer retryable"
        );
    }

    /// S3 stays enforced for the state that can actually double-send.
    #[test]
    fn test_committed_still_refuses_a_retry() {
        let committed = RequestState::Committed;
        assert_eq!(committed.on_failure(true), RequestState::Failed);
        assert_eq!(committed.on_failure(false), RequestState::Failed);
        let budget = Duration::from_secs(60);
        assert!(
            !committed.can_retry(0, 8, Instant::now(), Duration::from_secs(15), budget),
            "a streamed response that retries would double-send to the client"
        );
    }

    /// The retry-then-success path, and the reason the per-attempt reset inside
    /// the retry loop is necessary rather than merely harmless: a retried
    /// request must be able to receive a first byte again, which only works
    /// from a `may_commit()` state.
    #[test]
    fn test_retry_then_first_byte_is_not_a_panic() {
        // Attempt 0 fails before any byte -> Retryable.
        let failed = RequestState::Admitted.on_failure(false);
        assert_eq!(failed, RequestState::Retryable);

        // The caller resets for the next attempt, which is what the loop in
        // chat.rs does; the reset must produce a committable state.
        let retry = RequestState::Admitted;
        assert!(
            retry.may_commit(),
            "a fresh attempt must be able to receive a first byte"
        );

        // Attempt 1 succeeds: a first byte in either mode must not trip the
        // S3 debug_assert.
        assert_eq!(
            retry.on_first_byte(ResponseMode::Streaming),
            RequestState::Committed
        );
        assert_eq!(
            RequestState::InFlight.on_first_byte(ResponseMode::Buffered),
            RequestState::Assembling
        );

        // And `Retryable` itself is not committable, which is exactly why the
        // reset is necessary: committing from it would violate S3.
        assert!(
            !RequestState::Retryable.may_commit(),
            "a failed attempt must not be allowed to receive a first byte"
        );
    }

    /// `on_first_byte` panics from a state that cannot commit. `Retryable` is
    /// the state between a failed attempt and its retry, so this pins S3 for
    /// the exact state the reset exists to clear.
    #[test]
    #[should_panic]
    fn test_first_byte_from_retryable_is_a_panic() {
        let _ = RequestState::Retryable.on_first_byte(ResponseMode::Streaming);
    }
}
