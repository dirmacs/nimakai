use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::debug;

const RING_SIZE: usize = 100;
const LATENCY_DEGRADATION_SAMPLE_FLOOR: usize = 3;
const TIMEOUT_QUARANTINE_FAILURES: u32 = 2;
const TIMEOUT_QUARANTINE_MS: u64 = 60_000;

/// Distinguishes between locally-declared "dead" status and server-side (API-declared) degradation.
///
/// ## Dead (Local)
/// Based on observed behavior: high latency, consecutive failures, excessive output, etc.
/// The model might work fine for others, but it's problematic *for us* — we consider it "dead".
///
/// ## Degraded (Server-Side)
/// NVIDIA explicitly marks the model as degraded and returns `400 DEGRADED function cannot be invoked`.
/// This is a hard block from the API. The model is unavailable for everyone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelStatus {
    /// Model is healthy and operational
    Healthy,
    /// Model is "dead" by local heuristics (latency, failures, etc.)
    Dead,
    /// Model is "degraded" by server-side declaration (NVIDIA API)
    Degraded,
}

/// Result of recording a model request outcome.
/// Used to communicate whether special handling is needed (e.g., server-side degradation).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordOutcome {
    /// Request succeeded
    Success,
    /// Request failed, model may be dead locally
    Failed,
    /// Request failed due to server-side degradation (NVIDIA-declared)
    ServerDegraded,
}

/// Configuration for circuit breakers that detect problematic model behavior
#[derive(Clone, Debug)]
pub struct CircuitBreakerConfig {
    /// Max output tokens before triggering degradation (0 = disabled)
    pub max_output_tokens: u32,
    /// Max repeated n-gram count before triggering degradation (0 = disabled)
    pub max_repetitions: u32,
    /// Max consecutive assistant turns without tool calls (0 = disabled)
    pub max_consecutive_assistant_turns: u32,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            max_output_tokens: 32000,            // ~32K tokens, warn at high output
            max_repetitions: 5,                  // 5+ repeated n-grams triggers degradation
            max_consecutive_assistant_turns: 10, // 10 turns without tools = circuit break
        }
    }
}

struct ModelEntry {
    ring: [f64; RING_SIZE],
    ring_pos: usize,
    ring_len: usize,
    pub total: u64,
    pub success: u64,
    pub consecutive_failures: u32,
    pub last_ms: f64,
    pub output_token_count: u32,
    pub repetition_count: u32,
    pub consecutive_assistant_turns: u32,
    /// Track if this model has been marked as degraded by the server (NVIDIA)
    /// This is separate from local degradation (consecutive failures, high latency, etc.)
    pub server_degraded: bool,
    pub consecutive_timeouts: u32,
    pub timeout_quarantine_until: Option<Instant>,
    pub timeout_probe_in_flight: bool,
}

struct KeyFailureTracker {
    failures: HashMap<String, u32>,
}

impl KeyFailureTracker {
    fn new() -> Self {
        Self {
            failures: HashMap::new(),
        }
    }

    fn record_failure(&mut self, key_label: &str) {
        *self.failures.entry(key_label.to_string()).or_insert(0) += 1;
    }

    fn record_success(&mut self, key_label: &str) {
        self.failures.insert(key_label.to_string(), 0);
    }

    fn all_keys_failed(&self) -> bool {
        self.failures.values().all(|&f| f >= 3)
    }
}

impl ModelEntry {
    fn new() -> Self {
        ModelEntry {
            ring: [0.0; RING_SIZE],
            ring_pos: 0,
            ring_len: 0,
            total: 0,
            success: 0,
            consecutive_failures: 0,
            last_ms: 0.0,
            output_token_count: 0,
            repetition_count: 0,
            consecutive_assistant_turns: 0,
            server_degraded: false,
            consecutive_timeouts: 0,
            timeout_quarantine_until: None,
            timeout_probe_in_flight: false,
        }
    }

    fn push(&mut self, ms: f64) {
        self.ring[self.ring_pos] = ms;
        self.ring_pos = (self.ring_pos + 1) % RING_SIZE;
        if self.ring_len < RING_SIZE {
            self.ring_len += 1;
        }
    }

    fn samples(&self) -> &[f64] {
        &self.ring[..self.ring_len]
    }

    pub fn avg_ms(&self) -> Option<f64> {
        if self.ring_len == 0 {
            return None;
        }
        Some(self.samples().iter().sum::<f64>() / self.ring_len as f64)
    }

    pub fn p95_ms(&self) -> Option<f64> {
        if self.ring_len < 2 {
            return None;
        }
        let mut sorted: Vec<f64> = self.samples().to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((sorted.len() as f64) * 0.95) as usize;
        Some(sorted[idx.min(sorted.len() - 1)])
    }

    pub fn success_rate(&self) -> f64 {
        if self.total == 0 {
            return 100.0;
        }
        (self.success as f64 / self.total as f64) * 100.0
    }

    fn clear_timeout_state(&mut self) {
        self.consecutive_timeouts = 0;
        self.timeout_quarantine_until = None;
        self.timeout_probe_in_flight = false;
    }

    fn record_success(&mut self, ms: f64) {
        self.success += 1;
        self.consecutive_failures = 0;
        self.push(ms);
        self.clear_timeout_state();
    }

    fn record_failure(&mut self) {
        self.consecutive_failures += 1;
        self.consecutive_timeouts = 0;
        self.timeout_quarantine_until = None;
        self.timeout_probe_in_flight = false;
    }

    fn record_timeout(&mut self) {
        self.consecutive_failures += 1;
        self.consecutive_timeouts += 1;
        self.timeout_probe_in_flight = false;
        if self.consecutive_timeouts >= TIMEOUT_QUARANTINE_FAILURES {
            self.timeout_quarantine_until =
                Instant::now().checked_add(Duration::from_millis(TIMEOUT_QUARANTINE_MS));
        }
    }

    fn timeout_quarantine_active(&self, now: Instant) -> bool {
        self.timeout_quarantine_until
            .map(|until| until > now)
            .unwrap_or(false)
    }

    fn timeout_probe_ready(&self, now: Instant) -> bool {
        self.consecutive_timeouts >= TIMEOUT_QUARANTINE_FAILURES
            && !self.timeout_probe_in_flight
            && self
                .timeout_quarantine_until
                .map(|until| until <= now)
                .unwrap_or(false)
    }

    fn mark_timeout_probe_launched(&mut self, now: Instant) -> bool {
        if self.timeout_probe_ready(now) {
            self.timeout_probe_in_flight = true;
            return true;
        }
        false
    }

    fn latency_degraded(&self, spike_threshold_ms: f64) -> bool {
        if self.ring_len < LATENCY_DEGRADATION_SAMPLE_FLOOR {
            return false;
        }
        self.avg_ms()
            .map(|avg| avg > spike_threshold_ms)
            .unwrap_or(false)
    }

    fn circuit_degraded(&self, cb_config: &CircuitBreakerConfig) -> bool {
        (cb_config.max_output_tokens > 0 && self.output_token_count > cb_config.max_output_tokens)
            || (cb_config.max_repetitions > 0 && self.repetition_count >= cb_config.max_repetitions)
            || (cb_config.max_consecutive_assistant_turns > 0
                && self.consecutive_assistant_turns >= cb_config.max_consecutive_assistant_turns)
    }

    fn availability_degraded(&self, cb_config: &CircuitBreakerConfig) -> bool {
        self.consecutive_failures >= 3 || self.circuit_degraded(cb_config)
    }

    /// Calculate dynamic timeout for this model based on historical P95.
    /// Returns timeout_ms with buffer: p95 + max(2000ms, p95 * 0.5), capped at max_timeout.
    pub fn dynamic_timeout_ms(&self, max_timeout_ms: u64) -> u64 {
        self.dynamic_timeout_ms_with_policy(max_timeout_ms, 1000, 2)
    }

