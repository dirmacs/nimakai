use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};

use crate::config::KeyEntry;

const DEFAULT_MAX_IN_FLIGHT_PER_KEY: usize = 1_000_000;
const AIMD_SUCCESS_THRESHOLD: usize = 8;

pub struct KeyPool {
    keys: Vec<KeyEntry>,
    index: AtomicUsize,
    cooldowns: Vec<Mutex<Option<Instant>>>,
    permits: Vec<Arc<Semaphore>>,
    windows: Vec<Mutex<KeyWindow>>,
    max_in_flight_per_key: usize,
    /// Cumulative count of upstream 401/403 (auth failure) responses observed per key.
    auth_failures: Vec<AtomicU64>,
}

#[derive(Debug, Clone)]
struct KeyWindow {
    current: usize,
    successes_since_increase: usize,
}

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

pub struct KeyLease {
    pub key: String,
    pub idx: usize,
    pub label: Option<String>,
    _permit: OwnedSemaphorePermit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAcquireError {
    NoKeys,
    AllCoolingDown,
    AllBusy,
}

impl KeyPool {
    pub fn new(keys: Vec<KeyEntry>) -> Self {
        Self::with_max_in_flight(keys, DEFAULT_MAX_IN_FLIGHT_PER_KEY)
    }

    pub fn with_max_in_flight(keys: Vec<KeyEntry>, max_in_flight_per_key: usize) -> Self {
        let n = keys.len();
        let max_in_flight_per_key = max_in_flight_per_key.max(1);
        KeyPool {
            keys,
            index: AtomicUsize::new(0),
            cooldowns: (0..n).map(|_| Mutex::new(None)).collect(),
            permits: (0..n)
                .map(|_| Arc::new(Semaphore::new(max_in_flight_per_key)))
                .collect(),
            windows: (0..n)
                .map(|_| {
                    Mutex::new(KeyWindow {
                        current: max_in_flight_per_key,
                        successes_since_increase: 0,
                    })
                })
                .collect(),
            max_in_flight_per_key,
            auth_failures: (0..n).map(|_| AtomicU64::new(0)).collect(),
        }
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Pick the next available (non-rate-limited) key.
    /// Returns (key_string, pool_index) or None if all keys are cooling down.
    pub fn next_key(&self) -> Option<(String, usize)> {
        let n = self.keys.len();
        if n == 0 {
            return None;
        }
        let start = self.index.fetch_add(1, Ordering::Relaxed) % n;
        for i in 0..n {
            let idx = (start + i) % n;
            let cd = self.cooldowns[idx].lock().unwrap();
            match *cd {
                Some(expiry) if Instant::now() < expiry => continue, // still cooling
                _ => return Some((self.keys[idx].key.clone(), idx)),
            }
        }
        None
    }

    pub fn next_key_with_permit(&self) -> Result<KeyLease, KeyAcquireError> {
        let n = self.keys.len();
        if n == 0 {
            return Err(KeyAcquireError::NoKeys);
        }

        let start = self.index.fetch_add(1, Ordering::Relaxed) % n;
        let now = Instant::now();
        let mut active_seen = false;
        let mut busy_seen = false;

        for i in 0..n {
            let idx = (start + i) % n;
            let cd = self.cooldowns[idx].lock().unwrap();
            if matches!(*cd, Some(expiry) if now < expiry) {
                continue;
            }
            drop(cd);
            active_seen = true;

            if self.in_flight_for_key(idx) >= self.current_window(idx) {
                busy_seen = true;
                continue;
            }

            match self.permits[idx].clone().try_acquire_owned() {
                Ok(permit) => {
                    return Ok(KeyLease {
                        key: self.keys[idx].key.clone(),
                        idx,
                        label: self.get_key_label(idx),
                        _permit: permit,
                    });
                }
                Err(TryAcquireError::NoPermits) => {
                    busy_seen = true;
                }
                Err(TryAcquireError::Closed) => {
                    busy_seen = true;
                }
            }
        }

        if busy_seen || active_seen {
            Err(KeyAcquireError::AllBusy)
        } else {
            Err(KeyAcquireError::AllCoolingDown)
        }
    }

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

    fn auth_failures_for(&self, idx: usize) -> u64 {
        self.auth_failures
            .get(idx)
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0)
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
                window.successes_since_increase += 1;
                if window.successes_since_increase >= AIMD_SUCCESS_THRESHOLD {
                    window.current += 1;
                    window.successes_since_increase = 0;
                }
            } else {
                window.successes_since_increase = 0;
            }
        }
    }

    fn current_window(&self, idx: usize) -> usize {
        self.windows
            .get(idx)
            .map(|w| w.lock().unwrap().current)
            .unwrap_or(self.max_in_flight_per_key)
    }

    fn in_flight_for_key(&self, idx: usize) -> usize {
        self.max_in_flight_per_key
            .saturating_sub(self.permits[idx].available_permits())
    }

    pub fn get_key_label(&self, idx: usize) -> Option<String> {
        self.keys
            .get(idx)
            .map(|k| k.label.clone().unwrap_or_else(|| format!("key-{}", idx)))
    }

    pub fn active_count(&self) -> usize {
        self.status().iter().filter(|s| s.active).count()
    }

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
                    Some(expiry) if now < expiry => (false, expiry.duration_since(now).as_secs()),
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_key_entry(key: &str, label: &str) -> KeyEntry {
        KeyEntry {
            key: key.to_string(),
            label: Some(label.to_string()),
        }
    }

    #[test]
    fn test_next_key_round_robin() {
        let keys = vec![
            make_key_entry("key1", "doltares"),
            make_key_entry("key2", "ares"),
            make_key_entry("key3", "test"),
        ];
        let pool = KeyPool::new(keys);

        // First key should be key1
        let (k, idx) = pool.next_key().unwrap();
        assert_eq!(k, "key1");
        assert_eq!(idx, 0);

        // Second key should be key2
        let (k, idx) = pool.next_key().unwrap();
        assert_eq!(k, "key2");
        assert_eq!(idx, 1);

        // Third key should be key3
        let (k, idx) = pool.next_key().unwrap();
        assert_eq!(k, "key3");
        assert_eq!(idx, 2);

        // Wrap back to key1
        let (k, idx) = pool.next_key().unwrap();
        assert_eq!(k, "key1");
        assert_eq!(idx, 0);
    }

    #[test]
    fn test_single_key() {
        let keys = vec![make_key_entry("only-key", "single")];
        let pool = KeyPool::new(keys);

        let (k, idx) = pool.next_key().unwrap();
        assert_eq!(k, "only-key");
        assert_eq!(idx, 0);

        // Should keep returning the same key
        let (k, idx) = pool.next_key().unwrap();
        assert_eq!(k, "only-key");
        assert_eq!(idx, 0);
    }

    #[test]
    fn test_empty_pool() {
        let pool = KeyPool::new(vec![]);
        assert_eq!(pool.next_key(), None);
        assert_eq!(pool.len(), 0);
    }

    #[test]
    fn test_mark_rate_limited() {
        let keys = vec![make_key_entry("key1", "a"), make_key_entry("key2", "b")];
        let pool = KeyPool::new(keys);

        // Mark key1 as rate-limited for 1 second
        pool.mark_rate_limited(0, 1);

        // key1 should now be skipped
        let (k, idx) = pool.next_key().unwrap();
        assert_eq!(k, "key2");
        assert_eq!(idx, 1);
    }

    #[test]
    fn test_mark_auth_failed_sets_cooldown_and_counts() {
        let keys = vec![make_key_entry("key1", "a"), make_key_entry("key2", "b")];
        let pool = KeyPool::new(keys);

        pool.mark_auth_failed(0, Duration::from_secs(900));

        // key1 should now be skipped (in cooldown), same mechanism as mark_rate_limited
        let (k, idx) = pool.next_key().unwrap();
        assert_eq!(k, "key2");
        assert_eq!(idx, 1);

        let statuses = pool.status();
        assert!(!statuses[0].active);
        assert!(statuses[0].cooldown_secs_remaining > 0);
        assert!(statuses[0].cooldown_secs_remaining <= 900);
        assert_eq!(statuses[0].auth_failures, 1);
        assert_eq!(statuses[1].auth_failures, 0);
    }

    #[test]
    fn test_mark_auth_failed_zero_cooldown_disables_quarantine() {
        let keys = vec![make_key_entry("key1", "a")];
        let pool = KeyPool::new(keys);

        pool.mark_auth_failed(0, Duration::from_secs(0));

        // Cooldown must NOT be set when the configured cooldown is zero — the key stays
        // active and immediately reusable — but the failure is still counted.
        let statuses = pool.status();
        assert!(statuses[0].active);
        assert_eq!(statuses[0].cooldown_secs_remaining, 0);
        assert_eq!(statuses[0].auth_failures, 1);
        assert!(pool.next_key().is_some());
    }

    #[test]
    fn test_mark_auth_failed_is_cumulative() {
        let keys = vec![make_key_entry("key1", "a")];
        let pool = KeyPool::new(keys);

        pool.mark_auth_failed(0, Duration::from_secs(0));
        pool.mark_auth_failed(0, Duration::from_secs(0));
        pool.mark_auth_failed(0, Duration::from_secs(0));

        assert_eq!(pool.status()[0].auth_failures, 3);
    }

    #[test]
    fn test_status_default_auth_failures_zero() {
        let keys = vec![make_key_entry("key1", "a")];
        let pool = KeyPool::new(keys);

        assert_eq!(pool.status()[0].auth_failures, 0);
    }

    #[test]
    fn test_next_key_with_permit_enforces_per_key_limit() {
        let keys = vec![make_key_entry("key1", "a")];
        let pool = KeyPool::with_max_in_flight(keys, 1);

        let lease = pool.next_key_with_permit().unwrap();
        assert_eq!(lease.idx, 0);
        assert_eq!(pool.available_permits(), 0);
        assert!(matches!(
            pool.next_key_with_permit(),
            Err(KeyAcquireError::AllBusy)
        ));

        drop(lease);
        assert_eq!(pool.available_permits(), 1);
        assert!(pool.next_key_with_permit().is_ok());
    }

    #[test]
    fn test_next_key_with_permit_distinguishes_cooldown() {
        let keys = vec![make_key_entry("key1", "a")];
        let pool = KeyPool::with_max_in_flight(keys, 1);
        pool.mark_rate_limited(0, 30);

        assert!(matches!(
            pool.next_key_with_permit(),
            Err(KeyAcquireError::AllCoolingDown)
        ));
    }

    #[test]
    fn test_rate_limit_shrinks_dynamic_window() {
        let keys = vec![make_key_entry("key1", "a")];
        let pool = KeyPool::with_max_in_flight(keys, 4);

        assert_eq!(pool.status()[0].max_in_flight, 4);
        pool.record_rate_limited(0);
        assert_eq!(pool.status()[0].max_in_flight, 2);
        pool.record_rate_limited(0);
        assert_eq!(pool.status()[0].max_in_flight, 1);
    }

    #[test]
    fn test_successes_reopen_dynamic_window_slowly() {
        let keys = vec![make_key_entry("key1", "a")];
        let pool = KeyPool::with_max_in_flight(keys, 3);
        pool.record_rate_limited(0);
        assert_eq!(pool.status()[0].max_in_flight, 1);

        for _ in 0..AIMD_SUCCESS_THRESHOLD {
            pool.record_success(0);
        }
        assert_eq!(pool.status()[0].max_in_flight, 2);

        for _ in 0..AIMD_SUCCESS_THRESHOLD {
            pool.record_success(0);
        }
        assert_eq!(pool.status()[0].max_in_flight, 3);
    }

    #[test]
    fn test_available_permits_respects_dynamic_window() {
        let keys = vec![make_key_entry("key1", "a")];
        let pool = KeyPool::with_max_in_flight(keys, 3);
        pool.record_rate_limited(0);
        assert_eq!(pool.available_permits(), 1);

        let lease = pool.next_key_with_permit().unwrap();
        assert_eq!(pool.available_permits(), 0);
        assert!(matches!(
            pool.next_key_with_permit(),
            Err(KeyAcquireError::AllBusy)
        ));
        drop(lease);
        assert_eq!(pool.available_permits(), 1);
    }

    #[test]
    fn test_all_keys_cooldown_returns_none() {
        let keys = vec![make_key_entry("key1", "a"), make_key_entry("key2", "b")];
        let pool = KeyPool::new(keys);

        // Mark both keys as rate-limited
        pool.mark_rate_limited(0, 1);
        pool.mark_rate_limited(1, 1);

        // Should return None when all keys are cooling down
        assert_eq!(pool.next_key(), None);
    }

    #[test]
    fn test_status_active() {
        let keys = vec![make_key_entry("nvapi-ABCD1234", "test")];
        let pool = KeyPool::new(keys);

        let statuses = pool.status();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].label, "test");
        assert_eq!(statuses[0].key_hint, "...1234");
        assert!(statuses[0].active);
        assert_eq!(statuses[0].cooldown_secs_remaining, 0);
    }

    #[test]
    fn test_status_cooldown() {
        let keys = vec![make_key_entry("key1", "a")];
        let pool = KeyPool::new(keys);

        // Mark key with 10 second cooldown
        pool.mark_rate_limited(0, 10);

        let statuses = pool.status();
        assert!(!statuses[0].active);
        assert!(statuses[0].cooldown_secs_remaining > 0);
        assert!(statuses[0].cooldown_secs_remaining <= 10);
    }

    #[test]
    fn test_key_hint_short_key() {
        let keys = vec![KeyEntry {
            key: "abc".to_string(),
            label: None,
        }];
        let pool = KeyPool::new(keys);

        let statuses = pool.status();
        assert_eq!(statuses[0].key_hint, "****");
    }

    #[test]
    fn test_default_label() {
        let keys = vec![KeyEntry {
            key: "key1".to_string(),
            label: None,
        }];
        let pool = KeyPool::new(keys);

        let statuses = pool.status();
        assert_eq!(statuses[0].label, "key-0");
    }

    #[test]
    fn test_next_key_all_keys_rate_limited_returns_none() {
        // Test that next_key returns None when all keys are rate-limited
        let keys = vec![
            make_key_entry("key1", "a"),
            make_key_entry("key2", "b"),
            make_key_entry("key3", "c"),
        ];
        let pool = KeyPool::new(keys);

        // Mark all keys as rate-limited for 60 seconds
        pool.mark_rate_limited(0, 60);
        pool.mark_rate_limited(1, 60);
        pool.mark_rate_limited(2, 60);

        // Should return None when all keys are cooling down
        assert_eq!(pool.next_key(), None);
    }

    #[test]
    fn test_cooldown_expiration() {
        // Test that a key becomes available after cooldown expires
        let keys = vec![make_key_entry("key1", "a")];
        let pool = KeyPool::new(keys);

        // Mark key as rate-limited for 1 second
        pool.mark_rate_limited(0, 1);

        // Should return None while cooling down
        assert_eq!(pool.next_key(), None);

        // Wait for cooldown to expire
        std::thread::sleep(Duration::from_secs(1) + Duration::from_millis(100));

        // Should now return the key
        let (k, idx) = pool
            .next_key()
            .expect("key should be available after cooldown");
        assert_eq!(k, "key1");
        assert_eq!(idx, 0);
    }

    #[test]
    fn test_next_key_single_key_repeated() {
        // Test next_key with single key returns it consistently
        let keys = vec![make_key_entry("single-key", "only")];
        let pool = KeyPool::new(keys);

        // First call
        let (k, idx) = pool.next_key().expect("should return key");
        assert_eq!(k, "single-key");
        assert_eq!(idx, 0);

        // Second call should also return the same key
        let (k, idx) = pool.next_key().expect("should return key again");
        assert_eq!(k, "single-key");
        assert_eq!(idx, 0);

        // Third call still the same
        let (k, idx) = pool.next_key().expect("should return key third time");
        assert_eq!(k, "single-key");
        assert_eq!(idx, 0);
    }

    #[test]
    fn test_status_mixed_active_inactive() {
        // Test status returns correct state for mixed active/inactive keys
        let keys = vec![
            make_key_entry("key1-active", "active"),
            make_key_entry("key2-cooldown", "cooling"),
            make_key_entry("key3-active", "also-active"),
        ];
        let pool = KeyPool::new(keys);

        // Mark only key2 as rate-limited
        pool.mark_rate_limited(1, 30);

        let statuses = pool.status();
        assert_eq!(statuses.len(), 3);

        // Key 1 should be active
        assert_eq!(statuses[0].label, "active");
        assert!(statuses[0].active);
        assert_eq!(statuses[0].cooldown_secs_remaining, 0);

        // Key 2 should be inactive with cooldown
        assert_eq!(statuses[1].label, "cooling");
        assert!(!statuses[1].active);
        assert!(statuses[1].cooldown_secs_remaining > 0);
        assert!(statuses[1].cooldown_secs_remaining <= 30);

        // Key 3 should be active
        assert_eq!(statuses[2].label, "also-active");
        assert!(statuses[2].active);
        assert_eq!(statuses[2].cooldown_secs_remaining, 0);
    }

    #[test]
    fn test_get_key_label_with_label() {
        // Test get_key_label returns explicit label
        let keys = vec![make_key_entry("key1", "labeled-key")];
        let pool = KeyPool::new(keys);

        assert_eq!(pool.get_key_label(0), Some("labeled-key".to_string()));
    }

    #[test]
    fn test_get_key_label_out_of_bounds() {
        // Test get_key_label returns None for out of bounds index
        let keys = vec![make_key_entry("key1", "a")];
        let pool = KeyPool::new(keys);

        assert_eq!(pool.get_key_label(1), None);
    }

    #[test]
    fn test_get_key_label_default_format() {
        // Test get_key_label uses default format when no label
        let keys = vec![KeyEntry {
            key: "key1".to_string(),
            label: None,
        }];
        let pool = KeyPool::new(keys);

        assert_eq!(pool.get_key_label(0), Some("key-0".to_string()));
    }
}
