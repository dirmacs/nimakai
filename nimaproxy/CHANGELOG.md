# Changelog

## [0.13.7] - 2026-04-27

### Fixed

- **Racing error body logging**: Non-429 4xx/5xx responses buffer and log body before discarding;
  journal now shows exact NVIDIA error message (e.g. "Unexpected role user after tool")
- **Racing pool pruned**: Removed `qwen3-coder-480b-a35b-instruct` (persistent 500s on tool
  requests) and `devstral-2-123b-instruct-2512` (persistent 400s) from racing pool
- Racing pool: 11 → 9 active models

## [0.13.6] - 2026-04-27

### Fixed

- **Racing 4xx/5xx propagation**: `race_models` only accepts 2xx as winning responses;
  non-2xx are logged and skipped (not forwarded to client)
- **Racing 429 key-marking**: Captured `key_idx_for_spawn` correctly before `tokio::spawn`;
  429 now calls `state.pool.mark_rate_limited(key_idx, retry_after_secs)` on the right key
- **400 retry on invalid assistant message**: `resolve_model` retries when NVIDIA returns
  "Invalid assistant message" 400, same as DEGRADED model retry path
- **sanitize_tool_calls two-pass**: (1) `iter_mut()` loop fills null/missing `description` → `""`
  and null/missing `parameters` → `{"type":"object","properties":{}}`; (2) `retain()` filters
  empty-name tools. Prevents NVIDIA Jinja template `tool_use:98` 500 crash

### Added

- `GET /models` route alias in `main.rs` (OMP model discovery polls without `/v1/` prefix)
- 22 proxy_error_paths tests for racing/routing error behaviors
- 14 e2e_live tests for live NVIDIA API validation

### Changed

- Total tests: 364 (246 lib + 45 integration + 22 proxy_error_paths + 14 e2e_live +
  14 coverage_gaps + 24 live suites + 1 stress)

## [0.13.5] - 2026-04-26

### Fixed

- RUST_LOG narrowed to `nimaproxy=info,warn` — suppresses third-party reqwest/hyper DEBUG noise
- Removed all `eprintln!` DEBUG statements from proxy.rs
- Fixed hurl test `05-error-handling.hurl` Test 4

## [0.13.4] - 2026-04-26

### Fixed

- `transform_message_roles` now runs BEFORE `fix_message_ordering` in both `resolve_model`
  and `race_models` paths (pipeline reorder)
- Updated stale tests for new pipeline order

## [0.13.3] - 2026-04-26

### Changed

- `max_consecutive_assistant_turns` default raised from 5 to 10 in circuit breaker config

## [0.13.2] - 2026-04-25

### Fixed

- `fix_message_ordering` inserts `{"role":"assistant","content":null}` between tool→user
  transitions (content must be null, not empty string, per NVIDIA requirements)
- Deployed as production binary (systemd service)

All notable changes to nimaproxy will be documented in this file.

## [0.13.1] - 2026-04-25

### Fixed

- **Assistant message validation**: Messages with `tool_calls` must NOT have `content` field (NVIDIA NIM requirement)
- **Unexpected role 'user' after role 'tool'**: Insert assistant message between tool→user transitions (fixes OMP/Pawan integration)
- Ensure `content` is `null` (not empty string) when `tool_calls` present
- `sanitize_tool_calls()` now properly handles messages with `tool_calls` field

## [0.13.0] - 2026-04-20

### Added

- Detection and automatic retry for NVIDIA API "DEGRADED" model errors
- Coverage gap tests for model_stats edge cases (14 new tests)
- Proxy error path tests for connection failures (3 new tests)
- Test coverage for circuit breaker paths and degradation scenarios

### Fixed

- **tool_call_id forwarding error**: Assistant messages with `tool_call_id` fields are now stripped before forwarding to NVIDIA API, preventing Pydantic validation errors: "Extra inputs are not permitted"
- **DEGRADED model handling**: Proxy now detects "DEGRADED" errors from NVIDIA API and automatically retries with a different model instead of returning 400 to client
- Test failures in live E2E tests due to transient API errors (429/502/503)

### Changed

- Improved error handling for connection refusals (returns BAD_GATEWAY)
- Enhanced test coverage from ~89.66% to ~91-92%
- All 313+ tests now pass (224 lib, 45 integration, 19 proxy error paths, 14 coverage gaps, 11 e2e live)

### Technical Details

- `sanitize_tool_calls()` now explicitly removes `tool_call_id` from assistant messages
- Added degraded model detection in chat completion response handling
- Model stats tracking improved for consecutive failures and degradation flags

## [0.12.0] - Previous Release

- Initial release with racing mode and model routing
