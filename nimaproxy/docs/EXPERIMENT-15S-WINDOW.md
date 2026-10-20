# EXPERIMENT 15S — first-byte window 25 s → 15 s (pre-registered protocol)

**Status:** pre-registered, not yet run. Nothing in this document is a result; every number is a
baseline or a threshold set *before* the measurement.

**Evidence basis (read-only lanes, frozen 2026-10-20):**

| Lane | File | What it measured |
|---|---|---|
| A1 | `/tmp/opt-lane/A1.md` | TTFB/stall analysis on the post-start slice (4096 rows, 22:57:57Z Oct-19 → 09:40Z Oct-20) |
| A6 | `/tmp/opt-lane/A6.md` | Client TTFT vs proxy TTFB, per-hour, per-request matched ratios |
| B | `/tmp/opt-lane/B-ranked.md` | Evidence-synthesized ranked list; C2 names this experiment as the unlock |

**Code basis (verified):** `/opt/nimakai/nimaproxy/src/proxy/retry.rs` (budget formula),
`/opt/nimakai/nimaproxy/src/proxy/timeouts.rs` (per-model window resolution),
`/opt/nimakai/nimaproxy/src/lib.rs` (knob plumbing), `/opt/nimakai/nimaproxy/src/config/mod.rs`
(TOML parsing). A1 §5 carries the full knob table with `file:line` locators.

**Why 15 s and not 10 s:** A1's per-knob verdict rejects 10 s — it would reclassify 178 of 1748
ttfb attempts (10.2%) that currently succeed in [10 s, 25 s), 128 of which are single-attempt turns
with no retry available, and it contracts the retry budget 52000 → 22000 ms, likely zeroing the 9.5%
salvage rate. 15 s still exceeds ttfb p90 (13999 ms) by 7% and bounds the reclassification risk to
81 attempts instead of 178. A1 per-knob verdict, "stall window = 10 s specifically: NEEDS-MORE-DATA".

---

## 1. Config change

### 1.1 The single TOML addition

Under the existing `[timeouts]` section of the live config:

```toml
[timeouts]
first_byte_timeout_ms = 15000
```

`first_byte_timeout_ms` already exists as an optional key — `TimeoutsConfig` in
`/opt/nimakai/nimaproxy/src/config/mod.rs:161` declares
`pub first_byte_timeout_ms: Option<u64>`, parsed at `config/mod.rs:357-359`, carried into
`RuntimeControls` at `lib.rs:117` and `AppState` at `lib.rs:89`, and resolved per request by
`first_byte_timeout_for_model()` at `timeouts.rs:10-15`. So this experiment is **one config line and
a restart; no code change**.

### 1.2 What stays

* **`idle_timeout_ms` stays unset.** The mid-stream inter-chunk guard then keeps resolving to the
  per-model dynamic window — the old shared value — exactly as today. `idle_timeout_for_model()` at
  `timeouts.rs:22-27` returns `timeout_for_model()` when the knob is `None`. This is the decoupling
  C2/B already implements: the first-byte bound and the idle bound are independent
  (`timeouts.rs:10-15` vs `:22-27`), so lowering the TTFB window does **not** touch the mid-stream
  guard. Dynamic value observed live: **25000 ms** (A1 §5, read from the error strings
  `"upstream sent no bytes within 25000ms"`).
* **Everything else stays.** No change to `racing_timeout_ms`, `min_dynamic_timeout_ms`,
  `dynamic_sample_floor`, `admission_wait_ms`, `MAX_RETRIES`, `BUDGET_MULTIPLIER`, or
  `BUDGET_SLACK_MS`. The budget is *re-derived* automatically because it is a function of the window
  (see §2); that is the point of measuring it here.

### 1.3 Related keys and values

