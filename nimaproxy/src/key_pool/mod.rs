use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};

use crate::config::KeyEntry;

pub mod aimd;
pub mod report;
pub mod state;
#[cfg(test)]
mod tests;

use self::aimd::{AimdMode, KeyWindow};

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
    /// Which recovery rule the windows follow. `Fixed` (the default) is
    /// today's behaviour exactly; only `PressureAdaptive` reads the pressure
    /// ratio and the decayed credit.
    aimd_mode: AimdMode,
    /// Operator ceiling on the pressure-derived success bar. Only consulted
    /// when `aimd_mode` is [`AimdMode::PressureAdaptive`].
    aimd_max_success_bar: usize,
    /// Ceiling on how long a rate-limit bench lasts, in seconds. `None` = no
    /// cap, which is today's behaviour exactly. The cap is a MAXIMUM: it can
    /// only shorten a bench, never extend one, so an upstream that asks for
    /// 120 s is still benched for the full 120 s unless the cap is lower.
    rate_limit_bench_cap_secs: Option<u64>,
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
        Self::with_max_in_flight_and_aimd(keys, max_in_flight_per_key, AimdMode::Fixed)
    }

    /// The constructor the proxy uses. `mode` selects the recovery rule;
    /// `AimdMode::Fixed` reproduces [`KeyPool::with_max_in_flight`] exactly.
    pub(crate) fn with_max_in_flight_and_aimd(
        keys: Vec<KeyEntry>,
        max_in_flight_per_key: usize,
        aimd_mode: AimdMode,
    ) -> Self {
        Self::with_max_in_flight_aimd_and_bench_cap(keys, max_in_flight_per_key, aimd_mode, None)
    }

    /// The constructor the proxy uses when either knob is set (`None` for
    /// both reproduces [`KeyPool::with_max_in_flight_and_aimd`] exactly).
    /// `rate_limit_bench_cap_secs: None` reproduces
    /// [`KeyPool::with_max_in_flight_and_aimd`] exactly.
    pub(crate) fn with_max_in_flight_aimd_and_bench_cap(
        keys: Vec<KeyEntry>,
        max_in_flight_per_key: usize,
        aimd_mode: AimdMode,
        rate_limit_bench_cap_secs: Option<u64>,
    ) -> Self {
        Self::with_max_in_flight_aimd_bench_cap_and_bar(
            keys,
            max_in_flight_per_key,
            aimd_mode,
            rate_limit_bench_cap_secs,
            self::aimd::AIMD_BAR_MAX,
        )
    }

    /// The full constructor: recovery mode, bench cap and the operator's
    /// ceiling on the derived success bar. The ceiling is only read when the
    /// mode is [`AimdMode::PressureAdaptive`].
    pub(crate) fn with_max_in_flight_aimd_bench_cap_and_bar(
        keys: Vec<KeyEntry>,
        max_in_flight_per_key: usize,
        aimd_mode: AimdMode,
        rate_limit_bench_cap_secs: Option<u64>,
        aimd_max_success_bar: usize,
    ) -> Self {
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
                .map(|_| Mutex::new(KeyWindow::new(max_in_flight_per_key)))
                .collect(),
            max_in_flight_per_key,
            aimd_mode,
            rate_limit_bench_cap_secs,
            aimd_max_success_bar,
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
            let cd = self.cooldowns[idx].lock();
            match *cd {
                Some((expiry, _)) if Instant::now() < expiry => continue, // still cooling
                _ => return Some((self.keys[idx].key.clone(), idx)),
            }
        }
        None
    }

    pub fn next_key_with_permit(&self) -> Result<KeyLease, KeyAcquireError> {
        self.next_key_with_permit_excluding(None)
    }

    /// Pick the next available key that is not `exclude`.
    ///
    /// The exclusion is what makes a retry go to *another* key rather than
    /// cycling back to the one that just stalled: plain round-robin hands the
    /// next attempt the next slot, which is the failed key again as soon as
    /// the pool wraps. Retrying the identical prompt on a fresh key is the
    /// proxy-side half of the same-prompt-as-fresh retry rule.
    ///
    /// A single-key pool cannot satisfy an exclusion, so it degrades to the
    /// unexcluded pick: refusing would turn every stall into an admission
    /// failure, and a stall is observational-only (it never benches the key).
    pub fn next_key_with_permit_excluding(
        &self,
        exclude: Option<usize>,
    ) -> Result<KeyLease, KeyAcquireError> {
        let n = self.keys.len();
        if n == 0 {
            return Err(KeyAcquireError::NoKeys);
        }

        let start = self.index.fetch_add(1, Ordering::Relaxed) % n;
        let now = Instant::now();
        let mut active_seen = false;
        let mut busy_seen = false;

        // Two passes: the first skips the excluded key, the second drops the
        // exclusion so a single-key pool (or a pool whose other keys are all
        // cooling) still yields a lease rather than failing admission.
        for pass in 0..2 {
            for i in 0..n {
                let idx = (start + i) % n;
                if pass == 0 && exclude == Some(idx) {
                    continue;
                }
                let cd = self.cooldowns[idx].lock();
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
