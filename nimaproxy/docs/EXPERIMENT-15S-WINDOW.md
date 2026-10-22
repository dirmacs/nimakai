# EXPERIMENT 15S — first-byte window dynamic → 15 s (pre-registered protocol, rev 2)

> **Paths in this document are environment variables.** This is a public repository, so the
> protocol cites placeholders rather than one operator's host layout:
>
> ```bash
> # The HOST-side live config — the file the operator edits. It is NOT the
> # in-container /etc/nimaproxy/nimaproxy.toml, which is a different file (a
> bind mount of this one on a deployed host).
> export CONFIG_FILE="$HOME/.config/nimaproxy/nimaproxy.toml"
> export OMP_SESSION_DIR="$HOME/.omp/agent/sessions"     # omp transcript corpus
> export ANALYSIS_DIR=/path/to/analysis                 # dir holding the A*/B* analysis notes
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

**Rev 3.2 (2026-10-21, window floor `2026-10-21T14:50:43Z`, binary `2e6b6e0`):** the window that
is running now was started by the deploy of the fixed binary, so rev 3.0's window
(`2026-10-21T12:31:38Z..2026-10-21T14:50:43Z`) was voided and 2 h 19 m of it discarded; the floor
moved to the restart, so the 24 h minimum is not missed. **The operative criteria for the running
window are §11.2, the triggers are §11.3 as amended by §12, and the reporting procedure is §11.4.**
Rev 3.0's §10.4 rows 3, 4 and 8, its §10.6 trigger 5 and its §10.7 are superseded by them — §10.7
carries no rotation procedure at all, and the log WILL rotate inside this window. Rev 3.2 changes
**no threshold value**, and **no result observed in the running window has been examined**.

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

---

# 9. Rev 2.2 addendum — post-window evaluation and rollback (2026-10-21)

**Status:** evaluation closed early, **run CLOSED, verdict FAIL (the rollback bar fired)**. No threshold,
minimum, invariant, or rollback trigger is changed by this revision. Issued per §6 after the window
closed; the §3/§5 bars quoted below are the pre-registered rev 2.1 bars verbatim.

All figures in this addendum are computed from one frozen slice of the turn log: 62,501 lines,
sha256 `5686d37001fa6b827f1652edd7dc659e3b27e170051f9696943a88ca829ed2e5`, sliced by timestamp to
`2026-10-20T18:43:50Z .. 2026-10-21T12:31:38Z` (the floor, and the moment the proxy was stopped
for the rollback). The results artifact is `EXPERIMENT-15S-RESULTS.md`.

## 9.1 Why the run closed early (17.80 h, not the §4.4 24 h minimum)

The window ran from the floor `2026-10-20T18:43:50Z` to the rollback at `2026-10-21T12:31:38Z` —
**17.80 h elapsed** — against §4.4's >=24 h minimum. Two of the three §4.4 minima were met: >=300
`ttfb` rows (4228 in the slice) and >=20 stall attempts (309 in the slice). The >=24 h minimum was
**not** met, and §4.4 says a slice below the minimum must not have its criteria evaluated.

The run was closed anyway because the failing criteria were **already determined before the minimum
was reached** — extending the window cannot change either verdict:

* **T4 (retry firing, §5.1 trigger 4)** needs >=97% of 15 s stall rows to carry
  `"; retrying on another key"`. Measured 285/309 = 92.2%. Closing the remaining gap needs the 24
  non-retrying rows to be joined by 476 further all-perfect stall rows; at the observed rate of
  16.7 stalls/h only about 109 stall rows arrive in the remaining time. Any realistic extension
  widens the gap, because the non-retrying share is a property of the knob, not of sample size.
* **T8 (terminal-504 share, §5.1 trigger 8)** needs the terminal-504 share of >=15 s attempts to stay
  at or below the 43.9% baseline *with* a lower completion share. Measured 84.8% terminal-504 /
  14.7% completion. Dragging terminal-504 down to <=43.9% needs 328 further >=15 s attempts with
  zero 504s; only about 130 arrive.

The verdict is therefore decided at 17.80 h. Recording the slice as closed-with-verdict is the only
honest reading: an extension that cannot flip either failing bar would only delay the rollback that
§5.1 already mandates, while leaving the failing knob live on the gateway. §9.5 records the >=24 h
miss explicitly.

## 9.2 §4.2 grammar correction: `request_id` is `{YYYYMMDDHHMMSS}.{pid}.{seq}`

§4.2 states the grammar as `YYYYMMDDHHMMSS.<key-pool-idx>.<attempt-idx>` (3 dot-parts). That is
wrong. The real format, from `turn_log.rs:405-410` (the lines moved when the failure-record fix below
added its documentation; the format itself is unchanged):

```rust
let seq = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
let request_id = Some(format!("{}.{}.{}",
    Utc::now().format("%Y%m%d%H%M%S"), std::process::id(), seq));
