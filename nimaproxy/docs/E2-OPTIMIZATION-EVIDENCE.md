# E2 — Optimization Evidence Record (nimaproxy)

> **Paths in this document are environment variables.** This is a public repository, so the
> evidence record cites placeholders rather than one operator's host layout:
>
> ```bash
> export REPO_ROOT="$(git rev-parse --show-toplevel)"   # the nimakai checkout
> export CRATE_DIR="$REPO_ROOT/nimaproxy"                # the nimaproxy crate inside it
> export ANALYSIS_DIR=/path/to/analysis                 # dir holding the A*/P* analysis notes
> ```

**Date:** 2026-10-20
**Branch:** `chore/rust-ci` (`$REPO_ROOT`; working tree carries the uncommitted C1/C2/R/OptFix work described below)
**Status:** implementation evidence record. Verification (fmt/clippy/test) numbers are placeholders reserved for the orchestrator (§8).

## 0. Measurement basis

| Item | Value | Source |
|---|---|---|
| Snapshot | `$ANALYSIS_DIR/a1-snapshot.jsonl`, **47,536 lines** | `$ANALYSIS_DIR/A1.md:3` |
| Analysis floor | `timestamp >= 2026-10-19T22:57:57Z` (container `harden-v1` start) | `$ANALYSIS_DIR/A1.md:4`; `$ANALYSIS_DIR/A4.md:3` |
| Post-start population | 4,096 rows carrying `record_kind` — `ttfb` 1,748 / `completion` 1,675 / `loop` 673 | `$ANALYSIS_DIR/A1.md:5` |
| Volume | **~0.06 rps idle-bound** (mean in-flight ≈ 0.08 against a 48-slot admission ceiling; hourly range 0.003–0.41 rps) | `$ANALYSIS_DIR/P3.md:22-36`, `$ANALYSIS_DIR/Q-ranked.md:3-4` |
| Runtime | `#[tokio::main]` multi-thread, 8 workers; container `cpu.max = max 100000` (1.0 CPU), `nr_throttled = 0` | `$ANALYSIS_DIR/P1.md:12-21` |

Everything below is derived from the read-only lanes (`$ANALYSIS_DIR/A*.md`, `$ANALYSIS_DIR/P*.md`) plus the shipped tree. No lane file or repo file was modified by this doc.

---

## 1. C1 — sse_tap per-chunk alloc / lock / drain elimination

### Evidence numbers
- Chunk distribution over post-start completions (n=1,551 of 4,254): `chunk_count` **p50 65 / p95 530 / max 5151** (mean 166.2); `body_bytes` p50 15,617 / p95 121,818 / max 1,175,230.
- `bytes/chunk` is near-constant at **~241.6 B** across the whole distribution (p50 241.6, p95 260.9, max 302.0, mean 240.9), and `content_chars` p50 is 95 over p50 65 chunks — ~1.5 content chars per frame.
- Shape: **954/1,551 (61.5%)** of turns carry >50 chunks; 237 (15.3%) carry >200; only 2 carry ≤2.
- Source: `$ANALYSIS_DIR/A4.md:45-57`. The tap's overhead is per-chunk, so p50 pays 65× and p95 pays 530× (`$ANALYSIS_DIR/A4.md:57-59`).

### Cost sites pre-fix (verified against the shipped pre-diff tree)
| Site | Work | Scaling |
|---|---|---|
| `sse_tap.rs:105-106` | `std::mem::take(&mut *partial)` discarded the `String` capacity every chunk | realloc from scratch per chunk |
| `sse_tap.rs:112` | `buf[..pos].trim_end_matches('\r').to_string()` | 1 heap alloc + full memcpy per SSE line |
| `sse_tap.rs:113` | `buf.drain(..pos + 1)` inside the line loop | O(n²) worst case over a multi-line chunk |
| `sse_tap.rs:114-116` | `drop(partial)` … re-`lock()` inside the `while let` loop | 1 parking_lot lock/unlock pair per line |

(`$ANALYSIS_DIR/A4.md:66-73`; the pre-fix shape is visible in the removed lines of the shipped diff at `nimaproxy/src/proxy/sse_tap.rs`.)

