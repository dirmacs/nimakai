//! Model racing configuration: fan-out parallelism and deadlines.

use serde::Deserialize;
/// Racing config for speculative execution.
///
/// When enabled, fires N parallel requests to N models and returns the first response.
/// This is "model racing" — trades N×token budget for min(P50 latency).
#[derive(Deserialize, Clone, Debug)]
pub struct RacingConfig {
    /// Enable speculative execution (default: false)
    pub enabled: Option<bool>,
    /// List of models to race. Must have 2+ models.
    pub models: Option<Vec<String>>,
    /// Max parallel requests (default: 3, no upper cap - config value is trusted)
    pub max_parallel: Option<usize>,
    /// Timeout per request in ms (default: 8000ms)
    pub timeout_ms: Option<u64>,
    /// End-to-end wall-clock budget for racing plus sequential fallback. 0 disables.
    pub max_total_request_ms: Option<u64>,
    /// Strategy: "first_token" (return on first SSE token) or "complete" (default)
    pub strategy: Option<String>,
    /// Enable adaptive runtime fan-out under key/upstream pressure.
    pub adaptive: Option<bool>,
    /// Minimum racing fan-out when adaptive mode has enough capacity.
    pub min_parallel: Option<usize>,
    /// Fan-out target when the gateway is under moderate pressure.
    pub pressure_parallel: Option<usize>,
    /// Fan-out target when keys are cooling down or the gateway is heavily loaded.
    pub degraded_parallel: Option<usize>,
    /// Preferred fast models for normal racing.
    pub fast_models: Option<Vec<String>>,
    /// Slower or more expensive models used as backfill/fallback candidates.
    pub fallback_models: Option<Vec<String>>,
    /// If prompt text exceeds this many chars, cap racing fan-out.
    pub large_prompt_char_threshold: Option<usize>,
    /// Fan-out cap for large prompt requests. A value of 1 enables solo fallback.
    pub large_prompt_parallel: Option<usize>,
    /// Allow racing to degrade to a single model when fewer than two racers are viable.
    pub solo_fallback: Option<bool>,
    /// Interval in seconds between background upstream `/v1/models` rechecks that mark
    /// configured-but-vanished models degraded. Default 3600. `0` disables the recheck task.
    pub model_check_interval_secs: Option<u64>,
}
