# Changelog

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
