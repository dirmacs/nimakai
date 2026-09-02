//! Live upstream model catalog refresh.
//!
//! At startup, `main.rs` fetches the upstream `GET /v1/models` catalog with a pool key and
//! prunes every configured model list (routing, racing, fast, fallback) of ids that are not
//! present upstream, before `AppState` is constructed. This keeps the proxy from racing/routing
//! to models NVIDIA has quietly removed from the account's catalog.
//!
//! A periodic background task (spawned from `main.rs`, interval controlled by
//! `[racing].model_check_interval_secs`) re-fetches the catalog and marks any configured model
//! that has disappeared as server-degraded via the existing `ModelStatsStore` mechanism.
//!
//! Fail-open: if the upstream fetch fails (network error, timeout, non-2xx, empty body), the
//! configured model lists are left unchanged and the caller should log a `warn!` and continue
//! startup normally — a refresh failure must never prevent the proxy from starting.

use crate::AppState;
use reqwest::Client;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

/// Timeout for the upstream `/v1/models` fetch used both at startup and by the periodic
/// recheck task.
const FETCH_TIMEOUT_SECS: u64 = 5;

/// Pure function: split `models` into `(kept, pruned)` based on membership in `upstream`.
///
/// `kept` preserves the original order/content of `models` for every id present in `upstream`.
/// `pruned` lists (in original order) every id from `models` that is NOT present in `upstream`.
pub fn prune_missing_models(
    models: &[String],
    upstream: &HashSet<String>,
) -> (Vec<String>, Vec<String>) {
    let mut kept = Vec::with_capacity(models.len());
    let mut pruned = Vec::new();
    for m in models {
        if upstream.contains(m) {
            kept.push(m.clone());
        } else {
            pruned.push(m.clone());
        }
    }
    (kept, pruned)
}

/// Prune `models` against `upstream`, logging one `warn!` per pruned id (tagged with
/// `list_name` for context) and appending every pruned id to `pruned_out`. Returns the kept
/// list.
pub fn prune_and_log(
    models: Vec<String>,
    upstream: &HashSet<String>,
    list_name: &str,
    pruned_out: &mut Vec<String>,
) -> Vec<String> {
    let (kept, pruned) = prune_missing_models(&models, upstream);
    for id in &pruned {
        warn!(
            model = %id,
            list = list_name,
            "startup model refresh: pruning configured model not present in upstream /v1/models catalog"
        );
    }
    pruned_out.extend(pruned);
    kept
}

/// Fetch the upstream `/v1/models` catalog using a single pool key. 5s timeout.
///
/// Returns the set of model ids NVIDIA currently reports for this account. Errors (network,
/// timeout, non-2xx, unparseable/empty body) are returned as `Err` so the caller can fail open.
pub async fn fetch_upstream_model_ids(
    target: &str,
    api_key: &str,
) -> Result<HashSet<String>, String> {
    let client = Client::builder()
        .use_rustls_tls()
        .timeout(Duration::from_secs(FETCH_TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("http client build failed: {}", e))?;

    let url = format!("{}/v1/models", target.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(|e| format!("request to {} failed: {}", url, e))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(format!("upstream {} returned status {}", url, status));
    }

    let text = resp
        .text()
        .await
        .map_err(|e| format!("failed to read response body from {}: {}", url, e))?;
    let body: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("invalid JSON from {}: {}", url, e))?;

    let ids: HashSet<String> = body
        .get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    if ids.is_empty() {
        return Err(format!("upstream {} returned an empty model list", url));
    }

    Ok(ids)
}

/// Periodic re-check task body. Intended to be driven by `tokio::spawn`.
///
/// Every `interval_secs` seconds, re-fetches the upstream catalog and, for every model
/// currently configured on `state` that has disappeared upstream, marks it server-degraded via
/// `ModelStatsStore::record_server_degraded` (the same mechanism NVIDIA-declared degradation
/// uses) and logs a `warn!`. Logs one `info!` summary per successful check. On fetch failure,
/// logs a `warn!` and leaves model health untouched (fail-open).
///
/// Callers MUST check `interval_secs > 0` before spawning — `0` means the recheck task is
/// disabled and this function should not be invoked.
pub async fn run_periodic_recheck(
    state: Arc<AppState>,
    target: String,
    api_key: String,
    interval_secs: u64,
) {
    if interval_secs == 0 {
        return;
    }
    let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
    // The first tick fires immediately; consume it so we don't recheck right after the
    // startup refresh already performed in `main`.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        match fetch_upstream_model_ids(&target, &api_key).await {
            Ok(ids) => {
                let configured = state.configured_models();
                let mut missing = 0usize;
                for m in &configured {
                    if !ids.contains(m) {
                        warn!(
                            model = %m,
                            "periodic model recheck: configured model missing from upstream catalog; marking degraded"
                        );
                        state.model_stats.record_server_degraded(m);
                        missing += 1;
                    }
                }
                info!(
                    checked = configured.len(),
                    missing, "periodic model recheck complete"
                );
            }
            Err(e) => {
                warn!(error = %e, "periodic model recheck: upstream fetch failed; leaving model health unchanged");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(models: &[&str]) -> HashSet<String> {
        models.iter().map(|s| s.to_string()).collect()
    }

    fn vecs(models: &[&str]) -> Vec<String> {
        models.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_prune_missing_models_all_present() {
        let models = vecs(&["a/one", "b/two", "c/three"]);
        let upstream = set(&["a/one", "b/two", "c/three", "d/four"]);
        let (kept, pruned) = prune_missing_models(&models, &upstream);
        assert_eq!(kept, vecs(&["a/one", "b/two", "c/three"]));
        assert!(pruned.is_empty());
    }

    #[test]
    fn test_prune_missing_models_some_missing() {
        let models = vecs(&["a/one", "b/two", "c/three"]);
        let upstream = set(&["a/one", "c/three"]);
        let (kept, pruned) = prune_missing_models(&models, &upstream);
        assert_eq!(kept, vecs(&["a/one", "c/three"]));
        assert_eq!(pruned, vecs(&["b/two"]));
    }

    #[test]
    fn test_prune_missing_models_empty_upstream() {
        let models = vecs(&["a/one", "b/two"]);
        let upstream: HashSet<String> = HashSet::new();
        let (kept, pruned) = prune_missing_models(&models, &upstream);
        assert!(kept.is_empty());
        assert_eq!(pruned, vecs(&["a/one", "b/two"]));
    }

    #[test]
    fn test_prune_missing_models_empty_input() {
        let models: Vec<String> = Vec::new();
        let upstream = set(&["a/one"]);
        let (kept, pruned) = prune_missing_models(&models, &upstream);
        assert!(kept.is_empty());
        assert!(pruned.is_empty());
    }

    #[test]
    fn test_prune_and_log_accumulates_pruned_out() {
        let models = vecs(&["a/one", "b/two"]);
        let upstream = set(&["a/one"]);
        let mut pruned_out = Vec::new();
        let kept = prune_and_log(models, &upstream, "test.list", &mut pruned_out);
        assert_eq!(kept, vecs(&["a/one"]));
        assert_eq!(pruned_out, vecs(&["b/two"]));
    }

    #[tokio::test]
    async fn test_fetch_upstream_model_ids_network_error_is_err() {
        // Port 0 / reserved local address should fail fast rather than hang.
        let result = fetch_upstream_model_ids("http://127.0.0.1:1", "test-key").await;
        assert!(result.is_err());
    }
}
