# SUCCESSOR WINDOW — design (pre-registration input)

**Status:** DESIGN, not yet a protocol revision. This document is written so it can be
promoted to the next revision of the proxy experiment protocol with minimal editing. Nothing
in it is a result; every number is either a baseline quoted from a published artifact or a
threshold with its derivation stated. The two numbers this design itself measured are labelled
as such and are labelled as PARTIAL-WINDOW SMOKE measurements, not as window results.

**Why this window exists.** The owner has decided a dedicated successor window WILL run after
the current 24 h window (floor `2026-10-21T18:11:43Z`, the stage-6 deploy) closes. The 15 s window that preceded it
failed its own rollback bar and was rolled back, leaving three questions open:

1. the **81-bound recovery** — of the attempts a shorter first-byte window would convert from
   success into stall, how many turns recover;
2. the **per-turn salvage** — of turns containing a stall attempt, how many reach a clean
   completion on a later attempt;
3. the **client/proxy TTFT ratio** — never joined.

The first two became answerable when PR #35 made every failure-path record carry its turn key
(`loop_key`), and the third became *specifiable* because the owner chose to instrument the client
side first. This design uses the join that now exists and specifies the instrumentation that will
make the ratio joinable.

**What this design is built to avoid.** The 15 s experiment failed for reasons unrelated to what
it was measuring. Section 4 is the analysis of that failure, and every bar in section 3 is
constrained by it.

---

## 0. The central lesson from the 15 s window (read this first)

The 15 s window changed one live knob — `[timeouts] first_byte_timeout_ms` 25000 → 15000 — and
then measured the system. Three of its pass/fail bars were functions of that same knob:

| bar | measured | bar value | what it was actually a function of |
|---|---:|---:|---|
| retry firing after a 15 s stall | 285/309 = 92.2% | ≥97% | the knob: a shorter window changes *which* rows are "15 s stalls" and the give-up path's arithmetic |
| terminal-504 share of ≥15 s attempts | 317/374 = 84.8% | ≤43.9% | the knob: it is the share of the attempts the knob itself created |
| completion share of ≥15 s attempts | 55/374 = 14.7% | ≥53.5% | the knob, same reason |

Two of the three fired, so the run was rolled back — and the two questions the run existed to
answer (81-bound recovery, per-turn salvage) were **unmeasurable**, because the failure records
carried no turn key. The result: the change was rolled back, its central questions were not
answered, and the bars that decided the verdict were coupled to the change rather than to the
question.

That is the design constraint. A window whose pass/fail bars are computed over a population the
manipulated variable defines is measuring the manipulation, not the question. The successor is
built on three rules:

- **R1. The window measures the system, not a knob.** The successor changes **no timeout, no
  budget, no retry, and no model parameter**. Its population is the traffic the live
  configuration produces, and its bars are either (a) carried baselines from a published
  artifact, or (b) self-consistency and integrity gates that a defect in the *measurement
  machinery* trips, not a configuration change.
- **R2. Every bar has a named denominator and a named population, printed with the number.** The
  15 s run's worst moment was a "0.00% PASS" on a truncated_stream share computed over `ttfb`
  rows, where `failure_mode` is 0 by construction. `measure.py` prints the denominator beside
  every figure for exactly this reason.
- **R3. No bar may be unmeasurable-by-construction.** A bar whose population the window cannot
  construct is not a bar; it is a promise. Each bar in section 3 names the join it needs and
  states, with a measured number, that the join exists.

---

## 1. The three questions, stated precisely

All three are computed from the proxy's turn log (`<turn-log-path>`) over the
window slice `floor <= timestamp < ceil`. The schema and layout of the log are unchanged by this
design.

### 0.1 A population the log does NOT contain (measured; it changes what the window must report)

**The turn log does not record admission rejections.** A request rejected at the gateway's
admission stage — because no upstream permit was free, or because no key was available — is
returned to the client as **503 "gateway overloaded; retry later"** or **429 "all API keys
rate-limited"** and **never reaches a `log_turn_request` call**, so it produces **no row at
all** — no `ttfb` row, no `completion` row, and therefore no `loop_key`.

The source is explicit: `chat.rs` does

```rust
match acquire_gateway_permits_excluding(&state, failed_key_idx).await {
    Ok(permits) => permits,
    Err(response) => return response,   // returns BEFORE the first log_turn_request
}
```

and the first `log_turn_request` call site is ~50 lines later.

