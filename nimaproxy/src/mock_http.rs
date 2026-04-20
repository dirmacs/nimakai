//! Mock HTTP utilities for testing NVIDIA NIM proxy interactions.

use mockito::{Mock, Matcher, Server, ServerGuard};
use serde_json::{json, Value};

/// MockNvidiaAPI - mockito-based NVIDIA API mock wrapper
pub struct MockNvidiaAPI {
	server: ServerGuard,
	mocks: Vec<Mock>,
}

impl MockNvidiaAPI {
	pub async fn new() -> Self {
		MockNvidiaAPI {
			server: Server::new_async().await,
			mocks: Vec::new(),
		}
	}

	pub fn url(&self) -> String {
		self.server.url()
	}

	pub fn mock_chat_success(&mut self, model: &str, content: &str, status: usize) -> &mut Self {
		let mock = self
			.server
			.mock("POST", "/v1/chat/completions")
			.match_header("authorization", Matcher::Regex(r"Bearer .+".to_string()))
			.match_header("content-type", "application/json")
			
			.with_status(status)
			.with_header("content-type", "application/json")
			.with_body(
				json!({
					"id": "chatcmpl-test",
					"object": "chat.completion",
					"created": 1234567890,
					"model": model,
					"choices": [{
						"index": 0,
						"message": {
							"role": "assistant",
							"content": content
						},
						"finish_reason": "stop"
					}],
					"usage": {
						"prompt_tokens": 10,
						"completion_tokens": 20,
						"total_tokens": 30
					}
				})
				.to_string(),
			)
			.create();
		self.mocks.push(mock);
		self
	}

	pub fn mock_chat_stream(&mut self, model: &str, content_chunks: Vec<&str>) -> &mut Self {
		let mut sse_data = String::new();
		for (i, chunk) in content_chunks.iter().enumerate() {
			let is_last = i == content_chunks.len() - 1;
			let finish = if is_last { "stop" } else { "" };
			sse_data.push_str(&format!(
				"data: {{\"id\":\"chatcmpl-stream\",\"object\":\"chat.completion.chunk\",\"created\":1234567890,\"model\":\"{}\",\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":\"{}\"}},\"finish_reason\":\"{}\"}}]}}\n\n",
				model, chunk, finish
			));
		}
		sse_data.push_str("data: [DONE]\n\n");

		let mock = self
			.server
			.mock("POST", "/v1/chat/completions")
			.match_header("authorization", Matcher::Regex(r"Bearer .+".to_string()))
			.match_header("content-type", "application/json")
			.with_status(200)
			.with_header("content-type", "text/event-stream")
			.with_header("cache-control", "no-cache")
			.with_body(sse_data)
			.create();
		self.mocks.push(mock);
		self
	}

	pub fn mock_unauthorized(&mut self) -> &mut Self {
		let mock = self
			.server
			.mock("POST", "/v1/chat/completions")
			.with_status(401)
			.with_header("content-type", "application/json")
			.with_body(
				json!({
					"error": {
						"message": "Incorrect API key provided",
						"type": "invalid_request_error",
						"code": "invalid_api_key"
					}
				})
				.to_string(),
			)
			.create();
		self.mocks.push(mock);
		self
	}

	pub fn mock_rate_limited(&mut self, retry_after_secs: u64) -> &mut Self {
		let mock = self
			.server
			.mock("POST", "/v1/chat/completions")
			.with_status(429)
			.with_header("content-type", "application/json")
			.with_header("retry-after", &retry_after_secs.to_string())
			.with_body(
				json!({
					"error": {
						"message": "Rate limit exceeded",
						"type": "rate_limit_error",
						"code": "rate_limit_exceeded"
					}
				})
				.to_string(),
			)
			.create();
		self.mocks.push(mock);
		self
	}

	pub fn mock_server_error(&mut self, message: &str) -> &mut Self {
		let mock = self
			.server
			.mock("POST", "/v1/chat/completions")
			.with_status(500)
			.with_header("content-type", "application/json")
			.with_body(
				json!({
					"error": {
						"message": message,
						"type": "server_error",
						"code": "internal_server_error"
					}
				})
				.to_string(),
			)
			.create();
		self.mocks.push(mock);
		self
	}

