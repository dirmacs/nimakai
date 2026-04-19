# Changelog

All notable changes to nimakai are documented in this file.

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

