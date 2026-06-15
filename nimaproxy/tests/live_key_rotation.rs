//! Live gateway tests for key rotation and bounded key pressure.
//!
//! These tests target a running nimaproxy instance instead of constructing an
//! in-process proxy with one API key. That keeps the live suite aligned with
//! production behavior and avoids the old 100-request single-model Qwen loop.
//!
//! Run with:
//!   NIMAPROXY_URL=http://127.0.0.1:8080 cargo test --test live_key_rotation -- --nocapture

use reqwest::blocking::Client;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Barrier};
use std::time::Duration;

const DEFAULT_ROTATION_TURNS: usize = 6;
const DEFAULT_BURST_REQUESTS: usize = 4;
const REQUEST_TIMEOUT_SECS: u64 = 35;

#[derive(Clone, Debug)]
struct GatewaySnapshot {
    request_total: u64,
    racing_requests: u64,
    upstream_attempts: u64,
    overload_rejects: u64,
    no_key_rejects: u64,
    timeout_count: u64,
    rate_limit_count: u64,
    racing_all_failed: u64,
    racing_deadline_exceeded: u64,
}

impl GatewaySnapshot {
    fn delta(&self, before: &Self) -> Self {
        Self {
            request_total: self.request_total.saturating_sub(before.request_total),
            racing_requests: self.racing_requests.saturating_sub(before.racing_requests),
            upstream_attempts: self
                .upstream_attempts
                .saturating_sub(before.upstream_attempts),
            overload_rejects: self
                .overload_rejects
                .saturating_sub(before.overload_rejects),
            no_key_rejects: self.no_key_rejects.saturating_sub(before.no_key_rejects),
            timeout_count: self.timeout_count.saturating_sub(before.timeout_count),
            rate_limit_count: self
                .rate_limit_count
                .saturating_sub(before.rate_limit_count),
            racing_all_failed: self
                .racing_all_failed
                .saturating_sub(before.racing_all_failed),
            racing_deadline_exceeded: self
                .racing_deadline_exceeded
                .saturating_sub(before.racing_deadline_exceeded),
        }
    }
}

#[derive(Debug)]
struct ChatOutcome {
    status: u16,
    key_label: Option<String>,
    model: Option<String>,
    body_preview: String,
}

fn proxy_url() -> String {
    std::env::var("NIMAPROXY_KEY_ROTATION_URL")
        .or_else(|_| std::env::var("NIMAPROXY_URL"))
        .unwrap_or_else(|_| "http://127.0.0.1:8080".to_string())
        .trim_end_matches('/')
        .to_string()
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

fn http_client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .build()
        .expect("failed to build HTTP client")
}

fn get_json(client: &Client, proxy_url: &str, path: &str) -> Value {
    let url = format!("{proxy_url}{path}");
    let response = client
        .get(&url)
        .send()
        .unwrap_or_else(|e| panic!("GET {url} failed: {e}"));
    let status = response.status();
    let body = response.text().unwrap_or_default();
    assert!(
        status.is_success(),
        "GET {url} returned HTTP {status}: {body}"
    );
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("GET {url} returned invalid JSON: {e}"))
}

fn gateway_snapshot(client: &Client, proxy_url: &str) -> GatewaySnapshot {
    let stats = get_json(client, proxy_url, "/stats");
    let gateway = stats
        .get("gateway")
        .unwrap_or_else(|| panic!("/stats missing gateway object: {stats}"));

    let num = |field: &str| gateway.get(field).and_then(|v| v.as_u64()).unwrap_or(0);

    GatewaySnapshot {
        request_total: num("request_total"),
        racing_requests: num("racing_requests"),
        upstream_attempts: num("upstream_attempts"),
        overload_rejects: num("overload_rejects"),
        no_key_rejects: num("no_key_rejects"),
        timeout_count: num("timeout_count"),
        rate_limit_count: num("rate_limit_count"),
        racing_all_failed: num("racing_all_failed"),
        racing_deadline_exceeded: num("racing_deadline_exceeded"),
    }
}

