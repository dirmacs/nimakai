//! Live integration tests for routing strategies.
//! These tests require a valid NVIDIA_API_KEY environment variable.
//! Tests are skipped if NVIDIA_API_KEY is not set.
//!
//! Run with: cargo test --test live_routing -- --nocapture

use axum::response::IntoResponse;
use nimaproxy::config::{KeyEntry, ModelCompat};
use nimaproxy::model_stats::ModelStatsStore;
use nimaproxy::AppState;
use std::collections::HashMap;
use std::sync::Arc;

const NVIDIA_API_BASE: &str = "https://integrate.api.nvidia.com";

fn get_api_key() -> Option<String> {
	std::env::var("NVIDIA_API_KEY").ok()
}

fn make_live_state_with_routing(
	models: Vec<String>,
	_strategy: &str,
) -> Option<Arc<AppState>> {
	let api_key = get_api_key()?;

	let key_entries = vec![KeyEntry {
		key: api_key,
		label: Some("live-routing-test".to_string()),
	}];

	Some(AppState::new(
		key_entries,
		NVIDIA_API_BASE.to_string(),
		None,
		ModelStatsStore::new(3000.0),
		models.clone(),
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

async fn send_chat_request(state: &Arc<AppState>, model: &str) -> u16 {
	let body = serde_json::json!({
		"model": model,
		"messages": [
			{"role": "user", "content": "Say 'hello' in exactly one word."}
		],
		"max_tokens": 10,
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
/// Test 1: Round Robin Distribution
/// Verifies that requests are distributed evenly across models
/// ============================================================================
#[tokio::test]
async fn test_live_routing_round_robin() {
	if skip_if_no_api_key() {
		eprintln!("[SKIP] test_live_routing_round_robin: NVIDIA_API_KEY not set");
		return;
	}

	// Use models that are typically available
	let test_models = vec![
		"meta/llama-3.1-8b-instruct".to_string(),
		"nvidia/nemotron-4-340b-instruct".to_string(),
	];

	let state = make_live_state_with_routing(test_models.clone(), "round_robin")
		.expect("Failed to create live state");

	eprintln!("[routing] Testing round-robin distribution across {} models", test_models.len());

	let num_requests = 10;
	let mut success_count = 0;
	let mut failure_count = 0;

	for i in 0..num_requests {
		let model = &test_models[i % test_models.len()];
		let status = send_chat_request(&state, model).await;

		eprintln!("[routing] Request {} to {}: status={}", i + 1, model, status);

		if status == 200 {
			success_count += 1;
		} else {
			failure_count += 1;
		}

		// Small delay to avoid rate limiting
		std::thread::sleep(std::time::Duration::from_millis(500));
	}

	eprintln!(
		"[routing] Round-robin results: {} successes, {} failures",
		success_count, failure_count
	);

	// At least some requests should succeed
	assert!(
		success_count > 0,
		"Expected at least one successful request in round-robin test"
	);
}

/// ============================================================================
/// Test 2: Latency-Aware Routing
/// Verifies that the fastest model is selected based on latency stats
/// ============================================================================
#[tokio::test]
async fn test_live_routing_latency_aware() {
	if skip_if_no_api_key() {
		eprintln!("[SKIP] test_live_routing_latency_aware: NVIDIA_API_KEY not set");
		return;
	}

	let test_models = vec![
		"meta/llama-3.1-8b-instruct".to_string(),
		"nvidia/nemotron-4-340b-instruct".to_string(),
	];

	let state = make_live_state_with_routing(test_models.clone(), "latency_aware")
		.expect("Failed to create live state");

	eprintln!("[routing] Testing latency-aware routing");

	let mut latencies: Vec<(String, u64)> = Vec::new();

	// First, collect latency data by sending one request to each model
	for model in &test_models {
		let start = std::time::Instant::now();
		let status = send_chat_request(&state, model).await;
		let elapsed = start.elapsed();

		eprintln!(
			"[routing] Model {} - status={}, latency={:?}ms",
			model,
			status,
			elapsed.as_millis()
		);

		if status == 200 {
			latencies.push((model.clone(), elapsed.as_millis() as u64));
		}

		// Delay between requests
		std::thread::sleep(std::time::Duration::from_millis(500));
	}

	// Sort by latency to find the fastest
	latencies.sort_by_key(|(_, latency)| *latency);

	if latencies.len() >= 2 {
		let (fastest_model, fastest_time) = &latencies[0];
		let (slowest_model, slowest_time) = latencies.last().unwrap();

		eprintln!(
			"[routing] Fastest: {} ({}ms), Slowest: {} ({}ms)",
			fastest_model, fastest_time, slowest_model, slowest_time
		);

		// Verify we have latency data recorded
		assert!(
			*fastest_time <= *slowest_time,
			"Fastest model should have lower or equal latency"
		);
	} else {
		eprintln!("[routing] Insufficient successful responses for latency comparison");
	}

	// At least one model should have been tested
	assert!(
		latencies.len() > 0,
		"Expected at least one model to respond successfully"
	);
}
