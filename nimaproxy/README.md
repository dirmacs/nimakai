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
- Configurable timeout and parallelism

### 🛡️ Production Ready

- 364+ tests with ~92% coverage
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
[keys]
[[keys.entries]]
key = "nvapi-YOUR-API-KEY"
label = "primary"

[routing]
strategy = "latency_aware"
spike_threshold_ms = 3000
models = [
  "deepseek-ai/deepseek-v4-pro",
  "deepseek-ai/deepseek-v4-flash",
  "mistralai/mistral-medium-3.5-128b",
  "z-ai/glm-5.1",
  "stepfun-ai/step-3.7-flash",
  "moonshotai/kimi-k2.6",
  "qwen/qwen3.5-397b-a17b",
  "minimaxai/minimax-m2.7",
]

[racing]
enabled = true
models = [
  "deepseek-ai/deepseek-v4-pro",
  "deepseek-ai/deepseek-v4-flash",
  "mistralai/mistral-medium-3.5-128b",
  "z-ai/glm-5.1",
  "stepfun-ai/step-3.7-flash",
  "moonshotai/kimi-k2.6",
  "qwen/qwen3.5-397b-a17b",
  "minimaxai/minimax-m2.7",
]
max_parallel = 8
timeout_ms = 15000
strategy = "complete"
```

### Per-Model NVIDIA Defaults

`[model_params."<model>"]` mirrors the build.nvidia.com inference snippets for
the configured pool. The proxy sends these defaults upstream for direct,
auto-routed, and racing requests. `stream` is only a default when the caller
omits it.

| Model | max_tokens | temperature | top_p | Extra |
| --- | ---: | ---: | ---: | --- |
| `deepseek-ai/deepseek-v4-pro` | 16384 | 1.0 | 0.95 | `chat_template_kwargs.thinking=false` |
| `deepseek-ai/deepseek-v4-flash` | 16384 | 1.0 | 0.95 | `chat_template_kwargs.thinking=true`, `chat_template_kwargs.reasoning_effort=high` |
| `mistralai/mistral-medium-3.5-128b` | 16384 | 0.7 | 1.0 | `reasoning_effort=high` |
| `z-ai/glm-5.1` | 16384 | 1.0 | 1.0 | `seed=42`, `stream=true` default |
| `stepfun-ai/step-3.7-flash` | 16384 | 1.0 | 0.95 |  |
| `moonshotai/kimi-k2.6` | 16384 | 1.0 | 1.0 |  |
| `qwen/qwen3.5-397b-a17b` | 16384 | 0.6 | 0.95 | `top_k=20`, `presence_penalty=0`, `repetition_penalty=1` |
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
         ├─ Racing mode
         └─ Circuit breaker
```

## Testing

```bash
# Run all tests
cargo test

# Run specific test suite
cargo test --lib          # Library tests (246)
cargo test --test integration  # Integration tests (45)
cargo test --test proxy_error_paths  # Error paths (22)
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
                                     # Total live tests: 24
```

## Recent Changes (v0.13.7)

### Fixed

- **Racing 4xx/5xx**: Non-2xx responses skipped in racing; only first 2xx wins
- **Racing 429 key-marking**: 429 correctly marks originating key rate-limited
- **400 retry**: `resolve_model` retries on "Invalid assistant message" 400
- **Tool schema sanitization**: Null `description`/`parameters` → valid defaults; prevents NVIDIA Jinja `tool_use:98` 500
- **Error body logging**: 4xx/5xx bodies now logged to journal for debuggability

### Added

- `GET /models` route alias (OMP polls without `/v1/` prefix)

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
