# Model Routing Tests
# Tests routing to specific models and invalid model handling

# Test 1: Route to Mistral Small
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "mistralai/mistral-small-4-119b-2603",
  "messages": [{"role": "user", "content": "Say hello"}],
  "max_tokens": 50
}
HTTP 200
[Asserts]
jsonpath "$.choices[0].message.role" == "assistant"
jsonpath "$.choices[0].message.content" exists

# Test 2: Route to Mistral Large
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
jsonpath "$.choices[0].message.content" exists

# Test 3: Route to Qwen model
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
jsonpath "$.choices[0].message.content" exists

# Test 4: Route to GLM model
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
jsonpath "$.choices[0].message.content" exists

# Test 5: Invalid model name should return 4xx
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "invalid/model-name-that-does-not-exist",
  "messages": [{"role": "user", "content": "Hello"}]
}
HTTP 400
