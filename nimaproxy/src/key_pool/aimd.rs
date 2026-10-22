//! AIMD (additive-increase / multiplicative-decrease) window sizing.
//!
//! A rate-limited key's in-flight window shrinks **multiplicatively and
//! instantaneously** — one 429 halves it — while recovery is **additive**:
//! a key `deficit` slots below its ceiling needs `ceil(bar / deficit)`
//! successes to regain one slot.
//!
//! # The two bars
//!
//! [`AIMD_SUCCESS_THRESHOLD`] is the **static bar**: a fixed success count
//! calibrated for an upstream that 429s roughly 5 % of admitted requests. It
//! is the behaviour when the pressure-adaptive knob is off, and it is
//! byte-for-byte today's behaviour.
//!
//! [`AimdMode`] adds the **pressure-derived bar**, used only when the knob is
//! on. Instead of a fixed count, the required-success count is derived from
//! the key's own recent 429 ratio (see [`KeyPool::pressure_bar`] for the
//! derivation). Below a clamp point of roughly 29 % the derived bar sits at
//! the same 8 as the static constant, so at the measured 18-23 % operating
//! point it is **not** a cheaper bar that does the work — it is the decayed
//! credit, which lets net rather than consecutive successes bank a slot (see
//! below). The cheaper-bar half of the rule only engages above the clamp
//! point, or when the operator lowers the ceiling below 8.
//!
//! # Why the credit must survive a 429
//!
//! `record_rate_limited` resets `successes_since_increase` to zero, and a 429
//! also benches the key (`mark_rate_limited`), which stops `next_key` from
//! offering it any traffic — so a benched key earns no successes at all. With
//! a fixed cost of `ceil(8 / deficit)` a key that is 429'd again before it can
//! bank a run of consecutive successes never climbs back, and the window
//! ratchets down to 1 and stays there. That is the measured equilibrium
//! (per-key window 1 against a configured 4, window sum 4 of 16).
//!
//! So the pressure-adaptive mode keeps a **second, decayed** credit counter
//! (`pressure_successes`) that is halved rather than zeroed on a 429. It is
//! only ever *read* by the recovery decision, so with the knob off it is dead
//! weight; with the knob on it is what lets a key climb back out of the
//! bench cycle.

/// Number of successes needed to regain one in-flight slot for a key whose
/// window is exactly one step below its ceiling, under the static bar.
pub(crate) const AIMD_SUCCESS_THRESHOLD: usize = 8;

/// Bounds on the pressure-derived bar.
///
/// The bar is `ceil(2 (1 - p) / (p ln 2))` where `p` is the key's recent
/// 429 ratio, clamped to [`AIMD_BAR_MIN`], [`AIMD_BAR_MAX`] and then to the
/// operator's ceiling. The *clamp*, not the formula, is what makes the bar
/// equal [`AIMD_SUCCESS_THRESHOLD`] under light pressure: the formula yields
/// 55 at p = 5 % and only crosses 8 at p ~= 29 %, so below that clamp point
/// the effective bar is the ceiling (8 by default) and the adaptive rule costs
/// exactly what the fixed rule costs. Below the clamp point the two rules
/// differ in the *credit*, not in the bar.
pub(crate) const AIMD_BAR_MAX: usize = 8;
pub(crate) const AIMD_BAR_MIN: usize = 1;

/// Weight of the newest observation in the per-key 429 ratio.
///
/// The ratio is an EWMA over per-key outcomes (1.0 for a 429, 0.0 for a
/// success). A weight of 1/8 gives a half-life of ~5 observations, so the
/// ratio tracks a shift in upstream pressure within a few dozen requests
/// rather than over the whole uptime.
pub(crate) const AIMD_RATIO_WEIGHT: f64 = 0.125;

/// Shrink applied to the decayed recovery credit when the key is 429'd.
///
/// Halving (rather than zeroing) is the point: the credit survives a bench
/// cycle, so a key that is repeatedly throttled still accumulates progress.
/// It is deliberately the same multiplicative factor as the window shrink, so
/// the penalty side of AIMD keeps its shape and only the recovery side
/// changes.
pub(crate) const AIMD_CREDIT_DECAY: usize = 2;

