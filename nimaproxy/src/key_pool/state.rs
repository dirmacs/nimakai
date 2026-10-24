use super::aimd::{pressure_success_cost_capped, static_success_cost, AimdMode};
use super::BenchReason;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyStatus {
    pub label: String,
    pub key_hint: String,
    pub active: bool,
    pub cooldown_secs_remaining: u64,
    pub in_flight: usize,
    pub max_in_flight: usize,
    pub configured_max_in_flight: usize,
    /// The key's recent 429 ratio (EWMA, 1.0 = every outcome was a 429). This
    /// is the *input* to the pressure-adaptive recovery rule, exposed next to
    /// `max_in_flight` (its output) so a measurement window can see both. With
    /// the adaptive knob off it is still maintained, because it is a
    /// measurement of the upstream rather than of the policy.
    pub rate_limit_ratio: f64,
    /// Cumulative count of upstream 401/403 (auth failure) responses observed for this key.
    pub auth_failures: u64,
    /// Cumulative count of admitted upstream attempts whose effective wait for
    /// usable bytes elapsed. This includes send, first-byte, and body timeouts;
    /// the mid-stream idle rail; and the proxy's remaining request deadline
    /// cutting the wait short. Connection and body I/O errors are not stalls.
    /// Counted on the chat, racing, fallback, and
    /// legacy completions/embeddings request paths.
    ///
    /// Observational only — a stall never changes this key's window or
    /// cooldown. This counter is not a rate-limit bench: a key being benched
    /// for rate limiting and a key with observed stalls are separate states.
    pub stalls: u64,
    /// Why this key is benched, if it is. `RateLimit` means a temporary 429 —
    /// wait it out. `AuthFailure` means the credential is bad — replace the
    /// key. Before this, both lived in one `Instant`, so an operator could not
    /// tell them apart.
    pub bench_reason: Option<BenchReason>,
}

