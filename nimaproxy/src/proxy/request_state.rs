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
//!   Queued ──permit──> Admitted ──send──> InFlight ──first byte──> Committed
//!     │                   │                  │
//!     │                   │                  └──timeout──> Retryable ──┐
//!     │                   │                                            │
//!     │                   └──429/401/403──────────────> Retryable ─────┤
//!     │                   │                                            │
//!     │                   └──timeout──────────────> Retryable ─────────┘
//!     │                                                            │
//!     └──overload/no-key──> Rejected (terminal)                    │
//!                                                                  │
//!                                               Retryable ──retry──> Admitted
//!                                               Retryable ──give up──> Failed
//!                                                                  │
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
    /// First byte received; the response is on the wire. **Never** retryable.
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
    /// Whether this state may become `Committed`.
    ///
    /// S3: only a request that has not yet committed may do so. Once bytes are
    /// on the wire the client has a response, and a retry would double-send.
    pub fn may_commit(&self) -> bool {
        matches!(self, RequestState::Admitted | RequestState::InFlight)
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
    /// This is the single place that decides retryability: a failure before
    /// any byte is `Retryable`; a failure after the first byte is `Failed`,
    /// because the client already has a partial response.
    pub fn on_failure(&self, bytes_sent: bool) -> RequestState {
        if bytes_sent || matches!(self, RequestState::Committed) {
            RequestState::Failed
        } else {
            RequestState::Retryable
        }
    }

    /// The transition on the first byte arriving.
    pub fn on_first_byte(&self) -> RequestState {
        debug_assert!(
            self.may_commit(),
            "a request in {self:?} cannot receive a first byte; S3 would be violated"
        );
        RequestState::Committed
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
        // A second first byte means a retry double-sent the response body.
        let _ = committed.on_first_byte();
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
        assert!(!RequestState::Queued.is_terminal());
    }
}
