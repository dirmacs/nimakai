# Changelog

All notable changes to nimakai are documented in this file.

## [Unreleased]

## [0.15.7] - 2026-09-02

### Added

- **nimaproxy startup model refresh**: Before `AppState` is constructed, nimaproxy now fetches
  the upstream `GET /v1/models` catalog with a pool key (5s timeout) and prunes every configured
  model list (`[routing].models`, `[racing].models`, `fast_models`, `fallback_models`) of ids
  not present upstream, logging a `warn!` per pruned id and an `info!` summary. Fetch failures
  are fail-open: the configured lists are left unchanged and startup proceeds. Pruning that
  empties the routing or racing pool falls back to passthrough/solo mode the same way an empty
  config does today, with a loud `warn!`.
- **nimaproxy periodic model recheck**: New `[racing].model_check_interval_secs` config key
  (default `3600`, `0` disables) drives a background `tokio::spawn` task that re-fetches the
  upstream catalog and marks configured models that have disappeared as server-degraded via the
  existing `ModelStatsStore::record_server_degraded` mechanism.
- **`/stats` `pruned_models`**: Exposes the model ids pruned from the configured lists at
  startup.
- Added: auth-failure key quarantine (nimaproxy) — an upstream `401`/`403` on any inference
  path (direct chat, streaming, `completions`, `embeddings`, and each racing leg) is now
  treated as a bad KEY, not a bad request or model: `KeyPool::mark_auth_failed` puts the
  leased key on `[limits].auth_failure_cooldown_secs` (default `900`, `0` disables
  quarantine) and bumps a new cumulative per-key `auth_failures` counter, logs one `warn!`,
  and retries with the next available key (bounded to the pool size, mirroring the existing
  `completions`/`embeddings` retry loop). Racing legs quarantine the key only and never call
  the model degradation/timeout-quarantine hooks, so one bad key cannot make a healthy model
  look degraded. If every key is exhausted, the upstream 401/403 status and body are returned
  to the client unchanged. `/stats` now reports `auth_failures` per key and a top-level
  `auth_failure_cooldown_secs`.
- **Production pool refresh (2026-09-02)**: Re-verified the NVIDIA-hosted `nimaproxy.toml`
  racing/routing pool against the authenticated `/v1/models` catalog and live chat-completion
  probes (non-streaming `max_tokens=8` + streaming, per candidate). `z-ai/glm-5.1`,
  `stepfun-ai/step-3.7-flash`, `qwen/qwen3.5-397b-a17b`, `minimaxai/minimax-m2.7`, and
  `deepseek-ai/deepseek-v4-flash` are no longer in the authenticated catalog and were replaced
  with `openai/gpt-oss-120b`, `openai/gpt-oss-20b`, and `nvidia/nemotron-3-super-120b-a12b`
  (all proven live, 0.8s-3.5s). Candidate successors `deepseek-ai/deepseek-v4-flash-0731`,
  `deepseek-ai/deepseek-v4-pro-0813`, and `moonshotai/kimi-k3` were probed but consistently
  timed out (60-90s across 3 keys) and were not adopted. `moonshotai/kimi-k2.6` is kept per
  policy despite live probes returning 404 ("Function not found for account") on every
  non-broken pool key — flagged for owner follow-up, not auto-removed.

### Changed

- Version bump: nimakai/nimaproxy 0.15.6 -> 0.15.7.
- **Racing/routing pool** (6 models, was 8): `nvidia/nemotron-3-super-120b-a12b`,
  `openai/gpt-oss-20b`, `openai/gpt-oss-120b` (`fast_models`, lowest probe latency); kept
  `minimaxai/minimax-m3`, `nvidia/nemotron-3-ultra-550b-a55b`, `moonshotai/kimi-k2.6`
  (`fallback_models` — all measured unusually slow or unavailable in this probe session).

## [0.15.6] - 2026-06-15

### Changed

- Version bump: nimakai/nimaproxy 0.15.5 -> 0.15.6.

### Fixed

- **Live key rotation tests**: Replaced the old direct single-key Qwen load loop with bounded gateway tests against a running nimaproxy instance, covering key rotation, burst behavior, and post-burst usability without excessive live API calls.

## [0.15.5] - 2026-06-15

### Added

- **Racing request deadline**: `[racing].max_total_request_ms` caps the full racing plus sequential fallback path, preventing multi-model timeout chains from stretching a single client request indefinitely.
- **Timeout quarantine**: Models with repeated upstream timeouts are temporarily removed from normal racing/routing and reintroduced via a single half-open probe after cooldown.
- **Degradation buckets**: Racing now treats slow successful models as latency-degraded fallback capacity ahead of models with fresh availability failures.
- **Fallback telemetry**: `/stats.gateway`, `nimakai proxy status --json`, and parser tests expose solo fallback, sequential fallback, all-racers-failed, and racing deadline counters.

