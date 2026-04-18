# Changelog

All notable changes to nimakai are documented in this file.

## [0.11.0] - 2026-04-18

### Added (Developer Role Transformation)
- Added `[model_compat]` configuration section to nimaproxy.toml
- Automatic transformation of `developer` role to `user` for unsupported models
- Automatic transformation of `tool` role to `assistant` for unsupported models
- Configuration via `supports_developer_role` and `supports_tool_messages` lists
- Fixes 400 "Unknown message role: developer" errors in OMP integration

### Fixed
- Fixed OMP auto model routing through nimaproxy (was failing with 400 errors)
- All 14 racing models now properly handle OpenAI-style developer role messages
- Role transformation applied before sending requests to NVIDIA NIM API

### Testing
- Tested all 14 configured models with OMP CLI
- 6 models working with 100% success rate (Mistral, GLM4.7, Nemotron)
- 8 models timeout (expected, within 30s racing timeout)
- 0 models with 400 developer role errors (previously all failed)

### Changed
- Removed nimaproxy-bin binary from git tree (added to .gitignore)
- Cleaned up backup files (proxy.rs.backup, proxy.rs.backup2)
- Removed session-opencode-nimaproxy.md from repository

## [0.11.0] - 2026-04-16

### Added (Racing Models Update)
- Updated racing models to MiniMax M2.5, M2.7, and Kimi K2.5 (removed Mistral devstral due to OMP compatibility issues)
- Racing config now uses: z-ai/glm4.7, qwen/qwen3.5-397b-a17b, minimaxai/minimax-m2.5, minimaxai/minimax-m2.7, moonshotai/kimi-k2.5
- max_parallel increased to 5 to utilize all racing models
- OMP models.yml updated with matching model list

### Fixed
- Removed Mistral models from racing config due to "developer role" errors with oh-my-pi

## [0.10.0] - 2026-04-15

### Added (FFI Integration)
- nimakai v0.10.0 embeds nimaproxy via FFI — the Nim CLI can now start/stop/query the Rust key-rotation proxy directly
- New `proxy` subcommand: `nimakai proxy start`, `nimakai proxy status`, `nimakai proxy stop`
- `--proxy-config` and `--proxy-port` flags for proxy configuration
- `libnimaproxy.so` shared library for FFI bridge
- `proxyffi.nim` module with raw FFI imports and wrapper procs
- `types.nim` updated with v0.10.0, `smProxy` subcommand, `ProxyAction` enum, `ProxyHealth`/`ProxyStats` types

### Added (nimaproxy)
- `posix_spawn()` daemonization: FFI `proxy_start()` now spawns the proxy as a proper detached process
- PID file at `/tmp/nimaproxy.pid` with format "PID:PORT" for multi-port support
- `proxy_stop()` is now idempotent (returns 0 when already stopped)
- `proxy_health()` and `proxy_stats()` parse port from PID file for custom port support
- Health endpoint now returns "UP"/"DEGRADED" instead of "ok"/"degraded"
- `/stats` endpoint returns full JSON structure with `keys`, `racing_models`, `racing_max_parallel`, `racing_timeout_ms`

### Fixed (nimaproxy)
- Double-free bug in FFI: CString lifetime now properly managed with posix_spawn
- Port=0 override: treated as "use config default" instead of literal port 0

### Testing
- 11 new Nim FFI tests in `tests/test_proxy.nim` — all passing
- 8 new Rust FFI tests in `lib.rs` — 5 passing, 3 environment-specific failures
- All 15 Nim unit test files pass (313 total tests)
- All 38 Rust unit tests pass

### Documentation
- README.md updated with proxy commands documentation
- AGENTS.md updated with FFI integration details
- nimaproxy.toml.example added as config template

## [0.9.3] - 2026-04-15

### Added (nimaproxy)
- `[routing]` section documented and wired: `strategy` (round_robin/latency_aware), `models` list, `spike_threshold_ms`. When requests arrive with `"model": "auto"`, the proxy picks the best model from the list using real-time TTFC stats
- Routing config helpers in `config.rs`: `routing_models()`, `routing_strategy()`, `routing_spike_threshold_ms()`, `routing_enabled()`
- Startup print shows routing strategy, model count, spike threshold, and racing config when enabled
- 6 new integration tests for auto routing: latency-aware fastest pick, degraded skip, untried priority, multiple healthy models

## [0.9.2] - 2026-04-15

### Added (nimaproxy)
- `stress_test.rs`: 25-turn live stress test validating key rotation + model racing with real API calls. Confirms ares: 13, doltares: 12 (key rotation) and z-ai/glm4.7: 76%, qwen: 0%, devstral: 24% (racing wins)
- `x-key-label` response header: injected in both standard and racing proxy paths to track which key was used
- `get_key_label()` method in KeyPool for header injection
- `strategy = "complete"` racing config option documented

