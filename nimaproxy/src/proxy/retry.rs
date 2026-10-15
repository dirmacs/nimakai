//! The request-retry policy: one transition, one implementation.
//!
//! The three defects in the PR that introduced this module were all the same
//! shape — a retry decision that existed in more than one place, where fixing
//! one copy left the other wrong. Specifically:
//!
//! * the retry budget was sized from attempt windows only, while the code
//!   measured elapsed *request* time including the admission wait, so the
//!   guard evaluated false on every attempt and the retry silently never
//!   fired;
//! * the body-stall retry was added to the send-timeout branch, but
//!   production 504s all take the body branch, so the fix was dead code for
//!   this deployment.
//!
//! Both are prevented by making the decision one function.

use std::time::{Duration, Instant};

/// How long a key is benched after timing out, so a retry picks a different
/// key rather than re-acquiring the one that just stalled.
pub const TIMEOUT_RETRY_KEY_COOLDOWN_SECS: u64 = 60;

/// Total wall-clock ceiling for a request's upstream attempts, as a multiple
/// of one timeout window. With a 15000 ms window this permits exactly one
/// extra attempt, so a retry can never double the client's wait.
const BUDGET_MULTIPLIER: u32 = 2;

/// Scheduling slack on top of the budget.
///
/// Load-bearing for two measured reasons:
///
/// 1. A timeout is observed at `window + jitter` (302 ms against a 300 ms
///    window), so a bare 2x budget fails `elapsed + window <= budget` by a
///    few milliseconds and the retry can never fire.
/// 2. `elapsed` is measured from the start of the *request*, which includes
///    waiting for a permit. In production a single attempt measured 16546 ms
///    against a 15000 ms window, so two attempts need more than 2x a bare
///    window.
const BUDGET_SLACK_MS: u64 = 500;

/// The outcome of a retry decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    /// Another attempt should be made, on a different key.
    Retry,
    /// Give up and return the terminal response.
    GiveUp,
}

/// Retry policy for one request.
///
/// Constructed once per request from the model's timeout window and the
/// configured admission wait, then consulted by each timeout branch.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    max_retries: usize,
    window: Duration,
    budget: Duration,
}

impl RetryPolicy {
    /// Build a policy from the production knobs.
    ///
    /// * `max_retries` — attempt cap, normally `pool.len().min(MAX_RETRIES)`
    /// * `window_ms` — the per-attempt timeout window
    /// * `admission_wait_ms` — time a request may wait for a permit before
    ///   admission gives up; counted against the budget because it elapses
    ///   before the first attempt starts
    pub fn new(max_retries: usize, window_ms: u64, admission_wait_ms: u64) -> Self {
        Self {
            max_retries: max_retries.max(1),
            window: Duration::from_millis(window_ms),
            budget: Duration::from_millis(window_ms)
                .saturating_mul(BUDGET_MULTIPLIER)
                .saturating_add(Duration::from_millis(admission_wait_ms))
                .saturating_add(Duration::from_millis(BUDGET_SLACK_MS)),
        }
    }

    /// Decide whether `attempt` (0-based) should be retried.
    ///
    /// Both conditions must hold:
    ///
    /// * attempts remain, so the loop stays bounded;
    /// * one more full window fits in the budget measured from the start of
    ///   the request, so the client's total wait is capped.
    pub fn decide(&self, started: Instant, attempt: usize) -> RetryDecision {
        let attempts_remain = attempt + 1 < self.max_retries;
        let fits_budget = started.elapsed() + self.window <= self.budget;
        if attempts_remain && fits_budget {
            RetryDecision::Retry
        } else {
            RetryDecision::GiveUp
        }
    }

    /// The total wall-clock ceiling, for documentation and assertions.
    pub fn budget(&self) -> Duration {
        self.budget
    }

    /// The per-attempt window.
    pub fn window(&self) -> Duration {
        self.window
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(max_retries: usize, window_ms: u64, admission_wait_ms: u64) -> RetryPolicy {
        RetryPolicy::new(max_retries, window_ms, admission_wait_ms)
    }

    /// The retry fires when a full window still fits in the budget.
    #[test]
    fn test_retry_when_window_still_fits() {
        let p = policy(4, 300, 0);
        let started = Instant::now();
        assert_eq!(p.decide(started, 0), RetryDecision::Retry);
    }

    /// The final attempt never retries, regardless of budget.
    #[test]
    fn test_no_retry_on_final_attempt() {
        let p = policy(4, 300, 0);
        let started = Instant::now();
        assert_eq!(p.decide(started, 3), RetryDecision::GiveUp);
        assert_eq!(p.decide(started, 99), RetryDecision::GiveUp);
    }

    /// A request that has already spent its budget gives up even with
    /// attempts remaining. This is the liveness bound: a client is never asked
    /// to wait materially longer than the budget.
    #[test]
    fn test_no_retry_when_budget_is_spent() {
        let p = policy(8, 1000, 5000);
        // budget = 2*1000 + 5000 + 500 = 7500 ms
        let started = Instant::now() - Duration::from_millis(7000);
        assert_eq!(p.decide(started, 0), RetryDecision::GiveUp);
    }

    /// The admission wait must be inside the budget.
    ///
    /// This is the exact defect from production: a single attempt measured
    /// 16546 ms against a 15000 ms window because the request waited ~1.5 s
    /// for a permit first. A budget sized from the window alone (30500 ms)
    /// rejected the retry, because 16546 + 15000 > 30500.
    #[test]
    fn test_budget_includes_admission_wait() {
        let p = policy(8, 15_000, 5000);
        assert_eq!(
            p.budget(),
            Duration::from_millis(2 * 15_000 + 5000 + 500),
            "budget must be window*2 + admission_wait + slack"
        );
        // With the admission wait counted, an attempt that spent 16546 ms and
        // still has a full 15000 ms window left must be allowed to retry.
        let started = Instant::now() - Duration::from_millis(16_546);
        assert_eq!(p.decide(started, 0), RetryDecision::Retry);

        // Without the admission allowance the same request is refused.
        let without = policy(8, 15_000, 0);
        assert_eq!(without.budget(), Duration::from_millis(2 * 15_000 + 500));
        assert_eq!(without.decide(started, 0), RetryDecision::GiveUp);
    }

    /// A bare 2x budget would fail on scheduling jitter; the slack is what
    /// makes the retry actually reachable.
    #[test]
    fn test_slack_covers_timeout_jitter() {
        // Window 300 ms, observed timeout at 302 ms, no admission wait.
        let p = policy(8, 300, 0);
        let started = Instant::now() - Duration::from_millis(302);
        assert_eq!(
            p.decide(started, 0),
            RetryDecision::Retry,
            "302 ms elapsed against a 300 ms window must still permit one retry"
        );
    }

    /// The policy is a pure function of its inputs: same inputs, same budget.
    #[test]
    fn test_policy_is_deterministic() {
        assert_eq!(
            policy(4, 15000, 5000).budget(),
            policy(4, 15000, 5000).budget()
        );
        assert_eq!(
            policy(4, 15000, 5000).window(),
            Duration::from_millis(15000)
        );
    }

    /// A zero-attempt pool still yields a usable (non-retrying) policy rather
    /// than dividing by zero.
    #[test]
    fn test_policy_with_zero_max_retries_is_clamped() {
        let p = policy(0, 300, 0);
        let started = Instant::now();
        assert_eq!(p.decide(started, 0), RetryDecision::GiveUp);
    }
}
