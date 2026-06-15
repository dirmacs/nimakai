# Nimakai

NVIDIA NIM model latency benchmarker. Single-binary, written in Nim. v0.15.5. 90-model catalog with SWE-bench scores, stability scoring, and oh-my-opencode routing recommendations.

## Build & Test

```bash
cd nimakai
nimble build                       # → ./nimakai binary
nimble test                        # runs 17 isolated suites; test_proxy.nim is manual FFI

# One-liner rebuild and run
nimble build && ./nimakai
```

## Architecture

Two binaries in this repo:

### nimakai (Nim)

Single Nim binary with modules in `src/`:

```text
src/
  nimakai.nim   — Entry point, CLI dispatch, main loop, SIGINT handler
  nimakai/
    types.nim      — Types, enums, constants
    cli.nim        — CLI argument parsing with profiles
    metrics.nim    — Ring buffer, P50/P95/P99, jitter, stability score
    ping.nim       — HTTP ping + throughput measurement
    catalog.nim    — 90-model catalog with SWE-bench scores
    display.nim    — Table/JSON rendering, ANSI helpers, proxy footer
    config.nim     — Config file persistence + profile loading
    history.nim    — JSONL history persistence + trend detection
    opencode.nim   — OpenCode + oh-my-opencode integration
    recommend.nim  — Recommendation engine
    rechistory.nim — Recommendation history tracking (JSONL)
    sync.nim       — Backup, apply, rollback for OMO config
    watch.nim      — Watch mode alerting
    discovery.nim  — Live model discovery from NVIDIA API; syncFromProxy()
    proxyffi.nim   — Nim FFI bindings to libnimaproxy.so
    rustffi.nim    — Rust FFI bridge for concurrent HTTP pinging
    update.nim     — Fetch and update model catalog from NVIDIA NIM API
tests/          — 18 test files (17 in nimble test + manual FFI test_proxy.nim)
```

### nimaproxy (Rust)

Rust proxy in `nimaproxy/` subdirectory:

```text
nimaproxy/
  src/
    lib.rs                 — AppState, exports
    config.rs             — TOML config parsing
    key_pool.rs           — Key rotation, rate-limit tracking
    model_stats.rs        — Per-model latency tracking
    model_router.rs       — Latency-aware routing
    proxy.rs              — HTTP handlers
  tests/
    integration.rs       — 45 tests
    e2e_live.rs           — 14 live API tests
    stress_test.rs         — 1 live stress test (`NIMAPROXY_STRESS_TURNS` configurable)
    coverage_gaps.rs       — 14 coverage gap tests
    proxy_error_paths.rs   — 32 proxy error path tests
    live_chat.rs           — 5 live chat tests
    live_key_rotation.rs   — 2 key rotation tests
    live_routing.rs        — 2 routing tests
    live_conversation.rs   — 2 conversation tests
    live_streaming.rs      — 2 streaming tests
    live_circuit_breaker.rs — 2 circuit breaker tests
    live_tool_calls.rs     — 7 tool call tests
```

## Key Rules

- **Nim 2.0+ required** — uses `resp.code.int` not `parseInt($resp.code)` for HTTP status (fixed in 0.9.1)
- **SSL flag required** — build with `-d:ssl`; NIM endpoints are HTTPS
- **Release build uses size optimization** — `--opt:size` in the build task; keep binary small
- **`malebolgia` for parallelism** — used for concurrent model pinging; don't swap it out
- **90-model catalog is hardcoded in `catalog.nim`** — update there when new NIM models launch

## Config

```ini
# nim.cfg
api_key = nvapi-...
timeout_ms = 5000
num_results = 100

[profile.work]
models = ["minimaxai/minimax-m3", "stepfun-ai/step-3.7-flash"]
interval_ms = 2000
```

## Run Modes

```bash
./nimakai                           # continuous ping, live display
./nimakai watch                     # with latency alerts
./nimakai check                     # CI health check (exits non-zero if unhealthy)
./nimakai discover                  # compare live NVIDIA API vs. catalog
./nimakai sync                      # full catalog sync
./nimakai --profile work            # named benchmark profile
```

## nimaproxy — Key-Rotation Proxy

Standalone Rust binary in `nimaproxy/`. Exposes OpenAI-compatible API on localhost with key rotation and latency-aware routing.

```bash
cargo build --release --manifest-path=nimaproxy/Cargo.toml

# Copy and edit config
cp nimaproxy/nimaproxy.toml.example nimaproxy/nimaproxy.toml
# Edit nimaproxy.toml with your NVIDIA API keys

# Run
./nimaproxy/target/release/nimaproxy --config nimaproxy/nimaproxy.toml
```

Endpoints: `POST /v1/chat/completions`, `GET /v1/models`, `GET /health`, `GET /stats`

**Config example:**

```toml
listen = "127.0.0.1:8080"
target = "https://integrate.api.nvidia.com"

[[keys]]
key = "nvapi-YOUR_KEY_HERE"
label = "production"

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

[logging]
enabled = true
path = "/var/log/nimaproxy/turns.jsonl"

[timeouts]
min_dynamic_timeout_ms = 15000
dynamic_sample_floor = 25
```

Local latency degradation requires three samples; NVIDIA server-degraded
responses are still honored immediately. Solo mode and exhausted races can walk
unused fallback candidates sequentially after transient 5xx/timeouts. Clients
may send either `"auto"` or `"nimaproxy/auto"`.
Repeated upstream timeouts temporarily quarantine a model from normal candidate
pools; after cooldown, one half-open probe can recover it without flooding live
traffic with flaky candidates.
Candidate selection separates latency degradation from availability
degradation, so slow successful models stay ahead of models with fresh failures.
`/stats.gateway` reports solo fallback, sequential fallback, all-racers-failed,
and racing deadline counters for production triage.

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

`x-key-label` response header tracks which key was used for rotation debugging.

## Git Author

```bash
git -c user.name="bkataru" -c user.email="baalateja.k@gmail.com" commit
```