### Changed

- Version bump: nimakai/nimaproxy 0.15.4 -> 0.15.5.
- **Production racing tuning**: Healthy racing now uses `max_parallel=2`, a MiniMax M3 + GLM 5.1 + Step 3.7 fast pool, and `max_total_request_ms=25000`; the full eight-model pool remains available as fallback capacity.
- **Production degradation threshold**: Example and deployed routing config now use `spike_threshold_ms=12000` so stable 6-12s live winners are not mislabeled as degraded.

### Fixed

- **Live e2e tests**: Racing live tests now treat bounded 504/deadline responses as upstream unavailability, matching the new total request deadline behavior.

## [0.15.4] - 2026-06-14

### Added

- **nimaproxy uptime controls**: Dynamic per-key AIMD windows shrink usable key concurrency on 429 and reopen gradually after successful requests.
- **Admission wait**: `[limits].admission_wait_ms` lets saturated gateways wait briefly for a key/upstream slot before returning local 503/429.
- **Solo fallback and large-prompt caps**: Racing can fall back to one best model when fewer than two viable racers/key slots exist, and large prompts can cap fanout to avoid multiplying full-context requests.
- **Sequential fallback**: Solo mode and exhausted races can walk the ordered fallback pool on transient 5xx/timeouts before returning failure to the caller.
- **OMP model alias**: Requests using `"model": "nimaproxy/auto"` are normalized to `"auto"` for OpenAI-compatible client configs.
- **Turn logging**: `[logging]` now controls the turn log path, and the production service can write request metadata to `/var/log/nimaproxy/turns.jsonl`.
- **Telemetry**: `/health`, `/stats`, `nimakai proxy status`, and parser tests now expose key window capacity, available key permits, admission wait, configured per-key ceilings, and racing uptime controls.

### Changed

- Version bump: nimakai/nimaproxy 0.15.3 -> 0.15.4.
- Production-oriented racing defaults now use the eight-model uptime pool, `max_parallel=3`, pressure/degraded fanout of `2`, `max_upstream_in_flight=8`, `max_in_flight_per_key=2`, `admission_wait_ms=5000`, and a 15s dynamic timeout floor.
- Mistral Medium 3.5 and DeepSeek Pro remain documented in per-model params but are kept out of active routing/racing defaults after observed hard/schema failures.

### Fixed

- **Assistant normalization**: Assistant messages with no usable `tool_calls` are normalized to `content=""`; assistant messages with real tool calls keep `content=null`.
- **Hard model errors**: Deterministic 400 assistant/schema errors mark the model server-degraded immediately instead of being treated like ordinary latency noise.
- **Fallback telemetry**: Sequential solo/fallback wins are counted in `gateway.racing_wins`.
- **Turn logger safety**: Replaced the mutable global logger with `OnceLock`.

## [0.15.3] - 2026-06-12

### Added

- **nimaproxy adaptive racing**: `racing.adaptive=true` can shrink fanout under gateway pressure or partial key degradation while keeping `max_parallel=10` as the healthy ceiling.
- **Gateway limits**: New `[limits]` config caps total upstream in-flight requests and per-key in-flight requests before calls leave the proxy.
- **Tiered racing pools**: `fast_models` and `fallback_models` let racing prefer the faster core pool, then backfill with heavier or slower models when needed.
- **Gateway telemetry**: `/health`, `/stats`, and `nimakai proxy status` now expose upstream in-flight counts, request mix, overload/no-key/timeout/429 counts, fanout average, racing wins, and per-key in-flight counts.
- **Proxy FFI parser tests**: Added coverage for legacy and v0.15.3 proxy health/stats JSON payloads.
- **Quota-aware stress triage**: `NIMAPROXY_STRESS_TURNS` now controls the live stress-test turn count.

### Changed

- Version bump: nimakai/nimaproxy 0.15.2 -> 0.15.3.
- Production `nimaproxy.toml` now keeps port `8080`, `max_parallel=10`, adaptive fanout defaults of `min=2`, `pressure=6`, and `degraded=3`.
- Dynamic timeouts now use a configurable warm-up floor (`min_dynamic_timeout_ms=8000`, `dynamic_sample_floor=10`) before learned model latency can reduce request timeouts.
- Local latency degradation now requires at least three latency samples, so one slow successful call does not remove a model from healthy routing.

### Fixed

- **Gateway overload behavior**: Requests are rejected locally with 503 when global or per-key concurrency is exhausted instead of piling up against NVIDIA until all keys cool down.
- **Racing cleanup**: Losing racers are aborted after the first successful response, reducing unnecessary upstream work during successful races.
- **TUI proxy footer**: `UP` from nimaproxy health is now rendered as healthy, and adaptive racing is visible in the footer.