fn assert_gateway_ready(client: &Client, proxy_url: &str) -> u64 {
    let health = get_json(client, proxy_url, "/health");
    assert_eq!(
        health.get("status").and_then(|v| v.as_str()),
        Some("UP"),
        "nimaproxy target is not UP: {health}"
    );
    assert_eq!(
        health.get("racing_enabled").and_then(|v| v.as_bool()),
        Some(true),
        "live key rotation tests require racing_enabled=true: {health}"
    );
    let keys_total = health
        .get("keys_total")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    assert!(
        keys_total > 0,
        "live key rotation tests require configured keys: {health}"
    );
    keys_total
}

fn send_chat(client: &Client, proxy_url: &str, prompt: &str) -> ChatOutcome {
    let body = serde_json::json!({
        "model": "nimaproxy/auto",
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": 32,
        "temperature": 0.0
    });

    let response = client
        .post(format!("{proxy_url}/v1/chat/completions"))
        .header("Content-Type", "application/json")
        .body(serde_json::to_string(&body).expect("chat body should serialize"))
        .send()
        .expect("chat request failed before receiving an HTTP response");

    let status = response.status().as_u16();
    let key_label = response
        .headers()
        .get("x-key-label")
        .and_then(|v| v.to_str().ok())
        .map(ToOwned::to_owned);
    let body = response.text().unwrap_or_default();
    let parsed = serde_json::from_str::<Value>(&body).unwrap_or(Value::Null);
    let model = parsed
        .get("model")
        .and_then(|m| m.as_str())
        .map(ToOwned::to_owned);
    let body_preview: String = body.chars().take(240).collect();

    ChatOutcome {
        status,
        key_label,
        model,
        body_preview,
    }
}

fn upstream_unavailable(status: u16) -> bool {
    matches!(status, 429 | 502 | 503 | 504)
}

fn assert_only_expected_statuses(outcomes: &[ChatOutcome]) {
    for outcome in outcomes {
        assert!(
            outcome.status == 200 || upstream_unavailable(outcome.status),
            "unexpected live gateway status {} body={}",
            outcome.status,
            outcome.body_preview
        );
    }
}

#[test]
fn test_live_gateway_key_rotation_smoke() {
    let proxy_url = proxy_url();
    let client = http_client();
    let keys_total = assert_gateway_ready(&client, &proxy_url);
    let turns = env_usize("NIMAPROXY_KEY_ROTATION_TURNS", DEFAULT_ROTATION_TURNS);
    let before = gateway_snapshot(&client, &proxy_url);

    eprintln!("[key-rotation] target={proxy_url} turns={turns} keys_total={keys_total}");

    let mut outcomes = Vec::with_capacity(turns);
    for i in 0..turns {
        let outcome = send_chat(
            &client,
            &proxy_url,
            &format!("Reply with exactly one word: rotation-{i}"),
        );
        eprintln!(
            "[key-rotation] request={} status={} key={:?} model={:?}",
            i + 1,
            outcome.status,
            outcome.key_label,
            outcome.model
        );
        outcomes.push(outcome);
    }

    assert_only_expected_statuses(&outcomes);
    let successes: Vec<_> = outcomes.iter().filter(|o| o.status == 200).collect();
    if successes.is_empty() {
        eprintln!("[key-rotation] no successful live responses; upstream unavailable");
        assert!(
            outcomes.iter().all(|o| upstream_unavailable(o.status)),
            "all failed live responses should be upstream-unavailable statuses"
        );
        return;
    }

    let mut key_usage: HashMap<String, usize> = HashMap::new();
    for outcome in successes {
        if let Some(label) = &outcome.key_label {
            *key_usage.entry(label.clone()).or_insert(0) += 1;
        }
    }
    eprintln!("[key-rotation] key_usage={key_usage:?}");

    assert!(
        !key_usage.is_empty(),
        "successful gateway responses should expose x-key-label"
    );
    if keys_total > 1 && key_usage.values().sum::<usize>() >= 2 {
        assert!(
            key_usage.len() >= 2,
            "expected successful requests to rotate across keys; usage={key_usage:?}"
        );
    }

    let after = gateway_snapshot(&client, &proxy_url);
    let delta = after.delta(&before);
    eprintln!("[key-rotation] gateway_delta={delta:?}");
    eprintln!(
        "[key-rotation] transient_counts timeouts={} rate_limits={} all_failed={} deadlines={}",
        delta.timeout_count,
        delta.rate_limit_count,
        delta.racing_all_failed,
        delta.racing_deadline_exceeded
    );

    assert_eq!(delta.request_total, turns as u64);
    assert_eq!(delta.racing_requests, turns as u64);
    assert!(
        delta.upstream_attempts >= key_usage.values().sum::<usize>() as u64,
        "upstream attempts should cover successful gateway calls"
    );
    assert_eq!(
        delta.overload_rejects, 0,
        "sequential smoke should not overload"
    );
    assert_eq!(
        delta.no_key_rejects, 0,
        "sequential smoke should not exhaust keys"
    );
}