### Shipped anchors post-fix (`nimaproxy/src/proxy/sse_tap.rs:89-137`, `observe`)
- `:100` — one `let mut partial = self.inner.partial.lock();` per chunk, held for the whole scan; the comment at `:95-99` records why that cannot deadlock (`observe_line` and everything below it only touch other fields).
- `:112` — `partial.push_str(text)` appends **in place**, so capacity is retained across chunks (`:110-111`).
- `:117-126` — cursor walk `partial[consumed..].find('\n')`; `:123` passes an `&str` slice (`partial[consumed..end].trim_end_matches('\r')`) to `observe_line`, so **no `String` per line**.
- `:128-131` — a single `partial.drain(..consumed)` per chunk replaces the per-line drain.
- `:133-136` — `MAX_PARTIAL_LINE` overflow guard preserved.
- Regression test `split_feed_matches_single_call_feed` (`sse_tap.rs` tests module): a frame cut mid-line by one chunk then completed by the next must produce token/finish/chunk counters identical to the same bytes arriving in one chunk; `chunk_count` is the only field the split is allowed to move.

### Expected saving
- Removes 65 (p50) → 530 (p95) → 5,151 (max) per-line `String` allocs + memcpys and lock pairs per turn — **~0.2 ms (p50) / ~0.8 ms (p95) CPU per turn**, scaling linearly with generation length, with **no semantic change** (`$ANALYSIS_DIR/A4.md:80-84`, `$ANALYSIS_DIR/B-ranked.md:7-12`).
- Not touched, deliberately: the per-chunk `serde_json::from_str::<SseFrame>` (`sse_tap.rs:147`) is required by the tap's contract, and the per-chunk `tokio::time::timeout` idle guard (`streaming.rs:90`) is load-bearing — 41 post-start 504s prove that path (`$ANALYSIS_DIR/A4.md:91-95`).

### Verification
- `F3` in `$ANALYSIS_DIR/P3.md:189` — C1 is complete and correct: one `partial.lock()` per chunk (was two per line), in-place append, cursor walk, `&str` line slices, one drain per chunk; the git diff shows the removed per-line `drop(partial)`/re-lock and `.to_string()`.
- `P1-5` in `$ANALYSIS_DIR/P1.md:226-232` — `sse_tap::observe` holding one `parking_lot::Mutex` across the chunk's line scan is **FINE** (already the C1 fix); remaining per-tap locks (`prompt_tokens`/`completion_tokens`/`finish_reason`) are per-request objects and cannot contend across requests.

---

## 2. C2 — window split (first-byte vs mid-stream idle)

### Evidence numbers (stall trace, post-start)
- **67** ttfb rows with `error` containing `"no bytes within"` and `status_code == 504`; live window value **25,000 ms** read from the error strings; **65** carry `"; retrying on another key"` (retry fired), 2 are terminal.
- 61 of 67 stalls are the **last** attempt of their turn; 63 turns (3.2% of 1,975) contain at least one stall attempt.
- **Salvage rate — two readings, both reported** (`$ANALYSIS_DIR/A1.md:91-99`):
  - per-attempt: **0 / 67 = 0.0%** — a stalled attempt produced no bytes, so it can never carry its own completion half;
  - **per-turn: 6 / 63 = 9.5% salvaged**, 57/63 = 90.5% terminal 504.
- Latency: median full-turn 46,236 ms (salvaged, n=6) vs 26,438 ms (terminal, n=57) vs 7,602 ms (all-clean, n=1,468); median stall ttfb-half 26,417 ms — a stalled turn costs ~3.5× a clean turn even when it succeeds.
- **Reclassification risk of a blunt cut:** 178/1,748 ttfb attempts (**10.2%**) are `success`/200 with latency in `[10,000, 25,000)` ms — 97 in 10–15 s and 81 in 15–25 s; only 50 of those sit in turns with a second attempt, so **128** would go straight to terminal 504 (`$ANALYSIS_DIR/A1.md:162-170`, `$ANALYSIS_DIR/B-ranked.md:15`).
- Source: `$ANALYSIS_DIR/A1.md:81-102`.