    pub fn dynamic_timeout_ms_with_policy(
        &self,
        max_timeout_ms: u64,
        min_timeout_ms: u64,
        sample_floor: usize,
    ) -> u64 {
        if self.ring_len < sample_floor {
            return max_timeout_ms;
        }
        let Some(p95) = self.p95_ms() else {
            return max_timeout_ms;
        };
        let buffer = (p95 * 0.5).max(2000.0);
        let timeout = (p95 + buffer).min(max_timeout_ms as f64) as u64;
        timeout.max(min_timeout_ms.max(1000))
    }

    pub fn is_degraded(
        &self,
        model_id: &str,
        spike_threshold_ms: f64,
        cb_config: &CircuitBreakerConfig,
    ) -> bool {
        // Server-side degradation (NVIDIA-declared) takes precedence
        if self.server_degraded {
            return true;
        }
        if self.timeout_quarantine_active(Instant::now()) {
            return true;
        }
        if self.consecutive_failures >= 3 {
            return true;
        }
        if self.latency_degraded(spike_threshold_ms) {
            return true;
        }
        if cb_config.max_output_tokens > 0 && self.output_token_count > cb_config.max_output_tokens
        {
            return true;
        }
        if cb_config.max_repetitions > 0 && self.repetition_count >= cb_config.max_repetitions {
            return true;
        }
        if cb_config.max_consecutive_assistant_turns > 0
            && self.consecutive_assistant_turns >= cb_config.max_consecutive_assistant_turns
        {
            debug!(
            "Circuit breaker triggered for model '{}': consecutive_assistant_turns={} >= max_consecutive_assistant_turns={}",
            model_id,
            self.consecutive_assistant_turns,
            cb_config.max_consecutive_assistant_turns
        );
            return true;
        }
        false
    }

    /// Mark this model as degraded by the server (NVIDIA API declaration).
    /// This is separate from local degradation heuristics.
    pub fn mark_server_degraded(&mut self) {
        self.server_degraded = true;
        // Also increment consecutive failures to ensure local degradation tracking stays in sync
        self.consecutive_failures = self.consecutive_failures.saturating_add(3);
    }
}

/// Snapshot exported to /stats endpoint - per model+key combination.
pub struct ModelSnapshot {
    pub id: String,
    pub key_label: Option<String>,
    pub avg_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    pub total: u64,
    pub success: u64,
    pub success_rate: f64,
    pub sample_count: usize,
    pub consecutive_failures: u32,
    pub degraded: bool,
}

/// Thread-safe store of per-model stats. Shared across request handlers.
/// Also tracks per-key failure counts to detect when ALL keys are failing
/// for a given model (different keys can have different outcomes).
pub struct ModelStatsStore {
    inner: Mutex<HashMap<String, ModelEntry>>,
    pub spike_threshold_ms: f64,
    key_failures: Mutex<HashMap<String, KeyFailureTracker>>,
    circuit_breaker: CircuitBreakerConfig,
}

impl ModelStatsStore {
    pub fn new(spike_threshold_ms: f64) -> Self {
        Self::with_circuit_breaker(spike_threshold_ms, CircuitBreakerConfig::default())
    }

    pub fn with_circuit_breaker(spike_threshold_ms: f64, cb_config: CircuitBreakerConfig) -> Self {
        ModelStatsStore {
            inner: Mutex::new(HashMap::new()),
            spike_threshold_ms,
            key_failures: Mutex::new(HashMap::new()),
            circuit_breaker: cb_config,
        }
    }

    pub fn circuit_breaker_config(&self) -> CircuitBreakerConfig {
        self.circuit_breaker.clone()
    }

    pub fn record(&self, model_id: &str, ms: f64, ok: bool) {
        let mut map = self.inner.lock().unwrap();
        let entry = map
            .entry(model_id.to_string())
            .or_insert_with(ModelEntry::new);
        entry.total += 1;
        entry.last_ms = ms;
        if ok {
            entry.record_success(ms);
        } else {
            entry.record_failure();
        }
    }

    pub fn record_with_circuit_breaker(
        &self,
        model_id: &str,
        ms: f64,
        ok: bool,
        output_tokens: u32,
        repetition_count: u32,
        had_tool_call: bool,
    ) {
        let mut map = self.inner.lock().unwrap();
        let entry = map
            .entry(model_id.to_string())
            .or_insert_with(ModelEntry::new);
        entry.total += 1;
        entry.last_ms = ms;

        if ok {
            entry.record_success(ms);
            entry.output_token_count = output_tokens;
            entry.repetition_count = repetition_count;
            if had_tool_call {
                entry.consecutive_assistant_turns = 0;
            } else {
                entry.consecutive_assistant_turns += 1;
            }
        } else {
            entry.record_failure();
        }
    }

    pub fn record_with_key(&self, model_id: &str, key_label: &str, ms: f64, ok: bool) {
        self.record(model_id, ms, ok);
        let mut key_map = self.key_failures.lock().unwrap();
        let tracker = key_map
            .entry(model_id.to_string())
            .or_insert_with(KeyFailureTracker::new);
        if ok {
            tracker.record_success(key_label);
        } else {
            tracker.record_failure(key_label);
        }
    }

    pub fn record_timeout(&self, model_id: &str, ms: f64) {
        let mut map = self.inner.lock().unwrap();
        let entry = map
            .entry(model_id.to_string())
            .or_insert_with(ModelEntry::new);
        entry.total += 1;
        entry.last_ms = ms;
        entry.record_timeout();
    }

    pub fn record_timeout_with_key(&self, model_id: &str, key_label: &str, ms: f64) {
        self.record_timeout(model_id, ms);
        let mut key_map = self.key_failures.lock().unwrap();
        let tracker = key_map
            .entry(model_id.to_string())
            .or_insert_with(KeyFailureTracker::new);
        tracker.record_failure(key_label);
    }

    /// Record that a model has been marked as degraded by the server (NVIDIA API).
    /// This immediately marks the model as unavailable for routing until recovery.
    pub fn record_server_degraded(&self, model_id: &str) {
        let mut map = self.inner.lock().unwrap();
        let entry = map
            .entry(model_id.to_string())
            .or_insert_with(ModelEntry::new);
        entry.mark_server_degraded();
        eprintln!(
            "[nimaproxy] Model '{}' marked as server-degraded (NVIDIA API block)",
            model_id
        );
    }

    pub fn record_hard_error(&self, model_id: &str, reason: &str) {
        let mut map = self.inner.lock().unwrap();
        let entry = map
            .entry(model_id.to_string())
            .or_insert_with(ModelEntry::new);
        entry.mark_server_degraded();
        eprintln!(
            "[nimaproxy] Model '{}' quarantined after hard error: {}",
            model_id,
            &reason[..reason.len().min(240)]
        );
    }

    pub fn all_keys_failing_for_model(&self, model_id: &str) -> bool {
        let key_map = self.key_failures.lock().unwrap();
        key_map
            .get(model_id)
            .map(|t| t.all_keys_failed())
            .unwrap_or(false)
    }

