//! Aggregate READ paths over the key pool.
//!
//! Every function here takes a **non-atomic snapshot across three separate
//! mutexes** (`cooldowns`, `windows`, `permits`): each key's cooldown state,
//! window size and in-flight count are read one lock at a time, so the
//! resulting numbers are a point-in-time sample rather than a consistent
//! snapshot. A concurrent `record_rate_limited`/`record_success`/permit
//! acquisition can therefore land between the per-key reads, and a value
//! observed here may never have existed simultaneously with the others.
//!
//! Callers treat these as approximate (status/`/stats`, health and
//! backpressure reporting), not as a synchronisation primitive.

use std::time::Instant;

use super::state::KeyStatus;

impl super::KeyPool {
    pub fn available_permits(&self) -> usize {
        let now = Instant::now();
        self.permits
            .iter()
            .enumerate()
            .filter_map(|(i, sem)| {
                let cd = self.cooldowns[i].lock();
                if matches!(*cd, Some((expiry, _)) if now < expiry) {
                    None
                } else {
                    let in_flight = self
                        .max_in_flight_per_key
                        .saturating_sub(sem.available_permits());
                    Some(self.current_window(i).saturating_sub(in_flight))
                }
            })
            .sum()
    }

    pub fn window_capacity(&self) -> usize {
        let now = Instant::now();
        self.windows
            .iter()
            .enumerate()
            .filter_map(|(i, window)| {
                let cd = self.cooldowns[i].lock();
                if matches!(*cd, Some((expiry, _)) if now < expiry) {
                    None
                } else {
                    Some(window.lock().current)
                }
            })
            .sum()
    }

    /// Return status of all keys (for /health endpoint).
    pub fn status(&self) -> Vec<KeyStatus> {
        let now = Instant::now();
        self.keys
            .iter()
            .enumerate()
            .map(|(i, k)| {
                // Read all four pieces of per-key state under one critical
                // section, so a racing write cannot interleave between them.
                //
                // S6: the previous read took each lock separately — cooldown,
                // then in-flight, then the window, then the counters — so a
                // `mark_rate_limited` or `record_success` on another thread
                // could change one piece while another was being read. The
                // operator then saw a state that never existed: a key shown
                // as active while its window was already shrinking, or a stall
                // counted against a key that was simultaneously benched.
                //
                // Lock order here is cooldown then windows, matching the order
                // writers use (`mark_rate_limited` -> `record_rate_limited` ->
                // windows, then cooldown; `mark_auth_failed` -> counter then
                // cooldown). Holding both for the whole read is safe because
                // that order is consistent everywhere, so there is no
                // lock-order inversion.
                let cd = self.cooldowns[i].lock();
                let window = self.windows[i].lock();
                let (active, remaining, bench_reason) = match *cd {
                    Some((expiry, reason)) if now < expiry => {
                        (false, expiry.duration_since(now).as_secs(), Some(reason))
                    }
                    _ => (true, 0, None),
                };
                let in_flight = self
                    .max_in_flight_per_key
                    .saturating_sub(self.permits[i].available_permits());
                let max_in_flight = window.current;
                let hint = if k.key.len() >= 4 {
                    format!("...{}", &k.key[k.key.len() - 4..])
                } else {
                    "****".to_string()
                };
                KeyStatus {
                    label: k.label.clone().unwrap_or_else(|| format!("key-{}", i)),
                    key_hint: hint,
                    active,
                    cooldown_secs_remaining: remaining,
                    in_flight,
                    max_in_flight,
                    configured_max_in_flight: self.max_in_flight_per_key,
                    auth_failures: self.auth_failures_for(i),
                    stalls: self.stalls_for(i),
                    bench_reason,
                }
            })
            .collect()
    }

    pub fn get_key_label(&self, idx: usize) -> Option<String> {
        self.keys
            .get(idx)
            .map(|k| k.label.clone().unwrap_or_else(|| format!("key-{}", idx)))
    }
}