| Knob | Value during experiment | Source / locator |
|---|---|---|
| `[timeouts] first_byte_timeout_ms` | **15000** (the only change) | `config/mod.rs:161,357-359`; `lib.rs:89,117`; `timeouts.rs:10-15` |
| `[timeouts] idle_timeout_ms` | unset → dynamic 25000 ms | `config/mod.rs:165,363-365`; `timeouts.rs:22-27` |
| `[timeouts] min_dynamic_timeout_ms` | 8000 (default, unchanged) | `config/mod.rs:338-344` (A1 §5) |
| `[timeouts] dynamic_sample_floor` | 10 (default, unchanged) | `config/mod.rs:346-352` (A1 §5) |
| `[racing] racing_timeout_ms` | 8000 (default, unchanged) | `config/mod.rs:208-213` (A1 §5) |
| `[limits] admission_wait_ms` | 1500 (default, unchanged) | `config/mod.rs:321-326` (A1 §5) |
| `BUDGET_MULTIPLIER` | 2 (code constant, unchanged) | `retry.rs:22` |
| `BUDGET_SLACK_MS` | 500 (code constant, unchanged) | `retry.rs:35` |
| `MAX_RETRIES` | 8 (code constant, unchanged; never binding) | `chat.rs:25`; attempt count `chat.rs:69` |
| effective per-attempt budget | **32000 ms** (see §2) | `retry.rs:65-73` |

---

## 2. Budget re-derivation

**Formula** (`RetryPolicy::new`, `retry.rs:65-73`):

```
budget = window × BUDGET_MULTIPLIER + admission_wait_ms + BUDGET_SLACK_MS
       = window × 2                  + 1500                 + 500
```

Decision rule (`retry.rs:83-91` via `decide_for_window`, and mirrored at
`request_state.rs:117` `can_retry`): a retry fires only when
`started.elapsed() + window <= budget` **and** attempts remain.

**Arithmetic:**

| | window | budget = 2×w + 1500 + 500 | Δ |
|---|---:|---:|---:|
| Today | 25000 ms | 2×25000 + 2000 = **52000 ms** | — |
| Experiment | 15000 ms | 2×15000 + 2000 = **32000 ms** | **−20000 ms (−38.5%)** |

The 2000 ms constant is `admission_wait_ms` 1500 + `BUDGET_SLACK_MS` 500 (`retry.rs:35`,
`config/mod.rs:321-326`). A1 §5 records the 52000 ms baseline as "exactly one retry past the first
attempt".

**Second-attempt headroom.** At 15000 ms a turn has 32000 ms of request time, measured from request
start including the admission wait (`retry.rs:64`). The intended shape is: stall on the first attempt
at ~15000 ms (plus jitter), retry, then up to ~17000 ms for the second attempt — which is more than
the 15000 ms window it needs. Compare the 10 s case A1 rejected: budget 22000 ms leaves
`elapsed 10 s + 10 s = 20 s ≤ 22 s` only if the admission wait was ~0 (A1 §6a). At 15 s the
equivalent worst case is `15 s + 15 s = 30 s ≤ 32 s`, with the 2000 ms constant covering the
admission wait and the observed timeout jitter that `BUDGET_SLACK_MS` exists for (`retry.rs:26-34`:
a 302 ms observation against a 300 ms window). **Headroom is positive but thin: 2000 ms of slack
against a 15000 ms window.** The 2× multiplier means a retry can never double the client's wait
(`retry.rs:20-21`).