    /// Pick the best model from `candidates` for the next request.
    ///
    /// Priority:
    /// 1. Models with < 3 samples (untried) — round-robin among them first
    /// 2. Non-degraded models sorted by avg_ms ascending
    /// 3. If all degraded, pick lowest avg_ms anyway (graceful degradation)
    ///
    /// Note: Server-degraded models (NVIDIA API block) are always excluded.
    pub fn best_model(&self, candidates: &[String]) -> Option<String> {
        if candidates.is_empty() {
            return None;
        }
        let map = self.inner.lock().unwrap();
        let threshold = self.spike_threshold_ms;

        // Partition into (untried, tried_ok, tried_degraded)
        let mut untried: Vec<&String> = Vec::new();
        let mut ok: Vec<(&String, f64)> = Vec::new();
        let mut latency_degraded: Vec<(&String, f64)> = Vec::new();
        let mut availability_degraded: Vec<(&String, f64)> = Vec::new();
        let now = Instant::now();

        for m in candidates {
            match map.get(m) {
                None => untried.push(m),
                Some(e) => {
                    // Server-degraded models are always excluded
                    if e.server_degraded {
                        continue; // Skip this model entirely
                    }
                    if e.timeout_quarantine_active(now) {
                        continue;
                    }
                    if e.timeout_probe_ready(now) {
                        availability_degraded.push((m, e.avg_ms().unwrap_or(f64::MAX)));
                        continue;
                    }
                    // Models with < 3 total samples are "untried" — round-robin among them first
                    // Use total (includes failures) not ring_len (only successes) to properly detect untried state
                    if e.total == 0 || e.total < 3 {
                        untried.push(m);
                    } else if e.availability_degraded(&self.circuit_breaker) {
                        availability_degraded.push((m, e.avg_ms().unwrap_or(f64::MAX)));
                    } else if e.latency_degraded(threshold) {
                        latency_degraded.push((m, e.avg_ms().unwrap_or(f64::MAX)));
                    } else {
                        ok.push((m, e.avg_ms().unwrap_or(f64::MAX)));
                    }
                }
            }
        }

        if !untried.is_empty() {
            // Round-robin among untried using the total request count as seed
            let total: u64 = map.values().map(|e| e.total).sum();
            return Some(untried[(total as usize) % untried.len()].clone());
        }

        if !ok.is_empty() {
            ok.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
            return Some(ok[0].0.clone());
        }

        if !latency_degraded.is_empty() {
            latency_degraded
                .sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
            return Some(latency_degraded[0].0.clone());
        }

        // If everything available is actively failing, still return the least bad
        // candidate rather than making direct auto-routing unusable.
        if !availability_degraded.is_empty() {
            availability_degraded
                .sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
            return Some(availability_degraded[0].0.clone());
        }

        None
    }

    pub fn racing_candidates(&self, candidates: &[String], max: usize) -> Vec<String> {
        let mut map = self.inner.lock().unwrap();
        let threshold = self.spike_threshold_ms;
        let key_failures = self.key_failures.lock().unwrap();
        let now = Instant::now();
        let mut healthy: Vec<(String, Option<f64>)> = Vec::new();
        let mut latency_degraded: Vec<(String, Option<f64>)> = Vec::new();
        let mut availability_degraded: Vec<(String, Option<f64>)> = Vec::new();
        let mut probes: Vec<(String, Option<f64>)> = Vec::new();

        for m in candidates {
            let all_keys_failed = key_failures
                .get(m)
                .map(|t| t.all_keys_failed())
                .unwrap_or(false);
            if all_keys_failed {
                continue;
            }

            if let Some(e) = map.get(m) {
                // Server-degraded models are explicit upstream blocks and should
                // stay out of racing until the server-side condition clears.
                if e.server_degraded {
                    continue;
                }

                if e.timeout_quarantine_active(now) {
                    continue;
                }
                if e.timeout_probe_ready(now) {
                    probes.push((m.clone(), e.avg_ms()));
                    continue;
                }
                if e.availability_degraded(&self.circuit_breaker) {
                    availability_degraded.push((m.clone(), e.avg_ms()));
                } else if e.latency_degraded(threshold) {
                    latency_degraded.push((m.clone(), e.avg_ms()));
                } else {
                    healthy.push((m.clone(), e.avg_ms()));
                }
            } else {
                healthy.push((m.clone(), None));
            }
        }

        let by_latency = |a: &(String, Option<f64>), b: &(String, Option<f64>)| match (a.1, b.1) {
            (Some(av), Some(bv)) => av.partial_cmp(&bv).unwrap_or(std::cmp::Ordering::Equal),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        };

        healthy.sort_by(by_latency);
        latency_degraded.sort_by(by_latency);
        availability_degraded.sort_by(by_latency);
        probes.sort_by(by_latency);

        let out = select_candidates_with_probe(
            healthy,
            latency_degraded,
            availability_degraded,
            probes.clone(),
            max,
        );
        for model in &out {
            if probes.iter().any(|(probe, _)| probe == model) {
                if let Some(entry) = map.get_mut(model) {
                    entry.mark_timeout_probe_launched(now);
                }
            }
        }
        out
    }

    pub fn racing_candidates_tiered(
        &self,
        fast_candidates: &[String],
        fallback_candidates: &[String],
        max: usize,
    ) -> Vec<String> {
        let mut map = self.inner.lock().unwrap();
        let threshold = self.spike_threshold_ms;
        let key_failures = self.key_failures.lock().unwrap();
        let now = Instant::now();

        let collect = |candidates: &[String], map: &HashMap<String, ModelEntry>| {
            let mut healthy: Vec<(String, Option<f64>)> = Vec::new();
            let mut latency_degraded: Vec<(String, Option<f64>)> = Vec::new();
            let mut availability_degraded: Vec<(String, Option<f64>)> = Vec::new();
            let mut probes: Vec<(String, Option<f64>)> = Vec::new();
            let mut seen = HashSet::new();

            for m in candidates {
                if !seen.insert(m.clone()) {
                    continue;
                }
                let all_keys_failed = key_failures
                    .get(m)
                    .map(|t| t.all_keys_failed())
                    .unwrap_or(false);
                if all_keys_failed {
                    continue;
                }

                if let Some(e) = map.get(m) {
                    if e.server_degraded {
                        continue;
                    }
                    if e.timeout_quarantine_active(now) {
                        continue;
                    }
                    if e.timeout_probe_ready(now) {
                        probes.push((m.clone(), e.avg_ms()));
                        continue;
                    }
                    if e.availability_degraded(&self.circuit_breaker) {
                        availability_degraded.push((m.clone(), e.avg_ms()));
                    } else if e.latency_degraded(threshold) {
                        latency_degraded.push((m.clone(), e.avg_ms()));
                    } else {
                        healthy.push((m.clone(), e.avg_ms()));
                    }
                } else {
                    healthy.push((m.clone(), None));
                }
            }

            let by_latency = |a: &(String, Option<f64>), b: &(String, Option<f64>)| match (a.1, b.1)
            {
                (Some(av), Some(bv)) => av.partial_cmp(&bv).unwrap_or(std::cmp::Ordering::Equal),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            };

            healthy.sort_by(by_latency);
            latency_degraded.sort_by(by_latency);
            availability_degraded.sort_by(by_latency);
            probes.sort_by(by_latency);
            (healthy, latency_degraded, availability_degraded, probes)
        };

        let (fast_healthy, fast_latency_degraded, fast_availability_degraded, fast_probes) =
            collect(fast_candidates, &map);
        let (
            fallback_healthy,
            fallback_latency_degraded,
            fallback_availability_degraded,
            fallback_probes,
        ) = collect(fallback_candidates, &map);

        let mut healthy = Vec::new();
        let mut latency_degraded = Vec::new();
        let mut availability_degraded = Vec::new();
        let mut probes = Vec::new();
        let mut seen = HashSet::new();
        for bucket in [fast_healthy, fallback_healthy] {
            for (model, _) in bucket {
                if seen.insert(model.clone()) {
                    let avg = map.get(&model).and_then(|e| e.avg_ms());
                    healthy.push((model, avg));
                }
            }
        }
        for bucket in [fast_latency_degraded, fallback_latency_degraded] {
            for (model, _) in bucket {
                if seen.insert(model.clone()) {
                    let avg = map.get(&model).and_then(|e| e.avg_ms());
                    latency_degraded.push((model, avg));
                }
            }
        }
        for bucket in [fast_availability_degraded, fallback_availability_degraded] {
            for (model, _) in bucket {
                if seen.insert(model.clone()) {
                    let avg = map.get(&model).and_then(|e| e.avg_ms());
                    availability_degraded.push((model, avg));
                }
            }
        }
        for bucket in [fast_probes, fallback_probes] {
            for (model, _) in bucket {
                if seen.insert(model.clone()) {
                    let avg = map.get(&model).and_then(|e| e.avg_ms());
                    probes.push((model, avg));
                }
            }
        }

        let out = select_candidates_with_probe(
            healthy,
            latency_degraded,
            availability_degraded,
            probes.clone(),
            max,
        );
        for model in &out {
            if probes.iter().any(|(probe, _)| probe == model) {
                if let Some(entry) = map.get_mut(model) {
                    entry.mark_timeout_probe_launched(now);
                }
            }
        }
        out
    }

