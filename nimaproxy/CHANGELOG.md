# Changelog

All notable changes to nimaproxy will be documented in this file.

## [Unreleased]

## [0.15.7] - 2026-09-02

### Added

- Startup upstream catalog check: configured routing/racing/fast/fallback model lists are
  pruned of ids missing from `GET /v1/models` (fail-open on fetch error), with a `warn!` per
  pruned id; `/stats` exposes `pruned_models`.
- `[racing].model_check_interval_secs` (default 3600, `0` disables): periodic recheck marks
  configured models that vanished upstream as server-degraded.
- Added: auth-failure key quarantine — an upstream `401`/`403` on any inference path (direct
  chat, streaming, `completions`, `embeddings`, and each racing leg) is now treated as a bad
  KEY, not a bad request or model: `KeyPool::mark_auth_failed` puts the leased key on
  `[limits].auth_failure_cooldown_secs` (default `900`, `0` disables quarantine) and bumps a
  new cumulative per-key `auth_failures` counter, a `warn!` is logged once, and the request is
  retried with the next available key (bounded to the pool size, mirroring the existing
  `completions`/`embeddings` retry loop). Racing legs quarantine the key only and never call
  the model degradation/timeout-quarantine hooks, so one bad key cannot make a healthy model
  look degraded. If every key is exhausted, the upstream 401/403 status and body are returned
  to the client unchanged. `/stats` now reports `auth_failures` per key and a top-level
  `auth_failure_cooldown_secs`.

### Changed

- Version bump: 0.15.6 -> 0.15.7.

## [0.15.6] - 2026-06-15

### Changed

- Version bump: 0.15.5 -> 0.15.6.

### Fixed

- Live key rotation tests now target a running nimaproxy gateway with bounded request counts, covering key rotation, burst behavior, and post-burst usability without excessive live API calls.

## [0.15.5] - 2026-06-15

### Added

- `[racing].max_total_request_ms` caps the full racing plus sequential fallback path so one client request cannot wait through an unbounded chain of slow models.
- Timeout quarantine removes repeated timeout offenders from normal candidate pools and reintroduces them with a single half-open probe after cooldown.
- Racing candidate selection separates latency degradation from availability degradation, so slow successful models are preferred over models with fresh failures.
- `/stats.gateway` exposes solo fallback, sequential fallback, all-racers-failed, and racing deadline counters for production triage.

### Changed

- Version bump: 0.15.4 -> 0.15.5.
- Production racing now uses `max_parallel=2`, a MiniMax M3 + GLM 5.1 + Step 3.7 fast pool, and `max_total_request_ms=25000`; the full eight-model pool remains available as fallback capacity.
- Example and deployed routing config now use `spike_threshold_ms=12000` so stable 6-12s live winners are not mislabeled as degraded.

### Fixed

- Live racing e2e tests now treat bounded 504/deadline responses as upstream unavailability, matching the new total request deadline behavior.

## [0.15.4] - 2026-06-14

### Added

- Dynamic per-key AIMD windows: 429s halve a key's usable concurrency window; successful requests gradually reopen the window up to the configured per-key ceiling.
- `[limits].admission_wait_ms` for bounded local waiting before returning overload/no-key responses.
- Racing solo fallback and large-prompt fanout caps.
- Sequential fallback through the ordered model pool when solo mode or an exhausted race sees transient 5xx/timeouts.
- `nimaproxy/auto` model alias support for OMP/OpenAI-compatible provider configs.
- `/health` and `/stats` telemetry for key window capacity, available key permits, configured per-key ceilings, admission wait, and racing uptime controls.
- Config-driven turn logging through `[logging]`.

### Changed

- Version bump: 0.15.3 -> 0.15.4.
- Active routing/racing examples now use the eight-model uptime pool and omit Mistral Medium 3.5 / DeepSeek Pro from active races after observed hard/schema failures.
- Production-oriented defaults now use `max_parallel=3`, pressure/degraded fanout of `2`, `max_upstream_in_flight=8`, `max_in_flight_per_key=2`, `admission_wait_ms=5000`, and a 15s timeout floor.

### Fixed

- Assistant messages without real tool calls now keep string content, avoiding NVIDIA `content=None tool_calls=None` rejects.
- Assistant messages with real tool calls keep `content=null`.
- Deterministic 400 assistant/schema errors now quarantine the model immediately.
- Sequential solo/fallback wins are counted in `gateway.racing_wins`.
- The turn logger no longer uses a mutable static global.

## [0.15.3] - 2026-06-12

### Added

- Adaptive racing controls: `adaptive`, `min_parallel`, `pressure_parallel`, `degraded_parallel`, `fast_models`, and `fallback_models`.
- Gateway concurrency controls: `[limits].max_upstream_in_flight` and `[limits].max_in_flight_per_key`.
- Timeout learning controls: `[timeouts].min_dynamic_timeout_ms` and `[timeouts].dynamic_sample_floor`.
- `/health` and `/stats` now report gateway in-flight counts, configured limits, request counters, overload/no-key/timeout/429 counters, fanout average, racing wins, and per-key in-flight counts.
- `NIMAPROXY_STRESS_TURNS` controls the live stress-test turn count for quota-aware production triage.

### Changed

- Version bump: 0.15.2 -> 0.15.3.
- Racing uses tiered candidate selection when adaptive mode is enabled: healthy fast models first, healthy fallback models second, degraded candidates only as backfill.
- Successful races abort losing tasks after the first successful upstream response to reduce wasted NVIDIA work.
- Local latency degradation requires at least three latency samples; explicit NVIDIA server-degraded responses still take effect immediately.
- Current non-live test counts: 262 lib, 45 integration, 32 proxy_error_paths, and 14 coverage_gaps.

### Fixed

- Gateway overload is handled before upstream dispatch; saturated global or per-key concurrency returns 503 locally.
- Dynamic timeout calculation no longer shrinks below the configured floor until the configured sample floor is met.

## [0.15.2] - 2026-06-12

### Added

- Added `nvidia/nemotron-3-ultra-550b-a55b` and `minimaxai/minimax-m3` to routing and racing examples/configuration.
- Added `reasoning_budget` parsing and upstream injection for Nemotron 3 Ultra.

### Changed

- Version bump: 0.15.1 → 0.15.2.
- Configured pool count: 8 → 10 with `max_parallel` raised to 10.

### Fixed

- **Dynamic timeout warm-up**: Models with fewer than two successful latency samples now keep the configured max timeout instead of shrinking to the synthetic 7.5s fallback.

## [0.15.1] - 2026-05-31

### Fixed

- **Stream semantics**: `stream=true` from NVIDIA catalog snippets is retained for catalog fidelity but no longer forces JSON callers into SSE mode.
- **Direct request timeout**: Non-racing chat requests now honor configured dynamic upstream timeouts and return 504 on timeout.
- **Racing candidate fallback**: Healthy candidates are preferred; degraded latency/failure candidates backfill only when healthy capacity is insufficient.
- **Racing 429 handling**: Losing 429 racers do not globally cool keys when another model wins; all-key/all-race rate-limit cases return 429.

### Changed

- Version bump: 0.13.7 → 0.15.1.
- Updated docs and examples for the current eight-model pool and build.nvidia.com per-model defaults.
- Test counts at this release: 251 lib, 45 integration, 31 proxy_error_paths, 14 coverage_gaps, 14 e2e_live, 22 live suite tests, and 1 stress test.

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
