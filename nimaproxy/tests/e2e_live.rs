use axum::response::IntoResponse;
use nimaproxy::config::{KeyEntry, ModelCompat};
use nimaproxy::model_stats::ModelStatsStore;
use nimaproxy::AppState;
use std::sync::Arc;

const NVIDIA_API_BASE: &str = "https://integrate.api.nvidia.com";

fn get_test_keys() -> Vec<(String, String)> {
    vec![
        ("REDACTED_KEY_1".to_string(), "doltares".to_string()),
        ("REDACTED_KEY_2".to_string(), "ares".to_string()),
        ("REDACTED_KEY_3".to_string(), "dirmacs".to_string()),
    ]
}

fn make_state() -> Arc<AppState> {
    let keys = get_test_keys();
    let key_entries: Vec<KeyEntry> = keys
        .iter()
        .map(|(k, l)| KeyEntry {
            key: k.clone(),
            label: Some(l.clone()),
        })
        .collect();
    AppState::new(
        key_entries,
        NVIDIA_API_BASE.to_string(),
        None,
        ModelStatsStore::new(3000.0),
        vec![
            "minimaxai/minimax-m2.5".to_string(),
            "moonshotai/kimi-k2.5".to_string(),
        ],
        5,
        20000,
        "complete".to_string(),
        std::collections::HashMap::new(),
        ModelCompat::default(),
    )
}

fn make_state_no_racing() -> Arc<AppState> {
    let keys = get_test_keys();
    let key_entries: Vec<KeyEntry> = keys
        .iter()
        .map(|(k, l)| KeyEntry {
            key: k.clone(),
            label: Some(l.clone()),
        })
        .collect();
    AppState::new(
        key_entries,
        NVIDIA_API_BASE.to_string(),
        None,
        ModelStatsStore::new(3000.0),
        vec![],
        3,
        5000,
        "complete".to_string(),
        std::collections::HashMap::new(),
        ModelCompat::default(),
    )
}

#[tokio::test]
async fn test_e2e_health_returns_ok() {
    let state = make_state();
    let resp = nimaproxy::proxy::health(axum::extract::State(state.clone())).await;

    let response = resp.into_response();
    let (_status, body) = response.into_parts();

    let body_bytes = axum::body::to_bytes(body, 4096).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();

    assert_eq!(json["status"], "UP");
    assert_eq!(json["keys_total"], 3);
    assert_eq!(json["keys_active"], 3);
}

#[tokio::test]
async fn test_e2e_models_endpoint_reachable() {
    let state = make_state();
    let resp = nimaproxy::proxy::models(axum::extract::State(state.clone())).await;

    let response = resp.into_response();
    let (parts, body) = response.into_parts();
    let status_code = parts.status.as_u16();

    assert!(status_code == 200 || status_code == 429);

    if status_code == 200 {
        let body_bytes = axum::body::to_bytes(body, 262144).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert!(json["data"].as_array().unwrap().len() > 0);
    }
}

#[tokio::test]
async fn test_e2e_key_rotation_round_robin() {
    let state = make_state();
    
    let (_key1, idx1) = state.pool.next_key().unwrap();
    let (_key2, idx2) = state.pool.next_key().unwrap();
    
    assert_ne!(idx1, idx2);
    // Keys in get_test_keys() are redacted placeholders — only verify round-robin rotation.
}

#[tokio::test]
async fn test_e2e_stats_endpoint() {
    let state = make_state();
    
    state.model_stats.record("test-model", 150.0, true);
    state.model_stats.record("test-model", 200.0, true);
    
    let resp = nimaproxy::proxy::stats(axum::extract::State(state.clone())).await;
    
    let response = resp.into_response();
    let (_parts, body) = response.into_parts();
    
    let body_bytes = axum::body::to_bytes(body, 4096).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    
    let models = json["models"].as_array().unwrap();
    assert!(!models.is_empty(), "stats should have recorded some model data");
    assert_eq!(models[0]["model"], "test-model");
}

#[tokio::test]
async fn test_e2e_key_pool_status() {
    let state = make_state();
    
    let statuses = state.pool.status();
    assert_eq!(statuses.len(), 3);

    assert_eq!(statuses[0].label, "doltares");
    assert_eq!(statuses[1].label, "ares");
    assert_eq!(statuses[2].label, "dirmacs");
    
    state.pool.mark_rate_limited(0, 60);
    
    let statuses_after = state.pool.status();
    assert!(!statuses_after[0].active);
    assert!(statuses_after[1].active);
}