    /// Export current stats for all tracked models (for /stats endpoint).
    pub fn snapshot(&self) -> Vec<ModelSnapshot> {
        let map = self.inner.lock().unwrap();
        let threshold = self.spike_threshold_ms;
        let mut out: Vec<ModelSnapshot> = map
            .iter()
            .map(|(id, e)| ModelSnapshot {
                id: id.clone(),
                key_label: None,
                avg_ms: e.avg_ms(),
                p95_ms: e.p95_ms(),
                total: e.total,
                success: e.success,
                success_rate: e.success_rate(),
                sample_count: e.ring_len,
                consecutive_failures: e.consecutive_failures,
                degraded: e.is_degraded(id, threshold, &self.circuit_breaker),
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    pub fn get_key_failure_summary(&self) -> Vec<(String, Vec<(String, u32)>)> {
        let key_map = self.key_failures.lock().unwrap();
        key_map
            .iter()
            .map(|(model, tracker)| {
                let failures: Vec<(String, u32)> = tracker
                    .failures
                    .iter()
                    .map(|(k, v)| (k.clone(), *v))
                    .collect();
                (model.clone(), failures)
            })
            .collect()
    }

    pub fn get_model_timeout(&self, model_id: &str, max_timeout_ms: u64) -> u64 {
        let map = self.inner.lock().unwrap();
        match map.get(model_id) {
            Some(e) => e.dynamic_timeout_ms(max_timeout_ms),
            None => max_timeout_ms,
        }
    }

    pub fn get_model_timeout_with_policy(
        &self,
        model_id: &str,
        max_timeout_ms: u64,
        min_timeout_ms: u64,
        sample_floor: usize,
    ) -> u64 {
        let map = self.inner.lock().unwrap();
        match map.get(model_id) {
            Some(e) => {
                e.dynamic_timeout_ms_with_policy(max_timeout_ms, min_timeout_ms, sample_floor)
            }
            None => max_timeout_ms,
        }
    }

    #[cfg(test)]
    pub fn force_timeout_probe_ready_for_test(&self, model_id: &str) {
        let mut map = self.inner.lock().unwrap();
        if let Some(entry) = map.get_mut(model_id) {
            entry.timeout_quarantine_until = Instant::now().checked_sub(Duration::from_millis(1));
            entry.timeout_probe_in_flight = false;
        }
    }

    #[cfg(test)]
    pub fn timeout_quarantined_for_test(&self, model_id: &str) -> Option<bool> {
        let map = self.inner.lock().unwrap();
        map.get(model_id)
            .map(|entry| entry.timeout_quarantine_active(Instant::now()))
    }
}

fn select_candidates_with_probe(
    healthy: Vec<(String, Option<f64>)>,
    latency_degraded: Vec<(String, Option<f64>)>,
    availability_degraded: Vec<(String, Option<f64>)>,
    probes: Vec<(String, Option<f64>)>,
    max: usize,
) -> Vec<String> {
    if max == 0 {
        return Vec::new();
    }

    let mut out: Vec<String> = healthy.into_iter().take(max).map(|(m, _)| m).collect();
    let healthy_out_count = out.len();
    for (model, _) in latency_degraded {
        if out.len() >= max {
            break;
        }
        if !out.iter().any(|m| m == &model) {
            out.push(model);
        }
    }
    let latency_added = out.len() > healthy_out_count;

    if let Some((probe, _)) = probes.into_iter().next() {
        if out.is_empty() {
            out.push(probe);
        } else if out.len() < max {
            if !out.iter().any(|m| m == &probe) {
                out.push(probe);
            }
        } else if max >= 2 && !latency_added && !out.iter().any(|m| m == &probe) {
            out.pop();
            out.push(probe);
        }
    }

    for (model, _) in availability_degraded {
        if out.len() >= max {
            break;
        }
        if !out.iter().any(|m| m == &model) {
            out.push(model);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ring_buffer_wrap() {
        let mut entry = ModelEntry::new();

        // Fill beyond RING_SIZE (100)
        for i in 0..150 {
            entry.push((i as f64) * 10.0);
        }

        // Should have exactly 100 samples (ring wrapped)
        assert_eq!(entry.ring_len, 100);
    }

    #[test]
    fn test_avg_ms() {
        let mut entry = ModelEntry::new();
        entry.push(100.0);
        entry.push(200.0);
        entry.push(300.0);

        let avg = entry.avg_ms().unwrap();
        assert!((avg - 200.0).abs() < 0.001);
    }

    #[test]
    fn test_avg_ms_empty() {
        let entry = ModelEntry::new();
        assert_eq!(entry.avg_ms(), None);
    }

    #[test]
    fn test_p95_calculation() {
        let mut entry = ModelEntry::new();

        // 100 samples: 1-100
        for i in 1..=100 {
            entry.push(i as f64);
        }

        let p95 = entry.p95_ms().unwrap();
        // 95th percentile should be >= 95
        assert!(p95 >= 95.0);
    }

    #[test]
    fn test_p95_insufficient_samples() {
        let entry = ModelEntry::new();
        assert_eq!(entry.p95_ms(), None);

        let mut entry2 = ModelEntry::new();
        entry2.push(100.0);
        assert_eq!(entry2.p95_ms(), None);
    }

    #[test]
    fn test_success_rate() {
        let mut entry = ModelEntry::new();
        entry.total = 10;
        entry.success = 8;

        let rate = entry.success_rate();
        assert!((rate - 80.0).abs() < 0.001);
    }

    #[test]
    fn test_success_rate_zero_total() {
        let entry = ModelEntry::new();
        assert_eq!(entry.success_rate(), 100.0);
    }

    #[test]
    fn test_is_degraded_consecutive_failures() {
        let mut entry = ModelEntry::new();
        entry.consecutive_failures = 3;
        let cb = CircuitBreakerConfig::default();

        assert!(entry.is_degraded("test-model", 3000.0, &cb));
    }

    #[test]
    fn test_is_degraded_high_latency() {
        let mut entry = ModelEntry::new();
        entry.consecutive_failures = 1;
        entry.push(5000.0);
        entry.push(5000.0);
        entry.push(5000.0);
        let cb = CircuitBreakerConfig::default();

        assert!(entry.is_degraded("test-model", 3000.0, &cb));
    }

    #[test]
    fn test_is_not_degraded_by_single_slow_latency_sample() {
        let mut entry = ModelEntry::new();
        entry.push(9000.0);
        let cb = CircuitBreakerConfig::default();

        assert!(!entry.is_degraded("test-model", 3000.0, &cb));
    }

    #[test]
    fn test_not_degraded() {
        let mut entry = ModelEntry::new();
        entry.push(500.0);
        entry.push(600.0);
        entry.push(550.0);
        let cb = CircuitBreakerConfig::default();

        assert!(!entry.is_degraded("test-model", 3000.0, &cb));
    }

    #[test]
    fn test_is_degraded_token_threshold() {
        let mut entry = ModelEntry::new();
        entry.output_token_count = 35000;
        let cb = CircuitBreakerConfig {
            max_output_tokens: 32000,
            max_repetitions: 0,
            max_consecutive_assistant_turns: 0,
        };

        assert!(entry.is_degraded("test-model", 3000.0, &cb));
    }

    #[test]
    fn test_is_degraded_repetition_threshold() {
        let mut entry = ModelEntry::new();
        entry.repetition_count = 7;
        let cb = CircuitBreakerConfig {
            max_output_tokens: 0,
            max_repetitions: 5,
            max_consecutive_assistant_turns: 0,
        };

        assert!(entry.is_degraded("test-model", 3000.0, &cb));
    }

    #[test]
    fn test_is_degraded_consecutive_turns_threshold() {
        let mut entry = ModelEntry::new();
        entry.consecutive_assistant_turns = 12;
        let cb = CircuitBreakerConfig {
            max_output_tokens: 0,
            max_repetitions: 0,
            max_consecutive_assistant_turns: 10,
        };

        assert!(entry.is_degraded("test-model", 3000.0, &cb));
    }

    #[test]
    fn test_circuit_breaker_disabled() {
        let mut entry = ModelEntry::new();
        entry.output_token_count = 35000;
        entry.repetition_count = 7;
        entry.consecutive_assistant_turns = 12;
        let cb = CircuitBreakerConfig {
            max_output_tokens: 0,
            max_repetitions: 0,
            max_consecutive_assistant_turns: 0,
        };

        assert!(!entry.is_degraded("test-model", 3000.0, &cb));
    }

    #[test]
    fn test_model_stats_store_record_success() {
        let store = ModelStatsStore::new(3000.0);

        store.record("test-model", 500.0, true);

        let snapshot = store.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].id, "test-model");
        assert_eq!(snapshot[0].total, 1);
        assert_eq!(snapshot[0].success, 1);
    }

    #[test]
    fn test_model_stats_store_record_failure() {
        let store = ModelStatsStore::new(3000.0);

        store.record("test-model", 0.0, false);

        let snapshot = store.snapshot();
        assert_eq!(snapshot[0].total, 1);
        assert_eq!(snapshot[0].success, 0);
        assert_eq!(snapshot[0].consecutive_failures, 1);
    }

    #[test]
    fn test_best_model_untried_first() {
        let store = ModelStatsStore::new(3000.0);

        let candidates = vec!["model-a".to_string(), "model-b".to_string()];

        // Both untried - should return one (round-robin via total count)
        let best = store.best_model(&candidates).unwrap();
        assert!(candidates.contains(&best));
    }

    #[test]
    fn test_best_model_skips_degraded() {
        let store = ModelStatsStore::new(3000.0);

        // Mark model-a as degraded
        store.record("model-a", 5000.0, true);
        store.record("model-a", 5000.0, true);
        store.record("model-a", 5000.0, true);

        // model-b is healthy
        store.record("model-b", 500.0, true);

        let candidates = vec!["model-a".to_string(), "model-b".to_string()];
        let best = store.best_model(&candidates).unwrap();

        // Should pick model-b (not degraded)
        assert_eq!(best, "model-b");
    }

    #[test]
    fn test_best_model_latency_aware() {
        let store = ModelStatsStore::new(3000.0);

        store.record("fast-model", 200.0, true);
        store.record("fast-model", 250.0, true);
        store.record("fast-model", 300.0, true);

        store.record("slow-model", 2000.0, true);
        store.record("slow-model", 2500.0, true);
        store.record("slow-model", 3000.0, true);

        let candidates = vec!["fast-model".to_string(), "slow-model".to_string()];
        let best = store.best_model(&candidates).unwrap();

        // Should pick fast-model (lower avg)
        assert_eq!(best, "fast-model");
    }

    #[test]
    fn test_best_model_empty_candidates() {
        let store = ModelStatsStore::new(3000.0);
        assert_eq!(store.best_model(&[]), None);
    }

    #[test]
    fn test_snapshot_fields() {
        let store = ModelStatsStore::new(3000.0);

        store.record("model-a", 100.0, true);
        store.record("model-a", 200.0, true);
        store.record("model-a", 300.0, true);

        let snap = &store.snapshot()[0];
        assert_eq!(snap.id, "model-a");
        assert!(snap.avg_ms.is_some());
        assert!(snap.p95_ms.is_some());
        assert_eq!(snap.total, 3);
        assert_eq!(snap.success, 3);
        assert!(snap.sample_count > 0);
    }

    #[test]
    fn test_record_with_circuit_breaker_tracks_tokens() {
        let store = ModelStatsStore::new(3000.0);
        store.record_with_circuit_breaker("model-a", 500.0, true, 15000, 2, true);

        let snap = &store.snapshot()[0];
        assert_eq!(snap.degraded, false);
    }

    #[test]
    fn test_record_with_circuit_breaker_degrades_on_high_tokens() {
        let cb_config = CircuitBreakerConfig {
            max_output_tokens: 10000,
            ..Default::default()
        };
        let store = ModelStatsStore::with_circuit_breaker(3000.0, cb_config);
        store.record_with_circuit_breaker("model-a", 500.0, true, 15000, 2, true);

        let snap = &store.snapshot()[0];
        assert!(
            snap.degraded,
            "Model should be degraded when output tokens exceed threshold"
        );
    }

    #[test]
    fn test_record_with_circuit_breaker_degrades_on_repetitions() {
        let cb_config = CircuitBreakerConfig {
            max_output_tokens: 0,
            max_repetitions: 3,
            max_consecutive_assistant_turns: 0,
        };
        let store = ModelStatsStore::with_circuit_breaker(3000.0, cb_config);
        store.record_with_circuit_breaker("model-a", 500.0, true, 500, 5, true);

        let snap = &store.snapshot()[0];
        assert!(
            snap.degraded,
            "Model should be degraded when repetition count exceeds threshold"
        );
    }

    #[test]
    fn test_record_with_circuit_breaker_degrades_on_no_tool_calls() {
        let cb_config = CircuitBreakerConfig {
            max_output_tokens: 0,
            max_repetitions: 0,
            max_consecutive_assistant_turns: 3,
        };
        let store = ModelStatsStore::with_circuit_breaker(3000.0, cb_config);
        store.record_with_circuit_breaker("model-a", 500.0, true, 500, 0, false);
        store.record_with_circuit_breaker("model-a", 500.0, true, 500, 0, false);
        store.record_with_circuit_breaker("model-a", 500.0, true, 500, 0, false);

        let snap = &store.snapshot()[0];
        assert!(
            snap.degraded,
            "Model should be degraded after max consecutive assistant turns without tool calls"
        );
    }

    #[test]
    fn test_record_with_circuit_breaker_resets_on_tool_call() {
        let cb_config = CircuitBreakerConfig {
            max_output_tokens: 0,
            max_repetitions: 0,
            max_consecutive_assistant_turns: 3,
        };
        let store = ModelStatsStore::with_circuit_breaker(3000.0, cb_config);
        store.record_with_circuit_breaker("model-a", 500.0, true, 500, 0, false);
        store.record_with_circuit_breaker("model-a", 500.0, true, 500, 0, false);
        store.record_with_circuit_breaker("model-a", 500.0, true, 500, 0, true);

        let snap = &store.snapshot()[0];
        assert!(
            !snap.degraded,
            "Model should NOT be degraded after tool call resets counter"
        );
    }

    #[test]
    fn test_best_model_skips_circuit_breaker_degraded() {
        let cb_config = CircuitBreakerConfig {
            max_output_tokens: 1000,
            ..Default::default()
        };
        let store = ModelStatsStore::with_circuit_breaker(3000.0, cb_config);

        store.record_with_circuit_breaker("model-a", 500.0, true, 1500, 0, true);
        store.record_with_circuit_breaker("model-a", 500.0, true, 1500, 0, true);
        store.record_with_circuit_breaker("model-a", 500.0, true, 1500, 0, true);
        store.record_with_circuit_breaker("model-b", 500.0, true, 500, 0, true);
        store.record_with_circuit_breaker("model-b", 500.0, true, 500, 0, true);
        store.record_with_circuit_breaker("model-b", 500.0, true, 500, 0, true);

        let candidates = vec!["model-a".to_string(), "model-b".to_string()];
        let best = store.best_model(&candidates).unwrap();
        assert_eq!(best, "model-b");
    }

    #[test]
    fn test_record_with_key_tracks_per_key_failures() {
        let store = ModelStatsStore::new(3000.0);

        store.record_with_key("model-a", "key-a", 500.0, false);
        store.record_with_key("model-a", "key-a", 500.0, false);
        store.record_with_key("model-a", "key-a", 500.0, false);
        store.record_with_key("model-a", "key-b", 500.0, true);

        assert!(!store.all_keys_failing_for_model("model-a"));
    }

    #[test]
    fn test_all_keys_failing_when_all_keys_have_3_failures() {
        let store = ModelStatsStore::new(3000.0);

        store.record_with_key("model-a", "key-a", 500.0, false);
        store.record_with_key("model-a", "key-a", 500.0, false);
        store.record_with_key("model-a", "key-a", 500.0, false);
        store.record_with_key("model-a", "key-b", 500.0, false);
        store.record_with_key("model-a", "key-b", 500.0, false);
        store.record_with_key("model-a", "key-b", 500.0, false);

        assert!(store.all_keys_failing_for_model("model-a"));
    }

    #[test]
    fn test_all_keys_failing_returns_false_for_unknown_model() {
        let store = ModelStatsStore::new(3000.0);
        assert!(!store.all_keys_failing_for_model("unknown-model"));
    }

    #[test]
    fn test_record_with_key_updates_consecutive_failures() {
        let store = ModelStatsStore::new(3000.0);

        store.record_with_key("model-a", "key-a", 500.0, false);
        store.record_with_key("model-a", "key-a", 500.0, false);

        let snap = store.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].consecutive_failures, 2);

        store.record_with_key("model-a", "key-a", 500.0, true);
        let snap = store.snapshot();
        assert_eq!(snap[0].consecutive_failures, 0);
    }

    #[test]
    fn test_record_with_key_success_increments_success_count() {
        let store = ModelStatsStore::new(3000.0);

        store.record_with_key("model-a", "key-a", 500.0, true);
        store.record_with_key("model-a", "key-a", 500.0, true);
        store.record_with_key("model-a", "key-a", 500.0, false);

        let snap = store.snapshot();
        assert_eq!(snap[0].success, 2);
        assert_eq!(snap[0].total, 3);
    }

    #[test]
    fn test_racing_candidates_skips_all_keys_failed() {
        let store = ModelStatsStore::new(3000.0);

        store.record_with_key("model-a", "key-a", 500.0, false);
        store.record_with_key("model-a", "key-a", 500.0, false);
        store.record_with_key("model-a", "key-a", 500.0, false);
        store.record_with_key("model-a", "key-b", 500.0, false);
        store.record_with_key("model-a", "key-b", 500.0, false);
        store.record_with_key("model-a", "key-b", 500.0, false);
        store.record_with_key("model-b", "key-a", 500.0, true);

        let candidates = vec!["model-a".to_string(), "model-b".to_string()];
        let viable = store.racing_candidates(&candidates, 2);

        assert!(viable.contains(&"model-b".to_string()));
        assert!(!viable.contains(&"model-a".to_string()));
    }

    #[test]
    fn test_timeout_quarantine_excludes_model_before_probe_window() {
        let store = ModelStatsStore::new(3000.0);

        store.record_timeout("flaky", 15000.0);
        store.record_timeout("flaky", 15000.0);
        for _ in 0..3 {
            store.record("healthy", 500.0, true);
        }

        let candidates = vec!["flaky".to_string(), "healthy".to_string()];
        let racing = store.racing_candidates(&candidates, 2);
        let best = store.best_model(&candidates).unwrap();
        let snapshot = store.snapshot();
        let flaky = snapshot.iter().find(|s| s.id == "flaky").unwrap();

        assert_eq!(racing, vec!["healthy".to_string()]);
        assert_eq!(best, "healthy");
        assert!(flaky.degraded);
    }

    #[test]
    fn test_timeout_quarantine_allows_single_half_open_probe() {
        let store = ModelStatsStore::new(3000.0);

        store.record_timeout("flaky-a", 15000.0);
        store.record_timeout("flaky-a", 15000.0);
        store.record_timeout("flaky-b", 15000.0);
        store.record_timeout("flaky-b", 15000.0);
        store.force_timeout_probe_ready_for_test("flaky-a");
        store.force_timeout_probe_ready_for_test("flaky-b");

        for _ in 0..3 {
            store.record("healthy-a", 500.0, true);
            store.record("healthy-b", 700.0, true);
            store.record("healthy-c", 900.0, true);
        }

        let candidates = vec![
            "healthy-a".to_string(),
            "healthy-b".to_string(),
            "healthy-c".to_string(),
            "flaky-a".to_string(),
            "flaky-b".to_string(),
        ];
        let first = store.racing_candidates(&candidates, 3);
        let probe_count = first.iter().filter(|m| m.starts_with("flaky-")).count();
        let second = store.racing_candidates(&candidates, 3);
        let second_probe_count = second.iter().filter(|m| m.starts_with("flaky-")).count();

        assert_eq!(first.len(), 3);
        assert_eq!(probe_count, 1);
        assert_eq!(second_probe_count, 1);
        assert_ne!(
            first.iter().find(|m| m.starts_with("flaky-")),
            second.iter().find(|m| m.starts_with("flaky-"))
        );
    }

    #[test]
    fn test_success_clears_timeout_quarantine() {
        let store = ModelStatsStore::new(3000.0);

        store.record_timeout("flaky", 15000.0);
        store.record_timeout("flaky", 15000.0);
        store.force_timeout_probe_ready_for_test("flaky");
        assert!(store
            .racing_candidates(&["flaky".to_string()], 1)
            .contains(&"flaky".to_string()));

        store.record("flaky", 400.0, true);
        assert_eq!(store.timeout_quarantined_for_test("flaky"), Some(false));
    }

    #[test]
    fn test_racing_candidates_prioritize_slow_success_over_failing_model() {
        let store = ModelStatsStore::new(3000.0);

        for _ in 0..3 {
            store.record("healthy", 500.0, true);
            store.record("slow-success", 5000.0, true);
            store.record("previously-fast-failing", 100.0, true);
        }
        for _ in 0..3 {
            store.record("previously-fast-failing", 100.0, false);
        }

        let candidates = vec![
            "previously-fast-failing".to_string(),
            "slow-success".to_string(),
            "healthy".to_string(),
        ];
        let racing = store.racing_candidates(&candidates, 2);

        assert_eq!(
            racing,
            vec!["healthy".to_string(), "slow-success".to_string()]
        );
    }

    #[test]
    fn test_best_model_prefers_latency_degraded_over_availability_degraded() {
        let store = ModelStatsStore::new(3000.0);

        for _ in 0..3 {
            store.record("slow-success", 5000.0, true);
            store.record("previously-fast-failing", 100.0, true);
        }
        for _ in 0..3 {
            store.record("previously-fast-failing", 100.0, false);
        }

        let candidates = vec![
            "previously-fast-failing".to_string(),
            "slow-success".to_string(),
        ];
        let best = store.best_model(&candidates).unwrap();

        assert_eq!(best, "slow-success");
    }
}

// === NEW TESTS FOR GETTER METHODS ===

#[cfg(test)]
mod getter_tests {
    use super::*;

