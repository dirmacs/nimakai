//! AIMD (additive-increase / multiplicative-decrease) window sizing.
//!
//! A rate-limited key's in-flight window shrinks **multiplicatively and
//! instantaneously** — one 429 halves it — while recovery is **additive**:
//! a key `deficit` slots below its ceiling needs
//! `ceil(AIMD_SUCCESS_THRESHOLD / deficit)` successes to regain one slot.
//! The two directions are deliberately asymmetric: the penalty is a single
//! event, the reward is earned over many successful requests.

/// Number of successes needed to regain one in-flight slot for a key whose
/// window is exactly one step below its ceiling.
pub(crate) const AIMD_SUCCESS_THRESHOLD: usize = 8;

#[derive(Debug, Clone)]
pub(crate) struct KeyWindow {
    pub(crate) current: usize,
    pub(crate) successes_since_increase: usize,
}
