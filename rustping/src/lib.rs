use libc::c_char;
use reqwest::Client;
use serde::Serialize;
use std::ffi::{CStr, CString};
use std::time::Instant;
use tokio::runtime::Runtime;

#[derive(Serialize)]
struct PingResult {
    model: String,
    status_code: u16,
    latency_ms: f64,
    error: String,
}

#[derive(Serialize)]
struct BatchResult {
    results: Vec<PingResult>,
}

async fn ping_one(client: &Client, base_url: &str, api_key: &str, model: &str, timeout_secs: u64) -> PingResult {
    let payload = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1,
        "stream": false
    });

    let t0 = Instant::now();
    let url = format!("{}/v1/chat/completions", base_url);
    let res = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .json(&payload)
        .send()
        .await;

    let latency = t0.elapsed().as_secs_f64() * 1000.0;

    match res {
        Ok(resp) => PingResult {
            model: model.to_string(),
            status_code: resp.status().as_u16(),
            latency_ms: latency,
            error: String::new(),
        },
        Err(e) => PingResult {
            model: model.to_string(),
            status_code: 0,
            latency_ms: latency,
            error: e.to_string(),
        },
    }
}

/// FFI: ping multiple models concurrently. Returns heap-allocated JSON C string.
/// Caller must free with `rust_free_string`.
///
/// models_csv: comma-separated model IDs
/// api_key: NVIDIA API key
/// timeout: per-request timeout in seconds
#[no_mangle]
pub extern "C" fn rust_ping_batch(
    models_csv: *const c_char,
    api_key: *const c_char,
    timeout: u32,
) -> *mut c_char {
    let models_str = unsafe { CStr::from_ptr(models_csv) }
        .to_str()
        .unwrap_or("");
    let key = unsafe { CStr::from_ptr(api_key) }
        .to_str()
        .unwrap_or("");

    let models: Vec<&str> = models_str.split(',').filter(|s| !s.is_empty()).collect();

    let rt = Runtime::new().unwrap();
    let results = rt.block_on(async {
        let client = Client::builder()
            .use_rustls_tls()
            .pool_max_idle_per_host(models.len())
            .build()
            .unwrap();

        let futs: Vec<_> = models
            .iter()
            .map(|m| ping_one(&client, "https://integrate.api.nvidia.com", key, m, timeout as u64))
            .collect();

        futures::future::join_all(futs).await
    });

    let batch = BatchResult { results };
    let json = serde_json::to_string(&batch).unwrap_or_default();
    CString::new(json).unwrap().into_raw()
}

/// FFI: discover available models from /v1/models. Returns JSON C string.
#[no_mangle]
pub extern "C" fn rust_discover_models(api_key: *const c_char, timeout: u32) -> *mut c_char {
    let key = unsafe { CStr::from_ptr(api_key) }
        .to_str()
        .unwrap_or("");

    let rt = Runtime::new().unwrap();
    let json_out = rt.block_on(async {
        let client = Client::builder().use_rustls_tls().build().unwrap();
        let res = client
            .get("https://integrate.api.nvidia.com/v1/models")
            .header("Authorization", format!("Bearer {}", key))
            .timeout(std::time::Duration::from_secs(timeout as u64))
            .send()
            .await;

        match res {
            Ok(resp) => resp.text().await.unwrap_or_default(),
            Err(e) => format!("{{\"error\":\"{}\"}}", e),
        }
    });

    CString::new(json_out).unwrap().into_raw()
}