	pub fn mock_bad_request(&mut self, message: &str) -> &mut Self {
		let mock = self
			.server
			.mock("POST", "/v1/chat/completions")
			.with_status(400)
			.with_header("content-type", "application/json")
			.with_body(
				json!({
					"error": {
						"message": message,
						"type": "bad_request_error",
						"code": "bad_request"
					}
				})
				.to_string(),
			)
			.create();
		self.mocks.push(mock);
		self
	}

	pub fn mock_timeout(&mut self) -> &mut Self {
		// Simulate timeout by not responding (connection drop)
		// In mockito, we simulate this by returning a connection error
		// The actual timeout happens when the server doesn't respond
		let mock = self
			.server
			.mock("POST", "/v1/chat/completions")
			.with_status(504)  // Gateway timeout
			.with_header("content-type", "application/json")
			.with_body(
				json!({
					"error": {
						"message": "Gateway timeout",
						"type": "timeout_error",
						"code": "timeout"
					}
				})
				.to_string(),
			)
			.create();
		self.mocks.push(mock);
		self
	}

	pub fn mock_models_success(&mut self, models: Vec<&str>) -> &mut Self {
		let models_json: Vec<Value> = models
			.iter()
			.map(|m| {
				json!({
					"id": m,
					"object": "model",
					"created": 1234567890,
					"owned_by": "nvidia"
				})
			})
			.collect();

		let mock = self
			.server
			.mock("GET", "/v1/models")
			.match_header("authorization", Matcher::Regex(r"Bearer .+".to_string()))
			.with_status(200)
			.with_header("content-type", "application/json")
			.with_body(
				json!({
					"object": "list",
					"data": models_json
				})
				.to_string(),
			)
			.create();
		self.mocks.push(mock);
		self
	}

	pub fn mock_not_found(&mut self) -> &mut Self {
		let mock = self
			.server
			.mock("POST", "/v1/chat/completions")
			.with_status(404)
			.with_header("content-type", "application/json")
			.with_body(
				json!({
					"error": {
						"message": "Model not found",
						"type": "invalid_request_error",
						"code": "model_not_found"
					}
				})
				.to_string(),
			)
			.create();
		self.mocks.push(mock);
		self
	}

	pub fn mock_chat_custom(&mut self, response_body: Value) -> &mut Self {
		let mock = self
			.server
			.mock("POST", "/v1/chat/completions")
			.match_header("authorization", Matcher::Regex(r"Bearer .+".to_string()))
			.match_header("content-type", "application/json")
			.with_status(200)
			.with_header("content-type", "application/json")
			.with_body(response_body.to_string())
			.create();
		self.mocks.push(mock);
		self
	}

	pub fn mock_chat_with_body_match(
		&mut self,
		expected_body: Value,
		response_body: Value,
	) -> &mut Self {
		let mock = self
			.server
			.mock("POST", "/v1/chat/completions")
			.match_header("authorization", Matcher::Regex(r"Bearer .+".to_string()))
			.match_header("content-type", "application/json")
			.match_body(Matcher::PartialJson(expected_body))
			.with_status(200)
			.with_header("content-type", "application/json")
			.with_body(response_body.to_string())
			.create();
		self.mocks.push(mock);
		self
	}