### The shared-window coupling
Pre-change, one `window` value fed both bounds: `chat.rs:401` built it, it bounded the TTFB wait at `chat.rs:409` **and** the mid-stream inter-chunk guard via `chat.rs:543` → `streaming.rs:80` (`$ANALYSIS_DIR/A1.md:200`, `$ANALYSIS_DIR/A1.md:206-211`, `$ANALYSIS_DIR/A4.md:22`). Consequence: **no staged reduction of the first-byte bound was possible** — moving it silently moved the inter-chunk guard too, which is why A1's bottom line ranks the decoupling above every knob change.

### Shipped knobs + accessors + independence tests
- Config: `timeouts.first_byte_timeout_ms: Option<u64>` and `timeouts.idle_timeout_ms: Option<u64>` with accessors `Config::first_byte_timeout_ms()` / `Config::idle_timeout_ms()` (`nimaproxy/src/config/mod.rs:158-163`, `:351-366`); `main.rs:225-226` wires them into `RuntimeControls`, `main.rs:281-282` into `AppState`.
- AppState: `first_byte_timeout_ms` / `idle_timeout_ms` fields (`lib.rs:88-90`), stored from `RuntimeControls` (`lib.rs:397-398`).
- Accessors (`nimaproxy/src/proxy/timeouts.rs:5-30`):
  - `first_byte_timeout_for_model(state, model)` — `Some(ms)` override, else `timeout_for_model` (per-model dynamic). Documented as the value the retry budget is sized against.
  - `idle_timeout_for_model(state, model)` — `Some(ms)` override, else the same dynamic value.
- `None`/`None` reproduces the pre-change shared value exactly, so an unconfigured deployment is byte-identical to today.
- Independence tests (`lib.rs` tests module):
  - `test_windows_unset_both_equal_dynamic_value` — both knobs unset → both windows equal `timeout_for_model`.
  - `test_first_byte_knob_leaves_idle_window_dynamic` — `Some(12000)` on first-byte moves only the TTFB window.
  - `test_idle_knob_leaves_first_byte_window_dynamic` — `Some(60000)` on idle moves only the mid-stream window.
- Membership/budget wiring: `chat.rs:92-96` builds `RetryPolicy::new(n, first_byte_timeout_for_model(...), admission_wait_ms)` — the budget stays keyed to the first-byte window, documented at `chat.rs:86-91`.

### Guardrail
- Default path behavior byte-identical (both values = the dynamic timeout when unset); budget tests pass (`$ANALYSIS_DIR/B-ranked.md:18`).
- Expected effect by design: **no latency change by itself** — it is the prerequisite unlock for the staged 25 s → 15 s reduction, where 15 s still exceeds ttfb p90 (14 s) and the reclassification risk is bounded to **81** attempts vs 178 at 10 s (`$ANALYSIS_DIR/B-ranked.md:17`, `$ANALYSIS_DIR/A1.md:198-199`).
- The 10 s window and the 60 s cap remain rejected (§7).

---

## 3. R1 — per-attempt loop-invariant hoists

Per-attempt census (what was redone on every attempt of `for attempt in 0..n`, `chat.rs:98`, `n = pool.len().clamp(1, MAX_RETRIES)`), from `$ANALYSIS_DIR/P4.md:1,25-45`:

| Work | Site (pre-change) | What it cost |
|---|---|---|
| Upstream URL rebuilt | `chat.rs:113` `format!("{}/v1/chat/completions", state.target)` | 1 String alloc + fmt per attempt; `state.target` is immutable for the process lifetime |
| `Accept` header recomputed | `chat.rs:116` `accept_header_for_body(&body)` → `chat_helpers.rs:111-115` | **a full `serde_json::from_slice::<Value>` of the entire request body per attempt**, only to read the `stream` bool |
| Turn summary re-parsed (fallback loop) | `fallback.rs:77` `request_turn_summary(&req_body)` | one full JSON parse per key attempt (correctly hoisted in `chat.rs:71` since before) |
| Non-streaming body concat twice | `chat.rs:680,682` `collected.concat()` | **two full-body `Vec<u8>` allocations + two memcpys** of the whole response body |

Cached across attempts already (unchanged): `original_body` (`chat.rs:40`, a `Bytes` refcount bump), `original_model`, the resolved `model_id`/rewritten `body` (`chat.rs:66`), `request_turn_summary` (`chat.rs:71`), `RetryPolicy` (`chat.rs:92-96`), `started`.