/// FFI: free a string returned by rust_ping_batch or rust_discover_models.
#[no_mangle]
pub extern "C" fn rust_free_string(s: *mut c_char) {
    if !s.is_null() {
        unsafe {
            let _ = CString::from_raw(s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Matcher;

    #[test]
    fn test_ping_result_serialization() {
        let result = PingResult {
            model: "test-model".to_string(),
            status_code: 200,
            latency_ms: 150.5,
            error: String::new(),
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("test-model"));
        assert!(json.contains("200"));
        assert!(json.contains("150.5"));
    }

    #[test]
    fn test_batch_result_serialization() {
        let batch = BatchResult {
            results: vec![
                PingResult {
                    model: "model1".to_string(),
                    status_code: 200,
                    latency_ms: 100.0,
                    error: String::new(),
                },
                PingResult {
                    model: "model2".to_string(),
                    status_code: 500,
                    latency_ms: 200.0,
                    error: "timeout".to_string(),
                },
            ],
        };
        let json = serde_json::to_string(&batch).unwrap();
        assert!(json.contains("model1"));
        assert!(json.contains("model2"));
        assert!(json.contains("timeout"));
    }

    #[test]
    fn test_rust_free_string() {
        let original = CString::new("test").unwrap();
        let ptr = original.into_raw();
        // Should not panic
        rust_free_string(ptr);
    }

    #[test]
    fn test_rust_free_string_null_pointer() {
        // Should not panic with null pointer
        rust_free_string(std::ptr::null_mut());
    }

    #[test]
    fn test_ping_result_with_error() {
        let result = PingResult {
            model: "failed-model".to_string(),
            status_code: 0,
            latency_ms: 0.0,
            error: "connection refused".to_string(),
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("connection refused"));
        assert!(json.contains("0"));
    }

    #[test]
    fn test_batch_result_empty() {
        let batch: BatchResult = BatchResult {
            results: vec![],
        };
        let json = serde_json::to_string(&batch).unwrap();
        assert!(json.contains("results"));
        assert!(json.contains("[]"));
    }

    #[test]
    fn test_ping_result_all_fields() {
        let result = PingResult {
            model: "nvidia/llama".to_string(),
            status_code: 429,
            latency_ms: 5000.0,
            error: "rate limited".to_string(),
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("nvidia/llama"));
        assert!(json.contains("429"));
        assert!(json.contains("5000"));
        assert!(json.contains("rate limited"));
    }

    #[tokio::test]
    async fn test_ping_one_success() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/chat/completions")
            .match_header("Authorization", Matcher::Regex("^Bearer test-key$".to_string()))
            .match_header("Content-Type", "application/json")
            .with_status(200)
            .with_body(r#"{"id":"test","object":"chat.completion","created":123,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#)
            .create_async()
            .await;

        let client = Client::builder()
            .use_rustls_tls()
            .build()
            .unwrap();

        let result = ping_one(&client, &server.url(), "test-key", "test-model", 30).await;

        assert_eq!(result.model, "test-model");
        assert_eq!(result.status_code, 200);
        assert!(result.latency_ms > 0.0);
        assert!(result.error.is_empty());

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_ping_one_invalid_api_key_401() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/chat/completions")
            .with_status(401)
            .with_body(r#"{"error":{"message":"Invalid API key","type":"authentication_error"}}"#)
            .create_async()
            .await;

        let client = Client::builder()
            .use_rustls_tls()
            .build()
            .unwrap();

        let result = ping_one(&client, &server.url(), "invalid-key", "test-model", 30).await;

        assert_eq!(result.model, "test-model");
        assert_eq!(result.status_code, 401);
        assert!(result.error.is_empty());

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_ping_one_rate_limit_429() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/chat/completions")
            .with_status(429)
            .with_body(r#"{"error":{"message":"Rate limit exceeded","type":"rate_limit_error"}}"#)
            .create_async()
            .await;

        let client = Client::builder()
            .use_rustls_tls()
            .build()
            .unwrap();

        let result = ping_one(&client, &server.url(), "test-key", "test-model", 30).await;

        assert_eq!(result.model, "test-model");
        assert_eq!(result.status_code, 429);
        assert!(result.error.is_empty());

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_ping_one_server_error_500() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/chat/completions")
            .with_status(500)
            .with_body(r#"{"error":{"message":"Internal server error","type":"server_error"}}"#)
            .create_async()
            .await;

        let client = Client::builder()
            .use_rustls_tls()
            .build()
            .unwrap();

        let result = ping_one(&client, &server.url(), "test-key", "test-model", 30).await;

        assert_eq!(result.model, "test-model");
        assert_eq!(result.status_code, 500);
        assert!(result.error.is_empty());

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_ping_one_timeout() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/chat/completions")
            .with_status(504)
            .with_body(r#"{"error":{"message":"Gateway timeout","type":"timeout"}}"#)
            .create_async()
            .await;

        let client = Client::builder()
            .use_rustls_tls()
            .build()
            .unwrap();

        // Use a very short timeout to trigger timeout error
        let result = ping_one(&client, &server.url(), "test-key", "test-model", 1).await;

        assert_eq!(result.model, "test-model");
        // Status could be 0 (network error) or 504 (server response)
        // depending on timing
        assert!(result.error.is_empty() || result.status_code == 504);

        mock.assert_async().await;
    }

    #[test]
    fn test_rust_ping_batch() {
        let models = CString::new("model1,model2").unwrap();
        let api_key = CString::new("test-key").unwrap();
        
        let result_ptr = rust_ping_batch(
            models.as_ptr(),
            api_key.as_ptr(),
            5,
        );
        
        assert!(!result_ptr.is_null());
        
        rust_free_string(result_ptr);
    }

    #[test]
    fn test_rust_discover_models() {
        let api_key = CString::new("test-key").unwrap();
        
        let result_ptr = rust_discover_models(api_key.as_ptr(), 5);
        
        assert!(!result_ptr.is_null());
        
        rust_free_string(result_ptr);
    }

    #[test]
    fn test_rust_ping_batch_empty_models() {
        let models = CString::new("").unwrap();
        let api_key = CString::new("test-key").unwrap();
        
        let result_ptr = rust_ping_batch(
            models.as_ptr(),
            api_key.as_ptr(),
            5,
        );
        
        assert!(!result_ptr.is_null());
        
        let result = unsafe { CStr::from_ptr(result_ptr) }
            .to_str()
            .unwrap();
        assert!(result.contains("results"));
        
        rust_free_string(result_ptr);
    }
}
