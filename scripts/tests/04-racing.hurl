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
HTTP *

# Test 2: Racing with multiple messages
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "auto",
  "messages": [
    {"role": "user", "content": "What is 2+2?"}
  ],
  "max_tokens": 50
}
HTTP *

# Test 3: Racing should return quickly
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "auto",
  "messages": [{"role": "user", "content": "Hi"}],
  "max_tokens": 10
}
HTTP *