**Expectation: the retry still fires ≥95% of the time.** Reasoning from A1 §4: 65 of 67 stall rows
already carry `"; retrying on another key"` under the 52000 ms budget, and the observed stall
ttfb-half latency has median 26417 ms — under a 15000 ms window the stall is observed ~11 s earlier,
so `elapsed` at decision time is smaller, not larger. The retry decision compares
`elapsed + window` against a budget that shrank by exactly the same amount the window shrank
(2× the 10 s reduction = 20 s, matching the budget's 20 s cut), so the *ratio* of elapsed-at-decision
to budget is roughly preserved. The residual risk is the admission wait: a request that waited close
to the full 1500 ms for a permit and then stalled will have less slack. That is what the 500 ms
`BUDGET_SLACK_MS` absorbs. Hence ≥95% expected firing, and ≥97% pre-registered as the pass bar
below (§3) — the gap between the two is the measurement's job to resolve.

---

## 3. Pre-registered success / failure criteria

All criteria are evaluated on the experiment window defined in §4. Every threshold cites its source.
"Baseline" numbers are the frozen A1/A6 post-start figures.

### 3.1 Primary

| Metric | Baseline | Success | Failure |
|---|---:|---|---|
| ttfb p95 | **23162 ms** (A1 §1) | **≤ 19000 ms** | **> 21000 ms** |
| Reclassified attempts (200-success ttfb arriving in [15000, 25000) ms) | 81 in [15 s, 25 s) + 97 in [10 s, 15 s) (A1 §6a: 178 total in [10 s, 25 s); 97 + 81 split) | recovery bar: **≥ 50%** of the 81-bound reach a successful completion (clean/tool_calls) on a later attempt | **< 25%** of the 81-bound recover |
| Salvage rate per turn (turns with a stall attempt that reach a clean completion half on a later attempt) | **9.5%** (6/63, A1 §4) | **≥ 9.5%** (no regression) | < 9.5% |
| Retry still fires after a 15 s stall (share of 15 s stall rows carrying `"; retrying on another key"`) | 65/67 = 97.0% at 25 s (A1 §4) | **≥ 97%** | < 97% |
| Client/proxy TTFT ratio p95, per-request matched | **13.31×** (A6 §2) | **≤ 13.31×** | > 13.31× |

The 81-bound: A1 §6a splits the 178 currently-successful [10 s, 25 s) attempts into 97 in 10–15 s and
**81 in 15–25 s**. Those 81 are exactly what a 15000 ms window converts from success into stall. Their
recovery rate is the central quantity of the experiment, because A1 §3 measured that a slow first byte
is "a coin flip, not a death sentence" — 53.5% of ≥15 s first-byte arrivals still complete, and only
43.9% go terminal-504. If ≥50% of the 81-bound recover, the window cut is net-positive; below 25% it
is destroying successes faster than it saves tail latency.

### 3.2 Invariants (must hold; any violation = immediate rollback)

1. **Zero rows containing `"upstream sent no bytes within 15000ms"` in the terminal form with no
   following attempt that is not a legitimate single-attempt turn.** Operationally: no row whose error
   reads `body stalled after 15000ms` / `no bytes within 15000ms` *terminal give-up* for a turn that
   still had attempts remaining and budget left (`retry.rs:83-91`). The idle-bound stall message
   `"upstream body stalled after {}ms; retrying on another key"` (`chat.rs:625`, A6 §2 code table)
   must still read 25000 ms, never 15000 ms — that is the decoupling check.
2. **`truncated_stream` flat.** A1 §3 measures 4 truncated_stream outcomes out of 157 ≥15 s attempts
   (2.5%). Any increase beyond the baseline share is evidence the cut truncates in-flight successful
   generations — the same failure mode A1 §6b priced at 93 turns for a 60 s cap.

### 3.3 Tie-breaking rule

If ttfb p95 lands in the neutral band (19000 < p95 ≤ 21000 ms) **and** all invariants hold **and**
the 81-bound recovery is ≥50%, the result is **inconclusive-extend**, not success: re-run for another
24 h. A primary metric in the neutral band with a healthy recovery bar does not authorize a further
cut to 10 s, and it does not authorize calling the change a failure.

---

## 4. Measurement procedure

### 4.1 Log source

`/var/log/nimaproxy/turns.jsonl` (A6 §1 names this path as the source of the `record_kind=="ttfb"`
rows; the container writes it via `TurnLog`, `turn_log.rs`). Same file, same walk as the A1/A6 lanes.
Analysis floor for the experiment window: the container restart timestamp that loads
`first_byte_timeout_ms = 15000`. **No pre-start rows may enter the experiment slice.**

### 4.2 record_kind fields used

Per A1's turn model (`request_id` grammar `YYYYMMDDHHMMSS.<key-pool-idx>.<attempt-idx>`, 3
dot-parts):