#[test]
fn test_live_gateway_bounded_burst_keeps_key_pool_usable() {
    let proxy_url = proxy_url();
    let client = http_client();
    let keys_total = assert_gateway_ready(&client, &proxy_url);
    let request_count = env_usize("NIMAPROXY_KEY_ROTATION_BURST", DEFAULT_BURST_REQUESTS);
    let before = gateway_snapshot(&client, &proxy_url);

    eprintln!("[key-burst] target={proxy_url} requests={request_count} keys_total={keys_total}");

    let barrier = Arc::new(Barrier::new(request_count));
    let mut handles = Vec::with_capacity(request_count);
    for i in 0..request_count {
        let client = client.clone();
        let proxy_url = proxy_url.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            send_chat(
                &client,
                &proxy_url,
                &format!("Reply with exactly one word: burst-{i}"),
            )
        }));
    }

    let outcomes: Vec<ChatOutcome> = handles
        .into_iter()
        .map(|h| h.join().expect("burst worker panicked"))
        .collect();
    for (idx, outcome) in outcomes.iter().enumerate() {
        eprintln!(
            "[key-burst] request={} status={} key={:?} model={:?}",
            idx + 1,
            outcome.status,
            outcome.key_label,
            outcome.model
        );
    }

    assert_only_expected_statuses(&outcomes);

    let after = gateway_snapshot(&client, &proxy_url);
    let delta = after.delta(&before);
    eprintln!("[key-burst] gateway_delta={delta:?}");
    eprintln!(
        "[key-burst] transient_counts timeouts={} rate_limits={} all_failed={} deadlines={}",
        delta.timeout_count,
        delta.rate_limit_count,
        delta.racing_all_failed,
        delta.racing_deadline_exceeded
    );

    assert_eq!(delta.request_total, request_count as u64);
    assert_eq!(delta.racing_requests, request_count as u64);
    assert_eq!(
        delta.overload_rejects, 0,
        "bounded burst should stay below configured gateway capacity"
    );
    assert_eq!(
        delta.no_key_rejects, 0,
        "bounded burst should not force all keys exhausted"
    );

    let probe = send_chat(&client, &proxy_url, "Reply with exactly one word: recovery");
    eprintln!(
        "[key-burst] recovery status={} key={:?} model={:?}",
        probe.status, probe.key_label, probe.model
    );
    assert!(
        probe.status == 200 || upstream_unavailable(probe.status),
        "gateway should remain usable after bounded burst; got {} body={}",
        probe.status,
        probe.body_preview
    );

    let health = get_json(&client, &proxy_url, "/health");
    assert_eq!(
        health.get("status").and_then(|v| v.as_str()),
        Some("UP"),
        "gateway should remain UP after bounded burst: {health}"
    );
}
