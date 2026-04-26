# Fuzz Tests
# Tests boundary values and special characters

# Test 1: Very long message
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "auto",
  "messages": [{"role": "user", "content": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}],
  "max_tokens": 20
}
HTTP 200
[Asserts]
jsonpath "$.choices[0].message.role" == "assistant"

# Test 2: Special characters in message
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "auto",
  "messages": [{"role": "user", "content": "Test special chars: !@#$%^&*()_+-=[]{}|;':\",./<>?"}],
  "max_tokens": 50
}
HTTP 200
[Asserts]
jsonpath "$.choices[0].message.role" == "assistant"

# Test 3: Unicode characters
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "auto",
  "messages": [{"role": "user", "content": "Unicode test: 你好世界 🌍 café résumé"}],
  "max_tokens": 50
}
HTTP 200
[Asserts]
jsonpath "$.choices[0].message.role" == "assistant"

# Test 4: Boundary temperature (0.0)
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "auto",
  "messages": [{"role": "user", "content": "test"}],
  "temperature": 0.0,
  "max_tokens": 20
}
HTTP 200
[Asserts]
jsonpath "$.choices[0].message.role" == "assistant"

# Test 5: Boundary temperature (2.0)
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "auto",
  "messages": [{"role": "user", "content": "test"}],
  "temperature": 2.0,
  "max_tokens": 20
}
HTTP 200
[Asserts]
jsonpath "$.choices[0].message.role" == "assistant"

# Test 6: Boundary top_p (0.001)
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "auto",
  "messages": [{"role": "user", "content": "test"}],
  "top_p": 0.001,
  "max_tokens": 20
}
HTTP 200
[Asserts]
jsonpath "$.choices[0].message.role" == "assistant"

# Test 7: Boundary top_p (1.0)
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "auto",
  "messages": [{"role": "user", "content": "test"}],
  "top_p": 1.0,
  "max_tokens": 20
}
HTTP 200
[Asserts]
jsonpath "$.choices[0].message.role" == "assistant"

# Test 8: Very small max_tokens
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "auto",
  "messages": [{"role": "user", "content": "test"}],
  "max_tokens": 1
}
HTTP 200
[Asserts]
jsonpath "$.choices[0].message.role" == "assistant"

# Test 9: Newline and tab characters in message
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "auto",
  "messages": [{"role": "user", "content": "Line1\nLine2\tTab here"}],
  "max_tokens": 50
}
HTTP 200
[Asserts]
jsonpath "$.choices[0].message.role" == "assistant"

# Test 10: Random/invalid model name format
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "random-model-12345!@#$",
  "messages": [{"role": "user", "content": "test"}]
}
HTTP 400
