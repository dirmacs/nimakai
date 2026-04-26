# Changelog

All notable changes to nimakai are documented in this file.

## [0.13.7] - 2026-04-27

### Fixed (nimaproxy)

- **Racing error body logging**: Non-429 4xx/5xx responses now buffer and log body before discarding,
  so journal shows exact NVIDIA error instead of just status code
- **Racing pool pruned**: Removed `qwen3-coder-480b-a35b-instruct` (persistent 500s) and
  `devstral-2-123b-instruct-2512` (persistent 400s) from racing pool — neither model
  ever won a race; both burned key quota via cascading 429s
- Racing pool: 11 → 9 models

## [0.13.6] - 2026-04-27

### Fixed (nimaproxy)

- **Racing 4xx/5xx propagation**: Racing no longer forwards 4xx/5xx to client; only 2xx responses win
- **Racing 429 key-marking**: 429 now correctly calls `mark_rate_limited()` on the originating key
  (previously `key_idx` was captured incorrectly in spawn closure)
- **400 Invalid assistant message retry**: `resolve_model` now retries on
  "Invalid assistant message" 400 (same retry path as DEGRADED model errors)
- **Tool schema sanitization**: `sanitize_tool_calls()` two-pass fix — null/missing
  `description` → `""`, null/missing `parameters` → `{"type":"object","properties":{}}`;
  prevents NVIDIA Jinja 500 `tool_use:98` crash

### Added (nimaproxy)

- **GET /models alias**: Added route without `/v1/` prefix — OMP polls `/models` for discovery
- **mock + live tests**: 22 proxy_error_paths tests, 14 e2e_live tests

## [0.13.5] - 2026-04-26

### Fixed (nimaproxy)

- **RUST_LOG scope**: Narrowed to `nimaproxy=info,warn` to suppress third-party DEBUG noise
- Fixed hurl test `05-error-handling.hurl` Test 4 failure
- Removed all DEBUG `eprintln!` statements from proxy.rs

## [0.13.4] - 2026-04-26

### Fixed (nimaproxy)

- **tool→developer ordering**: Fixed `fix_message_ordering` running after `transform_message_roles`
  (now runs before) so developer role inserted between tool→user transitions is seen correctly
- Removed remaining DEBUG logging from proxy.rs

## [0.13.3] - 2026-04-26

### Changed (nimaproxy)

- Raised `max_consecutive_assistant_turns` default from 5 to 10 in circuit breaker

## [0.13.2] - 2026-04-25

### Fixed (nimaproxy)

- **Pipeline reorder**: `transform_message_roles` now runs BEFORE `fix_message_ordering`
- **content=null for tool_calls**: `fix_message_ordering` inserts `{"role":"assistant","content":null}`
- Deployed as production binary

## [0.13.1] - 2026-04-25

### Fixed (nimaproxy)

- **Assistant message validation**: Messages with `tool_calls` must NOT have `content` field (NVIDIA NIM requirement)
- **Unexpected role 'user' after role 'tool'**: Insert assistant message between tool→user transitions
- `sanitize_tool_calls()` sets `content` to `null` (not empty string) when `tool_calls` present

## [0.13.0] - 2026-04-24

### Added (nimaproxy)

- **Turn logging**: JSONL turn logging for observability (`turn_log.rs`, `nimaproxy-query` binary)
- **Tool call ID validation**: `validate_mistral_tool_call_ids()` for Mistral models (9 alphanumeric chars)
- **Count validation**: Tool calls/responses count matching validation
- **Log query tool**: `nimaproxy-query` binary for analyzing turn logs

### Fixed (nimaproxy)

- **Config fix**: Removed `supports_developer_role` causing 400 errors with NVIDIA NIM
- **Tool message transformation**: Fixed `supports_tool_messages=["all"]` config
- **Compilation errors**: Fixed format string syntax in `validate_mistral_tool_call_ids`
- **Live tests**: Fixed `test_mismatched_tool_calls_and_responses` compilation

### Changed (nimaproxy)

- Test count: 241 lib + 45 integration + 19 proxy_error_paths + 14 coverage_gaps + 11 e2e_live + 7 tool_call_id = 337 total
- Coverage: ~92% (with validation and logging code)

### Added (nimakai)

- FFI integration with nimaproxy v0.13.0
- `nimakai proxy start/stop/status` commands for managing Rust proxy

## [0.12.0] - 2026-04-19

### Added (Universal Compatibility)

- **Mistral params now Mistral-only**: `add_generation_prompt` and `continue_final_message` only injected for Mistral models
- **MiniMax XML-to-JSON transformation**: System message injection prevents XML tool call output
- **3 API keys active**: doltares, ares, backup for rate limit distribution

### Fixed

- Fixed `Validation: Unsupported parameter(s)` errors for Qwen, GLM, and other non-Mistral models
- Fixed `Unknown message role: developer` errors from OMP/agent conversations  
- Fixed runaway conversation loops caused by unparseable tool responses
- Fixed rate limiting with multi-key rotation
- Restored MiniMax and Kimi models to racing config (14 total models)

### Testing

- All 14 racing models verified working
- Zero 400 errors since deployment
- Success rates: 92-100% across all models