### Shipped sites
- `chat.rs:103-109` — comment plus hoist: `let upstream_url = format!("{}/v1/chat/completions", state.target);` and `let mut accept_header = accept_header_for_body(&body);` **before** the loop; the loop body now sends `.post(upstream_url.as_str())` and `.header("Accept", accept_header)`.
- DEGRADED recompute — `chat.rs:346-352`: the auto-routing path is the one place `body` is reassigned (`chat.rs:325-333`); the Accept header is recomputed immediately after the reassignment, **before** the `mistral_validation_error` early-return and before the `continue`, so the value dominates both paths (the Q-ranked note at `$ANALYSIS_DIR/Q-ranked.md:14-16` flagged exactly this ordering trap).
- `fallback.rs:64-73` — `request_turn_summary(&req_body)`, `upstream_url`, and `accept_header` all hoisted before the key loop (`req_body` is never reassigned inside it); loop body uses `.post(upstream_url.as_str())` and `.header("Accept", accept_header)`.
- `chat.rs:709-712` — the non-streaming body is bound once: `let assembled = collected.concat(); usage_tokens = non_streaming_usage(&assembled); Body::from(assembled)` — one alloc + one memcpy instead of two.

### Behavior-preserving argument
`state.target` is an immutable field of `Arc<AppState>` never mutated after construction, so the formatted URL is byte-identical on every iteration; `accept_header_for_body` is a pure function of `body` alone, `body` is invariant across retries (only `Bytes`-cloned per attempt), and its only reassignment in the whole handler is the DEGRADED path, which now recomputes the header. `collected.concat()` is deterministic. Hence the emitted HTTP request and response body are unchanged (`$ANALYSIS_DIR/P4.md:102-111`, `$ANALYSIS_DIR/Q-ranked.md:21`).

---

## 4. R2 — membership test without a per-request `Vec` alloc

### Per-request cost (pre-change)
`validate_model_exists` (`messages.rs:39-56`) runs on **every chat request** (`chat.rs:52`) and called `state.configured_models()` (`lib.rs:404-407`), which locked `available_models`, cloned every model `String` into a fresh `Vec` (~2 KB, O(n) models) and then linearly scanned it — the catalog *contents* are cached, the *membership test* was not (`$ANALYSIS_DIR/P4.md:64`, `$ANALYSIS_DIR/P4.md:117`).

### Shipped change
- `lib.rs:407-425` — new `AppState::is_model_configured(&self, model: &str) -> bool`: loads the `available_models` snapshot once and tests membership **in place** across router models, then racing models, then available models, in the same order `configured_models` would have built. The doc comment records that `push_unique`'s duplicate filtering is irrelevant for a boolean answer and is deliberately not replicated.
- `messages.rs:42-52` — `validate_model_exists` rewritten to call `is_model_configured` and to keep the passthrough contract: an empty configured list accepts any model (unchanged).
- Tests: the `validate_model_membership_tests` module at `messages.rs:695-756` covers membership true/false and passthrough behavior.

### Argument
Same accept/reject outcomes as before for every input; the only difference is that no `Vec<String>` of the catalog is materialized. Passthrough (empty → accept any) preserved (`$ANALYSIS_DIR/Q-ranked.md:30`).

---

## 5. R3 — arc-swap adoption, NARROWED

**Scope correction, recorded deliberately:** lane P5 §9 (`$ANALYSIS_DIR/P5.md:145-166`) and Q's R3 first listed **three** conversion sites (`lib.rs:298` `racing_wins`, `lib.rs:405` `available_models`, `http.rs:208` `pruned_models`). The shipped change converts **two**.

### Shipped
- Dependency + lockfile version: `arc-swap = "1.9"` in `nimaproxy/Cargo.toml` (`@@ -32,6 +32,7 @@`), resolved in `nimaproxy/Cargo.lock` (the 10-line lock diff).
- Converted fields:
  - `AppState::available_models: ArcSwap<Vec<String>>` — `lib.rs:76` declaration, `ArcSwap::from_pointee(available_models)` at `lib.rs:388`, read lock-free via `.load()` in `configured_models` (`lib.rs:406`).
  - `AppState::pruned_models: ArcSwap<Vec<String>>` — `lib.rs:77` declaration, `ArcSwap::from_pointee(Vec::new())` at `lib.rs:389`, published build-then-store at `main.rs:281` (`state.pruned_models.store(Arc::new(pruned_models))` replacing the `Mutex` guard write), read at `http.rs:208` (`(*state.pruned_models.load_full()).clone()`).

