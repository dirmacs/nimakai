# Changelog

All notable changes to nimakai are documented in this file.

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
