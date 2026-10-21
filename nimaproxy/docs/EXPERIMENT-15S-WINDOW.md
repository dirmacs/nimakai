# EXPERIMENT 15S — first-byte window dynamic → 15 s (pre-registered protocol, rev 2)

> **Paths in this document are environment variables.** This is a public repository, so the
> protocol cites placeholders rather than one operator's host layout:
>
> ```bash
> export CONFIG_FILE=/etc/nimaproxy/nimaproxy.toml       # the deployed config (adjust to taste)
> export OMP_SESSION_DIR="$HOME/.omp/agent/sessions"     # omp transcript corpus
> ```

**Status:** pre-registered, not yet run. Nothing in this document is a result; every number is a
baseline or a threshold set *before* the measurement.

**Rev 2 (2026-10-20, retargeted to the merged tree at `5a24d89`, PR #29):** the
`chore/rust-ci` rebase onto `origin/main` replaced the branch's single-budget retry
policy with main's split fast/body-stall budgets (`RetryPolicy::new_split`,
`decide`/`decide_body_stall`/`decide_under`) and replaced the branch's
`idle_timeout_ms` Option knob with main's `mid_stream_idle_timeout_ms` (180 s
default). The experiment's premise survives: `first_byte_timeout_ms` is restored
as an Option override end-to-end, and the idle rail is independent of it. What
changed in this revision: §1 knob table (names, defaults, locators), §2 budget
derivation (two rows), §3.2 invariant 1 (idle message now reads the resolved
idle window), §5 rollback verification strings. Thresholds, minima, and the
reporting shape are unchanged from rev 1.

**Evidence basis (read-only lanes, frozen 2026-10-20):**

| Lane | File | What it measured |
|---|---|---|
| A1 | `$ANALYSIS_DIR/A1.md` | TTFB/stall analysis on the post-start slice (4096 rows, 22:57:57Z Oct-19 → 09:40Z Oct-20) |
| A6 | `$ANALYSIS_DIR/A6.md` | Client TTFT vs proxy TTFB, per-hour, per-request matched ratios |
| B | `$ANALYSIS_DIR/B-ranked.md` | Evidence-synthesized ranked list; C2 names this experiment as the unlock |

**Code basis (verified at `5a24d89`):** `nimaproxy/src/proxy/retry.rs`
(`new_split`, two budgets, `decide_under`),
`nimaproxy/src/proxy/request_state.rs` (`can_retry` with explicit window+budget),
`nimaproxy/src/proxy/timeouts.rs` (`first_byte_timeout_for_model`,
`idle_timeout_for_model`, `timeout_for_model`),
`nimaproxy/src/lib.rs` (knob plumbing), `nimaproxy/src/config/mod.rs`
(TOML parsing).

**Why 15 s and not 10 s:** A1's per-knob verdict rejects 10 s — it would reclassify 178 of 1748
ttfb attempts (10.2%) that currently succeed in [10 s, 25 s), 128 of which are single-attempt turns
with no retry available, and it contracts the fast budget 52000 → 22000 ms, likely zeroing the 9.5%
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

`first_byte_timeout_ms` exists as an optional key — `TimeoutsConfig` in
`nimaproxy/src/config/mod.rs:171` declares
`pub first_byte_timeout_ms: Option<u64>`, parsed at `config/mod.rs:386-387`, carried into
`RuntimeControls` at `lib.rs:128` and `AppState` at `lib.rs:95`, and resolved per request by
`first_byte_timeout_for_model()` at `timeouts.rs:112-117` (`None` → per-model dynamic
via `timeout_for_model()`). So this experiment is **one config line and
a restart; no code change**.

### 1.2 What stays

* **`mid_stream_idle_timeout_ms` stays unset.** The mid-stream inter-chunk guard then resolves to
  the 180 s default (`config/mod.rs:375-382`: unset or 0 → 180000; `lib.rs:150`;
  `timeouts.rs:132-137` `idle_timeout_for_model`, where 0 → dynamic as the only
  dynamic path). The first-byte bound and the idle bound are independent
  (`timeouts.rs:112-117` vs `:132-137`), so lowering the TTFB window does **not**
  touch the mid-stream guard. Rev-1 note: the branch's `idle_timeout_ms` Option
  knob no longer exists; its "unset → dynamic 25000" row is superseded by main's
  measured 180 s default (#22: live step-5-preview generations pause 118–148 s
  mid-stream then resume to [DONE], and the earlier 60 s value cut generations
  the upstream would have finished).