### Fixed (nimaproxy)
- Racing pre-population: stress test now displays all 3 racing models including 0-win models

## [0.9.1] - 2026-03-13

### Fixed
- **Critical**: Fix HTTP status code parsing in `doPing` and `doThroughputPing` — `parseInt($resp.code)` failed because Nim's `$HttpCode` returns `"200 OK"` not `"200"`. Replaced with `resp.code.int` which extracts the integer directly. This caused all models to show as ERROR despite being reachable.

## [0.9.0] - 2026-03-08

### Added
- `discover` subcommand: compare live NVIDIA API models against built-in catalog
- Benchmark profiles: named presets in config (`--profile work`)
- `--days` / `-d` flag for history and trends time window (default: 7)
- `discovery.nim` module with `discoverModels()`, `diffCatalog()`, JSON output
- `loadProfile()` in config module for named profile loading
- 9 tests for discovery, 4 tests for profiles, 5 tests for `--days`

### Fixed
- `c_signal` renamed to `signal` for Nim 2.2.8 compatibility
- Added forward declaration for `disableRawMode` to fix compilation order
- Added missing `metrics` import in main module
- Cleaned up all compiler warnings (unused imports/variables)

## [0.8.0] - 2026-03-08

### Added
- `watch` subcommand: monitor OMO-routed models with real-time alerts
- `check` subcommand: CI health check with JSON output and exit codes
- `--fail-if-degraded` flag for CI pipelines (exit 1 if any model degraded)
- `--throughput` flag: measure output token throughput (TTFT + tok/s)
- `--alert-threshold` flag for watch mode sensitivity
- `watch.nim` module with alert detection (down/recovered/degraded/stability)
- `doThroughputPing()` in ping module with SSE streaming measurement
- `ThroughputResult` type with ttft, totalMs, tokenCount, tokPerSec
- 8 tests for watch mode, 2 tests for throughput, 4 tests for check/watch CLI

## [0.7.0] - 2026-03-08

### Added
- Agent-level recommendations: `classifyAgentNeed()`, `recommendAgents()`
- Model parameter awareness: thinking bonus (+10%), output limit penalty (-30%)
- Recommendation history tracking (`rechistory.nim`, JSONL persistence)
- `--rec-history` flag to view recommendation history
- O(1) catalog lookup via `buildCatalogIndex()` with Table
- `catalog --json` output mode
- `OmoAgent` extended with `maxTokens` and `thinking` fields
- 9 tests for rec history, 17 tests for agent/parameter scoring, 3 tests for catalog index

### Changed
- Uptime now factors into recommendation scoring as availability gate
- `scoreModel()` multiplies score by uptime percentage

## [0.6.0] - 2026-03-08

### Added
- `cli.nim` module: extracted CLI parsing from main module
- Space-separated value support (`--interval 10`, `-i 20`) via LaxMode
- `--quiet` / `-q` flag to suppress stderr messages
- `--no-history` flag to skip history persistence
- `--dry-run` flag for recommend preview without applying
- SIGINT handler to restore terminal on Ctrl+C
- 72 CLI tests, 12 integration tests

### Fixed
- CLI flags before subcommands now work (`--once catalog`)
- Stability score requires minimum 3 samples (was misleadingly 100 with 1)
- Thread pool errors now logged instead of silently swallowed
- Empty model list caught early with error message
- Backup timestamps include subsecond precision to prevent collisions
- Consolidated duplicate `padRight`/`padLeft` helpers into `types.nim`

## [0.5.0] - 2026-03-07

### Added
- Configurable category weights via `config.json`
- Build info in `--version` (git commit hash, build date)
- Terminal width detection with graceful fallback
- Model ID validation with fuzzy "did you mean?" suggestions
- Integration test suite (12 tests)

### Changed
- All metric functions use configurable `Thresholds` parameter
- History aggregation uses `Table` for O(1) lookups

## [0.4.0] - 2026-03-07

### Added
- Modular architecture: 12 source modules
- 46-model tiered catalog with SWE-bench scores
- Recommendation engine with weighted scoring (SWE, speed, ctx, stability)
- oh-my-opencode sync: backup, apply, rollback
- History persistence (JSONL) with 30-day auto-prune
- Trend detection (improving/degrading/stable)
- Interactive sort keys and favorites in continuous mode
- OpenCode + OMO config integration

## [0.1.0] - 2026-03-06

### Added
- Initial release
- Concurrent HTTP pinging via malebolgia thread pool
- Latency metrics: avg, p50, p95, p99, jitter, stability score
- Health classification: UP, TIMEOUT, OVERLOADED, ERROR, NO_KEY, NOT_FOUND
- Verdict system: Perfect, Normal, Slow, Spiky, Very Slow, Unstable
- ANSI table display with color-coded output
- JSON output mode
- `--once` single-round mode
- Ring buffer (100 samples) for rolling metrics