**Measured on the deployed window** (floor `2026-10-21T18:11:43Z`, the proxy's own
`/stats` gateway counters against the log's row counts):

| population | count | visible to the log? |
|---|---:|---|
| `ttfb` rows in the log | 1201 | yes |
| `request_total` (client requests) | 3569 | — |
| `upstream_attempts` | 1570 | — |
| `overload_rejects` (503) | 1178 | **no** |
| `no_key_rejects` (429) | 1154 | **no** |
| `admission_rejects` (503 + 429) | **2332** | **no** |
| `rate_limit_count` (upstream's own 429s) | 369 | **no** (counter only) |
| `timeout_count` (first-byte timeouts) | 49 | yes (the log's 504 rows) |

So **2332 admission rejections produced no row in the log** — roughly **twice** the log's
entire 1201-row attempt population.

**This is a fleet-level condition, not a measurement curiosity.** The requests that never
reached the upstream are **1999 of 3569 client requests = 56.0%**; the turn log describes
the **44.0%** that got through. The mechanism is measured, not inferred: an upstream 429
calls `mark_rate_limited(key, 60)`, benching that key for 60 s (`key_pool/state.rs`;
called from `chat.rs:353`, `http.rs:408` and `:618`, `racing.rs:751`), and with 4 keys
benched **in turn** the pool frequently has **0–2 usable keys**. Sampling the live key
pool every 5 s for 60 s: **zero active keys in 5 of 12 samples**, effective
`max_in_flight` of 0–2 against the configured `max_upstream_in_flight = 16` and
`max_in_flight_per_key = 4`. A quiet 90 s interval with no traffic from the analyser
still produced **11 rejects** and 4 upstream 429s, and the reject counters rose steadily
across the session at ~4.6/min — so this is standing traffic, not a burst artefact.

**The two populations are not a partition of each other**, and the window must not present
them as one: `request_total` 3569 < `upstream_attempts` 1570 + `admission_rejects` 2332 =
3902, because one client request can be refused at admission several times (the retry loop
re-enters admission). §2.7 gives the metrics and the denominator.

**The mechanism, measured: AIMD holds the per-key windows below their ceiling.** The
`/stats` per-key view shows every key *configured* with `max_in_flight = 4` while the
*live* windows sat at `[4, 1, 4, 2]` and later `[4, 2, 4, 2]` — an effective capacity of
7–12, not the configured 16, varying per key and over time. The cause is in
`key_pool/state.rs`: a 429 does `window.current = (window.current / 2).max(1)` — one
event, instantaneously — while regaining a slot needs `ceil(AIMD_SUCCESS_THRESHOLD /
deficit)` **successes** (`AIMD_SUCCESS_THRESHOLD = 8`; a key at window 1 needs 8
successes to return to 4, at window 2 it needs 5). The penalty is one event; the reward
is earned over many.

**The upstream's 429 rate is 22.6%, and the AIMD constant is calibrated for ~5%.**
Measured since the deploy: `upstream_attempts` 2562, `rate_limit_count` 580 → **22.64%**.
The code's own comment says "with ~5% of admitted requests hitting a 429, keys sat
throttled for long stretches"; the measured rate is 4.5× that, so the controller's
setpoint is unreachable and the windows are held down permanently.

**Why this matters for the window's options.** The binding constraint is the **sum of the
per-key windows**, not the global permit: raising `max_upstream_in_flight` above that
sum would simply never bind. So the config comment's remedy ("raising the window
converts overload_rejects into upstream 429s") does not describe this failure mode. The
options are (a) reduce the upstream's 429 rate — the primary cause, and outside the
proxy; (b) make AIMD recovery cheaper, which is a **code** change needing its own window
and its own measurement; (c) raise `max_in_flight_per_key`, which raises the ceiling the
windows fall from — but the measured 429 rate says the ceiling is not what binds.

**This is not the proxy shedding load by its own admission policy; the upstream is
throttling it.** The proxy's own shedding is a *separate* pair of counters —
`overload_rejects` (503) and `no_key_rejects` (429 "all API keys rate-limited") — and
`rate_limit_count` is the upstream's. The three are counted separately, so the chain is
unambiguous: upstream throttles → a key's window halves and it is benched 60 s
(`chat.rs:345-355`, `retry_after` defaults to 60) → the windows' sum falls below the
global permit → admission refuses → **those refusals write no turn-log row**, so the
largest failure population is the one population the log cannot see.

**Three consequences the window must respect.**

1. **The log understates the client-visible failure rate.** The requests that failed
   *fastest* — the ones the fleet felt — are exactly the ones with no row. Any statement of
   the form "N requests failed" computed from the log alone is wrong by the shed-load
   count.
2. **The per-turn join is missing those turns too.** A rejected request carries no
   `loop_key`, so a turn whose *only* attempt was admission-rejected is not in the turn
   population at all. Q1 and Q2's denominators are therefore the *log-side* turn
   population, which excludes turn-level losses entirely. This is stated rather than
   patched: the design does not claim to measure them, and §2.7 adds the counter-side
   figures so the reader can see the size of the gap.
3. **The `rejected` class in §2.3a is a DIFFERENT population, and the class mapping must be
   read from the code, not assumed.** `orphan_class()` tests `error` **first**, so a 504
   carrying `"upstream sent no bytes within 25000ms"` is classified **`failure`**, not
   `rejected`; only a 502 with an empty `error` is `rejected`. Measured on ONE pinned
   snapshot of the deployed window (so the figures are not read at different moments from
   a growing log): 1146 rows at status 200 with no error, **50** at status 504 **with** an
   error (→ `failure`), **4** at status 502 with an empty error (→ `rejected`), and the
   completion-less classes come out `{failure 50, non_streaming 8, rejected 4,
   streaming_orphan 1}` = 63 total. So the "46× 504 + 3× 502" figures that appear in
   earlier notes are **not** the `rejected` class, and any factor computed from them is
   wrong. Against the correct class the factor is: the log's **4** `rejected` rows versus
   the **2332** admission rejects, i.e. the log understates the shed-load by ~**583×**;
   against the log's *whole* attempt population (1201 rows) it is ~1.94×. The two factors
   differ by two orders of magnitude, which is exactly why the denominator must be named.

**What the window must therefore do at the close.** It must read the gateway counters and
report them **beside** the log-side metrics, as a second population:

| metric | source | note |
|---|---|---|
| `upstream_attempts` | `/stats` → `gateway` | the log-side attempts, for cross-check |
| `overload_rejects` | `/stats` → `gateway` | 503s — the load-shedding path |
| `no_key_rejects` | `/stats` → `gateway` | 429s — the no-key path |
| `rate_limit_count` | `/stats` → `gateway` | the **upstream's** own 429s, which the proxy did not cause |
| `admission_rejects_total` | derived | `overload_rejects + no_key_rejects` |

**The counters are cumulative since process start, so the window must take a DELTA across
its floor and ceiling**, not a raw reading. The floor happens to be a process start in
this window (the deploy), so the delta equals the raw reading *here* — but that is a
property of this window, not a general one, and the close script must compute the delta.

**The `/stats` endpoint is the only one that carries these.** Probed on the live proxy:
`/metrics`, `/gateway/metrics` and `/prometheus` all return 404; `/stats` returns the
counters. A close script that assumes a Prometheus endpoint will read nothing and report
a zero.

**This is a measurement-scope finding, not a defect to fix in this window.** Adding a
turn-log row to the admission-reject path would be a behaviour change to a live proxy
mid-window. It is recorded for the successor window's own binary, where the row can be
added before the window's floor.

### Q1 — The 81-bound recovery

> **Question.** Of the attempts a 15 000 ms first-byte window would have converted from a success
> into a stall, what fraction of the turns that contain them recover — i.e. reach a clean
> completion (`clean_stop` or `tool_calls`) on a later attempt of the same turn?

- **Exact quantity.** `R = |{t ∈ T_band : t reaches a clean completion after its first stall}| / |{t ∈ T_band : t contains a stall}|`, where `T_band` is the set of turns containing at
  least one `ttfb` attempt with `15000 ≤ latency_ms < 25000`.
- **Why that band.** A1 §6a split the 178 then-successful [10 s, 25 s) attempts into 97 in
  [10 s, 15 s) and **81 in [15 s, 25 s)**: the 81 are precisely the set a 15 s window converts.
  The live first-byte window is dynamic and currently reads 25 000 ms, so the band
  `[15000, 25000)` is the live-window analogue of the 15 s protocol's bound. The window does not
  have to re-run the 15 s cut to measure the recovery property of that population: the property
  is a property of the *turns*, and the turns are in the log.
- **The population is real, and it is measured.** On the partial window as of
  `2026-10-21T18:26Z` (the **stage-4** slice: floor `2026-10-21T14:50:43Z`, ceiling `2026-10-22T23:59:59Z`, live log
  read directly, snapshot not frozen): **113 attempts in the band over 109 turns, 15 of those
  turns also containing a stall attempt, all 15 with a keyed stall**. The band is populated and
  the join exists. This is a smoke measurement, not a result.

### Q2 — The per-turn salvage

> **Question.** Of the turns that contain at least one stall attempt, what fraction reach a clean
> completion on a later attempt of the same turn?

- **Exact quantity.** `S = |{t ∈ T_stall : t reaches a clean completion after its first stall}| / |T_stall|`, where `T_stall` is the set of turns containing at least one `ttfb` attempt
  whose `error` contains `no bytes within`.
- **Population.** `T_stall`, all record kinds, joined on the `loop_key` object canonicalised as
  JSON with sorted keys. The denominator is turns, not attempts and not rows.
- **Same smoke measurement.** 110 stall attempts over 93 turns; **110/110 stall attempts carry
  their turn key**, 0 unkeyed. The population the 15 s run could not build is now buildable.

### Q3 — The client/proxy TTFT ratio

> **Question.** For a request the client can name, what is the ratio of the client's own
> time-to-first-token to the proxy's measured time-to-first-byte, and what is its p95?

- **Exact quantity.** `ratio(request) = client_ttft_ms / proxy_ttfb_ms`, matched per request;
  reported as a distribution with its p95 over the matched set.
- **Status: NOT COMPUTABLE in this window, by the owner's decision, and this design says so
  rather than shipping a proxy number.** The log records the proxy's clock only. The owner's
  decision (cutover record, owner decision 4) is: instrument the client side first, join in a
  later window. Section 7 specifies that instrumentation.
- **What the instrumentation work found (measured, §7.0).** The client's `ttft` **does** exist,
  per assistant message, in the session transcripts — so the client-side half of Q3 needs no
  new measurement. What does **not** exist is the **join key**: the proxy's `request_id` is
  only in a response **header**, and the transcript records the body and metadata, not
  headers. Of 37 472 `provider == "nimaproxy"` records, **zero** carry a proxy request id.
  The cheapest fix is a **turn-log field** carrying the response body's `id` — the key the
  client already records — which the proxy does **not** parse today on either path: the
  top-level frame id and the top-level body id must BOTH be extracted, which is new work
  rather than a reuse. That is a **binary change**, so it must be deployed **before** this
  window's floor, or the window splits across two binaries.
- **So Q3's status depends on a decision that is not yet made**, and the design says which
  decision: either the log field is added before the floor (and Q3 becomes computable in
  this window), or it is not (and Q3 stays NOT COMPUTABLE, with the reason). **This window
  changes no timeout, no budget, no retry and no model parameter** (R1) — and a log field is
  none of those, but it *is* a binary change, so it moves the floor and therefore interacts
  with the sequencing in §5.2. The window's own bars are unaffected either way.

---

## 2. The metrics, exactly

Each metric names its record kinds, its fields, and its join key. `measure.py` implements all of
them; nothing below is unimplemented.

### 2.1 Row-level (no join)

| metric | record kind | fields | definition |
|---|---|---|---|
| `ttfb_rows` | `ttfb` | — | count of `ttfb` rows in the slice; the only valid denominator for a success/failure rate (turn_log.rs's denominator rule) |
| `ttfb_p50_ms`, `ttfb_p95_ms` | `ttfb` | `latency_ms` | nearest-rank percentiles, `ceil(p/100·n)` 1-based — the same definition the 15 s results artifact used, so the numbers stay comparable |
| `stall_rows` | `ttfb` | `error` | rows whose `error` contains `no bytes within` |
| `retry_firing_pct` | `ttfb` | `error` | share of stall rows whose `error` also contains `retrying on another key` |
| `terminal_stalls` | `ttfb` | `error` | stall rows without the retry string |
| `truncated_stream`, `length_cutoff` | `completion` | `failure_mode` | counts. **`failure_mode` is only ever set on `completion` rows** — 0 of the slice's `ttfb` rows carry one — so these are counted over completions and never over `ttfb` |
| `ge15_ttfb_attempts` | `ttfb` | `latency_ms` | attempts with `latency_ms ≥ 15000` |
| `unkeyed_stall_rows` | `ttfb` | `error`, `loop_key` | stall rows with `loop_key` absent. This is the number that must be **0** for Q1 and Q2 to be computable at all |
| `request_max_tokens` distribution | `ttfb` | `request_max_tokens` | reported, because the effective value is `min(caller, cap)` and a row below the cap is the client's budget, not a defect |

### 2.2 Per-attempt join (full `request_id`)

`request_id` is `{YYYYMMDDHHMMSS}.{pid}.{seq}` with `seq` unique **per record** — so the id
identifies a record, and the ttfb half and the completion half of one attempt share it. That is
the per-attempt join. It is **not** a turn join, and the 15 s protocol's "group by the first two
dot-parts" was a per-second time bucket; this design does not use it and does not revive it.

| metric | denominator | definition |
|---|---|---|
| `ge15_joined` | ≥15 000 ms `ttfb` attempts | of those, the ones with a `completion` half under the same `request_id` |
| `ge15_truncated`, `ge15_truncated_share_pct` | `ge15_joined` | `failure_mode == "truncated_stream"` on the joined set. This is the invariant's own denominator, not a `ttfb` count |

### 2.3 Id-join integrity (the gate)

The request-id race (issue `dirmacs/nimakai#44`, fixed in PR #48) let two concurrent requests take
each other's id, so an attempt's completion record could be written under another attempt's id.
PR #48 removed the process-global slot. The window must *verify* that, not assume it.

**The population the join can possibly cover.** A `completion` row is written **only** on the
streaming path. A non-streaming request writes a `ttfb` row carrying its usage and **no**
completion row; a failed or rejected request writes an error row and no completion row. So
those ids **can never** hold a completion half, by design, and they are not evidence of a lost
id. The gate is therefore computed over the **eligible** population — ids whose `ttfb` half was
a *successful* attempt — and never over all `ttfb` ids.

**This was measured, and the distinction is load-bearing** (post-deploy window, floor
`2026-10-21T18:11:43Z`, live log):

| era | ttfb ids | raw coverage (all ttfb ids) | max reachable | eligible coverage |
|---|---:|---:|---:|---:|
| 15 s window (race live) | 9388 | 91.96% | 92.01% | 8633/8638 = **99.94%** |
| pre-deploy 3.3 h (race live) | 1143 | 89.68% | 89.68% | 1025/1025 = **100.00%** |
| post-deploy (race fixed) | 313 | 92.01% | 92.65% | 288/290 = **99.31%** |

The raw coverage tracks the *ceiling* set by the traffic mix, not the join's health: the
pre-fix and post-fix readings differ by 0.05 pp while the race was present in one and absent
in the other. A coverage metric over all `ttfb` ids therefore **cannot discriminate** the race's
presence from its absence, and the §3.3 bar built on it is unreachable by construction. The
eligible-population coverage is the instrument that discriminates.

| metric | denominator | definition | why it is the right instrument |
|---|---|---|---|
| `ttfb_ids`, `completion_ids` | — | distinct `request_id` by record kind | the two populations the join relates |
| `ttfb_ids_by_class` | distinct `ttfb` ids | `failure` / `rejected` / `unsuccessful` / `non_streaming` / `streaming_orphan` (see §2.3a) | classifies every completion-less id by **cause**, so the raw total can never be quoted without its parts |
| `eligible_ids` | distinct `ttfb` ids | ids whose `ttfb` half was a **success** (`streaming_orphan` class) | the only ids a completion half is *possible* for — the gate's denominator |
| `eligible_ids_with_completion_half` | `eligible_ids` | eligible ids that do have a `completion` row | join coverage over the population where the join is meaningful |
| `lost_successful_ids_mature` | `eligible_ids` | eligible ids with **no** completion half, older than the detached writer's bound | the race's victim signature. **Mature** means older than `idle_window * 20 + 60 s` (3660 s at the live 180 s idle window) |
| `lost_successful_ids_in_flight` | `eligible_ids` | eligible ids with no completion half, **younger** than that bound | **not** a defect: the detached completion writer may still be waiting. Measured: 2 of the post-deploy window's 2 orphans were 0.3 and 0.0 minutes old, and one resolved within minutes |
| `integrity_pct_eligible_with_both_halves` | `eligible_ids` | eligible coverage as a percentage | the number the gate in §6 is written against |
| `ids_carrying_2plus_completion_records` | distinct ids | ids with **two or more** `completion` rows | the race's **thief** signature, and it is the sharp one: no legitimate path writes two completions under one id. Measured 0 in every era |
| `ids_carrying_3plus_records` | distinct ids | ids with more than two records | retained as a structural invariant, but it is **not** the thief signature: a stolen id carries one extra record, so 3+ is the wrong threshold — the thief shows up at **2+ completions** |

#### 2.3a The four causes of a completion-less id

A `ttfb` id with no completion half has **four** causes, and only one of them is a lost id:

| class | cause | can it ever hold a completion half? |
|---|---|---|
| `failure` | the attempt errored (a stall, or any other error) — a failure record has no completion | no, by design |
| `rejected` | `status_code != 200`: the request was refused upstream and never streamed | no, by design |
| `non_streaming` | usage is on the `ttfb` row; a non-streaming request never writes a completion | no, by design |
| `streaming_orphan` | a **successful streaming** request whose completion half is missing | **yes** — this is the only #44 candidate, and only when mature |

A metric that reports the raw `ttfb_ids_with_no_completion_half` without this split is
uninterpretable: the total is dominated by the three by-design classes, and the "0.2% rate"
style figure that follows from it overstates the defect rate by roughly 4×.

### 2.4 Per-turn join (`loop_key` object)

`loop_key` is `{session_id, loop_seq, round_index}`. It is canonicalised as JSON with sorted keys,
so two records of the same turn join regardless of field order in the file. `loop` summary rows
are counted for reconciliation and are **not** folded into the turn population — turn_log.rs's
denominator rule says a loop summary is not a model call.

**Ordering rule.** "Recovery" and "salvage" are ordered quantities. A clean completion recorded
*before* a turn's first stall is not a recovery from it. This is stated here because an unordered
join would inflate both metrics.

**The rule as implemented, exactly.** Both shipped implementations keep the turn's **EARLIEST**
clean completion (the first `clean_stop` or `tool_calls` completion by timestamp) and count the
turn as recovered only when that earliest clean completion is **not earlier than** the turn's
first stall. In other words: *a turn is recovered when its earliest clean completion did not
precede its first stall.*

**The other reading, and its effect.** The wording in §8 below ("reaching a clean completion
after their first stall") can also be read existentially — *the turn reached SOME clean
completion after its first stall* — which would additionally count a turn that completed cleanly
both before and after its stall. The two readings differ on such turns only. Measured on the
live window's log:

| metric | implemented rule (strict) | existential reading | difference |
|---|---|---|---|
| Q1 band recovery | 20/21 = 95.24 % | 21/21 = 100 % | 1 turn |
| Q2 per-turn salvage | 123/190 = 64.74 % | 128/190 = 67.37 % | 5 turns |

Every figure quoted in this document is the **implemented (strict)** rule, because that is what
`measure.py` and the close script both compute. **Both readings pass both bars**, so no verdict
in this document depends on the choice; the strict rule is reported because it is the one that
produced the numbers. The design's wording is kept as the specification of the *intent*, and the
implementation is the stricter of the two — a discrepancy worth stating rather than hiding.

| metric | denominator | definition |
|---|---|---|
| `turns_total` | — | turns with ≥1 `ttfb` attempt in the slice |
| `turns_with_a_stall_attempt` | `turns_total` | `T_stall`; the denominator for both Q2 and Q1's recovery |
| `turns_with_a_keyed_stall_attempt` | `turns_with_a_stall_attempt` | the joinable subset; must equal the previous, or the schema fix has regressed |
| `salvage_turns`, `salvage_rate_pct` | `turns_with_a_stall_attempt` | turns whose **earliest** clean completion is not earlier than their first stall (the rule as implemented, above) — **Q2's answer** |
| `recovered_turns`, `recovery_rate_pct` | `turns_with_a_stall_attempt` | turns reaching *any* completion after their first stall — reported beside salvage, because "recovered" and "salvaged cleanly" are different claims |
| `stall_attempts_carrying_a_turn_key` | `stall_attempts_total` | the join's coverage over the population that matters |
| `error_rows_with_turn_key` | `error_rows` | the same coverage over every error row, which is the schema fix's actual contract |

### 2.5 The 81-bound band (Q1)

| metric | denominator | definition |
|---|---|---|
| `attempts_in_band` | **success** `ttfb` rows | `15000 ≤ latency_ms < 25000` — see the exclusion note below |
| `turns_with_an_attempt_in_band` | — | `T_band` |
| `turns_with_a_band_attempt_and_a_stall_attempt` | `T_band` | the band turns that actually stalled under the live window |
| `band_turns_that_stalled_and_reached_a_clean_completion` | the previous | the recovery numerator |
| `recovery_pct_of_band_turns_with_a_stall` | the previous | **Q1's answer** |

**The band excludes stall rows, and that exclusion is load-bearing.** The band is "the set a
15 000 ms first-byte window would have converted **from success** into stall", so it is
identified from **success** rows only. A stall row's `latency_ms` is the **window that
fired**, not a first-byte latency — and under a 15 s window that value is `15xxx`, which
falls **inside** `[15000, 25000)`. Measured across the log, the stall rows carry 944 distinct
`latency_ms` values from 15 228 to 30 733, hundreds of them inside the band.

So a band test that does not exclude stalls explicitly counts already-**failed** attempts as
conversions. The defect is **invisible in the live 25 s window** — there a stall's value is
25 000+, outside the half-open band — and it would unmask itself the moment the timeout
changed, which is exactly what a window that studies a different timeout does. **A metric
whose correctness depends on the value of the knob it is measuring is not a measurement.**

Both implementations (`close-window-20261021.sh` and `measure.py`) now exclude stall rows
explicitly, with the reason in the code. On the data that exists the change is
**correctness-not-impact** (the band is 83 turns on the live window and 494 on the 15 s-era
slice, with and without the guard, because every turn holding a `15xxx` stall also holds a
band success). The guard was proven with a plant: a stall row with `latency_ms = 15500`
under a unique `loop_key` enters the band under the unguarded test (84 turns) and is
correctly excluded under the guarded one (83).

### 2.6 What `measure.py` reports it cannot compute

Printed explicitly, so nobody mistakes an absent number for a zero:

- the client-side TTFT and therefore the client/proxy TTFT ratio — the client clock is not
  instrumented (section 7);
- the 81-bound for a 15 s cut that is not live — the band population above is its live-window
  analogue, and the design does not pretend otherwise;
- **the client-visible failure rate**, from the log alone. The log cannot see the
  admission-rejected requests (§0.1), so the log-side failure count is a floor, not the
  rate. The counter-side figures below are what close the gap, and the window publishes
  both.

### 2.7 The counter-side population (the log's blind spot, measured)

These are read from the proxy's `/stats` endpoint, **not** from the log, and they are a
**second population** — the requests the log never saw. They are reported beside the
log-side metrics, never mixed into them.

| metric | source | definition |
|---|---|---|
| `request_total` | `/stats` → `gateway` | **client** requests, counted once each — the population the client actually experiences |
| `upstream_attempts` | `/stats` → `gateway` | upstream calls made; a retried request makes several, so this is **not** the client-request count |
| `overload_rejects` | `/stats` → `gateway` | **503** "gateway overloaded" — the permit path |
| `no_key_rejects` | `/stats` → `gateway` | **429** "all API keys rate-limited" — the no-key path |
| `rate_limit_count` | `/stats` → `gateway` | the **upstream's** 429s — a different cause, not the proxy's shedding |
| `timeout_count` | `/stats` → `gateway` | first-byte timeouts — the population the log *does* see |
| `admission_rejects_total` | derived | `overload_rejects + no_key_rejects` — the refusals with no log row |
| `admission_rejects_per_client_request` | derived | `admission_rejects_total / request_total` |
| `upstream_429_rate` | derived | `rate_limit_count / upstream_attempts` — the upstream's throttling rate, which is what drives the AIMD shrink |
| `key_window_sum` | `/stats` → `keys[].max_in_flight` | the **effective** per-key capacity, summed — the binding constraint, not the global permit |
| `key_windows_configured` | `/stats` → `keys[].configured_max_in_flight` | the ceiling the windows fall from, for comparison with the sum above |
| `active_keys` | `/stats` → `keys[].active` | how many keys are usable at all |

**The counters are NOT a partition of `request_total`, and the window must not treat them
as one.** `record_request` is called **once** per client request, *before* the retry
loop; `acquire_gateway_permits` is called **per attempt** *inside* it. So one client
request can be refused at admission several times, and `upstream_attempts +
admission_rejects` legitimately **exceeds** `request_total`.

Measured on the deployed window: `request_total` 3485, `upstream_attempts` 1513,
`admission_rejects` 2294 — so 3807 > 3485, and the "rate"
`admission_rejects / upstream_attempts` comes out at **151.9%**, which is an artefact of
the wrong denominator, not a real rate. Against `request_total` the same 2294 is
**65.8%**. **`request_total` is the denominator; `upstream_attempts` is not.**

**All eight are DELTAS across the window's floor and ceiling**, because the counters are
cumulative since process start. A raw reading charges the window with the previous
process's rejects.

**The floor and ceiling readings are taken from the live proxy at the moment they are
needed**, and the close script must record them with their timestamps — otherwise the
delta is not reproducible after the container is replaced.

**These are reported, not scored.** No bar in §3 is computed over them, because there is
no published baseline for the admission-reject rate and inventing one would be a guess.
They are reported so the reader can see the size of the log's blind spot next to the
figures the log does support.

---

## 3. The bars, and where each threshold comes from

Every bar below is either carried from a published artifact (named) or derived from a measurement
this design specifies. No bar is a guess. Where a bar cannot yet be derived, that is said and the
derivation measurement is part of the design.

### 3.1 The three questions' bars

| # | metric | bar | derivation |
|---|---|---|---|
| B1 | per-turn salvage rate (Q2) | **≥ 9.5%** to pass, **< 9.5%** to fail | Carried from the 15 s protocol §3.1, where it was `6/63 = 9.5%` (A1 §4). It is the only published baseline for this quantity. Rev 3.0 already flagged it as a *carried* bar rather than a derived one, and this design keeps that flag: **the successor window's salvage measurement is the FIRST measurement of it, so its pass/fail is against a baseline taken under a different binary and a different window.** That is stated rather than hidden, and §3.4 explains what the design does about it |
| B2 | 81-bound band recovery (Q1) | **≥ 50%** to pass, **< 25%** to fail | Carried from the 15 s protocol §3.1: A1 §3 measured that a slow first byte is "a coin flip, not a death sentence" — 53.5% of ≥15 s first-byte arrivals still complete, 43.9% go terminal-504. 50% is the midpoint of that baseline and 25% is its lower edge. The band population is the same kind of quantity the 15 s protocol defined, so the bar transfers with the population |
| B3 | client/proxy TTFT ratio p95 (Q3) | **≤ 13.31×** to pass, **> 13.31×** to fail | Carried from A6 §2, the published per-request matched baseline. **Not evaluated in this window** — the client is not instrumented (owner decision 4). It is registered here so the next window inherits it, and §7 says what must exist for it to be evaluable |

**What makes B1 and B2 falsifiable rather than decorative.** Each has an outcome space with a
named reading, stated in §8. A bar that can only be passed is not a bar.

### 3.2 The integrity gate (not a question, a precondition)

| # | metric | bar | derivation |
|---|---|---|---|
| G1 | `error_rows_without_a_turn_key` | **must be 0** | The schema fix's contract is that every failure-path record carries its turn key. Measured **0** on the partial window (145/145 error rows keyed). A non-zero value means Q1 and Q2 are not computable, and the window's honest verdict on both is UNMEASURABLE, not a pass and not a fail |
| G2 | `ids_carrying_2plus_completion_records` | **must be 0** | The #44 race's **thief** signature: no legitimate path writes two completions under one id. Measured **0** in every era measured (15 s window, pre-deploy, post-deploy). A non-zero value means PR #48's fix is not holding in this binary and the per-attempt join is contaminated |
| G3 | `integrity_pct_eligible_with_both_halves` | **≥ 99.9%** | Derived in §3.3, over the **eligible** population (successful streaming attempts), not over all `ttfb` ids |
| G4 | `lost_successful_ids_mature` | **must be 0** | The race's **victim** signature: a successful streaming attempt whose completion half is missing, older than the detached writer's 61-minute bound. A mature lost id is a defect by itself; measured 3 in the 15 s window, 0 in the pre-deploy and post-deploy eras |

**G4 is "must be 0" and not a rate because the eligible population is large.** At the live
traffic volume a mature lost id is a per-million event, so a *percentage* bar either fires on
noise or is loose enough to miss a reintroduction. Measured: 3 mature lost ids in 8638 eligible
(0.035%) with the race live, and 0 in 1315 eligible with it fixed. The absolute count separates
those cleanly; a 0.5% rate bar would have passed in both eras and so discriminates nothing.

### 3.3 Deriving G3 and G4, because "99%" is not a number until it is measured

**The first version of this derivation was wrong, and the measurement that showed it is
recorded here rather than replaced.** It read the 15 s window's `3903/4228 = 92.3%` raw
coverage as the pre-fix baseline and set G3 at ≥99% on the reasoning that a reintroduction
"would drag coverage back toward 92%". Two independent measurements falsify that:

1. **The 99% bar is unreachable by construction.** A `completion` row is written only on the
   streaming path, so ids in the `failure`, `rejected` and `non_streaming` classes can never
   hold a completion half. The **max reachable** raw coverage is therefore set by the traffic
   mix, not by the join's health. Measured ceilings: 92.01% (15 s window), 89.68% (pre-deploy),
   92.65% (post-deploy). A 99% bar could not have passed on **any** of them, so it was not a
   bar — it was a guaranteed failure.
2. **Raw coverage cannot discriminate the race's presence from its absence.** The pre-fix
   reading is 91.96% and the post-fix reading is 92.01% — a 0.05 pp difference — while the
   race was present in one and absent in the other. The metric moved with the traffic mix, not
   with the defect.

**The corrected instrument, and its derivation.** The gate is computed over the **eligible**
population: ids whose `ttfb` half was a successful streaming attempt, the only ids a completion
half is *possible* for. Measured eligible coverage:

| era | eligible ids | with a completion half | eligible coverage | mature lost ids | 2+ completions under one id |
|---|---:|---:|---:|---:|---:|
| 15 s window (race live) | 8638 | 8633 | 99.94% | 3 | 0 |
| pre-deploy 3.3 h (race live) | 1025 | 1025 | 100.00% | 0 | 0 |
| post-deploy (race fixed) | 290 | 288 | 99.31% (2 in flight) | 0 | 0 |

So the pre-fix eligible-coverage baseline is **99.94%** (3 mature lost ids) and the post-fix
reading is **100% of mature ids** (the 2 post-deploy orphans are 0.3 and 0.0 minutes old, i.e.
still inside the detached writer's 61-minute bound, and one of them resolved within minutes).
G3 is set at **≥99.9%**, one order of magnitude above the 0.035% pre-fix loss rate and far enough
from 100% not to fire on a single in-flight id; G4 is set at **0 mature lost ids**, the exact
post-fix reading.

**The G3 threshold is a reintroduction detector, not a precision instrument.** It cannot certify
the join is perfect. What it can do is tell the difference between "PR #48's fix is live" and
"the race is back" — which is the only question the gate needs to answer — and unlike the first
version it does so with an instrument that actually moves when the race does.

**The first-hour split is retained** as a second derivation measurement, computed on the
**eligible** population: at the close, the id-join integrity numbers are computed over the full
slice and over the slice with its first hour removed, and both are published.

### 3.3b A bar the design deliberately does NOT carry

The 15 s protocol's ttfb p95 bar (≤19 000 / >21 000 ms) is **not** a bar of this window. It was a
function of the 15 s cut — the results artifact says so in its own words: the p95 "passing" at
16 429 ms was "a pass produced by the cut, not evidence that the cut is harmless". Carrying it
into a window that changes no knob would make a knob-independent window fail on a knob-coupled
bar, which is precisely the failure this design exists to avoid. The window **reports** ttfb p50
and p95 with their denominators and **attaches no pass/fail verdict to them**.

### 3.4 The carried-baseline problem, stated rather than hidden

B1 and B2 are carried from a different binary, a different window and a different first-byte
configuration. Two responses, both in the design:

1. **The window publishes both the carried-bar verdict and the absolute measurement**, so a
   reader can act on the number even if they reject the bar.
2. **The design includes a baseline re-derivation measurement** at the same close: compute the
   same two quantities over the **pre-window part of the same log** (timestamps before the
   floor), split by era. That gives the carried bars a same-log, same-instrument companion, and
   the artifact reports the window's figure beside it. The pre-window slice spans several timeout
   eras, so it is context and not a like-for-like baseline — the 15 s results artifact said the
   same about its own pre-window comparison — but it is strictly more evidence than the carried
   number alone.

---

## 4. Failure-mode analysis: how this window could fail its own bar for reasons unrelated to its questions

Each row names the failure, the mechanism, and the design's answer.

| # | failure mode | mechanism | the design's answer |
|---|---|---|---|
| F1 | **The bars are coupled to a knob the window changes.** | Exactly what happened to the 15 s window: three of its bars were functions of the cut. | R1: the window changes **no timeout, budget, retry or model parameter**. Its bars are carried baselines (B1, B2, B3) and integrity gates (G1–G4). No bar is computed over a population a live change defines |
| F2 | **A bar's denominator is a count of the wrong record kind.** | `failure_mode` is only on `completion` rows, so a share computed over `ttfb` rows is 0 by construction. The 15 s run published a "0.00% PASS" on exactly that error before correcting it. | R2: `measure.py` prints the denominator beside every figure, and §2.1 states the record-kind rule in the metric's own row |
| F3 | **The per-turn join silently does not exist.** | Before PR #35, 309/309 stall rows were unkeyed. If the fix regresses, the two central metrics become unmeasurable — and a reader could mistake an empty population for a zero rate. | G1 is a hard gate on `error_rows_without_a_turn_key == 0`. If it is non-zero, the artifact reports UNMEASURABLE for Q1 and Q2 **with the count**, never a rate. `measure.py` prints `unkeyed_stall_rows` and the keyed-stall population in every run, so an empty denominator is visible rather than silent |
| F4 | **The request-id race contaminates the per-attempt join.** | The race's signature is invisible to an orphan check: a stolen id still looks complete. | G2 and G4 catch the thief and the victim separately. §3.3 derives their thresholds from the pre-fix measurement, and §3.3's first-hour split stops early-traffic noise from firing them |
| F5 | **An upstream incident inside the window reads as a property of the proxy.** | A1 §4's cluster 1 was a 20-stall, 525 s upstream-wide event; a window that averages over it would report the incident as the proxy's behaviour. | The per-hour table and the key split are published, per the 15 s protocol's §4.5, and hours with ≥10 distinct sessions are flagged as concurrency outliers. A rate that exists only in the outlier hours is reported as such |
| F6 | **A restart inside the window voids the elapsed part and nobody notices.** | The floor is the container restart timestamp; a mid-window restart silently splits the slice across two binaries. | §5: any restart moves the floor and voids the elapsed part. The window's slice is filtered on `timestamp ≥ floor`, and the floor is recorded with the binary's image revision label so a mid-window deploy is visible in the artifact |
| F7 | **The log rotates inside the window and the close reads only the live file.** | The logger rotates at 64 MiB; `nimaproxy-query` opens exactly one file, so it cannot read across a rotation. A naive glob also puts the live file first, reversing the window in time **with the right line count**. | §5.4: the close script's three checks (completeness, count, part-boundary ordering) are mandatory, and `measure.py` accepts a directory of parts and concatenates rotated-first, live-last, printing every part's boundary timestamps |
| F8 | **A minimum is not met and the criteria are evaluated anyway on a smaller slice.** | The 15 s run closed at 17.80 h against a 24 h minimum. §9.1 justified it because the failing bars could not be reversed by waiting — a justification that does not transfer to a window whose bars are integrity gates. | §5.3: if a minimum is not met at 24 h, **extend the window**; do not evaluate on a smaller slice. The 15 s run's early close is explicitly *not* a precedent for this window |
| F9 | **The client-side ratio is faked from proxy-side numbers.** | The tempting move is to report the proxy's own ttfb as a stand-in for the client's ttft. | §7: the ratio is reported as NOT COMPUTABLE, with the reason. `measure.py` prints that line in every run |
| F10 | **A p95 quoted without its population.** | A6 showed the per-hour client gap is driven by session concurrency, so an aggregate ratio is a concurrency artefact as often as it is a proxy property. | Every metric in §2 names its denominator and `measure.py` prints it; the per-hour table is part of the close artifact |

---

## 5. The floor and the sequencing discipline

### 5.1 The floor

The analysis floor is the **container restart timestamp** that starts the window. The figures
in this document were measured against the **stage-4** window, whose floor was
`2026-10-21T14:50:43Z` (the deploy of the stage-4 binary, revision `2e6b6e0`). The window that
was live when this document was published has floor `2026-10-21T18:11:43Z` (the stage-6 deploy,
revision `fdef683`); re-measure anything time-sensitive against that floor.
The successor window's floor is the restart that starts *it*, and it must be recorded with:

- the restart timestamp, to the second, in UTC;
- the live config's sha256 before and after;
- the image's `org.opencontainers.image.revision` label, so the binary under test is named;
- the turn-log's byte count at the floor, so the rotation expectation is computed rather than
  assumed.

**No pre-floor row may enter the slice.** The slice filter is `floor <= timestamp < ceil` on the
snapshot's own timestamps, never on the live file.

### 5.2 What live changes happen, and when

**None during the window.** That is the point. The proxy's configuration, binary and log schema
are held constant for the whole window.

**The one change the successor window needs is a BINARY change, and that is a correction.**
This section previously said the client-side instrumentation of §7 "is a **client-side** change
and does not touch the proxy, its config, its container or its log". The §7.0 measurement
showed that is not quite true: the join key requires a **turn-log field**
(`response_body_id`), which is a change to the proxy's binary and to the log's schema. The
client-side *export* is client-side; the *key* is not.

**What that changes here, stated precisely:**

- the log field is **added**, not repurposed. A reader that parses by field name is
  unaffected, so the window's own metrics do not change. But the protocol's "the schema and
  layout of the log are unchanged by this design" is **no longer true**, and the artifact
  must record the added field and the binary revision that introduced it.
- a binary change **moves the floor**, because the floor is the container restart timestamp
  (§5.1). So the field must be deployed **before** the window's floor, and the window starts
  at that deploy.
- if the field is **not** deployed before the floor, the window still runs and Q3 is reported
  as NOT COMPUTABLE, with the reason. The instrumentation is never a precondition for the
  window's own questions (Q1, Q2) — only for Q3.

Sequencing, in order:

1. **Before the floor:** the turn-log's `response_body_id` field is added and deployed, and the
   client-side export of §7.1 is deployed and verified to emit its records somewhere joinable.
   If either is not ready, the window still runs — Q3 is reported as NOT COMPUTABLE with the
   reason, and the window's other two questions are unaffected. The instrumentation is never a
   precondition for the window, only for Q3.
2. **At the floor:** the restart that starts the window. Any earlier restart in the same day
   moves the floor and voids the elapsed part; that is expected and is recorded, not hidden.
3. **During the window:** no restart, no config edit, no deploy. The 15 s protocol's own
   annotation makes the point: a tuning change was deliberately *not* deployed mid-window because
   the restart would reset the 24 h clock, and the live file was reverted to floor state so a
   restart could not arm it. The same discipline applies here, with the added constraint that a
   mid-window deploy would also change the binary under test.
4. **At the close:** the snapshot is frozen per §5.4, then and only then are the criteria scored.
   The close script's **check 0** (the binary-boundary assertion) is what verifies that no
   deploy slipped in: it fails if the running container's `StartedAt` is later than the floor.

### 5.3 Minimum length and minima

| minimum | bar | derivation |
|---|---|---|
| window length | **≥ 24 h** post-restart, covering a full diurnal cycle | carried from §4.4: the baseline window spans 22Z–09Z with zero-traffic hours 03–05Z, so a shorter slice is not a diurnal sample |
| `ttfb` rows | **≥ 300** | carried from §4.4; the 15 s window had 4228 in 17.8 h, so 300 is ~2 h of traffic and is a floor for decidability, not an aspiration |
| stall attempts | **≥ 20** | carried from §4.4: 1/20 = 5% granularity, which is the finest a share can be decided at |
| **turns with a stall attempt** | **≥ 20** | **new, and it is the minimum that matters for this window.** B1 and B2 are computed over turns, so the turn population is what needs to be decidable. Measured on the partial window: 93 stall turns in ~3.6 h, so 20 is comfortably reachable |
| **error rows carrying a turn key** | **100%** | G1 |

If a minimum is not met at 24 h, **extend the window**. Do not evaluate the criteria on a smaller
slice.

### 5.4 The snapshot and its three checks

The log is append-only and rotates at 64 MiB. The close therefore:

1. **copies** the parts (a read can race the rotation), rotated parts in `LC_ALL=C` name order and
   the live file **last** — the live name is a lexicographic prefix of the rotated names, so a glob
   puts it first and the window runs backwards in time;
2. verifies the file set afterwards — every `turns.jsonl*` file in the log directory must be
   represented in the snapshot. It does **not** re-copy on a change: a rotation that happens
   *during* the copies is **detected** and the close fails loudly (the file-set check fails,
   exit 2), rather than the snapshot being re-taken. That is the weaker but honest guarantee,
   and it is the one the script provides;
3. concatenates and records every part's line count and the combined sha256;
4. runs **both** checks, because they catch different failures: the concatenated line count equals
   the sum of the parts' counts (catches an omitted file), and the first record's timestamp is
   older than the last (catches a reordering, which the count cannot see);
5. takes the floor and ceiling on the **snapshot's own timestamps**, never on the live file.

`measure.py` implements the same ordering when given a directory, and prints each part's boundary
timestamps so a reordering is visible rather than silent.

---

## 6. The id-join integrity gate

The current window's artifact already carries an id-join integrity column (`ttfb` ids with no
completion half; ids carrying 2+ completions) because the #44 race contaminated the per-attempt
join. The successor window **keeps that column and makes it a gate** — over the **eligible**
population defined in §2.3, not over all `ttfb` ids, for the reason measured there.

**The gate, stated exactly.** The window's results are scored only if all four hold:

- **G1** `error_rows_without_a_turn_key == 0`;
- **G2** `ids_carrying_2plus_completion_records == 0`;
- **G3** `integrity_pct_eligible_with_both_halves ≥ 99.9%`;
- **G4** `lost_successful_ids_mature == 0`.

**G4 counts only MATURE lost ids.** A completion half is written by a detached task bounded by
`idle_window * 20 + 60 s` (3660 s at the live 180 s idle window), so a successful streaming
attempt younger than that bound may simply still be in flight. Measured on the post-deploy
window: 2 orphans, 0.3 and 0.0 minutes old, and one resolved within minutes. An in-flight id
is reported separately as `lost_successful_ids_in_flight` and is **not** a gate failure —
otherwise the gate would measure the *close time* rather than the traffic.

**What the window does if the gate fails, per gate, because "fail" is not one action:**

| gate fails | what it means | the window's action |
|---|---|---|
| G1 non-zero | the schema fix has regressed; Q1 and Q2 cannot be computed | **Report Q1 and Q2 as UNMEASURABLE, with the unkeyed count.** Do not score B1 or B2. Do not extend the window hoping the rows appear — they will not. File the regression as a defect and re-window |
| G2 non-zero | PR #48's race fix is not holding in this binary | **Stop. The per-attempt join is contaminated, so the ≥15 s invariant denominators and every row-level share computed through the join are suspect.** Report the row-level counts (which do not use the join) as measured, report the joined metrics as contaminated, and file the defect. A window scored on a contaminated join is a window whose numbers look clean |
| G3 below 99.9% or G4 non-zero | either the race has returned, or the early-traffic artefact of §3.3 | **Re-run the integrity numbers with the first hour removed.** If the later slice passes, publish both numbers and score on the later slice with the difference disclosed. If the later slice also fails, treat it as G2 |
| any gate fails and the window's questions are unaffected | — | The row-level metrics that do not use the join are still reported, with their denominators. A failed gate does not licence withholding the numbers that are sound |

**What the gate is not.** It is not a certification that the join is exact. No threshold on a
contaminated-by-construction join can be. It is a **reintroduction detector with a stated
sensitivity**, derived in §3.3 from the pre-fix measurement.

---

## 7. The client-side TTFT instrumentation

### 7.0 What was measured about the client side, and the one thing that is missing

Owner decision 4 is to instrument the client side **first**, so the join exists when the
window runs. That work has been done, and its result changes the specification below.

**The transcripts are not where this design said.** Not `$OMP_SESSION_DIR/**/*.jsonl`, but
per-session files under `<client-session-dir>/<cwd-slug>/<ts>_<uuid>.jsonl`,
indexed by `<client-agent-db>` (`threads.rollout_path`). The assistant
message carries: `api, content, duration, model, provider, responseId, role, stopReason,
timestamp, ttft, usage, completedAt, contextSnapshot, credentialId`.

**The `ttft` field exists, per assistant message, in milliseconds** — so this design's
premise that the instrumentation is "an export of an existing field, not a new
measurement" is **correct**. So are its other premises: `provider` is present, so a
non-proxy provider can be excluded rather than averaged in, and `usage.input` /
`usage.output` are present, so the ratio can be conditioned on the response shape.

**The join key does NOT exist on the client side.** Measured: of **37 472** assistant
records with `provider == "nimaproxy"` across the whole session corpus, **zero** carry a
proxy-format request id (`YYYYMMDDHHMMSS.pid.seq`) in any structural field. A corpus-wide
scan for that shape returns 47 hits, and **every one** is an assistant's own prose
discussing the id format — not a recorded field.

**Traced through one request** (`curl -D headers -o body` with a unique prompt):

| | value | where it lives |
|---|---|---|
| proxy log `request_id` | `20261021202707.1.3211` | the turn log, and the `x-request-id` **response header** |
| response body `id` | `94cbcaebd9c18988` | the upstream's id, relayed in the body — the client records it as `message.responseId` |
| client `responseId` | the body id | the transcript |

The header id **never reaches the client**: the transcript records the message body and
metadata, not response headers. And the body id is **not in the turn log** — a live
request's body id (`93128f90d83ab5b9`) was searched for across the whole log and is
absent.

So the join as originally specified — `client_request_id == request_id` — is **not
achievable today**, and the `session_id` + time-window fallback is the only join available
as things stand.

### 7.0.1 The cheapest fix, and it is not a client change

**The key the client already has is the one the log is missing.** The client's
`responseId` is the response body's `id`, and the proxy **already sees** that id — it
passes through the proxy in the bytes. Seeing it is not extracting it, and the two
differ here:

- **non-streaming**: the proxy assembles the body (`chat.rs:750-753`), so the id is in the
  body bytes — but it is **not extracted from them**. The body is parsed for usage
  (`chat_helpers.rs`, `non_streaming_usage`), and nothing reads the body's own `id`;
- **streaming**: the id is in the **first SSE chunk** — measured:
  `data: {"id":"8e3aa5adfa6be8e2","choices":[...` — but the proxy's SSE tap does **not**
  extract it. The tap's only `get("id")` is `sse_tap.rs:348`, inside `tool_call_key`, where
  `entry` is a `choices[].delta.tool_calls[]` entry: that is the **tool-call** id, not the
  top-level response-body id. The crate's only other `get("id")` calls read a model id and an
  Anthropic-format tool-call id. **No top-level response-body id is extracted anywhere in the
  crate**, so the extraction has to be added — it is not a reuse;
- the turn log has **no field** to carry it today (`TurnRecord`'s fields are listed at
  `turn_log.rs:41-159`; none is a response/body id).

**The fix is therefore a log field, not a client change and not a wire change:** add the
upstream body id to the turn log (e.g. `response_body_id`), populated from the id the proxy
**must be taught to parse** — a top-level frame id captured by the SSE tap, and a top-level
body id read from the assembled body. Then

```
client.responseId  ==  turn_log.response_body_id   ->   turn_log.request_id   ->   ttfb.latency_ms
```

— an **exact** join on a key the client already records, with **no client change** and
**no change to what any client sees** (the body id on the wire is untouched).

**The ordering constraint, which is the easy thing to get wrong.** On the non-streaming
path the id is available only once the body is **assembled**, which is *after* the
first-byte record is written. So the id belongs on the **completion** record, and the
existing (already integrity-gated) `ttfb` ↔ `completion` join carries it to the `ttfb`
row. On the streaming path the id arrives in the first chunk, i.e. at first-byte time.

**Two consequences the successor window's own design must state.** (1) The log's schema
is **no longer unchanged** — an added field is backward-compatible for a reader that
parses by field name, but the protocol's "the schema and layout of the log are unchanged
by this design" is no longer true, and the artifact must say so. (2) The change lands in
the **binary**, so it must be deployed **before** the window's floor or it splits the
window across two binaries — the exact F6 failure the close script's new check 0 exists
to catch.

### 7.1 The record's shape

One JSON object per request, written append-only to a file the window can read:

| field | type | why |
|---|---|---|
| `client_response_id` | string | **the join key**: the response body's `id`, which the client already records as `message.responseId`. It joins to the turn log's `response_body_id` (see §7.0.1) |
| `proxy_request_id` | string | the proxy's `request_id` for the same request, carried through from the log side, so the client record joins to the proxy's `request_id` directly |
| `session_id` | string | so the client record can also join to the proxy's `loop_key.session_id`, and so a weaker time-window join remains available as a fallback |
| `client_ttft_ms` | integer | the client's own time-to-first-token, measured from the moment the client issued the request |
| `client_first_token_at` | RFC 3339 UTC | the wall-clock instant, so the join can be audited rather than trusted |
| `model` | string | so the ratio is computed per model, not across models |
| `provider` | string | so a non-proxy provider is excluded rather than silently averaged in |
| `prompt_tokens` / `completion_tokens_at_first_token` | integer | so the ratio can be conditioned on the response shape, which is what A6 §3 showed drives the per-hour gap |

**Under what key.** The primary join is `client_response_id == response_body_id`, which
resolves to the proxy's `request_id` and thence to the `ttfb` row — **exact**, and with no
time-window matching. The **secondary** join, used only when the body id is absent, is
`session_id` plus a time window — and it must be reported as a weaker join, with its match
rate, exactly as A6 §2 did.

**The exporter exists: `design/export-client-ttft.py`.** It reads the session corpus and
writes the records above as append-only JSONL, filtered to the proxy's provider. Measured on
a full-corpus run: **37 577 records — 35 062 with a ttft, 2 515 that FAILED before producing
a token** (emitted with `client_ttft_ms: null`, not dropped), 35 715 with a
`client_response_id`; client ttft p50 4176 ms, p95 48 870 ms. The two counts reconcile
exactly (35 062 + 2 515 = 37 577).

**The failed-before-a-token class is a survivorship-bias trap, and the exporter's first
version fell into it.** Its prefilter required a `"ttft"` key to be present — but a request
that failed before a token has **no** `ttft` field at all — so it dropped exactly the class
the design says must not be dropped silently, **2 515 records, 6.7% of the population, and
precisely the ones the client experienced as failures**. It was caught by a disagreement: the
exporter reported 0 of that class over 35 034 emitted, while a direct search of the newest
files found 12. **A ratio computed over only the requests that succeeded is a ratio over a
survivorship-biased population.**

**Its join claim is tested against the real log, and it does not join yet.** 3 733 distinct
client `response_id`s were searched across 70 516 live log lines: **zero** matches. That is
the expected result before the `response_body_id` field exists, and the exporter's summary
prints that status on every run — so the artifact cannot quote a client-side p95 without the
reader seeing which join produced it.

**One correction the design must carry:** the transcripts are not at
`$OMP_SESSION_DIR/**/*.jsonl`. They are at
`<client-session-dir>/<cwd-slug>/<ts>_<uuid>.jsonl`, indexed by
`<client-agent-db>` (`threads.rollout_path`). The exporter takes the corpus
root as `--sessions` and **requires** it explicitly; the shipped tool bakes no
host path, so the corpus location is supplied by the operator.

**How the ratio is then computed.**

1. For each client record, find the proxy `ttfb` row with the same `request_id`.
2. `ratio = client_ttft_ms / ttfb.latency_ms`, over the matched set only.
3. Report the distribution and its **p95** over the matched set, **with the match rate published
   beside it** — a p95 over a 40% match is not the population's p95.
4. Publish the **per-hour table** (hour, proxy n, proxy p50/p95, client n, client p50/p95, gap
   p50/p95, ratio) and evaluate the p95 criterion **both** on all hours and on the normal-hour
   subset, flagging hours with ≥10 distinct sessions as concurrency outliers. A6 §1/§3 showed the
   per-hour gap is driven by session concurrency and proxy TTFB p50, not by network, so an
   aggregate-to-aggregate comparison is the wrong instrument.
5. **The bar is B3: p95 ≤ 13.31×**, A6 §2's published baseline. It is registered here so the
   window that inherits the instrumentation does not have to invent one.

**What this window does about it.** Nothing, and that is the honest answer: the client is not
instrumented yet, the log holds no client clock, and `measure.py` prints NOT COMPUTABLE. The
instrumentation is a prerequisite for a *later* window, and this design's job is to make sure the
later window inherits a key, a shape and a bar rather than improvising them.

---

## 8. The two per-turn questions, and their outcome spaces

### Q1 — the 81-bound recovery

- **Computed over:** turns containing at least one `ttfb` attempt with `15000 ≤ latency_ms <
  25000` **and** at least one stall attempt (`T_band ∩ T_stall`).
- **Numerator:** those turns whose **earliest** clean completion (`clean_stop` / `tool_calls`) is
  not earlier than their first stall. (The existential reading of the same rule would count one
  further turn on the live window — 21/21 rather than 20/21 — and changes no verdict; see the
  ordering rule in §2.4.)
- **Bar:** ≥50% pass, <25% fail (B2).
- **Outcome space, so the answer cannot be unfalsifiable:**

| outcome | reading | action |
|---|---|---|
| ≥50% | the population a shorter window would convert is recovered by the turn more often than not; a shorter window is *plausible* and the next experiment can be designed | proceed to the cutoff-derivation experiment, with this window's rate as its prior |
| 25%–50% | inconclusive. The recovery is real but not decisive at this population size | extend the window if the minimum population is not met; otherwise report inconclusive with the absolute rate, and do **not** authorize a further cut |
| <25% | the turns that stall in that band mostly do not recover. A shorter window would be converting successes into failures faster than it saves tail latency | the cutoff-derivation experiment is **blocked** on this evidence, and the block is recorded with the number |
| population empty (`T_band ∩ T_stall` is small) | the band exists (measured: 113 attempts, 109 turns on the partial window) but too few of its turns stalled to decide | **UNMEASURABLE, not a pass and not a fail.** Extend the window; do not score a rate on a handful of turns |

### 8.1a The cutoff decision needs TWO independent measurements, not one

Q1's outcome space above says what a Q1 answer *authorises*. It is worth being exact
about what it does **not** authorise, because the two blockers are independent and a
reader could reasonably assume one answer settles the question.

| | what it measures | what it decides |
|---|---|---|
| **Q1** (81-bound recovery) | whether the turns a *shorter* window would convert tend to recover | whether a shorter window is **plausible** |
| **the benefit side** (censored tail) | how many stalls a *longer* window would save | what a longer window **buys** |

The benefit side **cannot be measured from the log at all**: a stall row records the
window that fired, not the latency the attempt would have taken, so the number of stalls
a 45 s window would save is not in the log and no re-analysis of it can produce it. It
needs a long-timeout window (see `CUTOFF-DERIVATION.md` §6.3).

**So a Q1 verdict of "≥50%, proceed" does not by itself authorise a cutoff change.** It
authorises *designing* the cutoff-derivation experiment. The experiment itself is gated
on the benefit side too, and the two gates are independent:

- Q1 < 25% → the experiment is **blocked** by Q1. The benefit side does not rescue it.
- Q1 ≥ 50% → the experiment is **unblocked by Q1**, and still gated on the benefit side.
- Q2 ≥ 9.5% (already measured at **60.66%**, §8.1 — the stage-4 snapshot) → the experiment's cost side is
  cheap, which is a *prior* on the answer, not a substitute for the measurement.

The action this design therefore commits to, stated now so it cannot be renegotiated
after the numbers are in: **no cutoff value is published until BOTH the Q1 verdict and a
measured benefit side exist.**

### Q2 — the per-turn salvage

- **Computed over:** `T_stall`, all turns containing at least one stall attempt.
- **Numerator:** those whose **earliest** clean completion is not earlier than their first stall.
  (The existential reading would count five further turns on the live window — 128/190 rather
  than 123/190 — and changes no verdict; see the ordering rule in §2.4.)
- **Bar:** ≥9.5% pass, <9.5% fail (B1), carried from A1 §4's `6/63`.
- **Outcome space:**

| outcome | reading | action |
|---|---|---|
| ≥9.5% | the rolled-back configuration salvages stalled turns at least as well as the 15 s-era baseline did | no configuration action; publish the number as the first per-turn salvage measurement that exists |
| <9.5% | the rolled-back configuration salvages stalled turns **worse** than the 15 s-era baseline | **This is a finding about the live configuration, and the response is to fix the configuration, not to extend the window** — the same response rev 3.0/§11.3 specified for its own triggers. The knob is already deleted, so the fix is elsewhere (admission, budgets, key pool) and the artifact says which |
| 9.5% ± the sample's granularity | within noise of the bar | report the absolute rate and the granularity (`1/n`), and do not claim a regression or an improvement. The 15 s run's own truncated_stream invariant taught this lesson: a bar of 1.4 rows cannot be crossed by 2 rows and mean anything |

**Why the ordered predicate matters to both.** Without the ordering rule, a clean completion
recorded before a turn's first stall would be counted as a recovery from it, inflating both
numbers. With it, a turn whose only clean completion preceded its stall is correctly excluded.
`measure.py` implements the ordering and states it in its output.

**The ordered predicate has teeth, measured.** On the live slice the ordering excluded **0**
turns, which means it was *untested*, not proven — a metric that excludes none of the cases it
claims to exclude is indistinguishable from a no-op on that data. So a clean completion was
**planted** one second before a non-salvaged stall turn's own stall, under that turn's
`loop_key`. The control: the same planted turn counted under an unordered join.

| | salvaged turns |
|---|---:|
| ORDERED (clean completion after the stall) — what `measure.py` does | 30 |
| UNORDERED (any clean completion in the turn) | 31 |

The ordering excludes exactly the planted turn, and `measure.py` reports the baseline count
unchanged on the planted snapshot. **The predicate discriminates.**

### 8.1 What the LIVE window already measures about these two questions

Measured on the running window (floor `2026-10-21T18:11:43Z`, slice read live), so the
successor window inherits a prior rather than starting from nothing:

| question | value | bar | population | verdict |
|---|---|---|---|---|
| Q2 per-turn salvage | **60.66%** (37/61) | B1 = 9.5% | 61 stall turns (min 20) | **DECIDABLE, and it PASSES** — 6.4× the baseline, at a granularity of 1/61 = 1.64% — *the stage-4 snapshot; the later stage-6 window measures 64.74% (123/190) on a larger population* |
| Q1 81-bound recovery | 100.0% (8/8) | B2 = ≥50% pass, <25% fail | **8** band-and-stall turns (min 20) | **UNMEASURABLE so far** — the population is below the minimum, and §8.2 projects it clear of 20 by the close |

**Q1's 100% is not a pass and must never be published as one.** The outcome space is explicit:
a population below the minimum yields UNMEASURABLE, not a verdict, and 8/8 has a granularity of
1-in-8. The number to publish is **8 with its denominator**, not the percentage.

**Q2 is the strongest positive signal in the experiment**: the fleet's own retry behaviour
salvages roughly **five in six** stalled turns, against the 15 s-era baseline of 6/63 = 9.5%.
That is the prior the cutoff-derivation experiment is designed against — the cost side of a
tighter cutoff is much lower than the 15 s-era baseline implied.

**What it does not settle.** The cutoff's *benefit* side is a different quantity from the
salvage rate and remains censored (`CUTOFF-DERIVATION.md` §5): a stall row records the window,
not the latency, so the stalls a higher cutoff would save are not in the log. The salvage rate
prices the cost side; it does not supply the benefit side.

### 8.2 Q1's population is on track, so the window need not be extended for it

Q1's denominator is the **band-and-stall** turns, and the design's minimum is 20. Measured at
2.95 h into the running 24 h window: **83** band turns, **61** stall turns, **8**
band-and-stall turns. The arrival rate is **2.72/h** (projected **65.2** at 24 h), and the
second half alone — with no deploy ramp — gives **4.07/h** (projected **97.8**). Both
projections clear the minimum of 20 with room to spare, so **Q1 should be DECIDABLE at the
close and the window does not need extending for it.**

**The previous 24 h produced ZERO band-and-stall turns** — because PR #35 (the failure-path
turn keys) had not landed, so the per-turn join could not be built at all. The 8 in the
current window are the **first such measurement that exists**.

**One trap worth naming**, because it produces 0 silently: a stall row's `latency_ms` is the
window that fired, not a first-byte latency. The band is the set a 15 s cut would convert
**from success**, so an ad-hoc scan that classifies a stall row by its latency gets a
different (and wrong) band. `measure.py` computes it correctly; see §2.5.

---

## 9. What this design deliberately does NOT answer

| not answered | why |
|---|---|
| **The first-byte cutoff itself.** | Deriving it is a separate experiment (issue #39) that needs its own pre-registration, and this window deliberately changes no knob so that it cannot fail for knob-coupled reasons again. This window's Q1 answer is the *prior* that experiment will be designed against |
| **Whether 15 s (or any value) is the right cut.** | Same reason. A window that changes the knob to test the knob is the 15 s window |
| **The client/proxy TTFT ratio, as a measurement.** | The client is not instrumented. The design specifies the instrumentation (§7) and registers the bar (B3) for the window that inherits it. Reporting a proxy-side stand-in would be the F9 failure |
| **Whether the ≥15 s outcome distribution is healthy.** | The 15 s protocol's terminal-504 and completion-share bars were functions of the cut. Under the live dynamic window the ≥15 s population is a different population, and the design reports the counts and shares with denominators but attaches no verdict, because a verdict would be comparing across populations that the knob defined |
| **The ttfb p95 verdict.** | Same reason, stated in §3.3b. Reported, not judged |
| **Whether the request-id join is *exact*.** | G1–G4 are a reintroduction detector, not a certification. Claiming exactness would be claiming what no threshold on a contaminated-by-construction join can deliver |
| **Whether the per-turn salvage bar is the *right* bar.** | It is carried from a different binary and window. §3.4 says so, publishes the absolute number beside the verdict, and includes the same-log pre-window companion measurement. Improving the bar is a later revision's job, and it needs a derivation this window does not attempt |
| **Per-model behaviour.** | Only one model is routable (`stepfun-ai/step-5-preview`); a per-model split would be a split of one |
| **Anything about the client's own request shape beyond what the proxy logs.** | The proxy records the *effective* `request_max_tokens`, not the client's ask — a distinction rev 3.2 already had to arbitrate with the config file. This design does not re-litigate it |

---

## 10. The measurement script

`design/measure.py` implements every metric in §2 and every gate in §6.
Paths of the form `design/…` are relative to the crate root, `nimaproxy/`.

```
measure.py SNAPSHOT FLOOR CEIL [--json]
```

- `SNAPSHOT` is a frozen file, or a **directory of parts** (rotated parts in `LC_ALL=C` name
  order, live file last — §5.4's ordering, implemented rather than described).
- It **streams** the input line by line. No snapshot is fully resident; the per-turn join keeps
  only per-turn state bounded by the number of distinct turn keys, and the per-attempt join keeps
  only a small slot per distinct `request_id`.
- It prints the population definition — source files, the timestamp filter, the record kinds, the
  join keys, and every part's boundary timestamps — before any number.
- Every metric is printed **with its denominator**.
- It prints, explicitly, what it cannot compute: the client TTFT, the ratio, and the bound under a
  cut that is not live.
- `--json` emits the same numbers as one object for a machine-readable artifact.

**It was run against the real live log** (read-only; the live proxy, its config, its containers
and its log were not touched). Floor `2026-10-21T14:50:43Z` (the **stage-4** window's floor),
ceiling
`2026-10-22T23:59:59Z`, on a growing file of ~67.6k lines:

```
ttfb rows                                            1300
ttfb p50 / p95 (ms)                                  3786 / 26502
stall rows                                           110
retry firing                                         98.18%          (denominator: stall rows)
terminal stalls                                      2               (denominator: stall rows)
completions                                          1167
loop rows                                            286
error rows                                           145
error rows carrying a turn key                       145             (denominator: error rows)
error rows WITHOUT a turn key                        0               (denominator: error rows)
unkeyed stall rows                                   0               (denominator: stall rows)
>=15000 ms ttfb attempts                             236             (denominator: ttfb rows)
truncated_stream                                     31              (denominator: completion rows)
length_cutoff                                        7               (denominator: completion rows)

>=15 s attempts                                      236
of those, joined to a completion half                122             (denominator: >=15 s attempts)
truncated_stream on the joined set                   0               (denominator: 122 joined)
truncated share                                      0.0%

distinct ttfb request_ids                            1300
distinct completion request_ids                      1167
ids holding BOTH halves                              1167            (denominator: distinct ttfb request_ids)
ttfb ids with NO completion half                     133             (denominator: distinct ttfb request_ids)
  of which the ttfb half was a success               19              (denominator: 133)
ids carrying 3+ records                              0
per-attempt join coverage                            89.77%

turns with >=1 ttfb attempt                          1120
turns with >=1 stall attempt (DENOMINATOR)           93              (denominator: turns total)
  of which the stall attempt carried its turn key    93
stall attempts total                                 110
stall attempts carrying a turn key                   110             (denominator: stall attempts total)
SALVAGE: stalled turns reaching a clean completion   76              (denominator: turns with a stall attempt)
salvage rate                                         81.72%
RECOVERY: stalled turns with any completion half     84              (denominator: turns with a stall attempt)
recovery rate (any completion)                       90.32%

attempts in the band [15000, 25000)                  113             (denominator: ttfb rows)
turns with an attempt in the band                    109
  of those, turns that also stalled                  15              (denominator: band turns)
band turns that stalled AND reached a clean completion 12             (denominator: 15)
band recovery rate (stalled band turns only)         80.0%
```

These are **partial-window smoke measurements on an unfrozen, growing file**, quoted so the tool's
behaviour is demonstrated against real data. They are **not** window results: the window is still
open, the ceiling used here is a placeholder, and the numbers will move. What they establish is
that the machinery works and the populations exist.

Two readings from that run that the design acts on:

- **the join exists** — 110/110 stall attempts and 145/145 error rows carry their turn key, so Q1
  and Q2 are computable;
- **G3 and G4 would fail on this partial slice** (coverage 89.77%, 19 lost successful ids), which
  is why §3.3's first-hour re-measurement is part of the design rather than a post-hoc excuse.

---

## 11. What the close artifact must contain

One artifact, as the protocol requires:

1. the frozen snapshot's path, line count and sha256, and the parts it was concatenated from;
2. the floor and ceiling, taken on the snapshot's own timestamps, with the binary's image revision
   label and the config's sha256;
3. the minima as measured (§5.3), including the new turn-population minimum;
4. the per-hour table (§4.5's shape) and the key split, with concurrency-outlier hours flagged;
5. the row-level metrics with their denominators (§2.1);
6. the id-join integrity numbers and the G1–G4 verdicts, with the first-hour re-measurement beside
   them (§6);
7. Q1 and Q2 with their populations, numerators, denominators, absolute rates **and** their
   carried-bar verdicts (§8), each with its outcome-space reading;
8. Q3 as NOT COMPUTABLE with the reason, and the §7 instrumentation's status;
9. the commands that recompute every published number against the named snapshot — this document's
   §10 and `measure.py` are written to be that command set;
10. a criterion reported as unmeasurable says so and says why. It does not substitute a proxy
    number and present it as the metric.