* **Everything else stays.** No change to `racing_timeout_ms`, `min_dynamic_timeout_ms`,
  `dynamic_sample_floor`, `admission_wait_ms`, `BUDGET_MULTIPLIER`, or
  `BUDGET_SLACK_MS`. Both budgets are *re-derived* automatically because they are
  functions of the windows (see §2); that is the point of measuring it here.

### 1.3 Related keys and values

| Knob | Value during experiment | Source / locator |
|---|---|---|
| `[timeouts] first_byte_timeout_ms` | **15000** (the only change) | `config/mod.rs:171,386-387`; `lib.rs:95,128`; `timeouts.rs:112-117` |
| `[timeouts] mid_stream_idle_timeout_ms` | unset → 180000 ms default | `config/mod.rs:167,375-382`; `lib.rs:93,127,150`; `timeouts.rs:132-137` |
| `[timeouts] min_dynamic_timeout_ms` | 8000 (default, unchanged) | `config/mod.rs` (A1 §5) |
| `[timeouts] dynamic_sample_floor` | 10 (default, unchanged) | `config/mod.rs` (A1 §5) |
| `[racing] racing_timeout_ms` | 8000 (default, unchanged) | `config/mod.rs` (A1 §5) |
| `[limits] admission_wait_ms` | 1500 (default, unchanged) | `config/mod.rs` (A1 §5) |
| `BUDGET_MULTIPLIER` | 2 (code constant, unchanged) | `retry.rs:22` |
| `BUDGET_SLACK_MS` | 500 (code constant, unchanged) | `retry.rs:35` |
| effective fast budget | **32000 ms** (see §2) | `retry.rs:77-97,145` |
| effective body-stall budget | **212000 ms** (see §2) | `retry.rs:77-97,152` |

---

## 2. Budget re-derivation

**Formula** (`RetryPolicy::new_split`, `retry.rs:77-97`; ceilings at `:145`/`:152`):

```
fast budget       = first_byte × BUDGET_MULTIPLIER + admission_wait_ms + BUDGET_SLACK_MS
                  = first_byte × 2                 + 1500              + 500
body-stall budget = fast budget + idle_window
```

Decision rule (`retry.rs:105-142` via `decide`/`decide_body_stall` → `decide_under`,
and mirrored at `request_state.rs:123` `can_retry`): a retry fires only when
`started.elapsed() + window <= budget` **and** attempts remain, where the fast
branches pass the first-byte window against the fast budget and the mid-body
stall branch passes the idle window against the body-stall budget
(`chat.rs:750-756`).

**Arithmetic:**

| | first_byte | idle | fast = 2×fb + 1500 + 500 | body-stall = fast + idle | Δ |
|---|---:|---:|---:|---:|---:|
| Today | 25000 ms (dynamic) | 180000 ms | 2×25000 + 2000 = **52000 ms** | 52000 + 180000 = **232000 ms** | — |
| Experiment | 15000 ms | 180000 ms | 2×15000 + 2000 = **32000 ms** | 32000 + 180000 = **212000 ms** | fast **−20000 ms (−38.5%)**; body-stall −20000 ms (−8.6%) |

The 2000 ms constant is `admission_wait_ms` 1500 + `BUDGET_SLACK_MS` 500 (`retry.rs:35`,
`config/mod.rs`). A1 §5 records the 52000 ms baseline as "exactly one retry past the first
attempt" — that reading was taken against the single-budget tree; on the merged tree
the equivalent statement is that the fast budget admits exactly one first-byte
window past the first attempt.