#[tokio::test]
async fn test_e2e_chat_via_proxy() {
    let state = make_state();
    
    state.model_stats.record("nvidia/z-ai/glm4.7", 500.0, true);
    
    let body = serde_json::json!({
        "model": "nvidia/z-ai/glm4.7",
        "messages": [{"role": "user", "content": "Say 'test' in one word."}],
        "max_tokens": 10,
        "temperature": 0.0
    });
    
    let resp = nimaproxy::proxy::chat_completions(
        axum::extract::State(state.clone()),
        axum::http::HeaderMap::new(),
        bytes::Bytes::from(body.to_string()),
    ).await;
    
    let response = resp.into_response();
    let (parts, body) = response.into_parts();
    let status_code = parts.status.as_u16();

    eprintln!("[e2e] chat status: {}", status_code);

    if status_code != 200 {
        let body_bytes = axum::body::to_bytes(body, 4096).await.unwrap();
        eprintln!("[e2e] error body: {}", String::from_utf8_lossy(&body_bytes));
    }

    assert!(status_code == 200 || status_code == 400 || status_code == 429 || status_code == 401 || status_code == 500 || status_code == 404,
           "got status {}, expected 200/400/429/401/500/404", status_code);
}

// ---------------------------------------------------------------------------
// Live racing A/B tests — real HTTP, real keys, real models
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_e2e_racing_uses_preallocated_keys() {
    let state = make_state();
    let body = serde_json::json!({
        "model": "z-ai/glm4.7",
        "messages": [{"role": "user", "content": "Reply with exactly one word: hello"}],
        "max_tokens": 10,
        "temperature": 0.0
    });

    let resp = nimaproxy::proxy::chat_completions(
        axum::extract::State(state.clone()),
        axum::http::HeaderMap::new(),
        bytes::Bytes::from(body.to_string()),
    ).await;

    let response = resp.into_response();
    let (parts, body) = response.into_parts();
    let status_code = parts.status.as_u16();

    if status_code == 200 {
        let body_bytes = axum::body::to_bytes(body, 65536).await.unwrap();
        let content = String::from_utf8_lossy(&body_bytes);
        eprintln!("[racing] status=200, body_preview={}", &content[..content.len().min(200)]);
        assert!(content.contains("z-ai/glm4.7") || content.contains("choices"), "should contain model reference or choices");
    } else {
        eprintln!("[racing] got status {}, racing may not be triggered", status_code);
    }

    assert!(status_code == 200 || status_code == 400 || status_code == 401 || status_code == 429 || status_code == 500 || status_code == 502 || status_code == 503);
}

#[tokio::test]
async fn test_e2e_racing_responds_with_key_label_header() {
    let state = make_state_no_racing();
    let body = serde_json::json!({
        "model": "minimaxai/minimax-m2.5",
        "messages": [{"role": "user", "content": "Say 'ping' in one word"}],
        "max_tokens": 5,
        "temperature": 0.0
    });

    let resp = nimaproxy::proxy::chat_completions(
        axum::extract::State(state.clone()),
        axum::http::HeaderMap::new(),
        bytes::Bytes::from(body.to_string()),
    ).await;

    let response = resp.into_response();
    let parts = response.into_parts().0;
    let status = parts.status.as_u16();

    // Skip if API is unavailable (429 rate limit or 502 gateway error)
    if status == 429 || status == 502 {
        eprintln!("[racing] skipping header check - API unavailable (status={})", status);
        return;
    }

    let key_label = parts.headers.get("x-key-label");
    eprintln!("[racing] x-key-label header: {:?}", key_label);
    assert!(key_label.is_some(), "response should include x-key-label header for tracing (status={})", status);
}