* **`record_kind = "ttfb"`** — the per-attempt first-byte record. Fields read: `latency_ms`
  (ttfb-half latency), `status_code`, `error` (stall strings, retry strings), `key_label`,
  `timestamp`, `request_id`.
* **`record_kind = "completion"`** — the per-attempt completion half. Fields read:
  `latency_ms`, `finish_reason`, `failure_mode` (`clean_stop` / `tool_calls` / `truncated_stream`),
  `content_chars`, `chunk_count`, `body_bytes`, `stream_ended_cleanly`, `request_id`.
* **`record_kind = "loop"`** — excluded from latency joins; counted only to reconcile the population
  (A1 §1: 673 loop rows post-start, 439 loop-only turns with no ttfb/completion half).

### 4.3 Joins

* **per-attempt join** = same full `request_id` (ttfb half ↔ completion half of one attempt). Used
  for: the ≥15 s outcome table (A1 §3 shape), truncated_stream counts, stall salvage.
* **per-turn join** = group by the first two dot-parts of `request_id` (all attempts of one client
  turn). Used for: full-turn latency percentiles, the 81-bound recovery rate, salvage rate
  (denominator = turns containing at least one stall attempt).

Client-side corroboration (optional, secondary): match client TTFT from the omp transcripts
(`/home/dirmacs/.omp/agent/sessions/**/*.jsonl`, `type=="message"`, `role=="assistant"`,
`provider=="nimaproxy"`, field `ttft`) to proxy rows by time window, per A6 §2's per-request match.
The client ratio p95 ≤ 13.31× criterion is evaluated on that matched set only.

### 4.4 Window and minima

* **≥ 24 h** of post-restart traffic, covering at least one full diurnal cycle (the baseline window
  spans 22Z–09Z and has zero-traffic hours 03–05Z; A6 §1).
* **≥ 300 `record_kind="ttfb"` rows** in the experiment slice (baseline: 1748 in ~11 h).
* **≥ 20 stall attempts** (rows whose `error` contains `"no bytes within 15000ms"`) — enough for the
  ≥97% retry-firing bar to be decidable at ±1 row of granularity (1/20 = 5%).

If either minimum is not met at 24 h, extend the window; do not evaluate the criteria on a smaller
slice.

### 4.5 Confound control

A6 §1/§3 shows the per-hour client gap is driven by **session concurrency and proxy TTFB p50**, not
by network: hours 06Z (+20 018 ms median gap, 3.41×, 67.8% of turns ≥2×, 15 sessions, 121.2 mean
messages/request) and 09Z (+6 753 ms, 2.03×, 48.4% ≥2×) are concurrency outliers, and the "normal"
hours sit at 1.01×–1.20×.

Therefore:

1. **Publish a per-hour table** exactly like A6 §1 (hour, proxy n, proxy p50/p95, client n, client
   p50/p95, gap p50/p95, ratio) for the experiment window, and compare hour-by-hour against the
   baseline table — never aggregate-to-aggregate. A p95 win that exists only in the 06Z/09Z outlier
   hours is a concurrency artifact, not a window effect.
2. **Flag 06Z and 09Z as concurrency outliers** (and any hour with ≥10 distinct sessions) in that
   table; the primary ttfb-p95 criterion is evaluated **both** on all hours and on the normal-hour
   subset. Success requires the normal-hour subset to pass; an all-hours pass driven by the outlier
   hours is reported as such, not as a clean win.
