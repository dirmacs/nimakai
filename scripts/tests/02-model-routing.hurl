# Model Routing Tests
# Tests routing to specific models and invalid model handling

# Test 1: Route to Mistral Large
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "mistralai/mistral-large-3-675b-instruct-2512",
  "messages": [{"role": "user", "content": "Say hi"}],
  "max_tokens": 50
}
HTTP 200
[Asserts]
jsonpath "$.choices[0].message.role" == "assistant"

# Test 2: Route to Qwen model
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "qwen/qwen3.5-122b-a10b",
  "messages": [{"role": "user", "content": "Hello"}],
  "max_tokens": 50
}
HTTP 200
[Asserts]
jsonpath "$.choices[0].message.role" == "assistant"

# Test 3: Route to GLM model
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "z-ai/glm4.7",
  "messages": [{"role": "user", "content": "Hi"}],
  "max_tokens": 50
}
HTTP 200
[Asserts]
jsonpath "$.choices[0].message.role" == "assistant"

# Test 4: Route to Devstral model
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "mistralai/devstral-2-123b-instruct-2512",
  "messages": [{"role": "user", "content": "Hi"}],
  "max_tokens": 50
}
HTTP 200
[Asserts]
jsonpath "$.choices[0].message.role" == "assistant"

# Test 5: Invalid model name should return 404
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "nonexistent/model-name",
  "messages": [{"role": "user", "content": "Hello"}]
}
HTTP 404
