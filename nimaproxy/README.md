# nimaproxy

**NVIDIA NIM Proxy** — Production-ready key rotation, latency-aware routing, and racing mode for NVIDIA NIM API.

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Crates.io](https://img.shields.io/crates/v/nimaproxy.svg)](https://crates.io/crates/nimaproxy)
[![Tests](https://github.com/dirmacs/nimakai/actions/workflows/ci.yml/badge.svg)](https://github.com/dirmacs/nimakai/actions)

## Features

### 🔑 Key Rotation

- Automatic API key rotation on rate limits (429)
- Configurable cooldown periods
- Per-key failure tracking

### 🎯 Latency-Aware Routing

- Real-time model selection based on P95 latency
- Circuit breaker for degraded models
- Round-robin and latency-aware strategies

### 🏎️ Racing Mode (Speculative Execution)

- Fire N parallel requests, return first response
- Trade token budget for minimum P50 latency
- Adaptive fast/fallback pools, pressure-aware parallelism, and solo fallback
- Sequential fallback across unused candidates on transient solo/race failures
- `nimaproxy/auto` alias for provider-prefixed client configs
- Configurable timeout, global concurrency, per-key concurrency, and admission wait
- Dynamic per-key windows shrink on 429s and reopen after successful requests

### 🛡️ Production Ready

- Unit, integration, live, and stress test suites
- Graceful error handling and retry logic
- Comprehensive metrics and health checks

## Quick Start

### Installation

```bash
cargo install nimaproxy
```

Or build from source:

```bash
git clone https://github.com/dirmacs/nimakai.git
cd nimakai/nimaproxy
cargo build --release
```

### Configuration

Create `nimaproxy.toml`:

```toml
[[keys]]
key = "nvapi-YOUR-API-KEY"
label = "primary"

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

### Per-Model NVIDIA Defaults

`[model_params."<model>"]` mirrors the build.nvidia.com inference snippets for
the configured pool. The proxy sends hyperparameter defaults upstream for
direct, auto-routed, and racing requests. `stream=false` may be injected when
omitted; `stream=true` entries are retained for catalog fidelity, but the proxy
streams only when the caller explicitly sends `"stream": true`.

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

### Usage

```bash
# Start proxy
nimaproxy --config nimaproxy.toml --port 8080

# Or with environment
NIMAPROXY_CONFIG=nimaproxy.toml nimaproxy
```

## API

### Chat Completions

```bash
curl http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "auto",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

### Health Check

```bash
curl http://localhost:8080/health
```

### Model Stats

```bash
curl http://localhost:8080/stats
```

## Architecture

```text
Client → nimaproxy → NVIDIA NIM API
         ├─ Key rotation
         ├─ Latency routing
         ├─ Adaptive racing mode
         ├─ Gateway concurrency limits
         └─ Circuit breaker
```

## Testing

```bash
# Run all tests
cargo test

# Run specific test suite
cargo test --lib          # Library tests
cargo test --test integration  # Integration tests (45)
cargo test --test proxy_error_paths  # Error paths
cargo test --test coverage_gaps  # Coverage gaps (14)
cargo test --test e2e_live  # E2E live (14)

# Run with coverage
cargo tarpaulin --out Html

# Live API tests (racing suites)
cargo test --test live_chat         # Live chat (5)
cargo test --test live_key_rotation # Live key rotation (2)
cargo test --test live_conversation # Live conversation (2)
cargo test --test live_routing      # Live routing (2)
cargo test --test live_streaming    # Live streaming (2)
cargo test --test live_circuit_breaker # Live circuit breaker (2)
cargo test --test live_tool_calls   # Live tool calls (7)
                                     # Total live tests: 22

# Live stress test against a running proxy (defaults to 25 turns)
NIMAPROXY_STRESS_TURNS=2 cargo test --test stress_test -- --nocapture
```

## Recent Changes (v0.15.4)

### Added

- Dynamic per-key AIMD windows: 429s immediately shrink a key's usable concurrency; successful requests gradually reopen it.
- Bounded admission wait before returning local overload/no-key responses.
- Solo fallback when racing has fewer than two viable models/key slots, plus large-prompt fanout caps.
- Sequential fallback through the ordered model pool after transient 5xx/timeouts in solo mode or exhausted races.
- `nimaproxy/auto` is accepted as an alias for `auto`.
- `/health` and `/stats` expose dynamic key window capacity, available key permits, configured per-key ceilings, admission wait, and solo/large-prompt racing controls.
- Config-driven turn logging with a safe `OnceLock` logger.

### Changed

- Active routing/racing examples now use the eight-model uptime pool and keep Mistral Medium 3.5 / DeepSeek Pro out of the active race after observed hard/schema failures.
- Production-oriented defaults now use `max_parallel=3`, pressure/degraded fanout of `2`, `max_upstream_in_flight=8`, `max_in_flight_per_key=2`, `admission_wait_ms=5000`, and a 15s dynamic timeout floor.

### Fixed

- Assistant messages with no usable `tool_calls` are normalized to `content=""`; assistant messages with real tool calls keep `content=null`.
- Deterministic 400 assistant/schema errors are recorded as hard model degradation instead of being treated like ordinary latency noise.
- Sequential solo/fallback wins are included in `gateway.racing_wins` telemetry.
- Turn logging is initialized from `[logging]` instead of a hard-coded path.

## Previous Changes (v0.15.3)

### Added

- Adaptive racing controls for healthy, pressure, and degraded fanout levels.
- Fast/fallback racing pools for tiered candidate selection.
- Gateway concurrency limits and `/stats` telemetry for request mix, in-flight counts, fanout average, rejects, timeouts, 429s, and racing wins.
- `NIMAPROXY_STRESS_TURNS` for smaller live stress triage runs without editing the stress test.

### Fixed

- Gateway overload is rejected locally with 503 before upstream dispatch.
- Successful races abort losing upstream tasks after the first 2xx response.
- Dynamic timeout learning keeps a configured warm-up floor before enough latency samples exist.
- Local latency degradation waits for three samples so a single slow successful call does not sideline a model.

## Previous Changes (v0.15.2)

### Fixed

- **Direct request timeout**: Non-racing requests now honor the configured dynamic upstream timeout.
- **Dynamic timeout warm-up**: New or failure-only models keep the configured max timeout until enough latency history exists.
- **Racing capacity**: Degraded latency/failure candidates backfill races only when healthy capacity is insufficient.
- **Racing 429 handling**: Losing 429 racers no longer globally cool API keys when another model wins; all-key/all-race 429 cases return 429.
- **Stream semantics**: Catalog `stream=true` values no longer force JSON callers into SSE mode.

### Added

- Build.nvidia.com per-model defaults for the catalog-default nimaproxy pool.

See [CHANGELOG.md](CHANGELOG.md) for full history.

## License

MIT License - see [LICENSE](../LICENSE) for details.

## Contributing

1. Fork the repository
2. Create a feature branch
3. Add tests for new functionality
4. Ensure all tests pass: `cargo test`
5. Submit a pull request

## Related

- [nimakai](../) - NVIDIA NIM latency benchmarker
- [aegis](https://github.com/dirmacs/aegis) - Config manager for NIM models

```text
```
