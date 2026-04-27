# Nimakai

NVIDIA NIM model latency benchmarker. Single-binary, written in Nim. v0.14.0. 80-model catalog with SWE-bench scores, stability scoring, and oh-my-opencode routing recommendations.

## Build & Test

```bash
cd nimakai
nimble build                       # → ./nimakai binary
nimble test                        # runs 16 test suites

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
    catalog.nim    — 80-model catalog with SWE-bench scores
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
tests/          — 16 test files
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
    stress_test.rs         — 1 live stress test
    coverage_gaps.rs       — 14 coverage gap tests
    proxy_error_paths.rs   — 22 proxy error path tests
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
- **80-model catalog is hardcoded in `catalog.nim`** — update there when new NIM models launch

## Config

```ini
# nim.cfg
api_key = nvapi-...
timeout_ms = 5000
num_results = 100

[profile.work]
models = ["devstral-2-123b", "step-3.5-flash"]
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
  "meta/llama-3.3-70b-instruct",
  "qwen/qwen2.5-coder-32b-instruct",
  "moonshotai/kimi-k2-instruct",
  "mistralai/devstral-2-123b-instruct-2512",
]

[racing]
enabled = true
models = ["z-ai/glm4.7", "qwen/qwen3.5-397b-a17b", "mistralai/devstral-2-123b-instruct-2512"]
max_parallel = 3
timeout_ms = 8000
strategy = "complete"
```

`x-key-label` response header tracks which key was used for rotation debugging.

## Git Author

```bash
git -c user.name="bkataru" -c user.email="baalateja.k@gmail.com" commit
```
