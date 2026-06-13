<p align="center">
  <img src="assets/logo.svg" width="128" height="128" alt="nimakai logo">
</p>

<h1 align="center">nimakai</h1>

<p align="center">
  <strong>NVIDIA NIM model latency benchmarker, written in Nim.</strong>
</p>

<p align="center">
  <em>nimakai (నిమ్మకాయి) = lemon in Telugu. NIM + Nim = nimakai.</em>
</p>

---

A focused, single-binary tool that continuously pings NVIDIA NIM models and
reports latency metrics. Includes a 90-model catalog with SWE-bench scores,
recommendation engine for
[oh-my-opencode](https://github.com/bkataru/oh-my-opencode)
routing, watch mode with alerts, CI health checks, live model discovery, and
full sync mode. No bloat, no TUI framework, no telemetry. Just latency
numbers.

Also includes **nimaproxy** — a Rust-based key-rotation proxy for production use.

## Metrics

- **Latest** — most recent round-trip time
- **Avg** — rolling average (ring buffer, last 100 samples)
- **P50** — median latency
- **P95** — 95th percentile (tail spikes)
- **P99** — 99th percentile (worst case)
- **Jitter** — standard deviation (consistency)
- **Stability** — composite score 0-100 (P95 + jitter + spike rate + reliability)
- **Health** — UP / TIMEOUT / OVERLOADED / ERROR / NO_KEY / NOT_FOUND
- **Verdict** — Perfect / Normal / Slow / Spiky / Very Slow / Unstable / Not Active / Not Found
- **Up%** — uptime percentage

## Install

```bash
git clone https://github.com/dirmacs/nimakai.git
cd nimakai
nimble build
```

Requires Nim 2.0+ and OpenSSL.

## Usage

```bash
export NVIDIA_API_KEY="nvapi-..."

# Continuous monitoring (all models by default)
nimakai

# Single round, then exit
nimakai --once

# Specific models only
nimakai -m stepfun-ai/step-3.7-flash,qwen/qwen3.5-397b-a17b

# Sort by stability score
nimakai --sort stability

# Benchmark models from opencode.json
nimakai --opencode --once

# JSON output
nimakai --once --json
```

## Commands

```text
nimakai                    Continuous benchmark (default)
nimakai catalog            List all known models with metadata
nimakai recommend          Benchmark and recommend routing changes
nimakai watch              Monitor OMO-routed models with alerts
nimakai check              CI health check with exit codes
nimakai discover           Compare API models against catalog
nimakai history            Show historical benchmark data
nimakai trends             Show latency trend analysis (improving/degrading/stable)
nimakai opencode           Show models from opencode.json + OMO routing
nimakai proxy start        Start nimaproxy daemon (FFI integration)
nimakai proxy stop         Stop nimaproxy daemon
nimakai proxy status       Show nimaproxy live stats
```

## Recommendation Engine

nimakai can benchmark models and recommend optimal routing for oh-my-opencode categories:

```bash
# Advisory: show recommendations
nimakai recommend --rounds 3

# Full sync: backup -> diff -> apply to oh-my-opencode.json
nimakai recommend --rounds 5 --apply

# Rollback to previous config
nimakai recommend --rollback
```

Each OMO category is scored using weighted criteria:

| Category Need | SWE Weight | Speed Weight | Stability Weight |
|---------------|------------|--------------|------------------|
| Speed (quick) | 0.15 | 0.55 | 0.20 |
| Quality (deep, artistry) | 0.45 | 0.10 | 0.20 |
| Reliability (ultrabrain) | 0.25 | 0.20 | 0.40 |
| Vision (visual-engineering) | 0.30 | 0.20 | 0.30 |
| Balance (writing, default) | 0.30 | 0.30 | 0.25 |

## Interactive Keys (continuous mode)

| Key | Action |
|-----|--------|
| `A` | Sort by average latency |
| `P` | Sort by P95 latency |
| `S` | Sort by stability score |
| `N` | Sort by model name |
| `U` | Sort by uptime % |
| `1-9` | Toggle favorite on Nth model |
| `j` / `k` | Cursor down / up |
| `T` | Toggle pagination |
| `[` / `]` | Previous / next page |
| `/` | Enter filter mode (type to filter models) |
| `Esc` | Exit filter mode / clear filter |
| `Enter` | Detail view for selected model |
| `?` | Show key bindings help overlay |
| `Q` | Quit |

## Proxy Commands (FFI Integration)

nimakai includes FFI integration with nimaproxy, allowing you
to start/stop/query the Rust key-rotation proxy directly from the Nim
CLI:

```bash
# Start the proxy daemon
nimakai proxy start --proxy-config /path/to/nimaproxy.toml --proxy-port 8080

# Check live status
nimakai proxy status

# Stop the daemon
nimakai proxy stop
```

**Requirements:**

- `libnimaproxy.so` must be in the same directory as nimakai binary, or `LD_LIBRARY_PATH` must be set
- nimaproxy config file with API keys (see nimaproxy section below)

**Status output shows:**

- Overall health status
- Active key count
- Routing, racing, and adaptive fanout configuration
- Gateway request counters, fanout average, and overload/no-key/timeout/429 counts
- Per-key status (active/cooldown, key hint, in-flight count)
- Per-model latency stats (avg, P95, success rate, degradation)

## Options

| Flag | Short | Description | Default |
|------|-------|-------------|---------|
| `--once` | `-1` | Single round, then exit | continuous |
| `--models` | `-m` | Comma-separated model IDs | all models |
| `--interval` | `-i` | Ping interval in seconds | 5 |
| `--timeout` | `-t` | Request timeout in seconds | 15 |
| `--json` | `-j` | JSON output | table |
| `--sort` | | Sort: avg, p95, stability, name, uptime | avg |
| `--opencode` | | Use models from opencode.json | |
| `--rounds` | `-r` | Benchmark rounds for recommend | 3 |
| `--apply` | | Apply recommendations to oh-my-opencode.json | |
| `--rollback` | | Rollback oh-my-opencode.json from backup | |
| `--quiet` | `-q` | Suppress stderr status messages | |
| `--no-history` | | Don't write to history file | |
| `--dry-run` | | Preview recommend changes without applying | |
| `--rec-history` | | Show recommendation history | |
| `--throughput` | | Measure output token throughput | |
| `--alert-threshold` | | Alert threshold for watch mode | 50 |
| `--fail-if-degraded` | | Exit 1 if any model is degraded (check mode) | |
| `--days` | `-d` | Days of history to show | 7 |
| `--profile` | | Load named profile from config | |
| `--help` | `-h` | Show help | |
| `--version` | `-v` | Show version | |

## Configuration

Optional config at `~/.config/nimakai/config.json`:

```json
{
  "interval": 5,
  "timeout": 15,
  "thresholds": {
    "perfect_avg": 400,
    "perfect_p95": 800,
    "normal_avg": 1000,
    "normal_p95": 2000,
    "spike_ms": 3000
  },
  "profiles": {
    "fast": { "timeout": 5 }
  },
  "favorites": []
}
```

Use profiles with `nimakai --profile work` to load pre-configured settings.

Custom models can be added via `~/.config/nimakai/models.json` to extend the built-in catalog.

History is persisted to `~/.local/share/nimakai/history.jsonl` (30-day auto-prune).

## Architecture

### nimakai (Nim)

```text
src/
  nimakai.nim              Entry point, main loop, SIGINT handler
  nimakai/
    types.nim              Types, enums, constants
    cli.nim                CLI argument parsing with profiles
    metrics.nim            Pure metric functions (avg, p50, p95, p99, jitter, stability)
    ping.nim               HTTP ping + throughput measurement
    catalog.nim            90-model catalog with SWE-bench scores, O(1) index
    display.nim            Table/JSON rendering, ANSI helpers
    config.nim             Config file persistence + profile loading
    history.nim            JSONL history persistence + trend detection
    opencode.nim           OpenCode + oh-my-opencode integration
    recommend.nim          Recommendation engine (categories + agents + uptime)
    rechistory.nim         Recommendation history tracking (JSONL)
    sync.nim               Backup, apply, rollback for OMO config
    watch.nim              Watch mode alerting (down/recovered/degraded)
    discovery.nim          Live model discovery from NVIDIA API
    proxyffi.nim           FFI bindings and proxy health/stats JSON parsing
    rustffi.nim            Rust FFI bridge for concurrent HTTP pinging
    update.nim             Fetch and update model catalog from NVIDIA NIM API
tests/
    17 isolated suites run by `nimble test`
    test_proxy.nim         Manual FFI/service tests; starts/stops nimaproxy

### nimaproxy (Rust)

```text
nimaproxy/
  Cargo.toml               lib + bin + tests
  nimaproxy.toml           Config (NOT committed - contains API keys)
  nimaproxy.toml.example   Template for users
  .gitignore               Excludes nimaproxy.toml
  src/
    lib.rs                 Exports modules + AppState
    main.rs                Binary entry point
    config.rs              TOML config parsing
    turn_log.rs             Request logging and query analysis
    key_pool.rs            Key rotation, rate-limit tracking
    model_stats.rs          Per-model latency tracking
    model_router.rs        Latency-aware model selection
    proxy.rs               HTTP handlers
  tests/
    integration.rs         45 integration tests
    e2e_live.rs            14 E2E tests with real NVIDIA API
    stress_test.rs         1 live stress test (`NIMAPROXY_STRESS_TURNS` configurable)
    coverage_gaps.rs       14 coverage gap tests
    proxy_error_paths.rs   32 proxy error path tests
    live_chat.rs          5 live chat tests
    live_key_rotation.rs  2 key rotation tests
    live_routing.rs       2 routing tests
    live_conversation.rs  2 conversation tests
    live_streaming.rs     2 streaming tests
    live_circuit_breaker.rs 2 circuit breaker tests
    live_tool_calls.rs    7 tool call tests
```

## nimaproxy — Key-Rotation Proxy

Standalone Rust binary for production use. Provides OpenAI-compatible API with key rotation and latency-aware routing.

```bash
cd nimaproxy
cargo build --release

# Copy and edit config
cp nimaproxy.toml.example nimaproxy.toml
# Edit nimaproxy.toml with your NVIDIA API keys

# Run
./target/release/nimaproxy --config nimaproxy.toml
```

**Endpoints:**

- `GET /health` — Key pool status
- `GET /stats` — Per-model latency stats
- `GET /v1/models` — Passthrough to NVIDIA
- `GET /models` — Alias (without /v1/ prefix)
- `POST /v1/chat/completions` — Proxy with key rotation

**Features:**

- Round-robin key rotation across multiple API keys
- Automatic 429 handling with per-key cooldown
- Latency-aware model routing (`"model": "auto"`)
- Adaptive model racing with fast/fallback pools
- Gateway concurrency limits before upstream dispatch
- Per-model stats tracking (TTFC, success rate, degradation detection)
- `x-key-label` response header: tracks which key was used for rotation debugging

**Model Routing (V2):**

```toml
[routing]
strategy = "latency_aware"
spike_threshold_ms = 3000
models = [
  "deepseek-ai/deepseek-v4-pro",
  "nvidia/nemotron-3-ultra-550b-a55b",
  "deepseek-ai/deepseek-v4-flash",
  "mistralai/mistral-medium-3.5-128b",
  "z-ai/glm-5.1",
  "stepfun-ai/step-3.7-flash",
  "moonshotai/kimi-k2.6",
  "qwen/qwen3.5-397b-a17b",
  "minimaxai/minimax-m3",
  "minimaxai/minimax-m2.7",
]
```

When a request arrives with `"model": "auto"`, the proxy picks the best model from this list. Untried models (< 3 samples) get priority. Degraded models (≥3 consecutive failures or avg > spike_threshold_ms) are skipped.

**Model Racing (Speculative Execution):**

```toml
[racing]
enabled = true
models = [
  "deepseek-ai/deepseek-v4-pro",
  "nvidia/nemotron-3-ultra-550b-a55b",
  "deepseek-ai/deepseek-v4-flash",
  "mistralai/mistral-medium-3.5-128b",
  "z-ai/glm-5.1",
  "stepfun-ai/step-3.7-flash",
  "moonshotai/kimi-k2.6",
  "qwen/qwen3.5-397b-a17b",
  "minimaxai/minimax-m3",
  "minimaxai/minimax-m2.7",
]
max_parallel = 10
timeout_ms = 15000
strategy = "complete"
adaptive = true
min_parallel = 2
pressure_parallel = 6
degraded_parallel = 3
fast_models = [
  "stepfun-ai/step-3.7-flash",
  "qwen/qwen3.5-397b-a17b",
  "z-ai/glm-5.1",
  "moonshotai/kimi-k2.6",
  "minimaxai/minimax-m3",
]
fallback_models = [
  "minimaxai/minimax-m2.7",
  "deepseek-ai/deepseek-v4-flash",
  "mistralai/mistral-medium-3.5-128b",
  "deepseek-ai/deepseek-v4-pro",
  "nvidia/nemotron-3-ultra-550b-a55b",
]

[limits]
max_upstream_in_flight = 48
max_in_flight_per_key = 3

[timeouts]
min_dynamic_timeout_ms = 8000
dynamic_sample_floor = 10
```

**Per-Model NVIDIA Defaults:**

`nimaproxy` applies the build.nvidia.com inference defaults from
`[model_params."<model>"]` before sending requests upstream. `stream=false`
may be injected when omitted; `stream=true` entries are retained for catalog
fidelity, but the proxy streams only when the caller explicitly sends
`"stream": true`.

| Model | max_tokens | temperature | top_p | Extra |
| --- | ---: | ---: | ---: | --- |
| `deepseek-ai/deepseek-v4-pro` | 16384 | 1.0 | 0.95 | `chat_template_kwargs.thinking=false` |
| `nvidia/nemotron-3-ultra-550b-a55b` | 16384 | 1.0 | 0.95 | `reasoning_budget=16384`, `chat_template_kwargs.enable_thinking=true`; NVIDIA snippet streams, caller must opt in |
| `deepseek-ai/deepseek-v4-flash` | 16384 | 1.0 | 0.95 | `chat_template_kwargs.thinking=true`, `chat_template_kwargs.reasoning_effort=high` |
| `mistralai/mistral-medium-3.5-128b` | 16384 | 0.7 | 1.0 | `reasoning_effort=high` |
| `z-ai/glm-5.1` | 16384 | 1.0 | 1.0 | `seed=42`; NVIDIA snippet streams, caller must opt in |
| `stepfun-ai/step-3.7-flash` | 16384 | 1.0 | 0.95 |  |
| `moonshotai/kimi-k2.6` | 16384 | 1.0 | 1.0 |  |
| `qwen/qwen3.5-397b-a17b` | 16384 | 0.6 | 0.95 | `top_k=20`, `presence_penalty=0`, `repetition_penalty=1` |
| `minimaxai/minimax-m3` | 8192 | 1.0 | 0.95 | multimodal |
| `minimaxai/minimax-m2.7` | 8192 | 1.0 | 0.95 |  |

Fires N parallel requests to N models, returns first response. Trades N×token
budget for min(P50 latency). With `adaptive=true`, the healthy ceiling remains
`max_parallel=10`, but fanout drops to `pressure_parallel` or
`degraded_parallel` when gateway pressure rises or some keys are unavailable.
Keys and upstream slots are pre-allocated per race task, so saturated gateways
return a local 503 instead of forcing all callers into NVIDIA-side cooldowns.
Models are selected in round-robin order via `racing_cursor`, with fast models
preferred and fallback models used when capacity or health requires it.
Local latency degradation waits for three samples, while explicit
NVIDIA-degraded responses are still removed from routing immediately.

**Model Compatibility (Developer Role Transformation):**

```toml
[model_compat]
# Models that support the 'developer' role (don't need transformation)
# All models NOT in this list will have 'developer' role transformed to 'user'
supports_developer_role = []

# Models that support tool messages (don't need transformation)
# All models NOT in this list will have 'tool' role transformed to 'assistant'
supports_tool_messages = ["all"]
```

Transforms OpenAI-style `developer` and `tool` roles to `user` and
`assistant` for models that don't support them. This fixes 400 "Unknown
message role" errors when using OMP or other agents that send developer
role messages. By default, all models have roles transformed (empty lists =
transform all).

## License

MIT