## [0.15.2] - 2026-06-12

### Added

- **Catalog and proxy pool refresh**: Added `nvidia/nemotron-3-ultra-550b-a55b`
  and `minimaxai/minimax-m3` to the built-in catalog and nimaproxy routing/racing pools.
- **Nemotron defaults**: Added `reasoning_budget=16384` and
  `chat_template_kwargs.enable_thinking=true` support for NVIDIA catalog snippets.

### Changed

- Version bump: nimakai/nimaproxy 0.15.1 → 0.15.2.
- Catalog count: 88 → 90; nimaproxy configured pool count: 8 → 10 with `max_parallel` raised to 10.

### Fixed

- **nimaproxy dynamic timeout warm-up**: Models with fewer than two successful latency samples now keep the configured max timeout instead of shrinking to the synthetic 7.5s fallback.

## [0.15.1] - 2026-05-31

### Fixed

- **nimaproxy stream semantics**: `stream=true` from NVIDIA catalog snippets no longer forces JSON clients into SSE mode; callers must opt into streaming explicitly.
- **nimaproxy direct timeouts**: Non-racing chat requests now honor configured dynamic upstream timeouts and return 504 on timeout.
- **nimaproxy racing fallback**: Racing keeps healthy candidates first, then backfills with least-bad degraded latency/failure candidates when healthy capacity is insufficient.
- **nimaproxy racing 429 handling**: Losing 429 racers no longer globally cool API keys when another model wins; all-key/all-race rate-limit cases return 429.

### Changed

- Version bump: nimakai 0.15.0 → 0.15.1, nimaproxy 0.13.7 → 0.15.1.
- Docs refreshed for the eight-model nimaproxy pool, current test counts, and caller-controlled streaming behavior.

## [0.15.0] - 2026-05-04

### Added

- **Responsive TUI**: `printTable` now auto-switches to compact mode on terminals < 100
  columns, hiding JITTER/STAB/BAR/UP% columns while preserving MODEL/LATEST/AVG/P95/HEALTH/VERDICT
- **Column separators**: Added `│` (U+2502) vertical separators between all columns and `─`
  (U+2500) horizontal separator line matching content width
- **8 new models to catalog**: Total increased from 80 to 88. New entries include
  `deepseek-ai/deepseek-v4-pro` (83.0%), `mistralai/mistral-medium-3.5-128b` (82.0%),
  `deepseek-ai/deepseek-v4-flash` (81.5%), `minimaxai/minimax-m2.7` (79.0%),
  `z-ai/glm-5.1` (77.5%), `moonshotai/kimi-k2.6` (77.0%),
  `nvidia/nemotron-3-nano-omni-30b-a3b-reasoning` (44.0%),
  `mistralai/mistral-small-4-119b-2603` (35.0%)

### Fixed

- **TUI table rendering**: Separator width now correctly accounts for all columns; no more
  overflow or misalignment on narrow terminals
- **Catalog print spacing**: Missing space between capabilities and model ID in `printCatalog`
- **Catalog score ordering**: Previously-added v0.14.0 models were inserted after a 49.0-score
  entry instead of at the top; now correctly score-sorted descending

### Changed

- nimakai version bump: 0.14.0 → 0.15.0
- README/nimaproxy README: updated catalog count (88), test counts, model lists in examples
- nimaproxy README routing example: removed erroneous `nvidia/` prefix from model IDs



### Added (nimakai)

- **Responsive TUI key handling**: All inner-loop `break` statements replaced with `doRenderTui() + continue`; sort keys (A/P/S/N/U), filter mode, cursor (j/k), pagination (T/[/]) and overlay dismiss now respond instantly without waiting for the next ping cycle
- **`safeProxyHealth()` wrapper**: Swallows dynlib errors when `libnimaproxy.so` is absent; TUI starts cleanly without the proxy running
- **Proxy status footer**: `printTable` gains optional `proxyStatus: Option[ProxyHealth]` parameter (default `none`); renders active key count, routing, and racing status in footer when proxy is running
- **`syncFromProxy()`**: New exported proc in `discovery.nim` — fetches `/v1/models` from a locally running nimaproxy instance (3 s timeout, silent on error)

### Fixed (nimakai)

- **`parseDiscoverResponse` AssertionDefect**: Guarded `hasKey` call behind `data.kind == JObject` check; non-object JSON roots (arrays, null) no longer crash the parser

### Tests

- 17 new tests in `test_discovery.nim`: fuzz inputs (empty string, array root, truncated JSON, 50-entry batch), `diffCatalog` edge cases
- 22 new tests in `test_display.nim`: `filterStats`, `highlightQuery`, `pageLegend`, `latencyBar` suites

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
