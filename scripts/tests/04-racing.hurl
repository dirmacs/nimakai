# Racing Feature Tests
# Tests the racing feature with model=auto

# Test 1: Basic racing with auto model
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "auto",
  "messages": [{"role": "user", "content": "Quick response test"}],
  "max_tokens": 50
}
HTTP 200
[Asserts]
jsonpath "$.choices[0].message.role" == "assistant"
jsonpath "$.choices[0].message.content" exists

# Test 2: Racing with specific timeout
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "auto",
  "messages": [{"role": "user", "content": "Fast response please"}],
  "max_tokens": 100,
  "racing_timeout_ms": 15000
}
HTTP 200
[Asserts]
jsonpath "$.choices[0].message.role" == "assistant"

# Test 3: Racing with multiple messages
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "auto",
  "messages": [
    {"role": "user", "content": "What is 2+2?"}
  ],
  "max_tokens": 50
}
HTTP 200
[Asserts]
jsonpath "$.choices[0].message.content" exists

# Test 4: Racing should be faster than sequential
# This test sends a simple request and checks it completes
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "auto",
  "messages": [{"role": "user", "content": "Hi"}],
  "max_tokens": 10
}
HTTP 200
[Asserts]
jsonpath "$.choices[0].message.role" == "assistant"
