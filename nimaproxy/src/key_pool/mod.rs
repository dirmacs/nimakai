use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};

use crate::config::KeyEntry;

pub mod aimd;
pub mod report;
pub mod state;
#[cfg(test)]
mod tests;

use self::aimd::KeyWindow;

const DEFAULT_MAX_IN_FLIGHT_PER_KEY: usize = 1_000_000;

/// Why a key is benched.
///
/// Cooling and auth-quarantine used to share one `Instant`, so there was no
/// way to ask "which failure?": a key benched for a temporary 429 looked the
/// same as one whose credential was revoked. These are different operator
/// actions (wait it out, vs replace the key), and mixing them is the failure
/// mode that made the fleet-wide lockout invisible — every benched key looked
/// like a transient throttle when some were auth failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BenchReason {
    /// A real upstream 429: the key is temporarily throttled. Short cooldown.
    RateLimit,
    /// An upstream 401/403: the key's credential is bad. Longer quarantine, and
    /// counted in `auth_failures`.
    AuthFailure,
}

pub struct KeyPool {
    keys: Vec<KeyEntry>,
    index: AtomicUsize,
    cooldowns: Vec<Mutex<Option<(Instant, BenchReason)>>>,
    permits: Vec<Arc<Semaphore>>,
    windows: Vec<Mutex<KeyWindow>>,
    max_in_flight_per_key: usize,
    /// Cumulative count of upstream 401/403 (auth failure) responses observed per key.
    auth_failures: Vec<AtomicU64>,
    /// Cumulative count of upstream *stalls* observed per key: requests that
    /// connected and were admitted but produced no usable bytes inside the
    /// timeout window. Observational only — unlike `auth_failures` this never
    /// changes the key's window or cooldown, because an upstream-wide stall is
    /// not evidence the key is degraded.
    stalls: Vec<AtomicU64>,
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
            stalls: (0..n).map(|_| AtomicU64::new(0)).collect(),
        }
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
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
                Some((expiry, _)) if Instant::now() < expiry => continue, // still cooling
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
            if matches!(*cd, Some((expiry, _)) if now < expiry) {
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

    pub fn active_count(&self) -> usize {
        self.status().iter().filter(|s| s.active).count()
    }
}