### `racing_wins` — REVERTED, Mutex kept
`racing_wins: Mutex<HashMap<String, u64>>` (`lib.rs:163`) was **not** converted. It is an increment-per-win counter: `record_win` takes the lock and mutates in place (`lib.rs:267`), and `/stats` reads it (`lib.rs:299`). Converting it to `ArcSwap` would require read-modify-write under a separate lock anyway, or a build-then-store that **loses increments** whenever two wins race. The Mutex is therefore the correct primitive here, and the shipped test `test_available_models_swap_reads_are_atomic` (`lib.rs:1170-1198`) documents the boundary in its own comment: *"Races on `racing_wins` stay behind a Mutex precisely so increments are never lost; only the write-rare model lists use ArcSwap."* The test itself asserts readers of `available_models` never observe a torn list (model-a always present; model-b only ever alongside model-a) across 200 concurrent stores.

### MSRV fit
`arc-swap` 1.9.3 declares MSRV 1.60; the crate is edition 2021 (`$ANALYSIS_DIR/P5.md:4,147`), so the dependency fits without a toolchain change (`$ANALYSIS_DIR/Q-ranked.md:37`).

---

## 6. OptFixIdle — the idle-charge defect and its fix

### Defect
1. **Buffered stall charged against the first-byte window.** The mid-stream body-stall branch (non-streaming assembly over the tapped stream, `chat.rs:600-700`) called `record_model_timeout(..., request_timeout_ms)` and `can_retry(attempt, n, started, request_timeout_ms, budget)`, and reported `"upstream body stalled after {request_timeout_ms}ms"`. The failure was actually gated by the **per-chunk idle bound**, which after C2 is a *different* number from the TTFB window — so the recorded timeout, the retry decision, and the client-visible 504 all described the wrong window.
2. **`n == 1` dead end.** The loop bound was `n = pool.len().clamp(1, MAX_RETRIES)`, so a single-key pool ran `for attempt in 0..1` — one attempt, no retry at all — even though a stall is retryable whenever nothing has reached the client (`/tmp/nimakai/nimaproxy/src/proxy/chat.rs:109-113` shipped comment).

### Fix
- `chat.rs:600-700` (stall branch): charges `idle_window.as_millis()` into `record_model_timeout`, decides with the new `can_retry_with_window(attempt, attempts, started, idle_window, budget)`, and formats both the log `error` and the 504 body as `idle_window` milliseconds. The comment at `:641-646` records why: the failure was gated by the per-chunk gap bound, so that is the value the retry decision and the reported 504 must describe.
- `chat.rs:109-113` + loop bound: `let attempts = n.max(2);` and `for attempt in 0..attempts` — a single-key pool still gets the one retry the budget admits; multi-key pools are unaffected because `n` already exceeds 1 there, and the in-loop `can_retry` still caps the total client wait. The two `can_retry` call sites now pass `attempts` (not `n`) so the decision and the loop agree on whether attempt 1 exists (`chat.rs:470`, `chat.rs:648`).
- New shared-decision APIs, so both call sites use one implementation:
  - `request_state.rs:119-135` — `can_retry_with_window(attempt, n, started, window, budget)`; `can_retry` (`:143-149`) delegates to it.
  - `retry.rs:73-97` — `decide_for_window(started, attempt, window)`; `decide` (`:110-114`) delegates with `self.window`.

### Tests
- `retry.rs` tests: `test_decide_for_window_lets_a_smaller_window_retry` — the policy window and a shorter caller-supplied window can disagree; a buffered body-stall charged against the shorter idle window must be able to retry exactly where the policy window refuses.
- `request_state.rs` tests: `test_can_retry_with_window_lets_a_smaller_window_retry` — the same disagreement one level down.
- `tests/proxy_error_paths.rs:849-856` — new doc'd case: a timed-out direct request returns 504 within one budget window, and because the stall is retryable even on a single-key pool the model records **two** attempts before the terminal 504. The pre-existing assertion `assert_eq!(slow.total, 1)` at `proxy_error_paths.rs:908` was updated to `2` — the test now pins the `attempts = n.max(2)` behavior rather than the dead end.
- `chat_helpers.rs` tests: the hoisted-Accept contract test covering the DEGRADED recompute path.