/// Which recovery rule a key window follows.
///
/// The knob is [`AimdMode::Fixed`] by default: the static bar, i.e. exactly
/// today's behaviour, including the per-429 credit reset. Only
/// [`AimdMode::PressureAdaptive`] reads the decayed credit and the pressure
/// ratio, and neither of those is consulted anywhere else, so the off path is
/// unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AimdMode {
    /// Fixed success count (`AIMD_SUCCESS_THRESHOLD`), credit reset to zero on
    /// every 429. The current behaviour.
    Fixed,
    /// Required-success count derived from the key's recent 429 ratio; credit
    /// decayed (not zeroed) on a 429.
    PressureAdaptive,
}

#[derive(Debug, Clone)]
pub(crate) struct KeyWindow {
    pub(crate) current: usize,
    /// Consecutive successes banked under the static bar. Zeroed by a 429.
    pub(crate) successes_since_increase: usize,
    /// Decayed successes banked under the pressure-adaptive bar. Halved by a
    /// 429, so it accumulates across bench cycles. Only read when the pool's
    /// mode is [`AimdMode::PressureAdaptive`].
    pub(crate) pressure_successes: usize,
    /// EWMA of per-key 429 outcomes (1.0 = 429, 0.0 = success).
    pub(crate) pressure_ratio: f64,
}

impl KeyWindow {
    pub(crate) fn new(current: usize) -> Self {
        Self {
            current,
            successes_since_increase: 0,
            pressure_successes: 0,
            pressure_ratio: 0.0,
        }
    }
}

/// Successes required to regain one slot for a window `deficit` below its
/// ceiling, under the static bar.
///
/// This is the reference curve the pressure-adaptive rule is measured
/// against, and the exact behaviour when the knob is off.
pub(crate) fn static_success_cost(deficit: usize) -> usize {
    if deficit <= 1 {
        1
    } else {
        AIMD_SUCCESS_THRESHOLD.div_ceil(deficit)
    }
}

/// Successes required to regain one slot under the pressure-derived bar.
///
/// `ratio` is the key's recent 429 ratio. The bar is `ceil(2 (1 - ratio) /
/// (ratio ln 2))`, clamped to [`AIMD_BAR_MIN`], [`AIMD_BAR_MAX`] and then to
/// the operator's ceiling, and the cost is then `ceil(bar / deficit)` — the
/// same shape as [`static_success_cost`]. Below the ~29 % clamp point the two
/// rules use the same bar and differ only in the credit the decision reads.
///
/// `ratio == 0` (no 429 ever observed) yields [`AIMD_BAR_MAX`], i.e. exactly
/// the current conservative behaviour.
///
/// The derived bar is then capped at the operator's `bar_max` below, so a
/// misconfigured ceiling degrades to a safe bar rather than failing startup.
#[cfg(test)]
pub(crate) fn pressure_success_cost(deficit: usize, ratio: f64) -> usize {
    pressure_success_cost_capped(deficit, ratio, AIMD_BAR_MAX)
}

/// The operator-ceiling form: the derived bar is capped at `bar_max`
/// (clamped to the valid range) before the deficit division.
pub(crate) fn pressure_success_cost_capped(deficit: usize, ratio: f64, bar_max: usize) -> usize {
    let bar = pressure_bar(ratio).min(bar_max.clamp(AIMD_BAR_MIN, AIMD_BAR_MAX));
    if deficit <= 1 {
        return AIMD_BAR_MIN.max(1);
    }
    bar.div_ceil(deficit).max(1)
}

/// Derive the required-success bar from a 429 ratio.
///
/// The derivation is the AIMD fixed point. If each success adds one unit of
/// in-flight demand and each 429 removes half the window, the window is in
/// equilibrium when the expected drift is zero:
///
/// ```text
///   (1 - p) * (1 / bar)   =   p * (ln 2 / 2)
///   bar                   =   2 (1 - p) / (p ln 2)
/// ```
///
/// so a key whose recent 429 ratio is `p` needs about that many successes per
/// slot. At `p = 5 %` the formula gives 8.0 — the measured calibration point
/// of [`AIMD_SUCCESS_THRESHOLD`] — and it falls monotonically as pressure
/// rises, so a heavily throttled key recovers faster while a lightly
/// throttled one keeps the current cost.
pub(crate) fn pressure_bar(ratio: f64) -> usize {
    let p = ratio.clamp(0.0, 1.0);
    if p <= 0.0 {
        return AIMD_BAR_MAX;
    }
    let raw = 2.0 * (1.0 - p) / (p * std::f64::consts::LN_2);
    let rounded = raw.ceil() as usize;
    rounded.clamp(AIMD_BAR_MIN, AIMD_BAR_MAX)
}
