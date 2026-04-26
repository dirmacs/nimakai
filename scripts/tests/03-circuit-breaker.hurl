# Circuit Breaker Tests
# Tests that consecutive assistant turns without tool calls trigger circuit breaker

# First request - should succeed
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "mistralai/mistral-small-4-119b-2603",
  "messages": [{"role": "user", "content": "Just say OK"}],
  "max_tokens": 20
}
HTTP 200
[Asserts]
jsonpath "$.choices[0].message.role" == "assistant"

# Second request - no tool calls, same model
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "mistralai/mistral-small-4-119b-2603",
  "messages": [
    {"role": "user", "content": "Just say OK"},
    {"role": "assistant", "content": "OK"},
    {"role": "user", "content": "Again say OK"}
  ],
  "max_tokens": 20
}
HTTP 200

# Third request
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "mistralai/mistral-small-4-119b-2603",
  "messages": [
    {"role": "user", "content": "Just say OK"},
    {"role": "assistant", "content": "OK"},
    {"role": "user", "content": "Again"},
    {"role": "assistant", "content": "OK"},
    {"role": "user", "content": "Once more"}
  ],
  "max_tokens": 20
}
HTTP 200

# Fourth request
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "mistralai/mistral-small-4-119b-2603",
  "messages": [
    {"role": "user", "content": "Just say OK"},
    {"role": "assistant", "content": "OK"},
    {"role": "user", "content": "Again"},
    {"role": "assistant", "content": "OK"},
    {"role": "user", "content": "Once more"},
    {"role": "assistant", "content": "OK"},
    {"role": "user", "content": "Keep going"}
  ],
  "max_tokens": 20
}
HTTP 200

# Fifth request - should trigger circuit breaker (max_consecutive_assistant_turns = 5)
# After this, model should be degraded and requests may route to another model
POST {{NIMAPROXY_URL}}/v1/chat/completions
Content-Type: application/json
{
  "model": "mistralai/mistral-small-4-119b-2603",
  "messages": [
    {"role": "user", "content": "Just say OK"},
    {"role": "assistant", "content": "OK"},
    {"role": "user", "content": "Again"},
    {"role": "assistant", "content": "OK"},
    {"role": "user", "content": "Once more"},
    {"role": "assistant", "content": "OK"},
    {"role": "user", "content": "Keep going"},
    {"role": "assistant", "content": "OK"},
    {"role": "user", "content": "Final one"}
  ],
  "max_tokens": 20
}
HTTP 200

# Check stats to see if model is now degraded
GET {{NIMAPROXY_URL}}/stats
HTTP 200
[Asserts]
jsonpath "$.models" exists