---

## 7. Rejections

Every candidate that was evaluated and **not** shipped, with the one-line reason and its lane citation.

| # | Candidate | Reason (one line) | Citation |
|---|---|---|---|
| 1 | 10 s first-byte window | Reclassifies 178 currently-successful attempts (10.2%), 128 with no retry available, and shrinks the retry budget 52,000 → 22,000 ms, likely zeroing the 9.5% salvage rate | `$ANALYSIS_DIR/A1.md:162-175`, `$ANALYSIS_DIR/B-ranked.md:22` |
| 2 | 60 s per-turn total cap | p95 unaffected (56,794 < 60,000); would abort 93 in-flight successful generations for a p99/max-only gain | `$ANALYSIS_DIR/A1.md:188-190`, `$ANALYSIS_DIR/B-ranked.md:23` |
| 3 | Re-enable racing | Structurally unreachable: `racing_models.len() == 1` fails the `>= 2` gate at `chat.rs:56-61`/`lib.rs:405`, 0/3,857 post-start rows `is_racing`, live counters `racing_requests: 0` | `$ANALYSIS_DIR/A5.md:1,47,57`, `$ANALYSIS_DIR/B-ranked.md:25` |
| 4 | Heavy-prompt routing | TTFB is flat across prompt-size quartiles (p50 1,363→2,589 ms, p95 10,938–17,506 with no monotone trend); the 504 class is a key-wait stall, not a prompt-size effect | `$ANALYSIS_DIR/A5.md:80-88` |
| 5 | Client-side work | Median client add-on ≤ 269 ms (1.01–1.20×); the tail is the proxy's own retry chains (per-request ratio p95 13.31×, max 75.45×) | `$ANALYSIS_DIR/A6.md:29,49,83`, `$ANALYSIS_DIR/B-ranked.md:27` |
| 6 | rayon | No batch compute exists on any hot path — an I/O-bound streaming proxy; `par_iter` cannot await, so it cannot express the admission poll loop at all | `$ANALYSIS_DIR/P5.md:28-40` |
| 7 | async-lock (migrate off parking_lot) | Zero `.lock()`-then-`.await` sites in the entire crate; async-lock's own docs concede it is slower uncontended, and migration would make `SseTapInner` non-`Sync`-friendly across the stream boundary | `$ANALYSIS_DIR/P5.md:73-83` |
| 8 | backon / tokio-retry | The retry policy is a measured total wall-clock budget (`elapsed + window <= budget`, `BUDGET_SLACK_MS = 500` from an observed 16,546 ms attempt), not a backoff schedule; backon would insert sleeps and reintroduce the defect `retry.rs` exists to prevent | `$ANALYSIS_DIR/P5.md:85-95` |
| 9 | governor / tower rate-limit | The admission gate is "wait for a resource, then proceed" with a two-tier 503-overload vs 429-no-keys distinction, which GCRA cannot express; AIMD is multiplicative-decrease-on-event, not a fixed quota | `$ANALYSIS_DIR/P5.md:98-110` |
| 10 | singleflight / request coalescing | There is no duplicate fan-out to collapse: the retry loop is strictly sequential and racing sends *different* models deliberately; coalescing would change response semantics (per-key stats, streaming ownership, `x-key-label`) | `$ANALYSIS_DIR/P5.md:113-120`, `$ANALYSIS_DIR/P4.md:89-96` |
| 11 | moka for the model catalog | Catalog freshness requires exactness at startup (`prune_missing_models` prunes ids absent upstream); TinyLFU eventual eviction can answer "model absent" for a live model | `$ANALYSIS_DIR/P5.md:128` |
| 12 | smallvec / smallstr | Post-C1 the tap allocates zero per line; `smallvec` would target per-line temporary `Vec`s that no longer exist and `smallstr` targets `finish_reason`, written once per stream | `$ANALYSIS_DIR/P5.md:170-178` |
| 13 | dashmap (shard model_stats) | No lock is held across `.await` and the maps hold one entry per model; sharding only adds hashing. Utilization ≈ 4.5 × 10⁻⁹ at measured volume | `$ANALYSIS_DIR/P5.md:186`, `$ANALYSIS_DIR/P3.md:174-179` |
| 14 | turn_log blocking-flush fix (spawn_blocking / channel) | Real pattern hazard but ~120 blocking syscalls/hour at 0.06 rps; the fix wants a design plus rotation-semantics tests, so LATER not SHIP — P1 says SHIP, P3 and Q say LATER and this record sides with LATER | `$ANALYSIS_DIR/P1.md:211-225`, `$ANALYSIS_DIR/P3.md:190`, `$ANALYSIS_DIR/Q-ranked.md:42-45` |
| 15 | Admission poll → `Notify`/waker | 25 ms poll iterations are uncontended (a yield, not a worker burn); rewrite risk exceeds gain at current volume — revisit above 10 rps sustained | `$ANALYSIS_DIR/P1.md:199-209`, `$ANALYSIS_DIR/Q-ranked.md:47-48` |
| 16 | Quota raise (`max_upstream_in_flight`) | The 1.0-CPU cgroup quota is the actual binding constraint (`nr_throttled = 0` today); raising permits without raising the quota converts overload rejects into CPU starvation — a feedback loop | `$ANALYSIS_DIR/P1.md:170-181`, `$ANALYSIS_DIR/Q-ranked.md:51` |
| 17 | Timeout memoization (moka/OnceCell per model per window) | Converts a live-stat read into a request-start snapshot — a real semantic change: a timeout read mid-request should be able to differ from one at request start. Needs a TTL experiment first | `$ANALYSIS_DIR/P4.md:82,113,131`, `$ANALYSIS_DIR/Q-ranked.md:45` |

