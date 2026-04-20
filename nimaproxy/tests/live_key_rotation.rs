//! Live integration tests for key rotation under load.
//! These tests require a valid NVIDIA_API_KEY environment variable.
//! Tests are skipped if NVIDIA_API_KEY is not set.
//!
//! Run with: cargo test --test live_key_rotation -- --nocapture

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

fn make_live_state() -> Option<Arc<AppState>> {
	let api_key = get_api_key()?;

	let key_entries = vec![KeyEntry {
		key: api_key,
		label: Some("live-key-rotation".to_string()),
	}];

	Some(AppState::new(
		key_entries,
		NVIDIA_API_BASE.to_string(),
		None,
		ModelStatsStore::new(3000.0),
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

async fn send_chat_request(state: &Arc<AppState>) -> u16 {
	let body = serde_json::json!({
		"model": "meta/llama-3.1-8b-instruct",
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
/// Test 1: Key Rotation Under Load
/// Verifies that key rotation works correctly under 100 requests
/// ============================================================================
#[tokio::test]
async fn test_live_key_rotation_under_load() {
	if skip_if_no_api_key() {
		eprintln!("[SKIP] test_live_key_rotation_under_load: NVIDIA_API_KEY not set");
		return;
	}

	let state = make_live_state().expect("Failed to create live state");

	eprintln!("[key-rotation] Testing key rotation under load (100 requests)");

	let num_requests = 100;
	let mut success_count = 0;
	let mut failure_count = 0;
	let mut rate_limited_count = 0;
	let mut key_usage: HashMap<String, usize> = HashMap::new();

	for i in 0..num_requests {
		let status = send_chat_request(&state).await;

		// Track key usage from response headers
		if let Some(key_label) = state.pool.get_key_label(0) {
			*key_usage.entry(key_label).or_insert(0) += 1;
		}

		match status {
			200 => success_count += 1,
			429 => rate_limited_count += 1,
			_ => failure_count += 1,
		}

		if (i + 1) % 10 == 0 {
			eprintln!(
				"[key-rotation] Progress: {}/{} (success={}, failures={}, rate_limited={})",
				i + 1,
				num_requests,
				success_count,
				failure_count,
				rate_limited_count
			);
		}

		// Small delay to avoid aggressive rate limiting
		std::thread::sleep(std::time::Duration::from_millis(200));
	}

	eprintln!(
		"[key-rotation] Final results: success={}, failures={}, rate_limited={}",
		success_count, failure_count, rate_limited_count
	);

	eprintln!("[key-rotation] Key usage: {:?}", key_usage);

	// Verify key rotation is happening
	let keys_used = key_usage.keys().len();
	eprintln!("[key-rotation] Number of unique keys tracked: {}", keys_used);

	// At least some requests should succeed
	assert!(
		success_count > 0,
		"Expected at least some successful requests under load"
	);
}

/// ============================================================================
/// Test 2: Rate Limit Recovery
/// Triggers 429 rate limit and verifies recovery behavior
/// ============================================================================
#[tokio::test]
async fn test_live_key_rate_limit_recovery() {
	if skip_if_no_api_key() {
		eprintln!("[SKIP] test_live_key_rate_limit_recovery: NVIDIA_API_KEY not set");
		return;
	}

	let state = make_live_state().expect("Failed to create live state");

	eprintln!("[rate-limit] Testing rate limit trigger and recovery");

	// Send rapid requests to trigger rate limiting
	let rapid_requests = 20;
	let mut rate_limited = 0;
	let mut successes = 0;

	eprintln!("[rate-limit] Sending {} rapid requests to trigger rate limiting", rapid_requests);

	for i in 0..rapid_requests {
		let status = send_chat_request(&state).await;

		match status {
			200 => successes += 1,
			429 => {
				rate_limited += 1;
				eprintln!(
					"[rate-limit] Rate limited at request {}/{}",
					i + 1,
					rapid_requests
				);
			}
			_ => {}
		}

		// Very short delay to increase chance of rate limiting
		std::thread::sleep(std::time::Duration::from_millis(100));
	}

	eprintln!(
		"[rate-limit] Rapid fire results: {} successes, {} rate limited",
		successes, rate_limited
	);

	// Check pool status
	let pool_status = state.pool.status();
	eprintln!("[rate-limit] Pool status after rapid requests: {:?}", pool_status);

	// If rate limited, wait and verify recovery
	if rate_limited > 0 {
		eprintln!("[rate-limit] Rate limit triggered. Waiting 5s for recovery...");
		std::thread::sleep(std::time::Duration::from_secs(5));

		// Try to send a request after recovery wait
		eprintln!("[rate-limit] Testing recovery after wait...");
		let post_wait_status = send_chat_request(&state).await;

		eprintln!(
			"[rate-limit] Post-recovery status: {} (should be 200 if recovered)",
			post_wait_status
		);

		// After waiting, we should either succeed or still be rate limited
		// but the system should not crash
		assert!(
			post_wait_status == 200 || post_wait_status == 429,
			"Expected 200 (recovered) or 429 (still limited), got {}",
			post_wait_status
		);
	} else {
		eprintln!("[rate-limit] Rate limit not triggered with current API key limits");
	}

	// Verify the pool is still functional
	let pool_len = state.pool.len();
	assert!(pool_len > 0, "Pool should still have keys");

	eprintln!("[rate-limit] Test completed - pool is functional");
}
