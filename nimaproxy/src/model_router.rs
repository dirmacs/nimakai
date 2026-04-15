use std::sync::atomic::{AtomicUsize, Ordering};

use crate::model_stats::ModelStatsStore;

#[derive(Clone, Debug)]
pub enum Strategy {
    RoundRobin,
    LatencyAware,
}

impl Strategy {
    pub fn from_str(s: &str) -> Self {
        match s {
            "latency_aware" | "latency-aware" => Strategy::LatencyAware,
            _ => Strategy::RoundRobin,
        }
    }
}

/// Routes incoming requests to a configured model list.
///
/// - `RoundRobin`: cycles through models in order, ignoring latency data
/// - `LatencyAware`: defers to `ModelStatsStore::best_model()` — prefers untried
///   models first, then lowest avg TTFC, skips degraded (≥3 consecutive failures
///   or avg > spike_threshold_ms)
pub struct ModelRouter {
    pub models: Vec<String>,
    rr_index: AtomicUsize,
    strategy: Strategy,
}

impl ModelRouter {
    pub fn new(models: Vec<String>, strategy: Strategy) -> Self {
        ModelRouter {
            models,
            rr_index: AtomicUsize::new(0),
            strategy,
        }
    }

    /// Pick a model for the next request.
    /// Returns `None` only if the model list is empty.
    pub fn pick(&self, stats: &ModelStatsStore) -> Option<String> {
        if self.models.is_empty() {
            return None;
        }
        match self.strategy {
            Strategy::RoundRobin => {
                let idx = self.rr_index.fetch_add(1, Ordering::Relaxed) % self.models.len();
                Some(self.models[idx].clone())
            }
            Strategy::LatencyAware => stats
                .best_model(&self.models)
                .or_else(|| Some(self.models[0].clone())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_stats::ModelStatsStore;

    fn make_stats(threshold: f64) -> ModelStatsStore {
        ModelStatsStore::new(threshold)
    }

    #[test]
    fn test_round_robin_strategy() {
        let router = ModelRouter::new(
            vec![
                "model-a".to_string(),
                "model-b".to_string(),
                "model-c".to_string(),
            ],
            Strategy::RoundRobin,
        );
        let stats = make_stats(3000.0);

        let picks: Vec<String> = (0..5).map(|_| router.pick(&stats).unwrap()).collect();

        assert_eq!(picks[0], "model-a");
        assert_eq!(picks[1], "model-b");
        assert_eq!(picks[2], "model-c");
        assert_eq!(picks[3], "model-a");
        assert_eq!(picks[4], "model-b");
    }

    #[test]
    fn test_latency_aware_picks_fastest() {
        let stats = make_stats(3000.0);
        stats.record("fast", 200.0, true);
        stats.record("fast", 250.0, true);
        stats.record("fast", 300.0, true);

        stats.record("slow", 2000.0, true);
        stats.record("slow", 2500.0, true);
        stats.record("slow", 3000.0, true);

        let router = ModelRouter::new(
            vec!["fast".to_string(), "slow".to_string()],
            Strategy::LatencyAware,
        );

        let picked = router.pick(&stats).unwrap();
        assert_eq!(picked, "fast");
    }

    #[test]
    fn test_latency_aware_skips_degraded() {
        let stats = make_stats(3000.0);
        stats.record("degraded", 5000.0, true);
        stats.record("degraded", 5000.0, true);
        stats.record("degraded", 5000.0, true);

        stats.record("healthy", 500.0, true);

        let router = ModelRouter::new(
            vec!["degraded".to_string(), "healthy".to_string()],
            Strategy::LatencyAware,
        );

        let picked = router.pick(&stats).unwrap();
        assert_eq!(picked, "healthy");
    }

    #[test]
    fn test_pick_empty_models() {
        let router = ModelRouter::new(vec![], Strategy::RoundRobin);
        let stats = make_stats(3000.0);

        assert_eq!(router.pick(&stats), None);
    }

    #[test]
    fn test_latency_aware_untried_first() {
        let stats = make_stats(3000.0);
        let router = ModelRouter::new(
            vec!["model-a".to_string(), "model-b".to_string()],
            Strategy::LatencyAware,
        );

        // Neither model has been tried - picks based on total count
        let picked = router.pick(&stats).unwrap();
        assert!(["model-a", "model-b"].contains(&picked.as_str()));
    }

    #[test]
    fn test_strategy_from_str() {
        assert!(matches!(
            Strategy::from_str("round_robin"),
            Strategy::RoundRobin
        ));
        assert!(matches!(
            Strategy::from_str("latency_aware"),
            Strategy::LatencyAware
        ));
        assert!(matches!(
            Strategy::from_str("latency-aware"),
            Strategy::LatencyAware
        ));
        assert!(matches!(
            Strategy::from_str("unknown"),
            Strategy::RoundRobin
        ));
    }
}