```

So `request_id` = **`{YYYYMMDDHHMMSS}.{pid}.{seq}`**, where `seq` is a process-local counter that is
**unique per record** — not per attempt, not per turn. There is no key-pool index and no attempt
index anywhere in the id.

**Consequence for §4.3's per-turn join.** §4.3 defines the per-turn join as "group by the first two
dot-parts of `request_id`". Grouping by the first two dot-parts groups by `{second}.{pid}` — and
because the pid is constant for the window, that is a **per-second time bucket**, not a turn.
Measured on the slice:

| grouping | distinct values over 4228 `ttfb` rows |
|---|---:|
| first dot-part (`YYYYMMDDHHMMSS`) | 3776 |
| first two dot-parts (`{second}.{pid}`) | 3776 |
| full `request_id` | 4228 |

3776 first-parts and 3776 first-two-parts are identical because the pid never varies: both groupings
collapse to the same 3776 per-second buckets, and neither one is a turn. Two attempts of the same
client turn land in different buckets whenever they are more than one second apart, which is the
normal case after a 15 s stall. The §4.3 per-turn join is therefore **not a join at all** — it is a
time histogram, and every per-turn quantity §4.3 assigns to it (full-turn latency percentiles, the
81-bound recovery rate, the salvage denominator) was measuring seconds, not turns.

## 9.3 Schema gap: failure-path records carried `loop_key: None` by design

`loop_key` (`{session_id, loop_seq, round_index}`) is the only real turn key in the log. On the
slice:

| record kind | rows | with `loop_key` | without |
|---|---:|---:|---:|
| `ttfb` | 4228 | 3911 | 317 |
| `completion` | 3903 | 3903 | 0 |
| `loop` | 1166 | 1166 | 0 |
| **15 s stall rows** (subset of `ttfb`) | **309** | **0** | **309** |

This was not a collection accident. Before the fix, the field's own documentation said so: error-path records intentionally kept
`loop_key` `None`, because the round was already counted by `LoopTracker::begin` and the record
cannot name its loop, and the reader-side contract repeated it. The fix has since replaced both
with the opposite statement, so the pre-fix text is history rather than the current source. The stall rows — the entire population the two per-turn criteria are defined over — are
failure-path rows, so **every stall row in the slice was unkeyed**, and the 317 unkeyed `ttfb` rows
are exactly the failure paths.

So the per-turn join had no key to join on. §4.3's intended key was never available.

## 9.4 What is therefore unmeasurable from this run, and why that is not a threshold change

Because the per-turn join is a time bucket (§9.2) and the stall population was unkeyed (§9.3):

* **§3.1's 81-bound recovery bar is UNMEASURABLE from this run.** The 81-bound is a set of *turns*
  (attempts that the 15 s window converts from success into stall, and the later attempts that
  recover them). Neither the mis-grouped join nor the unkeyed stall rows can name a turn, so the
  recovery numerator and denominator cannot be constructed from the log at all. This is stated as a
  limitation of the run, not as a pass and not as a fail.
* **§3.1's per-turn salvage rate is UNMEASURABLE**, same reason: its denominator is "turns containing
  at least one stall attempt", which requires the turn key.
* **The per-attempt join does work.** §4.3's per-attempt join (same full `request_id`, ttfb half <->
  completion half) is unaffected by the grammar error, because the full id is unique per record and
  both halves of one attempt carry the same one. Measured on the slice: 4228 distinct ttfb ids,
  **3903 ids hold both halves**. That join is what the per-attempt metrics in the results artifact
  are computed on.

The fix for the schema gap is a **code change, not a threshold change**: failure-path records now
carry their loop key, so a future window can compute the per-turn quantities from the log. It
touches no §3 bar, no §4 minimum, no §3.2 invariant, and no §5.1 trigger. §6's rule — "a post-hoc
change to any threshold invalidates the run and must be recorded as a new protocol revision, not an
edit to this one" — is therefore **not violated**: this addendum is the new protocol revision, and it
edits no bar. The unmeasurability is recorded as a property of the run's data, and the verdict rests
entirely on the metrics that *were* measurable.

## 9.5 §4.4 minima, as measured

| Minimum | Bar | Measured | Met? |
|---|---|---|---|
| >=24 h post-restart | 24 h | **17.80 h** | **NO** — closed early, see §9.1 |
| >=300 `ttfb` rows | 300 | 4228 | YES |
| >=20 stall attempts | 20 | 309 | YES (the protocol's own floor for the >=97% bar to be decidable) |

T4 was decidable: 309 stall rows is 15x the protocol's 20-row floor, so +/-1 row of granularity is
+/-0.32 pp and the 92.2% reading is 4.8 pp below the bar. The verdict does not hinge on the truncated
window.

## 9.6 The rollback taken

§5.1 **trigger 4** (retry stops firing after a stall, <97%) and **trigger 8** (terminal-504 share
rises above the 43.9% baseline with a lower completion share) both fired. Either alone fires the
rollback; §5.2's 4-step rollback therefore applies in full:

1. **Delete the key** — the line `first_byte_timeout_ms = 15000` was removed from the `[timeouts]`
   section of the live config. It was **deleted, not pinned to 25000**: per §5.3 the key is an
   *override*, and the pre-experiment baseline was measured with the key **absent** (the dynamic
   `timeout_for_model()` value that happened to read 25000). Pinning it to 25000 would produce a
   different system from the baseline one; deletion is the only rollback provably byte-identical to
   the pre-experiment state.
2. **Verify the file** — `grep -c first_byte_timeout_ms <config>` returns 0 and the `[timeouts]`
   section parses as TOML; the file hash was recorded before and after so the change is auditable.
3. **Restart** — the proxy was restarted so the config is re-read; the restart timestamp
   `2026-10-21T12:31:38Z` is the new analysis floor.
4. **Verify dynamic rows return** — the restart's own startup log reports `timeout_floor=25000ms`,
   and the post-rollback slice's stall error strings read
   `"upstream sent no bytes within <dynamic>ms"` again (the 25000 ms form A1 §4 read from the
   baseline), with the fast budget back to `2x dynamic + 30000 + 500` (`retry.rs:77-97`).

The pre-window era's stall strings show both `15000` and `25000` window values; the post-rollback
slice shows only the dynamic value.

## 9.7 The two invariants: one verified, one not decided

**§3.2.1 (the mid-stream guard is decoupled from the first-byte rail) is VERIFIED.** 0 rows in the
slice read `upstream body stalled after 15000ms`; the idle bound stayed at 180000 ms for the whole
window.

**§3.2.2 (`truncated_stream` is flat) is NOT DECIDED, and the measurement is within noise of the
bar.** On the invariant's own denominator -- >=15 s attempts joined to their completion half, which
is a per-ATTEMPT join and so does not depend on the broken turn-join -- the share is **2/57 = 3.5%**
against the 2.5% baseline. That is above the bar, on a sample where the bar itself is 1.4 rows: 2
rows is within noise of it, and is not evidence that the cut truncates in-flight generations. For
scale, the same join over the pre-window part of the log gives 44/568 = 7.7%, and the raw counts are
305 `truncated_stream` across 6807 completions against the window's 32 across 3903; the pre-window
slice spans several timeout eras, so that is context rather than a like-for-like baseline, and the
governing bar is A1's 4/157 = 2.5%.

The invariant is the one that would have detected the cut truncating in-flight generations, so its
being undecided is a real gap in this run's evidence. The decisive measurement is the new window,
where the failure records carry their turn key and the per-turn join exists.

**A trap this invariant sets, recorded because it is easy to fall into.** `failure_mode` is only ever
set on **completion** records -- 0 of the slice's 4228 `ttfb` rows carry one. Dividing a count of
`truncated_stream` records by a count of `ttfb` rows is therefore 0 by construction, whatever the
streams did, and reading that 0 as a pass is a false one. The results artifact's reproduction steps
scope the count to completion records and print the record-kind split so the artefact is visible.

## 9.8 No threshold was changed by this revision

Rev 2.2 changes **no threshold, no minimum, no invariant, and no rollback trigger.** The §3.1 bars
(ttfb p95 <=19000/>21000, 81-bound recovery >=50%/<25%, salvage >=9.5%, retry firing >=97%, TTFT
ratio <=13.31x), the §3.2 invariants, and the §5.1 trigger table are exactly as pre-registered in
rev 2.1 and are quoted, not edited, in §9.1/§9.5/§9.6. What this revision changes is the protocol's
*stated grammar* (§4.2), the *validity of one join* (§4.3), and the *reported measurability* of two
per-turn criteria (§9.4) — i.e. corrections to the measurement procedure, plus the rollback action
§5.1 already mandated. The results artifact for this run is `EXPERIMENT-15S-RESULTS.md`.

---

# 10. Rev 3.0 — pre-registration for the rolled-back window (2026-10-21)

**Status: PRE-REGISTERED, before the window closes.** Issued per §6, after the 15 s run closed and
the rollback was taken, and before the new window's floor. Nothing in §1-§9 is edited by this
revision: the 15 s experiment's thresholds, minima, invariants and triggers stand as they were
pre-registered, and the results artifact for that run is `EXPERIMENT-15S-RESULTS.md`.

This section exists because a window with no criteria closes undecidably. §9.4 records that the 15 s
run's two decisive per-turn metrics were **unmeasurable** — and §9.7 records that the reason is
fixable: the failure-path records now carry their turn key. A window that can answer those
questions, and has no pre-registered bars for them, would produce numbers nobody could act on. So
they are registered here, before the window closes, not derived from it afterwards.

## 10.1 The floor, and the close

| | |
|---|---|
| floor | `2026-10-21T12:31:38Z` — the restart that applied the rollback |
| close | `2026-10-22T12:31:38Z`, or later if the minima in §10.5 are not met |
| first-byte window | **dynamic** — the override is deleted, so the window is the per-model dynamic value (25 s at the floor) |
| enabled model | `stepfun-ai/step-5-preview`, `max_tokens = 32768`, `top_k = 0`, circuit breaker `max_output_tokens = 36000` |
| analysis floor in the log | turn-log line 62206 |

## 10.2 What this window is FOR

It measures the **rolled-back configuration** — the state the proxy was in before the 15 s
experiment, plus three changes that are not the experiment:

1. the **step-5 tuning** (a 32 768-token cap, `top_k = 0`, and the breaker moved to 36 000 in
   lockstep with it);
2. the **turn-log schema fix** — the response-side tool-call count, and the failure records now
   carrying their turn key;
3. the **corrected launch spec**, which is what makes the window's data about the real proxy rather
   than about a placeholder-keyed one.

It is **not** a first-byte-window experiment. The 15 s-specific bars of §3.1 do not apply to it: the
>=97% retry-firing bar was defined over 15 s stall rows, the 81-bound recovery bar is defined over
the attempts a 15 s window converts from success into stall, and there is no such conversion set
here. The cutoff itself remains **underived**; the dynamic window is a fallback, not an answer.

## 10.3 The joins available, and what may not be compared

| quantity | join | available? |
|---|---|---|
| per-attempt | same full `request_id` (ttfb half <-> completion half) | YES |
| per-turn | `loop_key` = `{session_id, loop_seq, round_index}` | **YES** — this is what the schema fix bought |

**The 15 s run's per-turn numbers and this window's per-turn numbers must not be compared.** The
15 s run's were not computed at all (§9.4): the failure records carried no turn key, and the
protocol's join was a per-second time bucket. This window's are the first per-turn measurements of
the proxy's behaviour that exist. They establish a baseline; they do not improve on one.

**A caveat on the per-ATTEMPT join, measured.** The turn log publishes each record's id through a
process-global slot, and the request path reads that slot back to learn "its own" id. Two
concurrent requests can therefore take each other's id, and an attempt's completion record can be
written under a different attempt's id. Measured on the live log: the closed 15 s window had 4228
`ttfb` ids against 3903 completion ids, of which 3903 held both halves, and **8 successful
attempts had no completion half at all** — the signature of a lost id. The current window shows the
same 8 over a much smaller sample.

So the per-attempt join is usable but not exact, at a rate of the order of 0.2% of attempts in the
closed window. Every row-level criterion in §10.4 is computed on row counts and is unaffected; the
two criteria that use the join (bars 3 and 4) carry a join uncertainty of about that size, which
is far below the distance to their bars. The defect is tracked separately; if it is fixed before
this window closes, the caveat becomes moot and this note should say so.

## 10.4 The criteria

Each bar cites its source. Where a bar is carried over from the 15 s protocol, that is stated,
because the metric's population is the same kind of quantity even though the window is not.

| # | metric | denominator (exact) | bar | source |
|---|---|---|---|---|
| 1 | ttfb p95 | `ttfb` rows in the window | <=19000 ms success, >21000 ms fail | A1 §1, carried from §3.1 |
| 2 | ttfb p50 | `ttfb` rows in the window | reported, no bar | — |
| 3 | terminal-504 share | >=15 s `ttfb` attempts joined to their completion half | <=43.9% **and** completion share >=53.5% | A1 §3, carried from §5.1 trigger 8 |
| 4 | `truncated_stream` share | >=15 s `ttfb` attempts joined to their completion half | <=2.5% | A1 §3 (4/157) — **this denominator, not a `ttfb` count**: `failure_mode` is only set on completion records, so counting it over `ttfb` rows is 0 by construction |
| 5 | **per-turn recovery rate** | turns (by `loop_key`) containing >=1 stall attempt | **>=25%** | carried from §5.1 trigger 2's failure bar: no window-specific baseline exists, and this is the same kind of quantity — a stalled attempt's turn recovering. Below 25% the rolled-back configuration is itself suspect |
| 6 | **per-turn salvage rate** | turns (by `loop_key`) containing >=1 stall attempt, that reach a clean completion (`clean_stop` / `tool_calls`) on a later attempt | **>=9.5%** | carried from §3.1's salvage bar, for the same reason. This is the FIRST measurement of it, not a comparison |
| 7 | request-side tool-call count vs response-side | `completion` rows | the two must not be conflated; the response-side distinct count is the response's | §9.7's trap, restated as a check |
| 8 | the step-5 cap is effective | `ttfb` rows for the enabled model | `request_max_tokens` == 32768 on every request | the tuning this window exists to check |
| 9 | the breaker is not tripped inside the cap | completions | no breaker trip on a response within 32768 tokens | the breaker and the cap must move together |

Bars 5 and 6 are the ones this window exists to produce. They are registered with a carried-over bar
rather than a derived one, and the report must say so — a bar that is not derived from the window is
a weaker instrument than one that is, and pretending otherwise would repeat the failure this
revision is correcting.

## 10.5 Minima (carried from §4.4, unchanged)

| minimum | bar | why |
|---|---|---|
| window length | >=24 h, covering a full diurnal cycle | the same reason as §4.4: the baseline window spans 22Z-09Z and has zero-traffic hours |
| `ttfb` rows | >=300 | the same |
| stall attempts | >=20 | enough for a share to be decidable at +/-1 row of granularity |

If a minimum is not met at 24 h, **extend the window; do not evaluate the criteria on a smaller
slice** (§4.4). The 15 s run closed early for a different and stronger reason — its failing bars
could not be reversed by waiting — and that reasoning does not transfer here.

## 10.6 The rollback trigger for THIS window

Any single one fires, and the response is to fix the configuration, not to extend the window:

1. terminal-504 share above 43.9% with a lower completion share (A1 §3);
2. per-turn recovery below 25% (§10.4 bar 5);
3. `truncated_stream` share above 2.5% on the §10.4 bar-4 denominator;
4. the circuit breaker tripping on a response within the model's 32 768-token cap;
5. `request_max_tokens` not equal to 32768 for the enabled model — the tuning is not live.

## 10.7 Reporting

One artifact after the window closes, as §6 requires: the per-hour table, the nine criteria in §10.4
with their verdicts, the minima as measured, and the commands that recompute every published number
against a named frozen snapshot with its line count and sha256. If a criterion is unmeasurable, the
artifact says so and says why — it does not substitute a proxy number and present it as the metric.

---

# 11. Rev 3.1 — the window that the deploy started (2026-10-21)

**Status: PRE-REGISTERED, while the window is still open.** Rev 3.1 changes **no threshold value**:
the 19000/21000 ms ttfb p95 band, the 43.9% terminal-504 baseline, the 53.5% completion baseline,
the 2.5% `truncated_stream` baseline, the 25% per-turn recovery floor and the 9.5% per-turn salvage
floor all stand exactly as rev 3.0 and rev 2.2 set them. **No result observed in the running window
has been examined**, and this revision is published before the window closes, so it is not a post-hoc
edit of a threshold. What it corrects is a *denominator statement*, the *binary under test*, and the
*reporting procedure* — and it restates the affected content in full, so this section can be read
alone.

## 11.1 The floor, the close, and the binary under test

| | |
|---|---|
| floor | `2026-10-21T14:50:43Z` — the restart that deployed the fixed binary |
| close | `2026-10-22T14:50:43Z` (>=24 h), or later if the minima are not met |
| binary | `dirmacs/nimaproxy:stage4-20261021`, revision `2e6b6e0` (label `org.opencontainers.image.revision`) |
| first-byte window | **dynamic** — the 15 s override is deleted, so the window is the per-model dynamic value (25 s) |
| enabled model | `stepfun-ai/step-5-preview`, configured cap 32768, `top_k = 0`, circuit breaker 36000 |
| log at the floor | 50,603,788 bytes, turn-log line 64,839 |

**Rev 3.0's window was voided by that deploy.** It ran from `2026-10-21T12:31:38Z` and was closed
at `2026-10-21T14:50:43Z`, so 2 h 19 m of it is discarded. The owner elected the deploy (the
rolled-back configuration and the step-5 tuning were live, and the fixes in PRs #42, #43 and #45
were not); the floor therefore moves to the deploy and the window runs its full 24 h from there.
**The 24 h minimum is not missed** — the floor moved with the deploy, and only the 2 h 19 m is lost.

## 11.2 The criteria, restated in full

| # | metric | denominator (exact) | bar | source |
|---|---|---|---|---|
| 1 | ttfb p95 | `ttfb` rows in the window | <=19000 ms success, >21000 ms fail | A1 §1, carried from §3.1 |
| 2 | ttfb p50 | `ttfb` rows in the window | reported, no bar | — |
| 3 | terminal-504 share | **all >=15 s `ttfb` attempts** | <=43.9% **and** completion share >=53.5% | A1 §3, carried from §5.1 trigger 8 |
| 4 | `truncated_stream` share | **>=15 s `ttfb` attempts joined to their completion half** | <=2.5% | A1 §3 (4/157) |
| 5 | per-turn recovery rate | turns (by `loop_key`) containing >=1 stall attempt | **>=25%** | carried from §5.1 trigger 2 |
| 6 | per-turn salvage rate | turns (by `loop_key`) containing >=1 stall attempt that reach a clean completion on a later attempt | **>=9.5%** | carried from §3.1 |
| 7 | request-side vs response-side tool-call count | `completion` rows | the two must not be conflated | §9.7's trap, restated |
| 8 | the step-5 cap is effective | `ttfb` rows for the enabled model | **no row EXCEEDS 32768, and the MAXIMUM effective `request_max_tokens` over the window equals 32768** | the tuning this window exists to check |
| 9 | the breaker is not tripped inside the cap | completions | no breaker trip on a response within 32768 tokens | breaker and cap move together |

**Bars 3 and 4 have different populations, and the numbers prove it.** Bar 3's denominator is every
>=15 s attempt: in the closed 15 s window, 317 terminal-504 + 55 clean completions + 2 truncations
= 374 = 317 + 57. So bar 3's 84.8% / 14.7% were measured over all 374 attempts, while bar 4's
2/57 = 3.5% is over the 57 of them that have a completion half. Rev 3.0 gave both the same
denominator string, which invited reading 84.8% against 3.5% as if they shared a base.

**Bar 8 is a ceiling, not an equality, and it has to be.** The binary under test is the first to
compute `min(caller's budget, 32768)`, so an individual row can legitimately read **below** 32768:
that is the caller's own budget, not a missing cap. A per-row equality check would fire the first
time any client in the fleet named a smaller budget, and send a healthy window into a false "the
tuning is not live". The deploy itself already produced such rows — `request_max_tokens` of 32768
on 38 rows, **16 on 3 rows and 512 on 1** — so the fleet does name its own budgets, and a smaller
row is data, not a fault. The check is that the window's maximum effective value is 32768 and no row
exceeds it.

## 11.3 The rollback triggers, restated in full

Any single one fires, and the response is to fix the configuration, not to extend the window:

1. terminal-504 share above 43.9% with a lower completion share (A1 §3);
2. per-turn recovery below 25% (§11.2 bar 5);
3. `truncated_stream` share above 2.5% on the §11.2 bar-4 denominator;
4. the circuit breaker tripping on a response within the model's 32 768-token cap;
5. **the maximum effective `request_max_tokens` for the enabled model either exceeding 32768 or
   falling below it** — a value above the cap is the ceiling not holding, and a maximum below it
   means no request in the window reached the configured ceiling, which is the tuning not being
   exercised.

## 11.4 Reporting, restated in full

One artifact after the window closes: the per-hour table, the nine criteria in §11.2 with their
verdicts, the minima as measured, and the commands that recompute every published number against a
named frozen snapshot with its line count and sha256. A criterion that is unmeasurable is reported
as unmeasurable, with the reason — never as a proxy number presented as the metric.

**The log rotates inside this window, and the snapshot procedure must account for it.** The logger
rotates at 64 MiB and renames the live file to `turns.jsonl.<YYYYMMDD_HHMMSS>`. The log was
50,603,788 bytes at the floor, so the rotation is expected inside the window — **re-measure the byte
count at the close rather than trusting an estimate**, because the rotation time depends on the
write rate, and because the counter is seeded from the file's existing size on open, so a restart
does not postpone it by a whole threshold.

`nimaproxy-query` opens exactly ONE file (`File::open(path)`, `log_query.rs:342` — no directory
walk, no glob), so it **cannot read the window across a rotation**. The snapshot must be a
concatenation.

**The order is the trap, and a count check alone does not catch it.** The live file's name is a
lexicographic prefix of the rotated names, so a naive `cat turns.jsonl*` puts the live file FIRST
and the rotated ones after — newest-first, and the window concatenates BACKWARDS in time.

Measured on a simulation built the way a real rotation leaves the files — **disjoint**, the renamed
file holding the older records and the fresh live file holding only the newer ones: two 20,000-line
rotated files and a 24,913-line live file.

| order | lines | first record | last record |
|---|---:|---|---|
| naive `cat turns.jsonl*` | **64,913** | 2026-10-19T04:37:51.974Z | **2026-10-19T04:37:51.820Z** (backwards) |
| rotated first by timestamp, live last | **64,913** | 2026-06-14T06:07:55Z | 2026-10-21T14:54:16Z (correct) |

**The wrong order has exactly the right line count**, so the count cannot see it. But its boundary
timestamps are visibly reversed — the first record is *newer* than the last — and that is catchable.
So the two checks catch different failures and both are required:

- **the concatenated line count equals the sum of the files' counts** — this catches a file that was
  OMITTED. A glob that matches nothing (a rotated name whose date is eight digits, not seven)
  drops the file silently and the count comes up short with no error anywhere;
- **the first record's timestamp is OLDER than the last's**, and the last is the live file's — this
  catches a REORDERING, which the count cannot see.

So the snapshot is taken by **copying** the files (not by reading them, which can race the
rotation), and the copies are concatenated in this order:

```bash
LOGDIR=/var/log/nimaproxy
SNAP=$LOGDIR/snapshot-$(date -u +%Y%m%dT%H%M%SZ).jsonl

# 1. Freeze by COPYING, not by reading: a read can race the rotation.
#    The rotated names are turns.jsonl.<YYYYMMDD_HHMMSS> (plus a .<n> suffix
#    when two rotations land in the same second). They sort chronologically
#    among themselves under LC_ALL=C. The LIVE file is named LAST ON PURPOSE:
#    it is a lexicographic prefix of the rotated names, so a glob would put
#    it first and the window would concatenate backwards in time.
mkdir -p "$SNAP.d"
# 2. Capture the BEFORE state BEFORE the copies — the guard compares
#    against it after, so capturing it after the loop is vacuous.
#    The live file is identified by INODE, not size: the log's growth rate
#    is regime-dependent, so size equality false-fires on every close, and it
#    can pass a rotation that lands inside the copy. Measured 2026-10-22
#    (nimakai#56): pre-rotation 2026-10-21T14:50:43Z floor 50,603,788 bytes
#    to 2026-10-22T09:51:48Z rotation 67,109,329 bytes = ~0.83 MB/h over
#    19.0 h; post-rotation 09:51:48Z to 14:50:43Z close = ~0.50 MB/h over
#    5.0 h (live file still writing, ~0.36 MB/h at 17:23Z). The old single
#    figure (~1.07 MB/h) matched neither regime and is withdrawn. An inode
#    change means the live path was replaced (rotation); same inode means the
#    same file, however much it grew.
BEFORE_ROTATED=$(ls -1 "$LOGDIR"/turns.jsonl.20* 2>/dev/null | LC_ALL=C sort)
BEFORE_INODE=$(stat -c '%i' "$LOGDIR"/turns.jsonl)
for f in $(ls -1 "$LOGDIR"/turns.jsonl.20* 2>/dev/null | LC_ALL=C sort); do
  cp -- "$f" "$SNAP.d/"
done
cp -- "$LOGDIR"/turns.jsonl "$SNAP.d/turns.jsonl.LIVE-last"
# Re-check that nothing moved while the copies were taken: if a NEW
# rotated file appeared, or the live path was replaced, re-copy.
AFTER_ROTATED=$(ls -1 "$LOGDIR"/turns.jsonl.20* 2>/dev/null | LC_ALL=C sort)
AFTER_INODE=$(stat -c '%i' "$LOGDIR"/turns.jsonl)
[ "$BEFORE_ROTATED" = "$AFTER_ROTATED" ] || { echo "FAIL: a rotation landed during the copy"; exit 1; }
[ "$BEFORE_INODE" = "$AFTER_INODE" ] || { echo "FAIL: the live file was replaced during the copy"; exit 1; }
# The copied set must equal the source set: the step-4 count check
# re-counts the copied files, so it cannot catch an omission on its
# own (SUM == GOT by construction when a file never made it over).
[ "$(ls -1 "$SNAP.d"/turns.jsonl.20* 2>/dev/null | LC_ALL=C sort | xargs -n1 basename | LC_ALL=C sort)" = "$(echo "$AFTER_ROTATED" | xargs -n1 basename | LC_ALL=C sort)" ] || { echo "FAIL: the copied set differs from the source set"; exit 1; }

# 3. Concatenate, rotated first and the live file LAST, and record a manifest:
#    every file, its line count, and the combined sha256.
{ ls -1 "$SNAP.d"/turns.jsonl.20* 2>/dev/null | LC_ALL=C sort
  echo "$SNAP.d/turns.jsonl.LIVE-last"; } | xargs cat > "$SNAP"
wc -l "$SNAP.d"/turns.jsonl.20* "$SNAP.d"/turns.jsonl.LIVE-last "$SNAP"
sha256sum "$SNAP"

# 4. BOTH checks, because they catch different failures:
#    (a) the concatenated count equals the sum of the files' counts
#        -> catches an OMITTED file (a glob that matches nothing drops one
#           silently, and the count comes up short with no error);
#    (b) the first record is OLDER than the last, and the last is the live
#        file's -> catches a REORDERING, which (a) cannot see, because a
#           reordered concatenation has exactly the right count.
#    A mismatch on either means THE SNAPSHOT IS INCOMPLETE AND MUST NOT BE
#    SCORED -- a window scored on a partial slice looks like a clean window
#    with a smaller denominator.
SUM=$(cat "$SNAP.d"/turns.jsonl.20* "$SNAP.d"/turns.jsonl.LIVE-last 2>/dev/null | wc -l)
GOT=$(wc -l < "$SNAP")
[ "$SUM" = "$GOT" ] || { echo "FAIL: a file was omitted ($GOT != $SUM); the snapshot is incomplete, do not score it"; exit 1; }
FIRST=$(head -1 "$SNAP" | python3 -c 'import json,sys; print(json.load(sys.stdin)["timestamp"])')
LAST=$(tail -1 "$SNAP" | python3 -c 'import json,sys; print(json.load(sys.stdin)["timestamp"])')
echo "first: $FIRST"
echo "last:  $LAST"
python3 -c "import sys; sys.exit(0 if '$FIRST' < '$LAST' else 1)" \
  || { echo "FAIL: the snapshot runs backwards in time; do not score it"; exit 1; }

# 5. The cut is taken on the SNAPSHOT's own timestamps, never on the live file.
nimaproxy-query "$SNAP" "record_kind=ttfb count"
nimaproxy-query "$SNAP" "error~no bytes within count"
```

**The query path over a concatenation is pre-flighted, not assumed.** Run against the disjoint
triple above: the correctly-ordered concatenation held 64,913 lines, its first record was dated
2026-06-14T06:07:55Z and its last 2026-10-21T14:54:16Z, and `nimaproxy-query` read it —
`"count"` returned 64,913 and `"record_kind=ttfb count"` returned 49,351 — so a concatenated
snapshot is readable by the tool the operator will actually use.

**The floor and close are taken on the snapshot's own timestamps**, never on the live file, and the
log's byte count is recorded at the floor and re-measured at the close.

## 11.5 One number this window produces that no window has produced before

With the binary under test, a row's effective `request_max_tokens` can be **below** 32768, because
that is now a client budget rather than a config value. So at the close the artifact must report
**the distribution of `request_max_tokens` below 32768** for the enabled model: how many rows, and
which budgets. That count is the fleet's real exposure to the new semantics, and it is the number
nobody has yet.

---

# 12. Rev 3.2 — trigger 5, split (2026-10-21)

**Status: PRE-REGISTERED, while the window is still open.** Rev 3.2 changes **no threshold value**
and **no result observed in the running window has been examined**. It restates §11.3's trigger 5
in full, because as written it could fire a rollback for a reason that is not a fault. This is a
new revision, not an edit of §11.3's published text, per §6.

## 12.1 The problem with trigger 5 as written

§11.3's trigger 5 reads: the maximum effective `request_max_tokens` for the enabled model
"either exceeding 32768 or falling below it" fires the rollback. The two directions are not the
same kind of finding, and the response to any trigger is "fix the configuration, not extend the
window" — which is the wrong response to one of them.

The reason is that **the log records the EFFECTIVE value only.** Under the deployed semantics
(`min(caller's budget, 32768)`) a client that asks for 65536 against a 16384 cap reads **16384** in
the log — indistinguishable from a client that asked for 16384. So a maximum below the configured
cap is ambiguous between two very different situations:

- the cap was **reverted** (a real fault, and the thing trigger 5 exists to catch), or
- the cap is **live** and the traffic simply never reached it (not a fault at all).

The log cannot separate those, because it never recorded the client's own request. **The config
can**: the live config states the enabled model's configured cap, and that is the arbiter.

## 12.2 Trigger 5, restated

**5. The step-5 cap.** Two conditions, and they are not one condition:

- **5a. The maximum effective `request_max_tokens` over the window EXCEEDS 32768 — a rollback
  trigger on its own.** The ceiling did not hold. A well-behaved client cannot produce this under
  `min(caller, 32768)`, so it is exactly what a broken ceiling looks like.
- **5b. The maximum effective `request_max_tokens` over the window is BELOW 32768 — NOT a rollback
  trigger by itself.** It is a finding to be resolved by comparing the live config's configured cap
  for the enabled model against the window's maximum. **If the config says 32768 and the maximum is
  lower, the cap is live and the traffic did not reach it, so bar 8 is reported as UNEXERCISED with
  the config verified — not rolled back.** If the config says something other than 32768, the
  tuning was reverted and the rollback applies on that ground.

This is the same disambiguation the protocol applies elsewhere: a row-level measurement cannot
distinguish "the thing did not happen" from "the thing did not happen because the thing was
changed", so the configuration file arbitrates and the row-level number is reported as what it is.

## 12.3 What this changes about the evidence

It does not change any threshold, and it does not change the verdict of the 15 s run — that verdict
rests on the two row-level bars, T4 and T8, neither of which uses the per-request value.

It does change one thing the running window will produce: a row whose effective
`request_max_tokens` is below 32768 is **the client's own budget**, not a missing cap. The deploy
already showed this: in the first minutes of the window, `request_max_tokens` read 32768 on 280
rows and below it on 11 (16 on 3, 512 on 1, and 3000 on 7 — the last of those a real client
naming its own budget, not the operator's verification traffic). Under rev 3.0's per-row equality
those 11 rows would have read as "the tuning is not live" and rolled back a healthy window within
minutes. Under §12.2 the window's **maximum** is the measurement, and the maximum is 32768.

One consequence of the new semantics is disclosed here rather than discovered at the close: a
client that names a small budget now RECEIVES that budget, and can therefore be cut off at its own
threshold rather than the proxy's. Measured so far: of the 7 rows at a 3000-token budget, **none**
is a `length_cutoff` — the three completions among them returned 1072, 1430 and 1072 tokens — so the
risk is identified and quantified rather than merely described, and it is the argument for keeping
the enabled model's cap at 32768 instead of lowering it to meet a client's ask.