    #[test]
    fn test_get_model_timeout_default() {
        let store = ModelStatsStore::new(3000.0);
        let max_timeout = 30000u64;

        // Unknown model should return max_timeout
        let timeout = store.get_model_timeout("unknown-model", max_timeout);
        assert_eq!(timeout, max_timeout);
    }

    #[test]
    fn test_get_model_timeout_configured() {
        let store = ModelStatsStore::new(3000.0);
        let max_timeout = 30000u64;

        // Record some stats for a model
        store.record("test-model", 1000.0, true);
        store.record("test-model", 1200.0, true);
        store.record("test-model", 1100.0, true);

        let timeout = store.get_model_timeout("test-model", max_timeout);

        // Should be based on p95 + buffer, but within max
        assert!(timeout >= 1000); // At least 1s minimum
        assert!(timeout <= max_timeout);
    }

    #[test]
    fn test_get_model_timeout_uses_max_until_enough_samples() {
        let store = ModelStatsStore::new(3000.0);
        let max_timeout = 15000u64;

        store.record("failure-only-model", max_timeout as f64, false);
        assert_eq!(
            store.get_model_timeout("failure-only-model", max_timeout),
            max_timeout
        );

        store.record("one-success-model", 500.0, true);
        assert_eq!(
            store.get_model_timeout("one-success-model", max_timeout),
            max_timeout
        );
    }

