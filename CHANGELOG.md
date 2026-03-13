# Changelog

All notable changes to nimakai are documented in this file.

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
