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
                let cd = self.cooldowns[i].lock().unwrap();
                if matches!(*cd, Some(expiry) if now < expiry) {
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
                let cd = self.cooldowns[i].lock().unwrap();
                if matches!(*cd, Some(expiry) if now < expiry) {
                    None
                } else {
                    Some(window.lock().unwrap().current)
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
                let cd = self.cooldowns[i].lock().unwrap();
                let (active, remaining) = match *cd {
                    Some(expiry) if now < expiry => {
                        (false, expiry.duration_since(now).as_secs())
                    }
                    _ => (true, 0),
                };
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
                    in_flight: self.in_flight_for_key(i),
                    max_in_flight: self.current_window(i),
                    configured_max_in_flight: self.max_in_flight_per_key,
                    auth_failures: self.auth_failures_for(i),
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
