use std::time::Instant;

const DEFAULT_NUM_TURNS: usize = 25;

const SYSTEM_PROMPT: &str = r#"You are a coding assistant. Answer briefly."#;

fn proxy_url() -> String {
    std::env::var("NIMAPROXY_STRESS_URL")
        .or_else(|_| std::env::var("NIMAPROXY_URL"))
        .unwrap_or_else(|_| "http://127.0.0.1:8080".to_string())
        .trim_end_matches('/')
        .to_string()
}

fn stress_turns() -> usize {
    std::env::var("NIMAPROXY_STRESS_TURNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_NUM_TURNS)
}

fn assert_nimaproxy_target(client: &reqwest::blocking::Client, proxy_url: &str) {
    let health_url = format!("{}/health", proxy_url);
    let response = client.get(&health_url).send().unwrap_or_else(|e| {
        panic!(
            "stress test target is not reachable at {}: {}",
            health_url, e
        )
    });

    let status = response.status();
    let body = response.text().unwrap_or_default();
    assert!(
        status.is_success(),
        "stress test target health check failed at {}: HTTP {} body={}",
        health_url,
        status,
        body
    );

    let json: serde_json::Value = serde_json::from_str(&body).unwrap_or_else(|e| {
        panic!(
            "stress test target at {} does not look like nimaproxy /health JSON: {} body={}",
            health_url, e, body
        )
    });

    assert_eq!(
        json.get("status").and_then(|v| v.as_str()),
        Some("UP"),
        "stress test target at {} is not a healthy nimaproxy instance: {}",
        health_url,
        body
    );
    assert!(
        json.get("keys_total").and_then(|v| v.as_u64()).unwrap_or(0) > 0,
        "stress test target at {} has no configured keys: {}",
        health_url,
        body
    );
    assert!(
        json.get("racing_enabled").and_then(|v| v.as_bool()) == Some(true),
        "stress test requires racing_enabled=true at {}: {}",
        health_url,
        body
    );
}

#[test]
fn stress_test() {
    let num_turns = stress_turns();
    let sep = "=".repeat(80);
    println!("{}", sep);
    println!(
        "nimaproxy STRESS TEST - {} turns with racing + key rotation",
        num_turns
    );
    println!("{}", sep);
    println!();

    let proxy_url = proxy_url();
    println!("Target: {}", proxy_url);

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("Failed to build HTTP client");

    assert_nimaproxy_target(&client, &proxy_url);

    let mut total_requests = 0;
    let mut total_tokens = 0u64;
    let mut total_latency_ms = 0u64;
    let mut key_usage = std::collections::HashMap::new();
    let mut model_wins = std::collections::HashMap::new();
    model_wins.insert("minimaxai/minimax-m3".to_string(), 0);
    model_wins.insert("z-ai/glm-5.1".to_string(), 0);
    model_wins.insert("stepfun-ai/step-3.7-flash".to_string(), 0);
    model_wins.insert("moonshotai/kimi-k2.6".to_string(), 0);
    model_wins.insert("qwen/qwen3.5-397b-a17b".to_string(), 0);
    model_wins.insert("minimaxai/minimax-m2.7".to_string(), 0);
    model_wins.insert("nvidia/nemotron-3-ultra-550b-a55b".to_string(), 0);
    model_wins.insert("deepseek-ai/deepseek-v4-flash".to_string(), 0);
    let mut errors = Vec::new();

    let conversation: Vec<&str> = vec![
        "Write a hello world program in Rust.",
        "What is 123 * 456?",
        "Explain what a closure is in Python.",
        "Create a simple REST API endpoint.",
        "What is the time complexity of binary search?",
        "Write a function to reverse a linked list.",
        "Explain the difference between REST and GraphQL.",
        "What is a mutex in concurrent programming?",
        "Write a SQL query to find duplicates.",
        "What are SOLID principles?",
        "Explain async/await in JavaScript.",
        "Write a quick sort in Python.",
        "What is the difference between SQL and NoSQL?",
        "Explain memoization in dynamic programming.",
        "Write a binary search tree in TypeScript.",
        "What is the CAP theorem?",
        "Explain OAuth 2.0.",
        "Write a function to merge two sorted arrays.",
        "What is the difference between processes and threads?",
        "Explain the observer pattern.",
        "Write a LRU cache in Go.",
        "What is dependency injection?",
        "Explain a trie data structure.",
        "Write a function to detect a cycle in a linked list.",
        "What is the difference between mutable and immutable in Rust?",
    ];

    for turn in 0..num_turns {
        let user_message = conversation[turn % conversation.len()];
        let start = Instant::now();

        let request_body = serde_json::json!({
            "model": "auto",
            "messages": [
                {"role": "system", "content": SYSTEM_PROMPT},
                {"role": "user", "content": user_message}
            ],
            "max_tokens": 100,
            "temperature": 0.7
        });

        let mut attempt = 0;
        let mut success = false;

        while attempt < 3 && !success {
            attempt += 1;
            let body = serde_json::to_string(&request_body).unwrap();
            let resp = client
                .post(&format!("{}/v1/chat/completions", proxy_url))
                .header("Content-Type", "application/json")
                .body(body)
                .send();

            match resp {
                Ok(response) => {
                    let elapsed = start.elapsed().as_millis() as u64;

                    if response.status().is_success() {
                        // Get headers before consuming response body
                        let key_label = response
                            .headers()
                            .get("x-key-label")
                            .and_then(|v| v.to_str().ok())
                            .map(|s| s.to_string());

                        let resp_body = response.text().unwrap_or_default();
                        let data: serde_json::Value =
                            serde_json::from_str(&resp_body).unwrap_or(serde_json::Value::Null);

                        total_latency_ms += elapsed;

                        let model = data
                            .get("model")
                            .and_then(|m| m.as_str())
                            .unwrap_or("unknown");
                        *model_wins.entry(model.to_string()).or_insert(0) += 1;

                        if let Some(ref label) = key_label {
                            *key_usage.entry(label.clone()).or_insert(0) += 1;
                        }

                        let tokens = data
                            .get("usage")
                            .and_then(|u| u.get("total_tokens"))
                            .and_then(|t| t.as_u64())
                            .unwrap_or(0);
                        total_tokens += tokens;
                        total_requests += 1;
                        success = true;

                        let marker = if turn % 5 == 0 { "█" } else { "▌" };
                        println!(
                            "[{}{:02}] {}ms | toks: {:4} | model: {}",
                            marker,
                            turn + 1,
                            elapsed,
                            tokens,
                            model
                        );
                    } else {
                        let status = response.status();
                        if status.as_u16() == 429 {
                            println!("[{:02}] Rate limited, backing off...", turn + 1);
                            std::thread::sleep(std::time::Duration::from_millis(
                                500 * attempt as u64,
                            ));
                        } else {
                            errors.push(format!("Turn {}: HTTP {}", turn + 1, status));
                        }
                    }
                }
                Err(e) => {
                    errors.push(format!("Turn {}: {}", turn + 1, e));
                }
            }
        }

        if !success {
            println!("[{:02}] FAILED after {} attempts", turn + 1, attempt);
        }

        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    println!();
    println!("{}", sep);
    println!("RESULTS SUMMARY");
    println!("{}", sep);
    println!();
    println!("Total Requests:  {}", total_requests);
    println!("Total Tokens:   {}", total_tokens);
    println!(
        "Avg Latency:    {} ms",
        total_latency_ms / total_requests.max(1)
    );
    println!();
    println!("KEY USAGE:");
    let mut sorted_keys: Vec<_> = key_usage.iter().collect();
    sorted_keys.sort_by(|a, b| b.1.cmp(a.1));
    for (key, count) in sorted_keys {
        println!("  {}: {}", key, count);
    }
    println!();
    println!("MODEL WINS (Racing):");
    let mut sorted_models: Vec<_> = model_wins.iter().collect();
    sorted_models.sort_by(|a, b| b.1.cmp(a.1));
    for (model, count) in sorted_models {
        println!(
            "  {}: {} ({}%)",
            model,
            count,
            count * 100 / total_requests.max(1)
        );
    }
    println!();

    if !errors.is_empty() {
        println!("ERRORS:");
        for e in errors.iter().take(5) {
            println!("  {}", e);
        }
    } else {
        println!("No errors!");
    }

    println!();
    println!("{}", sep);
    println!("HYPOTHESIS TEST");
    println!("{}", sep);
    println!();
    println!(
        "Key rotation: {}",
        if key_usage.len() > 1 {
            "✓ YES"
        } else {
            "✗ NO"
        }
    );
    println!(
        "Racing:       {}",
        if model_wins.iter().filter(|(_, count)| **count > 0).count() > 1 {
            "✓ YES"
        } else {
            "✗ NO"
        }
    );
    println!();

    assert!(
        total_requests > 0,
        "stress test did not complete any successful requests; first errors: {:?}",
        errors.iter().take(5).collect::<Vec<_>>()
    );
    assert!(
        total_requests >= (num_turns / 2) as u64,
        "stress test completed too few successful requests: {}/{}; first errors: {:?}",
        total_requests,
        num_turns,
        errors.iter().take(5).collect::<Vec<_>>()
    );
    assert!(
        !key_usage.is_empty(),
        "stress test did not observe any x-key-label headers"
    );
    assert!(
        model_wins.values().any(|count| *count > 0),
        "stress test did not observe any successful racing model responses"
    );
}