    #[test]
    fn test_get_model_timeout_policy_floor_and_sample_floor() {
        let store = ModelStatsStore::new(3000.0);
        let max_timeout = 15000u64;

        for _ in 0..9 {
            store.record("fast-model", 500.0, true);
        }
        assert_eq!(
            store.get_model_timeout_with_policy("fast-model", max_timeout, 8000, 10),
            max_timeout
        );

        store.record("fast-model", 500.0, true);
        assert_eq!(
            store.get_model_timeout_with_policy("fast-model", max_timeout, 8000, 10),
            8000
        );
    }

    #[test]
    fn test_get_model_timeout_unknown_model() {
        let store = ModelStatsStore::new(3000.0);
        let max_timeout = 60000u64;

        // Model with no stats should return max_timeout
        let timeout = store.get_model_timeout("nonexistent", max_timeout);
        assert_eq!(timeout, max_timeout);
    }

    #[test]
    fn test_racing_candidates_empty() {
        let store = ModelStatsStore::new(3000.0);
        let candidates: Vec<String> = vec![];

        let result = store.racing_candidates(&candidates, 5);
        assert!(result.is_empty());
    }

    #[test]
    fn test_racing_candidates_tiered_prefers_healthy_fast_then_fallback() {
        let store = ModelStatsStore::new(3000.0);
        for _ in 0..3 {
            store.record("fast-degraded", 5000.0, true);
            store.record("fast-healthy", 700.0, true);
            store.record("fallback-healthy", 600.0, true);
        }

        let fast = vec!["fast-degraded".to_string(), "fast-healthy".to_string()];
        let fallback = vec!["fallback-healthy".to_string()];
        let result = store.racing_candidates_tiered(&fast, &fallback, 3);

        assert_eq!(
            result,
            vec![
                "fast-healthy".to_string(),
                "fallback-healthy".to_string(),
                "fast-degraded".to_string()
            ]
        );
    }

