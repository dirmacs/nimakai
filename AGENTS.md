# Nimakai — Agent Context

nimakai (నిమ్మకాయి, "lemon" in Telugu) is a NIM latency benchmarker written in Nim. Single binary, v0.15.4. Provides real-time stability scoring and routing recommendations for the dirmacs oh-my-opencode setup.

**Also includes:** nimaproxy — Rust key-rotation proxy for production use (in `nimaproxy/` subdirectory). v0.15.4 includes the uptime-oriented NVIDIA pool, dynamic per-key AIMD windows, bounded admission wait, solo racing fallback, large-prompt fanout caps, config-driven turn logging, adaptive racing/gateway limits, racing fallback/429 fixes, direct timeout handling, and the NVIDIA NIM assistant message validation fixes used by OMP/Pawan.

## FFI Integration (v0.15+)

nimakai embeds nimaproxy via FFI. The Nim CLI can start/stop/query the proxy directly:

```bash
nimakai proxy start --proxy-config /path/to/nimaproxy.toml --proxy-port 8080
nimakai proxy status
nimakai proxy stop
```

- `libnimaproxy.so` must be in the same directory as nimakai or `LD_LIBRARY_PATH` must be set
- Config file with API keys required (see nimaproxy section below)
- PID file at `/tmp/nimaproxy.pid` with format "PID:PORT"

## Architecture

### nimakai (Nim)

```text
src/
  nimakai.nim   — CLI entry: parse args, dispatch to subcommands
  nimakai/
    ping.nim      — HTTP ping: timed GET to NIM health endpoint, parse resp.code.int
    metrics.nim   — Ring buffer (last 100 samples), P50/P95/P99, jitter (stddev),
                    stability score 0–100 = composite of P95 + jitter + spike rate + uptime
    catalog.nim   — 90-model catalog: model IDs, context windows
    display.nim   — ncurses-style terminal table: live refresh, ANSI colors per health state
    config.nim    — Load nimakai.cfg, parse --profile flag, profile variable overrides
    recommend.nim — Score-based recommendation: given task type → best available model
    discovery.nim — discoverModels() via NVIDIA API, diffCatalog() vs hardcoded catalog; syncFromProxy()
    history.nim   — Persist latency samples to disk, read/display trends with --days flag
tests/          — 18 test files; `nimble test` runs 17 isolated suites, while
                  test_proxy.nim is a manual FFI/service suite
```

### nimaproxy (Rust)

```text
nimaproxy/
  Cargo.toml               lib + bin + tests
  nimaproxy.toml           Config (NOT committed - contains API keys)
  nimaproxy.toml.example   Template
  .gitignore               Excludes nimaproxy.toml
  src/
    lib.rs                 Exports modules + AppState
    main.rs                Binary entry point
    config.rs              TOML config parsing + unit tests
    key_pool.rs            Key rotation, rate-limit tracking + unit tests
    model_stats.rs         Per-model latency tracking + unit tests
    model_router.rs        Latency-aware model selection + unit tests
    proxy.rs               HTTP handlers
  tests/
    integration.rs         45 integration tests
    e2e_live.rs            14 E2E tests with real NVIDIA API
    stress_test.rs         1 live stress test (`NIMAPROXY_STRESS_TURNS` configurable)
    coverage_gaps.rs       14 coverage gap tests
    proxy_error_paths.rs   32 proxy error path tests
    live_chat.rs           5 live chat tests
    live_key_rotation.rs   2 key rotation tests
    live_routing.rs        2 routing tests
    live_conversation.rs   2 conversation tests
    live_streaming.rs      2 streaming tests
    live_circuit_breaker.rs 2 circuit breaker tests
    live_tool_calls.rs     7 tool call tests

## Racing (Speculative Execution)

V3 feature: fires N parallel requests to N models, returns first response.
Trades extra token budget for min(P50 latency). v0.15.4 keeps the production
healthy ceiling at 3 racers, backs off to 2 under pressure, and falls back to a
single best model when fewer than two viable racers/key slots exist.

```toml
[racing]
enabled = true
models = [
  "minimaxai/minimax-m3",
  "z-ai/glm-5.1",
  "stepfun-ai/step-3.7-flash",
  "moonshotai/kimi-k2.6",
  "qwen/qwen3.5-397b-a17b",
  "minimaxai/minimax-m2.7",
  "nvidia/nemotron-3-ultra-550b-a55b",
  "deepseek-ai/deepseek-v4-flash",
]
max_parallel = 3
timeout_ms = 15000
max_total_request_ms = 30000
strategy = "complete"
adaptive = true
min_parallel = 2
pressure_parallel = 2
degraded_parallel = 2
solo_fallback = true
large_prompt_char_threshold = 12000
large_prompt_parallel = 1
fast_models = [
  "minimaxai/minimax-m3",
  "z-ai/glm-5.1",
  "stepfun-ai/step-3.7-flash",
  "moonshotai/kimi-k2.6",
]
fallback_models = [
  "qwen/qwen3.5-397b-a17b",
  "deepseek-ai/deepseek-v4-flash",
  "minimaxai/minimax-m2.7",
  "nvidia/nemotron-3-ultra-550b-a55b",
]

