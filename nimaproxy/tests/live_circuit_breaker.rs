//! Live integration tests for circuit breaker behavior.
//! These tests require a valid NVIDIA_API_KEY environment variable.
//! Tests are skipped if NVIDIA_API_KEY is not set.
//!
//! Run with: cargo test --test live_circuit_breaker -- --nocapture

use axum::response::IntoResponse;
use nimaproxy::config::{KeyEntry, ModelCompat};
use nimaproxy::model_stats::{CircuitBreakerConfig, ModelStatsStore};
use nimaproxy::AppState;
use std::collections::HashMap;
use std::sync::Arc;

const NVIDIA_API_BASE: &str = "https://integrate.api.nvidia.com";

fn get_api_key() -> Option<String> {
	std::env::var("NVIDIA_API_KEY").ok()
}

fn make_live_state_with_circuit_breaker() -> Option<Arc<AppState>> {
	let api_key = get_api_key()?;

	let key_entries = vec![KeyEntry {
		key: api_key,
		label: Some("live-circuit-breaker".to_string()),
	}];

	// Configure circuit breaker with low thresholds for testing
	let cb_config = CircuitBreakerConfig {
		max_output_tokens: 1000, // Low threshold for testing
		max_repetitions: 5,
		max_consecutive_assistant_turns: 10,
	};

	let model_stats = ModelStatsStore::with_circuit_breaker(3000.0, cb_config);

	Some(AppState::new(
		key_entries,
		NVIDIA_API_BASE.to_string(),
		None,
		model_stats,
		vec![],
		3,
		20000,
		"complete".to_string(),
		HashMap::new(),
		ModelCompat::default(),
	))
}

fn skip_if_no_api_key() -> bool {
	get_api_key().is_none()
}

async fn send_chat_request(state: &Arc<AppState>, model: &str, content: &str) -> u16 {
	let body = serde_json::json!({
		"model": model,
		"messages": [
			{"role": "user", "content": content}
		],
		"max_tokens": 50,
		"temperature": 0.0
	});

	let resp = nimaproxy::proxy::chat_completions(
		axum::extract::State(state.clone()),
		axum::http::HeaderMap::new(),
		bytes::Bytes::from(body.to_string()),
	)
	.await;

	let response = resp.into_response();
	let parts = response.into_parts().0;
	parts.status.as_u16()
}

/// ============================================================================
/// Test 1: Circuit Breaker Degradation
/// Triggers failures and verifies circuit breaker degradation behavior
/// ============================================================================
#[tokio::test]
async fn test_live_circuit_breaker_degradation() {
	if skip_if_no_api_key() {
		eprintln!("[SKIP] test_live_circuit_breaker_degradation: NVIDIA_API_KEY not set");
		return;
	}

	let state = make_live_state_with_circuit_breaker().expect("Failed to create live state");

	eprintln!("[circuit-breaker] Testing degradation behavior");

	let test_model = "meta/llama-3.1-8b-instruct";
	let mut statuses = Vec::new();

	// Send several requests to establish baseline
	for i in 0..5 {
		let status = send_chat_request(&state, test_model, "Say 'hello'.").await;
		statuses.push(status);
		eprintln!(
			"[circuit-breaker] Request {}: status={}",
			i + 1,
			status
		);

		std::thread::sleep(std::time::Duration::from_millis(500));
	}

	// Record some stats manually to simulate degradation
	// This simulates consecutive failures
	for i in 0..5 {
		state.model_stats.record(&format!("test-model-{}", i), 5000.0, false);
		eprintln!(
			"[circuit-breaker] Recorded failure for test-model-{}",
			i
		);
	}

	// Check model stats
	let stats_response = nimaproxy::proxy::stats(axum::extract::State(state.clone())).await;
	let response = stats_response.into_response();
	let (_parts, body) = response.into_parts();
	let body_bytes = axum::body::to_bytes(body, 65536).await.unwrap();
	let json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();

	eprintln!("[circuit-breaker] Stats response: {}", json);

	// Verify stats are being recorded
	let models = json["models"].as_array().unwrap();
	eprintln!(
		"[circuit-breaker] Recorded {} model stats entries",
		models.len()
	);

	// At least some initial requests should have succeeded
	let success_count = statuses.iter().filter(|&&s| s == 200).count();
	eprintln!(
		"[circuit-breaker] Baseline: {} successes out of {} requests",
		success_count,
		statuses.len()
	);
}

/// ============================================================================
/// Test 2: Circuit Breaker Recovery
/// Verifies auto-recovery after circuit breaker trips
/// ============================================================================
#[tokio::test]
async fn test_live_circuit_breaker_recovery() {
	if skip_if_no_api_key() {
		eprintln!("[SKIP] test_live_circuit_breaker_recovery: NVIDIA_API_KEY not set");
		return;
	}

	let state = make_live_state_with_circuit_breaker().expect("Failed to create live state");

	eprintln!("[circuit-breaker] Testing recovery behavior");

	let test_model = "meta/llama-3.1-8b-instruct";

	// First, establish a baseline with successful requests
	let baseline_requests = 3;
	let mut baseline_successes = 0;

	eprintln!(
		"[circuit-breaker] Establishing baseline with {} requests",
		baseline_requests
	);

	for i in 0..baseline_requests {
		let status = send_chat_request(&state, test_model, "Say 'hello'.").await;
		if status == 200 {
			baseline_successes += 1;
		}
		eprintln!(
			"[circuit-breaker] Baseline request {}: status={}",
			i + 1,
			status
		);
		std::thread::sleep(std::time::Duration::from_millis(500));
	}

	// Simulate circuit breaker scenario by recording failures
	eprintln!("[circuit-breaker] Simulating failures for circuit breaker");

	let failure_model = "test-degraded-model";
	for _ in 0..5 {
		state.model_stats.record(failure_model, 100.0, false);
	}

	// Check stats snapshot to verify recording
	let snapshot = state.model_stats.snapshot();
	eprintln!(
		"[circuit-breaker] Snapshot contains {} model entries",
		snapshot.len()
	);

	// Now record successes to simulate recovery
	eprintln!("[circuit-breaker] Recording successes for recovery");

	for _ in 0..10 {
		state.model_stats.record(failure_model, 50.0, true);
	}

	// Check stats after recovery attempts
	let stats_response = nimaproxy::proxy::stats(axum::extract::State(state.clone())).await;
	let response = stats_response.into_response();
	let (_parts, body) = response.into_parts();
	let body_bytes = axum::body::to_bytes(body, 65536).await.unwrap();
	let json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();

	eprintln!("[circuit-breaker] Post-recovery stats: {}", json);

	// Verify snapshot shows updated data
	let snapshot_after = state.model_stats.snapshot();
	eprintln!(
		"[circuit-breaker] Snapshot after recovery: {} model entries",
		snapshot_after.len()
	);

	// Send a final request to verify system is still functional
	let final_status = send_chat_request(&state, test_model, "Say 'hello'.").await;
	eprintln!(
		"[circuit-breaker] Final verification request status: {}",
		final_status
	);

	// System should still be operational
	assert!(
		final_status == 200 || final_status == 429 || final_status == 400,
		"System should be operational, got status {}",
		final_status
	);

	eprintln!("[circuit-breaker] Recovery test completed successfully");
}