    #[test]
    fn test_racing_candidates_uses_degraded_as_fallback() {
        let store = ModelStatsStore::new(3000.0);

        // Make model-a degraded with high latency
        store.record("model-a", 5000.0, true);
        store.record("model-a", 5000.0, true);
        store.record("model-a", 5000.0, true);
        store.record("model-a", 5000.0, true);

        // model-b is healthy
        store.record("model-b", 500.0, true);
        store.record("model-b", 500.0, true);
        store.record("model-b", 500.0, true);

        let candidates = vec!["model-a".to_string(), "model-b".to_string()];
        let result = store.racing_candidates(&candidates, 5);

        // Healthy models are preferred, but degraded models remain fallback
        // capacity so racing does not collapse to immediate 502s under load.
        assert_eq!(result, vec!["model-b".to_string(), "model-a".to_string()]);
    }

    #[test]
    fn test_racing_candidates_skips_degraded_when_enough_healthy_models() {
        let store = ModelStatsStore::new(3000.0);

        store.record("healthy-a", 500.0, true);
        store.record("healthy-a", 500.0, true);
        store.record("healthy-a", 500.0, true);

        store.record("healthy-b", 700.0, true);
        store.record("healthy-b", 700.0, true);
        store.record("healthy-b", 700.0, true);

        store.record("degraded", 5000.0, true);
        store.record("degraded", 5000.0, true);
        store.record("degraded", 5000.0, true);

        let candidates = vec![
            "degraded".to_string(),
            "healthy-b".to_string(),
            "healthy-a".to_string(),
        ];
        let result = store.racing_candidates(&candidates, 2);

        assert_eq!(
            result,
            vec!["healthy-a".to_string(), "healthy-b".to_string()]
        );
        assert!(!result.contains(&"degraded".to_string()));
    }

    #[test]
    fn test_racing_candidates_backfills_least_bad_degraded_models() {
        let store = ModelStatsStore::new(3000.0);

        store.record("healthy", 500.0, true);
        store.record("healthy", 500.0, true);
        store.record("healthy", 500.0, true);

        store.record("degraded-slower", 8000.0, true);
        store.record("degraded-slower", 8000.0, true);
        store.record("degraded-slower", 8000.0, true);

        store.record("degraded-less-bad", 4000.0, true);
        store.record("degraded-less-bad", 4000.0, true);
        store.record("degraded-less-bad", 4000.0, true);

        let candidates = vec![
            "degraded-slower".to_string(),
            "healthy".to_string(),
            "degraded-less-bad".to_string(),
        ];
        let result = store.racing_candidates(&candidates, 2);

        assert_eq!(
            result,
            vec!["healthy".to_string(), "degraded-less-bad".to_string()]
        );
    }

    #[test]
    fn test_racing_candidates_orders_by_latency() {
        let store = ModelStatsStore::new(3000.0);

        // Record different latencies
        store.record("slow", 2000.0, true);
        store.record("slow", 2000.0, true);
        store.record("slow", 2000.0, true);

        store.record("fast", 500.0, true);
        store.record("fast", 500.0, true);
        store.record("fast", 500.0, true);

        store.record("medium", 1000.0, true);
        store.record("medium", 1000.0, true);
        store.record("medium", 1000.0, true);

        let candidates = vec!["slow".to_string(), "fast".to_string(), "medium".to_string()];
        let result = store.racing_candidates(&candidates, 5);

        // Should be ordered by latency (fastest first)
        assert_eq!(result[0], "fast");
        assert_eq!(result[1], "medium");
        assert_eq!(result[2], "slow");
    }

    #[test]
    fn test_best_model_empty_candidates() {
        let store = ModelStatsStore::new(3000.0);
        let candidates: Vec<String> = vec![];

        let result = store.best_model(&candidates);
        assert_eq!(result, None);
    }

    #[test]
    fn test_best_model_all_degraded() {
        let store = ModelStatsStore::new(3000.0);

        // Make both models degraded
        store.record("model-a", 5000.0, true);
        store.record("model-a", 5000.0, true);
        store.record("model-a", 5000.0, true);
        store.record("model-a", 5000.0, true);

        store.record("model-b", 6000.0, true);
        store.record("model-b", 6000.0, true);
        store.record("model-b", 6000.0, true);
        store.record("model-b", 6000.0, true);

        let candidates = vec!["model-a".to_string(), "model-b".to_string()];
        let result = store.best_model(&candidates);

        // Should still pick one (least degraded - lowest latency)
        assert!(result.is_some());
        // Should pick model-a as it has lower latency
        assert_eq!(result.unwrap(), "model-a");
    }

    #[test]
    fn test_best_model_picks_fastest() {
        let store = ModelStatsStore::new(3000.0);

        store.record("slow", 2000.0, true);
        store.record("slow", 2000.0, true);
        store.record("slow", 2000.0, true);

        store.record("fast", 500.0, true);
        store.record("fast", 500.0, true);
        store.record("fast", 500.0, true);

        let candidates = vec!["slow".to_string(), "fast".to_string()];
        let result = store.best_model(&candidates).unwrap();

        assert_eq!(result, "fast");
    }