[limits]
max_upstream_in_flight = 8
max_in_flight_per_key = 2
admission_wait_ms = 5000

[timeouts]
min_dynamic_timeout_ms = 15000
dynamic_sample_floor = 25
```

## Model Routing (V2)

When `model=auto` is sent, nimaproxy picks the best model from the configured list using real-time latency stats. Two strategies:

- **`round_robin`**: cycles through models in order, ignores latency data
- **`latency_aware`** (default): prefers fastest non-degraded model by avg TTFC

Degraded models (≥3 consecutive failures or avg > spike_threshold_ms) are skipped until they recover. Untried models (< 3 samples) get priority.

```toml
[routing]
strategy = "latency_aware"
spike_threshold_ms = 3000
models = [
  "minimaxai/minimax-m3",
  "z-ai/glm-5.1",
  "stepfun-ai/step-3.7-flash",
  "moonshotai/kimi-k2.6",
  "qwen/qwen3.5-397b-a17b",
  "minimaxai/minimax-m2.7",
  "nvidia/nemotron-3-ultra-550b-a55b",
  "deepseek-ai/deepseek-v4-flash",
]
```

Available racing models (current active pool, 8 total): minimaxai/minimax-m3, z-ai/glm-5.1, stepfun-ai/step-3.7-flash, moonshotai/kimi-k2.6, qwen/qwen3.5-397b-a17b, minimaxai/minimax-m2.7, nvidia/nemotron-3-ultra-550b-a55b, deepseek-ai/deepseek-v4-flash

Current pool model params mirror build.nvidia.com snippets: DeepSeek Pro/Flash
use `temperature=1.0`, `top_p=0.95`, `max_tokens=16384` with nested
`chat_template_kwargs` (`thinking`, and Flash `reasoning_effort=high`);
Nemotron 3 Ultra uses `temperature=1.0`, `top_p=0.95`, `max_tokens=16384`,
`reasoning_budget=16384`, and `chat_template_kwargs.enable_thinking=true`;
Mistral Medium 3.5 uses `temperature=0.7`, `top_p=1.0`, `reasoning_effort=high`;
GLM 5.1 uses `top_p=1.0` and `seed=42`; its NVIDIA snippet streams, but
nimaproxy requires callers to explicitly request `"stream": true`. Qwen 3.5
397B uses `temperature=0.6`, `top_k=20`, `presence_penalty=0`,
`repetition_penalty=1`; MiniMax M3 and M2.7 use `max_tokens=8192`.

## Metrics Reference

| Metric | How Computed |
|--------|-------------|
| Latest | Last round-trip time (ms) |
| Avg | Mean of ring buffer (last 100 samples) |
| P50 | Median of sorted ring buffer |
| P95 | 95th percentile |
| P99 | 99th percentile |
| Jitter | Standard deviation of ring buffer |
| Stability | `(100 - P95_penalty - jitter_penalty - spike_rate_penalty) * uptime_factor` |

Health states: `UP`, `TIMEOUT`, `OVERLOADED`, `ERROR`, `NO_KEY`, `NOT_FOUND`
Verdict labels: `Perfect`, `Normal`, `Slow`, `Spiky`, `Very Slow`, `Unstable`, `Not Active`

## Common Tasks

**Add a new model to the catalog:**

1. Edit `src/catalog.nim` — add entry to `MODEL_CATALOG` sequence
2. Set SWE-bench Verified score (or reasoning equivalent)
3. Run `nimble test` — `test_catalog.nim` validates catalog integrity
4. Rebuild: `nimble build`

**Add a new subcommand:**

1. Add proc in the relevant module (e.g., `discovery.nim`)
2. Add CLI dispatch case in `src/nimakai.nim`
3. Add test file `tests/test_<name>.nim`
4. Register test in `nimakai.nimble` task block

**Change stability score formula:**

- Formula in `src/metrics.nim` — `calcStability()` proc
- Re-run `nimble test` to catch regressions in `test_metrics.nim`

## Key Decisions

- **Nim over Rust** — name pun (NIM + Nim = nimakai), fast compile, small binary
- **`resp.code.int` not `parseInt($resp.code)`** — Nim's `$HttpCode` returns "200 OK" not "200"; fixed in 0.9.1
- **Ring buffer capped at 100** — balances memory and statistical relevance
- **Hardcoded catalog, not fetched** — NIM API doesn't expose capbility metadata; catalog is curated manually
- **`malebolgia` for parallel pinging** — concurrent HTTP without full async overhead

## Integration with oh-my-opencode

Nimkai's `recommend` subcommand outputs JSON consumed by aegis-opencode for routing config generation:

```bash
./nimakai recommend --task coding --format json
# → {"primary": "minimaxai/minimax-m3", "fallback": "stepfun-ai/step-3.7-flash"}
```

## nimaproxy v0.15.4 Critical Fixes (cumulative)

### Racing, Timeout, and Model Defaults

- Applies build.nvidia.com per-model defaults for the current catalog-default pool.
- Treats `stream=true` as caller-controlled response mode, not a forced model hyperparameter.
- Direct chat requests honor configured dynamic upstream timeouts and return 504 on timeout.
- Racing requests honor `max_total_request_ms` as a wall-clock deadline across racers and sequential fallback.
- Repeated upstream timeouts quarantine a model from normal racing/routing; expired quarantines allow one half-open probe.
- New or failure-only models keep the configured max timeout until enough latency history exists, then learned timeouts are clamped by `min_dynamic_timeout_ms`.
- Adaptive racing uses fast/fallback tiers and backs off from `max_parallel=3` to pressure/degraded fanout when gateway pressure rises.
- Dynamic per-key windows halve on 429 and reopen slowly after successful requests.
- Bounded admission wait gives key slots a short chance to free up before local 503/429 responses.
- Solo fallback keeps `auto` useful when racing cannot safely launch at least two upstream requests.
- Solo fallback and exhausted races can walk unused fallback candidates sequentially after transient 5xx/timeouts.
- Provider-prefixed `nimaproxy/auto` is accepted as an alias for `auto`.
- Large prompts can cap racing fanout to avoid burning multiple full-context requests.
- Gateway limits cap total upstream in-flight requests and per-key in-flight requests; saturated gateways return local 503s.
- `/health`, `/stats`, and `nimakai proxy status` expose gateway in-flight counts, key window capacity, request counters, fanout average, racing wins, and per-key in-flight counts.
- Racing excludes server-degraded/all-keys-failed models, but can backfill with least-bad degraded latency/failure candidates when healthy capacity is insufficient.
- Local latency degradation requires three samples; NVIDIA server-degraded responses are still honored immediately.
- Losing 429 racers do not globally cool keys when another model wins; all-key/all-race rate-limit cases return 429.

### Assistant Message Validation

NVIDIA NIM API requires assistant messages to have either `content` OR `tool_calls`, not both:

- **Issue**: `fix_message_ordering()` inserted messages with both `content` AND `tool_calls: []`
- **Fix**: Removed `tool_calls` from inserted messages
- **Impact**: Resolves OMP/Pawan integration errors when tool→user transitions occur

### Content Field Sanitization

When `tool_calls` is present, `content` must be `null` (not empty string):

- **Issue**: `sanitize_tool_calls()` set empty string `content: ""` for messages with `tool_calls`
- **Fix**: Sets `content` to `serde_json::Value::Null` instead
- **Impact**: Prevents "Assistant message must have either content or tool_calls, but not both" errors

### Message Ordering

Inserts empty assistant messages between `tool` and `user` roles to satisfy NVIDIA validation:

- Runs before `transform_message_roles()` in both `resolve_model()` and `race_models()`
- Handles all tool→user transitions in conversation history
- Ensures compatibility with OMP, Pawan, and similar frameworks

## Environment

- `NVIDIA_API_KEY` — required for NIM endpoint access; can also be set in `nimakai.cfg`
- `RUST_LOG` equivalent: `nimakai --verbose` flag
- Config file: `nimakai.cfg` in cwd, or `~/.config/nimakai/nimakai.cfg`