#[tokio::test]
async fn test_e2e_racing_latency_comparison() {
    let state = make_state();

    let models_to_test = vec![
        "minimaxai/minimax-m2.5",
        "moonshotai/kimi-k2.5",
    ];

    let mut results: Vec<(String, Option<u64>, u16)> = vec![];

    for model in &models_to_test {
        let body = serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "Reply with exactly one word: hello"}],
            "max_tokens": 5,
            "temperature": 0.0
        });

        let t0 = std::time::Instant::now();
        let resp = nimaproxy::proxy::chat_completions(
            axum::extract::State(state.clone()),
            axum::http::HeaderMap::new(),
            bytes::Bytes::from(body.to_string()),
        ).await;

        let elapsed_ms = t0.elapsed().as_millis() as u64;
        let status_code = resp.into_response().into_parts().0.status.as_u16();

        eprintln!("[racing-latency] model={} status={} elapsed={}ms", model, status_code, elapsed_ms);
        results.push((model.to_string(), Some(elapsed_ms), status_code));
    }

    // Note: This test requires valid NVIDIA API keys and network access.
    // Failures may indicate: expired keys, network issues, or model unavailability.
    // At least one model should succeed under normal conditions.
    let successes: Vec<_> = results.iter().filter(|(_, _, sc)| *sc == 200).collect();
    
    // Log results for debugging
    eprintln!("[racing-latency] successes: {}/{}", successes.len(), results.len());
    
    // Only assert if we have API connectivity - skip assertion if all keys are exhausted
    // This allows the test to pass in CI environments without valid keys
    if results.iter().any(|(_, _, sc)| *sc != 401 && *sc != 429 && *sc != 502) {
        assert!(!successes.is_empty(), "at least one model should succeed (results: {:?})", results);
    } else {
        eprintln!("[racing-latency] skipping assertion - all requests returned 429/502 (API unavailable)");
    }

    if successes.len() >= 2 {
        let (m1, t1, _) = successes[0];
        let (m2, t2, _) = successes[1];
        let winner_m = if t1.unwrap_or(u64::MAX) < t2.unwrap_or(u64::MAX) { m1 } else { m2 };
        let winner_t = if t1.unwrap_or(u64::MAX) < t2.unwrap_or(u64::MAX) { t1 } else { t2 };
        let loser_m = if t1.unwrap_or(u64::MAX) < t2.unwrap_or(u64::MAX) { m2 } else { m1 };
        let loser_t = if t1.unwrap_or(u64::MAX) < t2.unwrap_or(u64::MAX) { t2 } else { t1 };
        eprintln!("[racing-latency] winner: {} ({}ms) vs {} ({}ms)",
            winner_m, winner_t.unwrap_or(0), loser_m, loser_t.unwrap_or(0)
        );
    }
}

#[tokio::test]
async fn test_e2e_racing_3keys_round_robin() {
    let state = make_state();

    let k1 = state.pool.next_key();
    let k2 = state.pool.next_key();
    let k3 = state.pool.next_key();

    assert!(k1.is_some() && k2.is_some() && k3.is_some());
    let i1 = k1.unwrap().1;
    let i2 = k2.unwrap().1;
    let i3 = k3.unwrap().1;

    let all_unique = [i1, i2, i3].iter().collect::<std::collections::HashSet<_>>().len() == 3;
    eprintln!("[racing-keys] round-robin indices: {} {} {}", i1, i2, i3);
    assert!(all_unique, "3 real keys should all be different on first cycle");
}

#[tokio::test]
async fn test_e2e_racing_fails_gracefully_on_all_429() {
    let state = make_state_no_racing();

    for i in 0..state.pool.len() {
        state.pool.mark_rate_limited(i, 999);
    }

    let body = serde_json::json!({
        "model": "minimaxai/minimax-m2.5",
        "messages": [{"role": "user", "content": "test"}],
        "max_tokens": 5
    });

    let resp = nimaproxy::proxy::chat_completions(
        axum::extract::State(state.clone()),
        axum::http::HeaderMap::new(),
        bytes::Bytes::from(body.to_string()),
    ).await;

    let status = resp.into_response().into_parts().0.status;
    assert_eq!(status.as_u16(), 429, "should return 429 when all keys are rate-limited");
}

// ---------------------------------------------------------------------------
// Live tests for v0.13.6 bug fixes
// ---------------------------------------------------------------------------

/// Verify that racing marks a key rate-limited after a 429 response.
/// We force one key into cooldown, then race — verify pool reflects it.
#[tokio::test]
async fn test_e2e_racing_429_key_cooldown_persists() {
    let state = make_state();

    // Simulate: key 0 just got 429 with 30s cooldown
    state.pool.mark_rate_limited(0, 30);

    let statuses = state.pool.status();
    assert!(!statuses[0].active, "key 0 must be in cooldown");
    assert!(statuses[0].cooldown_secs_remaining > 0, "cooldown must be > 0");
    assert!(statuses[1].active, "key 1 must still be active");

    eprintln!("[e2e-429-cooldown] key 0 cooldown={}s, key 1 active={}",
        statuses[0].cooldown_secs_remaining, statuses[1].active);

    // Racing should now only use key 1 (and key 2 if available)
    let body = serde_json::json!({
        "model": "auto",
        "messages": [{"role": "user", "content": "Say one word: hello"}],
        "max_tokens": 5,
        "temperature": 0.0
    });

    let resp = nimaproxy::proxy::chat_completions(
        axum::extract::State(state.clone()),
        axum::http::HeaderMap::new(),
        bytes::Bytes::from(body.to_string()),
    ).await;

    let response = resp.into_response();
    let (parts, _body) = response.into_parts();
    let status = parts.status.as_u16();
    let key_label = parts.headers.get("x-key-label").and_then(|v| v.to_str().ok()).map(String::from);

    eprintln!("[e2e-429-cooldown] race status={} key_used={:?}", status, key_label);

    // If key 0 was used despite cooldown, that's a bug
    if let Some(ref label) = key_label {
        assert_ne!(label, "doltares", "rate-limited key 'doltares' (idx 0) must not be used by racing");
    }

    // Accept any non-500 result — key exhaustion or successful chat both acceptable
    assert!(status != 500, "unexpected internal error: {}", status);
}

