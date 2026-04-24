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
- 313+ tests with ~92% coverage
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
  "nvidia/meta/llama-3.3-70b-instruct",
  "nvidia/qwen/qwen2.5-coder-32b-instruct",
]

[racing]
enabled = true
models = ["z-ai/glm4.7", "qwen/qwen3.5-397b-a17b"]
max_parallel = 3
timeout_ms = 8000
strategy = "complete"
```

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

```
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
cargo test --lib          # Library tests (224)
cargo test --test integration  # Integration tests (45)
cargo test --test proxy_error_paths  # Error paths (19)
cargo test --test coverage_gaps  # Coverage gaps (14)

# Run with coverage
cargo tarpaulin --out Html
```

## Recent Changes (v0.13.1)

### Fixed
- **Assistant message validation**: Messages with `tool_calls` must NOT have `content` field (NVIDIA NIM requirement)
- **Unexpected role 'user' after role 'tool'**: Insert assistant message between tool→user transitions (fixes OMP/Pawan integration)
- `sanitize_tool_calls()` sets `content` to `null` (not empty string) when `tool_calls` present

## Recent Changes (v0.13.0)

### Fixed
- **tool_call_id forwarding**: Strips `tool_call_id` from assistant messages to prevent Pydantic errors
- **DEGRADED model handling**: Auto-retries with different model when NVIDIA marks model as degraded
- **Live test robustness**: Graceful handling of 429/502/503 errors

### Added
- 17 new tests for coverage gaps and error paths
- Improved circuit breaker and degradation tracking
- Enhanced connection error handling (BAD_GATEWAY)

See [CHANGELOG.md](CHANGELOG.md) for full details.

## License

MIT License - see [LICENSE](LICENSE) for details.

## Contributing

1. Fork the repository
2. Create a feature branch
3. Add tests for new functionality
4. Ensure all tests pass: `cargo test`
5. Submit a pull request

## Related

- [nimakai](../) - NVIDIA NIM latency benchmarker
- [aegis](https://github.com/dirmacs/aegis) - Config manager for NIM models
```
