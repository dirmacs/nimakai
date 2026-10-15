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
    /// Cumulative count of upstream 401/403 (auth failure) responses observed for this key.
    pub auth_failures: u64,
}

impl super::KeyPool {
    /// Mark a key as rate-limited for `secs` seconds.
    pub fn mark_rate_limited(&self, idx: usize, secs: u64) {
        if idx < self.cooldowns.len() {
            self.record_rate_limited(idx);
            let mut cd = self.cooldowns[idx].lock().unwrap();
            *cd = Some(Instant::now() + Duration::from_secs(secs));
        }
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
            let mut cd = self.cooldowns[idx].lock().unwrap();
            *cd = Some(Instant::now() + cooldown);
        }
    }

    pub fn record_rate_limited(&self, idx: usize) {
        if let Some(window) = self.windows.get(idx) {
            let mut window = window.lock().unwrap();
            window.current = (window.current / 2).max(1);
            window.successes_since_increase = 0;
        }
    }

    pub fn record_success(&self, idx: usize) {
        if let Some(window) = self.windows.get(idx) {
            let mut window = window.lock().unwrap();
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
                let needed = if deficit <= 1 {
                    1
                } else {
                    super::aimd::AIMD_SUCCESS_THRESHOLD.div_ceil(deficit)
                };
                window.successes_since_increase += 1;
                if window.successes_since_increase >= needed {
                    window.current += 1;
                    window.successes_since_increase = 0;
                }
            } else {
                window.successes_since_increase = 0;
            }
        }
    }

    pub(crate) fn current_window(&self, idx: usize) -> usize {
        self.windows
            .get(idx)
            .map(|w| w.lock().unwrap().current)
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
}
