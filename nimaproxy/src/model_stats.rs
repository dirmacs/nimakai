use std::collections::HashMap;
use std::sync::Mutex;

const RING_SIZE: usize = 100;

/// Per-model latency ring buffer. Uses TTFC (time-to-first-chunk) measured at
/// the proxy layer — the most meaningful latency signal for agentic coding.
struct ModelEntry {
    ring: [f64; RING_SIZE],
    ring_pos: usize,
    ring_len: usize,
    pub total: u64,
    pub success: u64,
    pub consecutive_failures: u32,
    pub last_ms: f64,
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

    pub fn is_degraded(&self, spike_threshold_ms: f64) -> bool {
        if self.consecutive_failures >= 3 {
            return true;
        }
        if let Some(avg) = self.avg_ms() {
            if avg > spike_threshold_ms {
                return true;
            }
        }
        false
    }
}

/// Snapshot exported to /stats endpoint.
pub struct ModelSnapshot {
    pub id: String,
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
pub struct ModelStatsStore {
    inner: Mutex<HashMap<String, ModelEntry>>,
    pub spike_threshold_ms: f64,
}

impl ModelStatsStore {
    pub fn new(spike_threshold_ms: f64) -> Self {
        ModelStatsStore {
            inner: Mutex::new(HashMap::new()),
            spike_threshold_ms,
        }
    }

    /// Record a completed request. `ms` is TTFC measured at the proxy.
    pub fn record(&self, model_id: &str, ms: f64, ok: bool) {
        let mut map = self.inner.lock().unwrap();
        let entry = map
            .entry(model_id.to_string())
            .or_insert_with(ModelEntry::new);
        entry.total += 1;
        entry.last_ms = ms;
        if ok {
            entry.success += 1;
            entry.consecutive_failures = 0;
            entry.push(ms);
        } else {
            entry.consecutive_failures += 1;
        }
    }

    /// Pick the best model from `candidates` for the next request.
    ///
    /// Priority:
    ///   1. Models with < 3 samples (untried) — round-robin among them first
    ///   2. Non-degraded models sorted by avg_ms ascending
    ///   3. If all degraded, pick lowest avg_ms anyway (graceful degradation)
    pub fn best_model(&self, candidates: &[String]) -> Option<String> {
        if candidates.is_empty() {
            return None;
        }
        let map = self.inner.lock().unwrap();
        let threshold = self.spike_threshold_ms;

        // Partition into (untried, tried_ok, tried_degraded)
        let mut untried: Vec<&String> = Vec::new();
        let mut ok: Vec<(&String, f64)> = Vec::new();
        let mut degraded: Vec<(&String, f64)> = Vec::new();

        for m in candidates {
            match map.get(m) {
                None => untried.push(m),
                Some(e) => {
                    if e.total == 0 || e.ring_len < 3 {
                        untried.push(m);
                    } else if e.is_degraded(threshold) {
                        degraded.push((m, e.avg_ms().unwrap_or(f64::MAX)));
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

        // All degraded — pick least bad
        if !degraded.is_empty() {
            degraded.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
            return Some(degraded[0].0.clone());
        }

        None
    }

    /// Export current stats for all tracked models (for /stats endpoint).
    pub fn snapshot(&self) -> Vec<ModelSnapshot> {
        let map = self.inner.lock().unwrap();
        let threshold = self.spike_threshold_ms;
        let mut out: Vec<ModelSnapshot> = map
            .iter()
            .map(|(id, e)| ModelSnapshot {
                id: id.clone(),
                avg_ms: e.avg_ms(),
                p95_ms: e.p95_ms(),
                total: e.total,
                success: e.success,
                success_rate: e.success_rate(),
                sample_count: e.ring_len,
                consecutive_failures: e.consecutive_failures,
                degraded: e.is_degraded(threshold),
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }
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

        assert!(entry.is_degraded(3000.0));
    }

    #[test]
    fn test_is_degraded_high_latency() {
        let mut entry = ModelEntry::new();
        entry.consecutive_failures = 1; // not enough for failure degradation
        entry.push(5000.0);
        entry.push(5000.0);
        entry.push(5000.0);

        assert!(entry.is_degraded(3000.0));
    }

    #[test]
    fn test_not_degraded() {
        let mut entry = ModelEntry::new();
        entry.push(500.0);
        entry.push(600.0);
        entry.push(550.0);

        assert!(!entry.is_degraded(3000.0));
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
}