3. **Check the key-pool shape.** A1 §4's cluster 1 (20 stalls, 525 s, even 5/5/5/5 across all four
   keys) is an upstream-wide event; a 15 s window will fire on it far more often. Report the
   per-cluster stall counts and the key split so an upstream incident in the window cannot be
   mistaken for the knob's effect.

---

## 5. Rollback

### 5.1 Triggers

Any single trigger fires the rollback. All are evaluated on the §4 slice.

| # | Trigger | Threshold | Source of the threshold |
|---|---|---|---|
| 1 | ttfb p95 in the failure band | > 21000 ms | §3.1 (baseline 23162, A1 §1) |
| 2 | 81-bound recovery collapses | < 25% of the [15 s, 25 s) attempts reach a clean completion | §3.1 (vs 53.5% ≥15 s completion, A1 §3) |
| 3 | Per-turn salvage regresses | < 9.5% | §3.1 (baseline 6/63, A1 §4) |
| 4 | Retry stops firing after a stall | < 97% of 15 s stall rows carry `"; retrying on another key"` | §3.1 (baseline 65/67, A1 §4) |
| 5 | Client tail worsens | client/proxy TTFT ratio p95 > 13.31× | §3.1 (A6 §2) |
| 6 | Mid-stream guard was loosened | any `"upstream body stalled after 15000ms"` row (idle bound must stay 25000) | §3.2 invariant 1; `chat.rs:625` |
| 7 | Truncation appears | `truncated_stream` share above the 2.5% baseline (4/157, A1 §3) | §3.2 invariant 2 |
| 8 | Terminal-504 share rises | terminal-504 share of ≥15 s attempts above the 43.9% baseline with a lower completion share | §3.1 (A1 §3) |

### 5.2 The 4-step rollback

1. **Delete the key.** Remove the line `first_byte_timeout_ms = 15000` from the `[timeouts]` section
   of the live config. Do not set it to 25000; delete it (see 5.3).
2. **Verify the file.** `grep -c first_byte_timeout_ms <config>` returns 0, and the `[timeouts]`
   section parses (`toml`-aware read, not a grep for a default you assume). Record the file's hash
   before and after so the change is auditable.
3. **Restart.** Restart the proxy so the config is re-read; the new analysis floor is the restart
   timestamp.
4. **Verify 25000 ms rows return.** In the post-rollback slice, stall error strings read
   `"upstream sent no bytes within 25000ms"` again (the form A1 §4 read from the baseline), and the
   budget is back to 52000 ms (`2×25000 + 1500 + 500`, `retry.rs:65-73`). If any 15000 ms string
   persists, the restart did not take — the rollback is not complete.

### 5.3 Why deletion, not revert-to-25000

`first_byte_timeout_ms` is an **override**, not the source of truth. Unset, it resolves to
`timeout_for_model()` — the per-model dynamic window (`timeouts.rs:13`, `config/mod.rs:354-356`
documents the `None` contract as "an unset knob leaves behavior byte-identical"). Setting the key
back to 25000 would produce a *different* system from the pre-experiment one: a hard 25000 ms pin
that no longer tracks the model's measured dynamic timeout (`min_dynamic_timeout_ms` 8000 floor,
`dynamic_sample_floor` 10, `config/mod.rs:338-352`). The pre-experiment baseline was measured with
the key **absent** — A1 §5 read the live 25000 ms from the error strings, i.e. the dynamic value that
happened to be 25000. Deleting the key restores that exact state; pinning it restores a lookalike.
Deletion is the only rollback that is provably byte-identical to the baseline.

---

## 6. Reporting

One artifact after the window: the per-hour table (§4.5), the four primary metrics with their
criteria verdicts, the two invariants, the cluster/key-split note, and — if the neutral band was hit —
the extend decision with its rationale. Every threshold in this document is pre-registered; a
post-hoc change to any threshold invalidates the run and must be recorded as a new protocol revision,
not an edit to this one.