	pub fn mock_tool_call(&mut self, model: &str, tool_name: &str, tool_args: &Value) -> &mut Self {
		let mock = self
			.server
			.mock("POST", "/v1/chat/completions")
			.match_header("authorization", Matcher::Regex(r"Bearer .+".to_string()))
			.match_header("content-type", "application/json")
			.match_body(Matcher::PartialJsonString(format!("\"model\":\"{}\"", model)))
			.with_status(200)
			.with_header("content-type", "application/json")
			.with_body(
				json!({
					"id": "chatcmpl-tool",
					"object": "chat.completion",
					"created": 1234567890,
					"model": model,
					"choices": [{
						"index": 0,
						"message": {
							"role": "assistant",
							"content": null,
							"tool_calls": [{
								"id": "call_123",
								"type": "function",
								"function": {
									"name": tool_name,
									"arguments": tool_args
								}
							}]
						},
						"finish_reason": "tool_calls"
					}],
					"usage": {
						"prompt_tokens": 50,
						"completion_tokens": 30,
						"total_tokens": 80
					}
				})
				.to_string(),
			)
			.create();
		self.mocks.push(mock);
		self
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn test_mock_api_creation() {
		let mock_api = MockNvidiaAPI::new().await;
		assert!(!mock_api.url().is_empty());
	}

	#[tokio::test]
	async fn test_mock_chat_success() {
		let mut mock_api = MockNvidiaAPI::new().await;
		mock_api.mock_chat_success("nvidia/test-model", "Hello!", 200);
		assert_eq!(mock_api.mocks.len(), 1);
	}

	#[tokio::test]
	async fn test_mock_unauthorized() {
		let mut mock_api = MockNvidiaAPI::new().await;
		mock_api.mock_unauthorized();
		assert_eq!(mock_api.mocks.len(), 1);
	}

	#[tokio::test]
	async fn test_mock_rate_limited() {
		let mut mock_api = MockNvidiaAPI::new().await;
		mock_api.mock_rate_limited(60);
		assert_eq!(mock_api.mocks.len(), 1);
	}

	#[tokio::test]
	async fn test_mock_server_error() {
		let mut mock_api = MockNvidiaAPI::new().await;
		mock_api.mock_server_error("Internal error");
		assert_eq!(mock_api.mocks.len(), 1);
	}

	#[tokio::test]
	async fn test_mock_models_success() {
		let mut mock_api = MockNvidiaAPI::new().await;
		mock_api.mock_models_success(vec!["model1", "model2"]);
		assert_eq!(mock_api.mocks.len(), 1);
	}

	// Test 1: mock_chat_with_body_match - exact body matching
	#[tokio::test]
	async fn test_mock_chat_with_body_match() {
		let mut mock_api = MockNvidiaAPI::new().await;
		let expected_body = json!({
			"model": "test-model",
			"messages": [{"role": "user", "content": "hello"}]
		});
		let response_body = json!({
			"id": "chatcmpl-body",
			"object": "chat.completion",
			"created": 1234567890,
			"model": "test-model",
			"choices": [{
				"index": 0,
				"message": {"role": "assistant", "content": "matched"},
				"finish_reason": "stop"
			}],
			"usage": {"prompt_tokens": 5, "completion_tokens": 10, "total_tokens": 15}
		});
		mock_api.mock_chat_with_body_match(expected_body, response_body);
		assert_eq!(mock_api.mocks.len(), 1);
	}

	// Test 2: mock_chat_custom - custom response scenarios
	#[tokio::test]
	async fn test_mock_chat_custom() {
		let mut mock_api = MockNvidiaAPI::new().await;
		let custom_response = json!({
			"id": "custom-123",
			"custom_field": "custom_value",
			"choices": []
		});
		mock_api.mock_chat_custom(custom_response);
		assert_eq!(mock_api.mocks.len(), 1);
	}

	// Test 3: mock_models_success - model listing edge cases
	#[tokio::test]
	async fn test_mock_models_success_edge_cases() {
		let mut mock_api = MockNvidiaAPI::new().await;
		// Test with empty model list
		mock_api.mock_models_success(vec![]);
		assert_eq!(mock_api.mocks.len(), 1);

		// Test with single model
		mock_api.mock_models_success(vec!["single-model"]);
		assert_eq!(mock_api.mocks.len(), 2);

		// Test with many models
		let many_models: Vec<&str> = vec!["model-1", "model-2", "model-3", "model-4", "model-5"];
		mock_api.mock_models_success(many_models);
		assert_eq!(mock_api.mocks.len(), 3);
	}

	// Test 4: Error path: connection refused
	#[tokio::test]
	async fn test_mock_error_connection_refused() {
		let mut mock_api = MockNvidiaAPI::new().await;
		// Simulate connection refused with 503
		let mock = mock_api.server
			.mock("POST", "/v1/chat/completions")
			.with_status(503)
			.with_header("content-type", "application/json")
			.with_body(json!({"error": {"message": "Service unavailable", "type": "service_error"}}).to_string())
			.create();
		mock_api.mocks.push(mock);
		assert_eq!(mock_api.mocks.len(), 1);
	}

	// Test 5: Error path: timeout during body
	#[tokio::test]
	async fn test_mock_error_timeout_during_body() {
		let mut mock_api = MockNvidiaAPI::new().await;
		// Simulate timeout with 504
		let mock = mock_api.server
			.mock("POST", "/v1/chat/completions")
			.with_status(504)
			.with_header("content-type", "application/json")
			.with_body(json!({"error": {"message": "Gateway timeout", "type": "timeout_error"}}).to_string())
			.create();
		mock_api.mocks.push(mock);
		assert_eq!(mock_api.mocks.len(), 1);
	}

	// Test 6: Error path: malformed JSON response
	#[tokio::test]
	async fn test_mock_error_malformed_json() {
		let mut mock_api = MockNvidiaAPI::new().await;
		// Simulate malformed JSON response
		let mock = mock_api.server
			.mock("POST", "/v1/chat/completions")
			.with_status(200)
			.with_header("content-type", "application/json")
			.with_body("{invalid json response".to_string())
			.create();
		mock_api.mocks.push(mock);
		assert_eq!(mock_api.mocks.len(), 1);
	}

	// Test 7: Error path: missing headers
	#[tokio::test]
	async fn test_mock_error_missing_headers() {
		let mut mock_api = MockNvidiaAPI::new().await;
		// Simulate missing content-type header
		let mock = mock_api.server
			.mock("POST", "/v1/chat/completions")
			.with_status(200)
			.with_body(json!({"id": "test"}).to_string())
			.create();
		mock_api.mocks.push(mock);
		assert_eq!(mock_api.mocks.len(), 1);
	}

	// Test 8: Error path: invalid status code
	#[tokio::test]
	async fn test_mock_error_invalid_status_code() {
		let mut mock_api = MockNvidiaAPI::new().await;
		// Simulate invalid status code (418 I'm a teapot)
		let mock = mock_api.server
			.mock("POST", "/v1/chat/completions")
			.with_status(418)
			.with_header("content-type", "application/json")
			.with_body(json!({"error": {"message": "I'm a teapot"}}).to_string())
			.create();
		mock_api.mocks.push(mock);
		assert_eq!(mock_api.mocks.len(), 1);
	}

	// Test 9: Streaming: empty chunk
	#[tokio::test]
	async fn test_mock_streaming_empty_chunk() {
		let mut mock_api = MockNvidiaAPI::new().await;
		// Simulate streaming with empty chunks
		let sse_data = "data: {\"id\":\"test\",\"choices\":[]}\n\ndata: [DONE]\n\n";
		let mock = mock_api.server
			.mock("POST", "/v1/chat/completions")
			.match_header("authorization", Matcher::Regex(r"Bearer .+".to_string()))
			.match_header("content-type", "application/json")
			.with_status(200)
			.with_header("content-type", "text/event-stream")
			.with_body(sse_data)
			.create();
		mock_api.mocks.push(mock);
		assert_eq!(mock_api.mocks.len(), 1);
	}

	// Test 10: Streaming: malformed SSE
	#[tokio::test]
	async fn test_mock_streaming_malformed_sse() {
		let mut mock_api = MockNvidiaAPI::new().await;
		// Simulate malformed SSE data
		let sse_data = "invalid sse format\nnot a data line\nbroken json {\n";
		let mock = mock_api.server
			.mock("POST", "/v1/chat/completions")
			.match_header("authorization", Matcher::Regex(r"Bearer .+".to_string()))
			.match_header("content-type", "application/json")
			.with_status(200)
			.with_header("content-type", "text/event-stream")
			.with_body(sse_data)
			.create();
		mock_api.mocks.push(mock);
		assert_eq!(mock_api.mocks.len(), 1);
	}
}
