use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::KeyEntry;

pub struct KeyPool {
    keys: Vec<KeyEntry>,
    index: AtomicUsize,
    cooldowns: Vec<Mutex<Option<Instant>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyStatus {
    pub label: String,
    pub key_hint: String,
    pub active: bool,
    pub cooldown_secs_remaining: u64,
}

impl KeyPool {
    pub fn new(keys: Vec<KeyEntry>) -> Self {
        let n = keys.len();
        KeyPool {
            keys,
            index: AtomicUsize::new(0),
            cooldowns: (0..n).map(|_| Mutex::new(None)).collect(),
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

    /// Mark a key as rate-limited for `secs` seconds.
    pub fn mark_rate_limited(&self, idx: usize, secs: u64) {
        if idx < self.cooldowns.len() {
            let mut cd = self.cooldowns[idx].lock().unwrap();
            *cd = Some(Instant::now() + Duration::from_secs(secs));
        }
    }

    pub fn get_key_label(&self, idx: usize) -> Option<String> {
        self.keys
            .get(idx)
            .map(|k| k.label.clone().unwrap_or_else(|| format!("key-{}", idx)))
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
        let (k, idx) = pool.next_key().expect("key should be available after cooldown");
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