---

## 8. Verification record

Measured by the orchestrator at phase end, 2026-10-20, on branch
`chore/rust-ci` with the full C1/C2/R1-R3/OptFixIdle tree. All commands run
from `$CRATE_DIR`.

| Gate | Command | Result |
|---|---|---|
| fmt | `cargo fmt --check` | clean (exit 0) |
| clippy | `cargo clippy --all-targets` | 0 errors, 0 warnings |
| lib suite | `cargo test --lib` | 408 passed, 0 failed |
| error paths | `cargo test --test proxy_error_paths` | 47 passed, 0 failed |
| integration | `cargo test --test integration` | 45 passed, 0 failed |

New-test roll call (each `cargo test --lib <filter>`, all green):
`split_feed_matches_single_call_feed` (sse_tap);
`test_windows_unset_both_equal_dynamic_value`,
`test_first_byte_knob_leaves_idle_window_dynamic`,
`test_idle_knob_leaves_first_byte_window_dynamic` (lib.rs window
independence); `test_available_models_swap_reads_are_atomic` (lib.rs ArcSwap,
1/1); `validate_model_membership_tests` module (messages.rs, 3/3);
`test_decide_for_window_lets_a_smaller_window_retry` (retry.rs, 1/1);
`test_can_retry_with_window_lets_a_smaller_window_retry` (request_state.rs,
1/1); hoisted-Accept contract test (chat_helpers.rs, 1/1);
`test_buffered_stall_retries_on_idle_window_not_first_byte_window` (1/1);
`test_single_key_pool_stall_permits_attempt_zero_and_refuses_attempt_one`
(1/1); `test_streaming_committed_stall_refuses_retry` (1/1);
`test_retry_loop_bound_matches_decision_bound` (1/1). Updated:
`test_direct_chat_request_uses_configured_timeout` now expects `slow.total ==
2` (the single-key retry fires before the terminal 504).

---

*Evidence sources (read-only): `$ANALYSIS_DIR/A1.md`, `A4.md`, `A5.md`, `A6.md`, `B-ranked.md`, `$ANALYSIS_DIR/P1.md`–`P5.md`, `Q-ranked.md`, and the shipped tree at `$CRATE_DIR`. No lane or repo file was modified while writing this document.*
