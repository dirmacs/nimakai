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

use axum::http::HeaderMap;
use std::time::{Duration, Instant};

/// Parse an upstream `Retry-After` duration in seconds.
///
/// Only the numeric delta-seconds form is supported. The HTTP-date form is
/// intentionally unsupported and, like a missing, malformed, or non-numeric
/// value, falls back to 60 seconds.
///
/// This helper replaces the identical inline parsers previously duplicated
/// across the chat, fallback, racing, and HTTP request paths. Keeping one
/// implementation is the point: the duplicated copies were the defect being
/// removed, so they must not grow another parsing variant.
pub(crate) fn upstream_retry_after_secs(headers: &HeaderMap) -> u64 {
    headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(60)
}

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
/// Constructed once per request from the model's timeout windows and the
/// configured admission wait, then consulted by each timeout branch.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    max_retries: usize,
    window: Duration,
    idle_window: Duration,
    budget: Duration,
    body_stall_budget: Duration,
}

impl RetryPolicy {
    /// Build a policy with a split first-byte / mid-stream window.
    ///
    /// * `first_byte_ms` — the per-attempt first-byte window: sending the
    ///   request and waiting for the first byte of the body
    /// * `idle_ms` — the per-attempt mid-stream idle window: the largest
    ///   tolerated gap between two chunks once the body has started
    ///
    /// The two budgets differ because the two failures spent different time.
    /// A send timeout or a no-bytes stall consumed at most one first-byte
    /// window, so its retry is admitted under the fast budget:
    /// `2 * first_byte + admission + slack`. A mid-body stall consumed a
    /// first-byte window AND ran into the idle rail, so its retry is
    /// admitted under the body-stall budget: `2 * first_byte + idle +
    /// admission + slack`. Sizing the body-stall retry from the fast budget
    /// would silently refuse retries the client could have afforded; sizing
    /// the fast retries from the body budget would let a fast failure wait
    /// longer than a single window's worth of extra patience.
    pub fn new_split(
        max_retries: usize,
        first_byte_ms: u64,
        idle_ms: u64,
        admission_wait_ms: u64,
    ) -> Self {
        Self {
            max_retries: max_retries.max(1),
            window: Duration::from_millis(first_byte_ms),
            idle_window: Duration::from_millis(idle_ms),
            budget: Duration::from_millis(first_byte_ms)
                .saturating_mul(BUDGET_MULTIPLIER)
                .saturating_add(Duration::from_millis(admission_wait_ms))
                .saturating_add(Duration::from_millis(BUDGET_SLACK_MS)),
            body_stall_budget: Duration::from_millis(first_byte_ms)
                .saturating_mul(BUDGET_MULTIPLIER)
                .saturating_add(Duration::from_millis(idle_ms))
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
        // The fast budget: for send timeouts and no-bytes stalls, which
        // consumed at most one first-byte window. `decide_body_stall` is the
        // only branch with a different spent shape.
        self.decide_under(started, attempt, self.window, self.budget)
    }

    /// Decide whether a mid-body stall on `attempt` (0-based) should be
    /// retried.
    ///
    /// Same attempt bound as [`Self::decide`], but the budget is the
    /// body-stall budget: the failed attempt already consumed a first-byte
    /// window and ran into the idle rail, so judging its retry by the fast
    /// budget would refuse retries the client's wait still has room for.
    /// The admitted retry is still a single extra attempt — the budget
    /// admits exactly one window pair, never an unbounded chain.
    pub fn decide_body_stall(&self, started: Instant, attempt: usize) -> RetryDecision {
        self.decide_under(started, attempt, self.window, self.body_stall_budget)
    }

    /// The shared decision shape: attempts remain, and one more first-byte
    /// window fits under the given budget. One function so the two budgets
    /// cannot drift into two decision rules.
    fn decide_under(
        &self,
        started: Instant,
        attempt: usize,
        window: Duration,
        budget: Duration,
    ) -> RetryDecision {
        let attempts_remain = attempt + 1 < self.max_retries;
        let fits_budget = started.elapsed() + window <= budget;
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

    /// The wall-clock ceiling for a mid-body stall retry: the fast budget
    /// plus one idle window, for the window the failed attempt spent
    /// mid-stream. Equals [`Self::budget`] when the windows are unsplit.
    pub fn body_stall_budget(&self) -> Duration {
        self.body_stall_budget
    }

    /// The per-attempt window.
    pub fn window(&self) -> Duration {
        self.window
    }

    /// The per-attempt mid-stream idle window.
    pub fn idle_window(&self) -> Duration {
        self.idle_window
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{header::RETRY_AFTER, HeaderValue};

    fn retry_after_headers(value: &'static str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static(value));
        headers
    }

    #[test]
    fn test_upstream_retry_after_numeric_delta_seconds() {
        assert_eq!(upstream_retry_after_secs(&retry_after_headers("7")), 7);
    }

    #[test]
    fn test_upstream_retry_after_numeric_zero() {
        assert_eq!(upstream_retry_after_secs(&retry_after_headers("0")), 0);
    }

    #[test]
    fn test_upstream_retry_after_absent_defaults_to_60() {
        assert_eq!(upstream_retry_after_secs(&HeaderMap::new()), 60);
    }

    #[test]
    fn test_upstream_retry_after_non_numeric_defaults_to_60() {
        assert_eq!(upstream_retry_after_secs(&retry_after_headers("soon")), 60);
    }

    #[test]
    fn test_upstream_retry_after_negative_defaults_to_60() {
        assert_eq!(upstream_retry_after_secs(&retry_after_headers("-5")), 60);
    }

    #[test]
    fn test_upstream_retry_after_http_date_defaults_to_60() {
        // The numeric-only scope is intentional and documented. HTTP-date is
        // therefore deliberately unsupported and uses the 60 s fallback.
        assert_eq!(
            upstream_retry_after_secs(&retry_after_headers("Wed, 21 Oct 2015 07:28:00 GMT")),
            60
        );
    }

    fn policy(max_retries: usize, window_ms: u64, admission_wait_ms: u64) -> RetryPolicy {
        RetryPolicy::new_split(max_retries, window_ms, window_ms, admission_wait_ms)
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

    /// The body-stall budget is the fast budget plus one idle window: the
    /// failed attempt consumed a first-byte window AND ran into the idle
    /// rail, so its retry is admitted under the wider ceiling.
    #[test]
    fn test_body_stall_budget_adds_one_idle_window() {
        let p = RetryPolicy::new_split(8, 25_000, 60_000, 5000);
        assert_eq!(
            p.budget(),
            Duration::from_millis(2 * 25_000 + 5000 + 500),
            "fast budget is unchanged by the split"
        );
        assert_eq!(
            p.body_stall_budget(),
            Duration::from_millis(2 * 25_000 + 60_000 + 5000 + 500),
            "body-stall budget must be 2*first_byte + idle + admission + slack"
        );
        // Equal windows still differ in shape: body-stall accounts for a
        // first-byte window spent AND an idle window's worth of mid-stream
        // time, so it stays wider than the fast budget even when the rails
        // coincide.
        let q = RetryPolicy::new_split(4, 15_000, 15_000, 5000);
        assert_eq!(
            q.body_stall_budget(),
            q.budget() + Duration::from_millis(15_000)
        );
    }

    /// A mid-body stall is retried where the fast budget would refuse: the
    /// attempt spent a first-byte window plus most of an idle window, so it
    /// is over the fast ceiling but inside the body-stall ceiling.
    #[test]
    fn test_body_stall_retry_admitted_past_fast_budget() {
        let p = RetryPolicy::new_split(8, 1000, 4000, 0);
        // Fast budget: 2*1000 + 0 + 500 = 2500 ms. Body budget: 2500 + 4000.
        // 3000 ms elapsed: 3000 + 1000 > 2500 (fast refuses) but
        // 3000 + 1000 <= 6500 (body-stall admits).
        let started = Instant::now() - Duration::from_millis(3000);
        assert_eq!(p.decide(started, 0), RetryDecision::GiveUp);
        assert_eq!(p.decide_body_stall(started, 0), RetryDecision::Retry);
    }

    /// The body-stall retry is still bounded: past its own ceiling, with
    /// attempts remaining, it gives up rather than chaining forever.
    #[test]
    fn test_body_stall_retry_refused_past_its_budget() {
        let p = RetryPolicy::new_split(8, 1000, 4000, 0);
        // Body budget is 6500 ms; 6000 elapsed + 1000 window = 7000 > 6500.
        let started = Instant::now() - Duration::from_millis(6000);
        assert_eq!(p.decide_body_stall(started, 0), RetryDecision::GiveUp);
        // And the attempt bound still applies on the wide budget too.
        assert_eq!(
            p.decide_body_stall(Instant::now(), 7),
            RetryDecision::GiveUp
        );
    }
}