impl super::KeyPool {
    /// Note that an upstream *stall* was observed on this key.
    ///
    /// A stall is an admitted upstream attempt whose effective wait for usable
    /// bytes elapsed. The effective wait may be a send, first-byte, or body
    /// timeout; the mid-stream idle rail; or the proxy's remaining request
    /// deadline cutting the wait short. Connection and body I/O errors are not
    /// stalls. This is counted on all request paths:
    /// chat, racing, fallback, and legacy completions/embeddings.
    ///
    /// This method is observational only: it never changes the key's window or
    /// cooldown. A stall is also not the same thing as a rate-limit bench; only
    /// an explicit upstream 429 benches a key for rate limiting.
    pub fn note_upstream_stall(&self, idx: usize) {
        if let Some(counter) = self.stalls.get(idx) {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Mark a key as rate-limited for `secs` seconds.
    ///
    /// Reserved for a real upstream 429, i.e. the upstream explicitly saying
    /// this key is throttled. This applies exactly one AIMD penalty and one
    /// temporary bench. Use [`KeyPool::record_rate_limited`] when only the
    /// penalty is wanted, or [`KeyPool::bench_key`] when only the temporary
    /// bench is wanted.
    pub fn mark_rate_limited(&self, idx: usize, secs: u64) {
        self.record_rate_limited(idx);
        self.bench_key(idx, secs);
    }

    /// Bench a key for `secs` seconds without changing its AIMD window.
    ///
    /// This applies only a temporary [`BenchReason::RateLimit`] cooldown. It
    /// does not halve the key's window, fold a 429 into its pressure ratio,
    /// or change its recovery credit. The configured bench cap is a
    /// MAXIMUM, never an extension: the requested duration is honoured in full
    /// unless it is longer than the cap. With no cap configured the requested
    /// duration is preserved exactly.
    ///
    /// An active [`BenchReason::AuthFailure`] quarantine is preserved: a
    /// credential-level 401/403 must not be downgraded by a later transient
    /// 429. An expired auth quarantine, like an active rate-limit cooldown,
    /// may be replaced by this bench.
    pub fn bench_key(&self, idx: usize, secs: u64) {
        if idx >= self.cooldowns.len() {
            return;
        }
        let secs = match self.rate_limit_bench_cap_secs {
            Some(cap) => secs.min(cap),
            None => secs,
        };
        let mut cd = self.cooldowns[idx].lock();
        if matches!(
            *cd,
            Some((expiry, BenchReason::AuthFailure)) if Instant::now() < expiry
        ) {
            return;
        }
        *cd = Some((
            Instant::now() + Duration::from_secs(secs),
            BenchReason::RateLimit,
        ));
    }

    /// Mark a key as having failed upstream auth (401/403 response). This is a distinct
    /// failure mode from rate-limiting: it means the key itself is bad (revoked/invalid),
    /// not that the account is temporarily throttled. Reuses the same cooldown mechanism as
    /// `mark_rate_limited` (no second skip path) so `next_key`/`next_key_with_permit` skip
    /// the key for the cooldown duration, and bumps the key's cumulative `auth_failures`
    /// counter (exposed via `status()` / `/stats`).
    ///
    /// `cooldown == Duration::ZERO` still records the failure but leaves the key's cooldown
    /// untouched — this is how `[limits].auth_failure_cooldown_secs = 0` disables auth-failure
    /// quarantine while still counting the failure.
    pub fn mark_auth_failed(&self, idx: usize, cooldown: Duration) {
        if idx >= self.cooldowns.len() {
            return;
        }
        if let Some(counter) = self.auth_failures.get(idx) {
            counter.fetch_add(1, Ordering::Relaxed);
        }
        if !cooldown.is_zero() {
            let mut cd = self.cooldowns[idx].lock();
            *cd = Some((Instant::now() + cooldown, BenchReason::AuthFailure));
        }
    }

    pub fn record_rate_limited(&self, idx: usize) {
        if let Some(window) = self.windows.get(idx) {
            let mut window = window.lock();
            // Penalty side, unchanged: one 429 halves the window, floored at 1.
            window.current = (window.current / 2).max(1);
            // Fold the 429 into the pressure ratio, which is what the
            // adaptive bar reads.
            Self::fold_outcome(&mut window, true);
            if self.aimd_mode == AimdMode::Fixed {
                // Today's behaviour: the consecutive-success credit is zeroed.
                window.successes_since_increase = 0;
            } else {
                // The credit is decayed, not zeroed. A 429 also benches the key
                // (`mark_rate_limited`), and a benched key receives no traffic
                // — so it earns no successes — which means a zeroed credit can
                // never be re-earned before the next 429 arrives. That is the
                // measured ratchet: every key pinned at a window of 1 against
                // a configured 4.
                window.successes_since_increase /= super::aimd::AIMD_CREDIT_DECAY;
            }
            window.pressure_successes /= super::aimd::AIMD_CREDIT_DECAY;
        }
    }

    pub fn record_success(&self, idx: usize) {
        if let Some(window) = self.windows.get(idx) {
            let mut window = window.lock();
            Self::fold_outcome(&mut window, false);
            if window.current < self.max_in_flight_per_key {
                // Proportional recovery. Shrink is multiplicative and
                // instantaneous (one 429 halves the window), so a fixed
                // additive cost is asymmetric: a key stuck at 1 against a
                // configured 2 needed 8 successes to recover one slot, while
                // the 429 that cost it took a single event. With ~5% of
                // admitted requests hitting a 429, keys sat throttled for
                // long stretches and two of four keys ran at half capacity
                // while the rest carried the traffic.
                //
                // A key one shrink below its ceiling is one step below, so it
                // takes proportionally fewer successes; a key k steps below
                // needs ceil(threshold / k). Idle keys (nothing in flight)
                // recover at the full threshold, since an unused slot being
                // held back cannot help anyone.
                let deficit = self.max_in_flight_per_key - window.current;
                window.successes_since_increase += 1;
                window.pressure_successes += 1;
                let grown = if self.aimd_mode == AimdMode::Fixed {
                    window.successes_since_increase >= static_success_cost(deficit)
                } else {
                    // The bar is derived from the key's own recent 429 ratio,
                    // so a heavily throttled key pays less than a lightly
                    // throttled one. The credit it reads is the decayed one,
                    // which is why the cost is not the same number even at the
                    // same ratio.
                    let needed = pressure_success_cost_capped(
                        deficit,
                        window.pressure_ratio,
                        self.aimd_max_success_bar,
                    );
                    window.pressure_successes >= needed
                };
                if grown {
                    window.current += 1;
                    window.successes_since_increase = 0;
                    window.pressure_successes = 0;
                }
            } else {
                window.successes_since_increase = 0;
                window.pressure_successes = 0;
            }
        }
    }

    /// Fold one per-key outcome into that key's 429 ratio (EWMA).
    ///
    /// Kept on the *fixed* path too: it is a measurement of the upstream, and
    /// a window that starts with the knob off must be able to show the
    /// pressure it was measured against.
    fn fold_outcome(window: &mut super::aimd::KeyWindow, rate_limited: bool) {
        let sample = if rate_limited { 1.0 } else { 0.0 };
        let w = super::aimd::AIMD_RATIO_WEIGHT;
        window.pressure_ratio = window.pressure_ratio * (1.0 - w) + sample * w;
    }

    pub(crate) fn current_window(&self, idx: usize) -> usize {
        self.windows
            .get(idx)
            .map(|w| w.lock().current)
            .unwrap_or(self.max_in_flight_per_key)
    }

    pub(crate) fn in_flight_for_key(&self, idx: usize) -> usize {
        self.max_in_flight_per_key
            .saturating_sub(self.permits[idx].available_permits())
    }

    pub(crate) fn auth_failures_for(&self, idx: usize) -> u64 {
        self.auth_failures
            .get(idx)
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Cumulative upstream stalls observed for this key. Observational only.
    pub(crate) fn stalls_for(&self, idx: usize) -> u64 {
        self.stalls
            .get(idx)
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0)
    }
}