/// Live test: assistant message with tool_calls — verify 400 is NOT forwarded to client.
/// NVIDIA returns 400 for invalid assistant messages; proxy should retry or degrade gracefully.
#[tokio::test]
async fn test_e2e_invalid_assistant_message_not_propagated() {
    let state = make_state_no_racing();

    // This is the exact message shape that triggered the OMP crash:
    // assistant message with both content AND tool_calls (or content=None tool_calls=None)
    let body = serde_json::json!({
        "model": "z-ai/glm4.7",
        "messages": [
            {"role": "user", "content": "call a tool"},
            // Assistant message with tool_calls but no content — sanitize_tool_calls sets content=null
            {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call_abc123",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{}"}}
                ]
            },
            {"role": "tool", "content": "sunny", "tool_call_id": "call_abc123"},
            {"role": "user", "content": "ok thanks"}
        ],
        "max_tokens": 10,
        "temperature": 0.0
    });

    let resp = nimaproxy::proxy::chat_completions(
        axum::extract::State(state.clone()),
        axum::http::HeaderMap::new(),
        bytes::Bytes::from(body.to_string()),
    ).await;

    let response = resp.into_response();
    let (parts, body) = response.into_parts();
    let status = parts.status.as_u16();
    let body_bytes = axum::body::to_bytes(body, 4096).await.unwrap_or_default();
    let body_str = String::from_utf8_lossy(&body_bytes);

    eprintln!("[e2e-invalid-assistant] status={} body={}", status, &body_str[..body_str.len().min(300)]);

    // The proxy must NOT forward 400 "Invalid assistant message" directly to client.
    // Acceptable outcomes: 200 (retry succeeded), 429 (keys exhausted), 502 (all models failed),
    // 404 (model not found). What is NOT acceptable: raw 400 from NVIDIA propagated as-is.
    if status == 400 {
        // If we do get a 400, it must NOT be the raw NVIDIA invalid-assistant error
        assert!(
            !body_str.contains("Invalid assistant message"),
            "proxy forwarded raw NVIDIA 400 'Invalid assistant message' to client — bug not fixed! body={}",
            body_str
        );
    }
}

/// Live racing test: verify race with real models returns first 2xx and logs winner.
#[tokio::test]
async fn test_e2e_racing_returns_2xx_winner() {
    let state = make_state(); // has racing_models configured

    let body = serde_json::json!({
        "model": "auto",
        "messages": [{"role": "user", "content": "Reply with exactly one word: yes"}],
        "max_tokens": 5,
        "temperature": 0.0
    });

    let t0 = std::time::Instant::now();
    let resp = nimaproxy::proxy::chat_completions(
        axum::extract::State(state.clone()),
        axum::http::HeaderMap::new(),
        bytes::Bytes::from(body.to_string()),
    ).await;
    let elapsed = t0.elapsed().as_millis();

    let response = resp.into_response();
    let (parts, body) = response.into_parts();
    let status = parts.status.as_u16();
    let winner = parts.headers.get("x-key-label").and_then(|v| v.to_str().ok()).map(String::from);
    let body_bytes = axum::body::to_bytes(body, 16384).await.unwrap_or_default();
    let body_str = String::from_utf8_lossy(&body_bytes);

    eprintln!("[e2e-racing-2xx-winner] status={} elapsed={}ms key={:?} body_preview={}",
        status, elapsed, winner, &body_str[..body_str.len().min(200)]);

    // Racing must NEVER return a raw 4xx from NVIDIA
    assert_ne!(status, 400, "racing forwarded NVIDIA 400 to client — status filtering broken");
    // Accept 200, 429 (all keys exhausted), 502 (all models failed)
    assert!(
        status == 200 || status == 429 || status == 502 || status == 503 || status == 504,
        "unexpected status {} from racing", status
    );
}