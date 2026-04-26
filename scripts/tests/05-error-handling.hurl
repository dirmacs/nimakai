# Error Handling Tests
# Tests various error conditions and expected status codes

# Test 1: Missing messages field
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "auto"
}
HTTP 400

# Test 2: Empty messages array
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "auto",
  "messages": []
}
HTTP *

# Test 3: Invalid role in messages
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "auto",
  "messages": [{"role": "invalid_role", "content": "test"}]
}
HTTP *

# Test 4: Missing Content-Type header - proxy accepts raw JSON
POST {{NIMAPROXY_URL}}/v1/chat/completions
{
  "model": "auto",
  "messages": [{"role": "user", "content": "test"}]
}
HTTP *

# Test 5: Invalid temperature (too high)
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "auto",
  "messages": [{"role": "user", "content": "test"}],
  "temperature": 5.0
}
HTTP *

# Test 6: Negative max_tokens
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "auto",
  "messages": [{"role": "user", "content": "test"}],
  "max_tokens": -1
}
HTTP 400

# Test 7: Invalid model name
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "nonexistent/model-name",
  "messages": [{"role": "user", "content": "test"}]
}
HTTP 404
