# Nimakai

NVIDIA NIM model latency benchmarker. Single-binary, written in Nim. v0.9.3. 80-model catalog with tier labels, stability scoring, and oh-my-opencode routing recommendations.

## Build & Test

```bash
cd nimakai
nimble build                       # → ./nimakai binary
nimble test                        # runs 15 test suites

# One-liner rebuild and run
nimble build && ./nimakai
```

## Architecture

Two binaries in this repo:

### nimakai (Nim)

Single Nim binary with modules in `src/`:

```
src/
  nimakai.nim   — Entry point, CLI dispatch (default/watch/check/discover/sync)
  metrics.nim   — Latency ring buffer, percentiles (P50/P95/P99), jitter, stability score
  catalog.nim   — 80-model catalog with tier labels (S+/S/A+/A/A-/B+/B/C)
  ping.nim      — HTTP ping to NIM endpoint, response time measurement
  display.nim   — Terminal UI: live metrics table, health state colors
  config.nim    — Config loading from nim.cfg / CLI flags, named benchmark profiles
  recommend.nim — oh-my-opencode model routing recommendation engine
  discovery.nim — Live model discovery vs. catalog diff (discover subcommand)
  history.nim   — Latency history storage and trend display
```

### nimaproxy (Rust)

Rust proxy in `nimaproxy/` subdirectory:

```
nimaproxy/
  src/
    lib.rs                 — AppState, exports
    config.rs             — TOML config parsing
    key_pool.rs           — Key rotation, rate-limit tracking
    model_stats.rs        — Per-model latency tracking
    model_router.rs       — Latency-aware routing
    proxy.rs              — HTTP handlers
  tests/
    integration.rs       — 18 tests
    e2e_live.rs           — 6 live API tests
    stress_test.rs         — 25-turn live stress test
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
  "nvidia/meta/llama-3.3-70b-instruct",
  "nvidia/qwen/qwen2.5-coder-32b-instruct",
  "nvidia/moonshotai/kimi-k2-instruct",
  "nvidia/mistralai/devstral-2-123b-instruct-2512",
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