**Second-attempt headroom.** At 15000 ms a turn has 32000 ms of fast-budget request
time, measured from request start including the admission wait (`retry.rs:64`). The
intended shape is: stall on the first attempt at ~15000 ms (plus jitter), retry, then
up to ~17000 ms for the second attempt — which is more than the 15000 ms window it
needs. Compare the 10 s case A1 rejected: budget 22000 ms leaves
`elapsed 10 s + 10 s = 20 s ≤ 22 s` only if the admission wait was ~0 (A1 §6a). At 15 s the
equivalent worst case is `15 s + 15 s = 30 s ≤ 32 s`, with the 2000 ms constant covering the
admission wait and the observed timeout jitter that `BUDGET_SLACK_MS` exists for (`retry.rs:26-34`:
a 302 ms observation against a 300 ms window). **Headroom is positive but thin: 2000 ms of slack
against a 15000 ms window.** The 2× multiplier means a retry can never double the client's wait
(`retry.rs:20-21`).

The body-stall budget is unaffected in shape: a mid-body stall that consumed a
first-byte window and ran into the idle rail retries under 212000 ms instead of
232000 ms. Because the window term also shrank (idle window unchanged at 180 s,
but the elapsed-at-decision is ~10 s smaller — the stall is observed earlier),
mid-body retries admit *more* readily than at baseline, not less. No separate
headroom analysis is needed for that arm.

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
|---|---|---:|---|---|
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
   still had attempts remaining and budget left (`retry.rs:105-142`). The idle-bound stall message
   `"upstream body stalled after {}ms; retrying on another key"` (`chat.rs:800`, reporting
   `idle_window.as_millis()`) must read **180000 ms**, never 15000 ms — that is the decoupling check.
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
(`$OMP_SESSION_DIR/**/*.jsonl`, `type=="message"`, `role=="assistant"`,
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

> 2026-10-21 07:28Z annotation (step-5 model-card tuning staged, NOT deployed):
> a separate tuning change (max_tokens 16384→32768, top_k=0, breaker
> 32000→36000, docs/comments) is baked into the repo checkout and staged in
> `$CONFIG_FILE`, but the container was deliberately
> NOT restarted: §1.2 "everything else stays" + §4.4's post-restart clock
> forbid mid-window restarts. The live proxy still serves max_tokens=16384.
> Deploy (restart) is parked for owner re-go at window close (~18:44Z),
> AFTER the §3/§5 evaluation. Live-file edits preserved
> `first_byte_timeout_ms = 15000` (line 134). This change is TTFB-neutral by
> construction (timeout knob untouched), but the restart itself would reset
> the 24h clock — hence the hold.
>
> 2026-10-21 07:36Z update: live file DISARMED. The staged values were armed,
> not parked (bind-mounted: any restart would have activated them), so the
> live file was reverted to floor state (16384/32000, knob 15000 kept) and
> verified by grep + TOML parse. Armed snapshot preserved at
> `nimaproxy.toml.armed-step5tuning-20261021`. Tuning now lives only in PR
> #31 (`tuning/step5-modelcard-20261021`, commit 3c73e22) — merge any time,
> restart after window close.

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
| 6 | Mid-stream guard was loosened | any `"upstream body stalled after 15000ms"` row (idle bound must stay 180000) | §3.2 invariant 1; `chat.rs:800` |
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
4. **Verify dynamic rows return.** In the post-rollback slice, stall error strings read
   `"upstream sent no bytes within <dynamic>ms"` again (the form A1 §4 read from the baseline,
   then 25000), and the fast budget is back to `2×dynamic + 1500 + 500` (`retry.rs:77-97`). If any
   15000 ms string persists, the restart did not take — the rollback is not complete.

### 5.3 Why deletion, not revert-to-25000

`first_byte_timeout_ms` is an **override**, not the source of truth. Unset, it resolves to
`timeout_for_model()` — the per-model dynamic window (`timeouts.rs:112-117`, `config/mod.rs:386-387`
documents the `None` contract as "an unset knob leaves behavior byte-identical"). Setting the key
back to 25000 would produce a *different* system from the pre-experiment one: a hard 25000 ms pin
that no longer tracks the model's measured dynamic timeout (`min_dynamic_timeout_ms` 8000 floor,
`dynamic_sample_floor` 10). The pre-experiment baseline was measured with
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