    #[test]
    fn test_snapshot_empty_store() {
        let store = ModelStatsStore::new(3000.0);
        let snapshot = store.snapshot();

        assert!(snapshot.is_empty());
    }

    #[test]
    fn test_snapshot_single_model() {
        let store = ModelStatsStore::new(3000.0);
        store.record("single-model", 100.0, true);

        let snapshot = store.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].id, "single-model");
        assert_eq!(snapshot[0].total, 1);
        assert_eq!(snapshot[0].success, 1);
    }

    #[test]
    fn test_snapshot_multiple_models_sorted() {
        let store = ModelStatsStore::new(3000.0);
        store.record("zebra", 100.0, true);
        store.record("alpha", 200.0, true);
        store.record("mango", 300.0, true);

        let snapshot = store.snapshot();

        // Should be sorted alphabetically by id
        assert_eq!(snapshot.len(), 3);
        assert_eq!(snapshot[0].id, "alpha");
        assert_eq!(snapshot[1].id, "mango");
        assert_eq!(snapshot[2].id, "zebra");
    }

    #[test]
    fn test_avg_ms_empty() {
        let entry = ModelEntry::new();
        assert_eq!(entry.avg_ms(), None);
    }

    #[test]
    fn test_avg_ms_single_sample() {
        let mut entry = ModelEntry::new();
        entry.push(42.0);

        let avg = entry.avg_ms();
        assert!(avg.is_some());
        assert_eq!(avg.unwrap(), 42.0);
    }

    #[test]
    fn test_success_rate_zero_total() {
        let entry = ModelEntry::new();
        // Zero total should return 100.0 (default success rate)
        assert_eq!(entry.success_rate(), 100.0);
    }

    // Test 19: Circuit breaker token threshold
    #[test]
    fn test_circuit_breaker_token_threshold() {
        let cb_config = CircuitBreakerConfig {
            max_output_tokens: 100,
            max_repetitions: 3,
            max_consecutive_assistant_turns: 5,
        };

        let mut entry = ModelEntry::new();

        // Initially not degraded
        assert!(!entry.is_degraded("test-model", 3000.0, &cb_config));

        // Set output token count above threshold
        entry.output_token_count = 150;
        assert!(
            entry.is_degraded("test-model", 3000.0, &cb_config),
            "Should be degraded due to token count"
        );
    }

    // Test 20: Best model with all degraded using consecutive failures
    #[test]
    fn test_best_model_all_degraded_by_failures() {
        let store = ModelStatsStore::new(3000.0);

        // Make all models degraded with high consecutive failures (3+ failures = degraded)
        store.record("model-a", 5000.0, false);
        store.record("model-a", 5000.0, false);
        store.record("model-a", 5000.0, false);

        store.record("model-b", 5000.0, false);
        store.record("model-b", 5000.0, false);
        store.record("model-b", 5000.0, false);

        let candidates = vec!["model-a".to_string(), "model-b".to_string()];

        // Should still return a model even if all are degraded (graceful degradation)
        let best = store.best_model(&candidates);
        assert!(
            best.is_some(),
            "Should return a model even if all are degraded"
        );
    }

    // ============ Server-Side Degradation Tests ============

    #[test]
    fn test_server_degraded_flag_initially_false() {
        let mut entry = ModelEntry::new();
        assert!(!entry.server_degraded);
    }

    #[test]
    fn test_mark_server_degraded() {
        let mut entry = ModelEntry::new();
        assert!(!entry.server_degraded);

        entry.mark_server_degraded();
        assert!(entry.server_degraded);
    }

    #[test]
    fn test_server_degraded_increments_consecutive_failures() {
        let mut entry = ModelEntry::new();
        let initial_failures = entry.consecutive_failures;

        entry.mark_server_degraded();

        // Should increment by 3 (the degradation threshold)
        assert!(entry.consecutive_failures >= initial_failures + 3);
    }

    #[test]
    fn test_is_degraded_server_degraded_takes_precedence() {
        let mut entry = ModelEntry::new();
        let cb_config = CircuitBreakerConfig::default();

        // Initially not degraded
        assert!(!entry.is_degraded("test-model", 3000.0, &cb_config));

        // Mark as server degraded
        entry.mark_server_degraded();

        // Should be degraded even with good latency and no failures
        assert!(entry.is_degraded("test-model", 3000.0, &cb_config));
    }

    #[test]
    fn test_record_server_degraded() {
        let store = ModelStatsStore::new(3000.0);

        // Record server degradation for a model
        store.record_server_degraded("test-model");

        // Verify the model is now marked as degraded
        let snapshot = store.snapshot();
        let model_snapshot = snapshot.iter().find(|s| s.id == "test-model").unwrap();
        assert!(model_snapshot.degraded);
    }

    #[test]
    fn test_server_degraded_model_excluded_from_best_model() {
        let store = ModelStatsStore::new(3000.0);

        // Add some healthy samples
        store.record("healthy-model", 500.0, true);
        store.record("healthy-model", 600.0, true);
        store.record("healthy-model", 550.0, true);

        // Mark another model as server-degraded
        store.record_server_degraded("server-degraded-model");

        let candidates = vec![
            "healthy-model".to_string(),
            "server-degraded-model".to_string(),
        ];

        // Should pick the healthy model, not the server-degraded one
        let best = store.best_model(&candidates).unwrap();
        assert_eq!(best, "healthy-model");
    }

    #[test]
    fn test_server_degraded_racing_candidates_excluded() {
        let store = ModelStatsStore::new(3000.0);

        // Add healthy samples
        store.record("fast-model", 200.0, true);
        store.record("fast-model", 250.0, true);
        store.record("fast-model", 300.0, true);

        // Mark a model as server-degraded
        store.record_server_degraded("degraded-model");

        let candidates = vec!["fast-model".to_string(), "degraded-model".to_string()];

        // Get racing candidates
        let racing = store.racing_candidates(&candidates, 2);

        // Should only include the healthy model
        assert_eq!(racing.len(), 1);
        assert_eq!(racing[0], "fast-model");
    }

    #[test]
    fn test_server_degraded_persists_across_snapshots() {
        let store = ModelStatsStore::new(3000.0);

        // Mark as server degraded
        store.record_server_degraded("persistent-model");

        // Take multiple snapshots
        for _ in 0..3 {
            let snapshot = store.snapshot();
            let model = snapshot
                .iter()
                .find(|s| s.id == "persistent-model")
                .unwrap();
            assert!(
                model.degraded,
                "Model should remain degraded across snapshots"
            );
        }
    }

    #[test]
    fn test_server_degraded_vs_local_degraded() {
        let store = ModelStatsStore::new(3000.0);

        // Model A: locally degraded (3 consecutive failures)
        store.record("local-degraded", 5000.0, false);
        store.record("local-degraded", 5000.0, false);
        store.record("local-degraded", 5000.0, false);

        // Model B: server degraded (excluded from routing)
        store.record_server_degraded("server-degraded");

        // Model C: healthy
        store.record("healthy", 500.0, true);
        store.record("healthy", 600.0, true);
        store.record("healthy", 550.0, true);

        // Debug: check snapshot to understand state
        let snapshot = store.snapshot();
        for s in &snapshot {
            eprintln!(
                "SNAPSHOT: id={}, total={}, ring_len={}, consec_fail={}, degraded={}",
                s.id, s.total, s.sample_count, s.consecutive_failures, s.degraded
            );
        }

        let candidates = vec![
            "local-degraded".to_string(),
            "server-degraded".to_string(),
            "healthy".to_string(),
        ];

        // Should pick healthy model (server-degraded is excluded, local-degraded is degraded)
        let best = store.best_model(&candidates).unwrap();
        assert_eq!(best, "healthy");
    }
}
