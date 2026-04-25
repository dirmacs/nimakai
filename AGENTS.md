# Nimakai — Agent Context

nimakai (నిమ్మకాయి, "lemon" in Telugu) is a NIM latency benchmarker written in Nim. Single binary, v0.13.0. Provides real-time stability scoring and routing recommendations for the dirmacs oh-my-opencode setup.

**Also includes:** nimaproxy — Rust key-rotation proxy for production use (in `nimaproxy/` subdirectory). v0.13.1 includes critical fixes for NVIDIA NIM assistant message validation and OMP/Pawan integration.

## FFI Integration (v0.13.0)

nimakai v0.13.0 embeds nimaproxy via FFI. The Nim CLI can start/stop/query the proxy directly:

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

```
src/
  nimakai.nim   — CLI entry: parse args, dispatch to subcommands
  ping.nim      — HTTP ping: timed GET to NIM health endpoint, parse resp.code.int
  metrics.nim   — Ring buffer (last 100 samples), P50/P95/P99, jitter (stddev),
                  stability score 0–100 = composite of P95 + jitter + spike rate + uptime
  catalog.nim   — 80-model catalog: model IDs, context windows
  display.nim   — ncurses-style terminal table: live refresh, ANSI colors per health state
  config.nim    — Load nim.cfg, parse --profile flag, profile variable overrides
  recommend.nim — Score-based recommendation: given task type → best available model
  discovery.nim — discoverModels() via NVIDIA API, diffCatalog() vs hardcoded catalog
  history.nim   — Persist latency samples to disk, read/display trends with --days flag

tests/          — 15 test files, one per module (test_metrics.nim, test_catalog.nim, etc.)
```

### nimaproxy (Rust)

```
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
    integration.rs         18 integration tests
    e2e_live.rs            6 E2E tests with real NVIDIA API (z-ai/glm4.7 model)
    stress_test.rs         25-turn live stress test (key rotation + racing validation)

## Racing (Speculative Execution)

V3 feature: fires N parallel requests to N models, returns first response.
Trades N×token budget for min(P50 latency).

```toml
[racing]
enabled = true
models = ["z-ai/glm4.7", "qwen/qwen3.5-397b-a17b", "mistralai/devstral-2-123b-instruct-2512"]
max_parallel = 3
timeout_ms = 8000
strategy = "complete"
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
  "nvidia/meta/llama-3.3-70b-instruct",
  "nvidia/qwen/qwen2.5-coder-32b-instruct",
  "nvidia/moonshotai/kimi-k2-instruct",
  "nvidia/mistralai/devstral-2-123b-instruct-2512",
]
```

Available racing models: z-ai/glm4.7, qwen/qwen3.5-397b-a17b, mistralai/devstral-2-123b-instruct-2512, moonshotai/kimi-k2-instruct, minimaxai/minimax-m2.7

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
**Hardcoded catalog, not fetched** — NIM API doesn't expose capbility metadata; catalog is curated manually
- **`malebolgia` for parallel pinging** — concurrent HTTP without full async overhead

## Integration with oh-my-opencode

Nimkai's `recommend` subcommand outputs JSON consumed by aegis-opencode for routing config generation:

```bash
./nimakai recommend --task coding --format json
# → {"primary": "nvidia/devstral-2-123b", "fallback": "stepfun-ai/step-3.5-flash"}
```

## nimaproxy v0.13.1 Critical Fixes

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

- `NVIDIA_API_KEY` — required for NIM endpoint access; can also be set in `nim.cfg`
- `RUST_LOG` equivalent: `nimakai --verbose` flag
- Config file: `nim.cfg` in cwd, or `~/.config/nimakai/nim.cfg`
