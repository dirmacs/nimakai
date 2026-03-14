# Nimakai

NVIDIA NIM model latency benchmarker. Single-binary, written in Nim. v0.9.1. 46-model catalog with tier labels, stability scoring, and oh-my-opencode routing recommendations.

## Build & Test

```bash
cd /opt/nimakai
nimble build                       # → ./nimakai binary
nimble test                        # runs 15 test suites

# One-liner rebuild and run
nimble build && ./nimakai
```

## Architecture

Single Nim binary with modules in `src/`:

```
src/
  nimakai.nim   — Entry point, CLI dispatch (default/watch/check/discover/sync)
  metrics.nim   — Latency ring buffer, percentiles (P50/P95/P99), jitter, stability score
  catalog.nim   — 46-model catalog with tier labels (S+/S/A+/A/A-/B+/B/C)
  ping.nim      — HTTP ping to NIM endpoint, response time measurement
  display.nim   — Terminal UI: live metrics table, health state colors
  config.nim    — Config loading from nim.cfg / CLI flags, named benchmark profiles
  recommend.nim — oh-my-opencode model routing recommendation engine
  discovery.nim — Live model discovery vs. catalog diff (discover subcommand)
  history.nim   — Latency history storage and trend display
```

## Key Rules

- **Nim 2.0+ required** — uses `resp.code.int` not `parseInt($resp.code)` for HTTP status (fixed in 0.9.1)
- **SSL flag required** — build with `-d:ssl`; NIM endpoints are HTTPS
- **Release build uses size optimization** — `--opt:size` in the build task; keep binary small
- **`malebolgia` for parallelism** — used for concurrent model pinging; don't swap it out
- **46-model catalog is hardcoded in `catalog.nim`** — update there when new NIM models launch

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

## Git Author

```bash
git -c user.name="bkataru" -c user.email="baalateja.k@gmail.com" commit
```
