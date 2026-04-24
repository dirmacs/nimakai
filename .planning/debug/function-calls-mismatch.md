---
status: investigating
trigger: "Not the same number of function calls and responses" error with Mistral models
created: 2026-04-25
updated: 2026-04-25
---

## Symptoms

### Expected Behavior
- Tool calls in assistant message should match tool responses in subsequent messages.
- NVIDIA API should accept well-formed tool call/response sequences.

### Actual Behavior
- API returns 400 Bad Request with error: "Not the same number of function calls and responses"
- Occurs with Mistral models (e.g., mistralai/devstral-2-123b-instruct-2512)

### Error Messages
- From logs: `{"status":400,"title":"Bad Request","detail":"Not the same number of function calls and responses"}`
- File: `/root/.omp/logs/http-400-requests/1776831199765-1yl6f9fh1uvx9.json`

### Timeline
- Historical issue, multiple occurrences in logs.
- Still occurs with current code.

### Reproduction
- Our test `test_mismatched_tool_calls_and_responses` attempts to reproduce by sending mismatched counts.
- Need to verify if the error is reproducible with current validation.

## Current Focus

hypothesis: "Tool call IDs from model are not 9 alphanumeric, causing API to see mismatch."
test: "Run test_mismatched_tool_calls_and_responses with live API to see actual error."
next_action: "Analyze test run evidence and check if model-generated tool call IDs match expected format."
reasoning_checkpoint: "Does the proxy correctly handle tool messages for Mistral models?"
tdd_checkpoint: "Need to verify that tool_call_id format validation works (9 alphanumeric)."

## Evidence

- 2026-04-24: Added `validate_mistral_tool_call_ids` function but not yet called in live path (only in chat_completions).
- 2026-04-24: Rewrote `test_mismatched_tool_calls_and_responses` to programmatic JSON construction.
- 2026-04-25: Subagent found root cause: Config `supports_developer_role=["all"]` caused developer messages to be sent directly to NVIDIA, causing 400 errors. Fixed by removing that config entry.
- 2026-04-25: Subagent found tool message transformation issue. Config now has `supports_tool_messages=Some(["all"])` which means NO transformation for tool messages. That is correct because NVIDIA NIM's Mistral models DO support tool role.
- 2026-04-25: `validate_mistral_tool_call_ids` added and wired into `chat_completions`. Validates tool_call_id format (9 alphanumeric). 
- 2026-04-25: Ran `test_mismatched_tool_calls_and_responses` (ignored test) with live API. Results:
  - Turn 1: status=200, model returned tool call with id `chatcmpl-tool-b530734b900ddca1` (not 9 alphanumeric, contains hyphen, longer than 9 chars).
  - Turn 2: Created mismatched payload (2 tool calls in assistant message, 1 tool response). Validation caught invalid tool call ID format (`chatcmpl-tool-b530734b900ddca1` not 9 alphanumeric) and returned 400 with error: "Tool call id 'chatcmpl-tool-b530734b900ddca1' is invalid. Must be exactly 9 alphanumeric characters for Mistral models."
  - Historical error "Not the same number of function calls and responses" is about count mismatch, not ID format. However, if model returns non-9-char IDs, the API might interpret as mismatch.

## Eliminated

- hypothesis: "Tool call ID format wrong" → validated: our validation expects 9 alphanumeric, but actual IDs from model (e.g., `chatcmpl-tool-b530734b900ddca1`) differ.
- hypothesis: "Config supports_developer_role wrong" → fixed: removed config entry, now default behavior (transform all models) is correct.
- hypothesis: "Tool message transformation breaking format" → config now set to NOT transform tool messages (`supports_tool_messages=["all"]`).
