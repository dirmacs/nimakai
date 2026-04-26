# Stress Tests
# Tests handling of multiple parallel requests

# Test 1: Multiple rapid requests to same model
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "qwen/qwen3.5-122b-a10b",
  "messages": [{"role": "user", "content": "Request 1"}],
  "max_tokens": 20
}
HTTP *

# Test 2: Request 2
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "qwen/qwen3.5-122b-a10b",
  "messages": [{"role": "user", "content": "Request 2"}],
  "max_tokens": 20
}
HTTP *

# Test 3: Request 3
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "qwen/qwen3.5-122b-a10b",
  "messages": [{"role": "user", "content": "Request 3"}],
  "max_tokens": 20
}
HTTP *

# Test 4: Request 4
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "qwen/qwen3.5-122b-a10b",
  "messages": [{"role": "user", "content": "Request 4"}],
  "max_tokens": 20
}
HTTP *

# Test 5: Request 5 - different model
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "z-ai/glm4.7",
  "messages": [{"role": "user", "content": "Request 5"}],
  "max_tokens": 20
}
HTTP *

# Test 6: Racing request under load
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "auto",
  "messages": [{"role": "user", "content": "Racing under load"}],
  "max_tokens": 50
}
HTTP *

# Test 7: Stats after stress test
GET {{NIMAPROXY_URL}}/stats
HTTP 200
[Asserts]
jsonpath "$.racing_max_parallel" exists
jsonpath "$.models" exists