---

## 7. Staging checklist (owner go/no-go)

The experiment touches the live config and needs owner approval. Staging state:

- [ ] PR #29 merged (payload: first-byte override + merged retry architecture)
- [ ] CI `rust` job green (currently blocked: `cargo not on PATH` on
  `dirmacs-stage-ci-nimakai` — runner ops, not the diff; see PR #29 comment)
- [ ] Stage1 image rebuilt from the merge commit and probed (`/health` + `/stats` 200)
- [ ] Live config backup taken (hash recorded); `[timeouts]` section reviewed
- [ ] Owner go: add `first_byte_timeout_ms = 15000`, restart, record the analysis floor timestamp

**Do NOT start the 24 h window until all five boxes are checked.**

---

## 8. Rev 2.1 addendum — live-config budget correction (2026-10-20, pre-evaluation)

**Status:** derivation correction only. No threshold, minimum, invariant, or
rollback trigger changes. Issued ~1 h after the window opened (floor
2026-10-20T18:43:50Z) and before any criterion is evaluated, per the §6
revision rule.

**What rev 2 got wrong:** §1.3/§2 used code-default constant values for keys
the live config actually sets. The live config
(`$CONFIG_FILE`, backup sha256 `0d8245d2…`) sets:

| Knob | Rev 2 assumed | Live value | Locator |
|---|---|---|---|
| `[limits] admission_wait_ms` | 1500 (code default, `config/mod.rs:331`) | **30000** | live config `:115` |
| `[timeouts] min_dynamic_timeout_ms` | 8000 (doc default) | **25000** | live config `:121` |
| `[timeouts] dynamic_sample_floor` | 10 (doc default) | **25** | live config `:122` |
| `[racing] timeout_ms` | 8000 (code default) | **25000** | live config `:70` |

**Corrected derivation** (formula unchanged: `retry.rs:87-95`
`budget = 2×first_byte + admission + 500`; `body = budget + idle`;
`chat.rs:166-171` passes live `state.admission_wait_ms`):

| | first_byte | idle | admission | fast = 2×fb + adm + 500 | body-stall = fast + idle |
|---|---:|---:|---:|---:|---:|
| Baseline (pre-window) | 25000 (dynamic) | 180000 | 30000 | **80500 ms** (rev 2 said 52000) | **260500 ms** (rev 2 said 232000) |
| Experiment | 15000 | 180000 | 30000 | **60500 ms** (rev 2 said 32000) | **240500 ms** (rev 2 said 212000) |

**What this changes in the prose:**

* §2 "thin headroom" paragraph: the worst-case second-attempt fit is
  `elapsed 15 s + 15 s = 30 s ≤ 60.5 s`, not `≤ 32 s`. The slack is
  **30500 ms, not 2000 ms** — the retry-fires bar (§3.1, ≥97%) is *easier*
  to clear than rev 2 argued, because the live admission term dominates.
  The ≥95% expectation and the ≥97% bar stand (a bar does not move because
  the slack is wider than described).
* §2 "2× multiplier" sentence: with a 30000 ms admission term the budget is
  no longer ~2× the window; the client-wait cap is 60500 ms, ~4× the
  15000 ms window. The experiment still bounds the cut's effect to the
  first-byte rail; it does not re-litigate the live admission setting.
* §5.2 step 4: post-rollback verification reads fast budget
  `2×dynamic + 30000 + 500`, not `2×dynamic + 1500 + 500`.
* The dynamic-floor rows (§5.3: 8000 floor, sample floor 10) describe code
  defaults, not live behavior; the live dynamic path is floor 25000 /
  samples 25. The deletion-not-pin argument is unaffected (it compares
  absent-key against pinned-key on the same live config).

**Why the run stays valid:** the error is in the *described* constants, not
in the manipulated variable. The window cut (25000 → 15000), the config
mechanism (one key + restart), the floor discipline, and every pass/fail
bar are exactly as pre-registered. The corrected slack only widens the
margin on the ≥97% retry-firing bar; it cannot manufacture a false pass on
any other criterion.
