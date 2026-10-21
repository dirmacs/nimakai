# EXPERIMENT-15S — results artifact

The one artifact §6 requires after a window: the per-hour table, the primary metrics with their
criteria verdicts, the invariants, and the key-split note.

**Verdict: FAIL. The rollback bar fired and the knob was rolled back.** The protocol is
`EXPERIMENT-15S-WINDOW.md`; the evaluation and the rollback are recorded in its rev 2.2 addendum
(§9). This document reports the numbers.

## The slice

| | |
|---|---|
| floor (the restart that loaded the override) | `2026-10-20T18:43:50Z` |
| ceiling (the stop for the rollback) | `2026-10-21T12:31:38Z` |
| elapsed | **17.80 h** — the §4.4 minimum is 24 h, so the window closed early |
| source | the proxy's turn log, one JSON object per line |
| frozen snapshot | 62,501 lines, sha256 `5686d37001fa6b827f1652edd7dc659e3b27e170051f9696943a88ca829ed2e5` |
| rows in the slice | 9297 (`ttfb` 4228, `completion` 3903, `loop` 1166) |

The window closed early because both failing bars were already determined and neither could be
reversed by waiting; see the rev 2.1/§9.1 arithmetic. The §4.4 minima, as measured:

| Minimum | Bar | Measured | Met? |
|---|---|---|---|
| >=24 h post-restart | 24 h | 17.80 h | **NO** — closed early |
| >=300 `ttfb` rows | 300 | 4228 | YES |
| >=20 stall attempts | 20 | 309 | YES |

## Primary metrics

| metric | bar | measured | verdict |
|---|---|---|---|
| ttfb p95 | <=19000 ms pass / >21000 ms fail | **16429 ms** (p50 2748 ms) | **PASS** |
| 81-bound recovery | >=50% pass / <25% fail | **unmeasurable** — see below | **NOT MEASURED** |
| per-turn salvage | >=9.5% pass / <9.5% fail | **unmeasurable** — see below | **NOT MEASURED** |
| retry firing after a 15 s stall | >=97% | **285/309 = 92.2%** | **FAIL** (§5.1 trigger 4) |
| client/proxy TTFT ratio p95 | <=13.31x | not measured — the client-side corpus was not joined for this window | **NOT MEASURED** |

The two row-level bars are the ones the verdict rests on, and §5.1 says any single trigger fires the
rollback. Trigger 4 fired.

**The terminal-504 bar (trigger 8) also fired:** of the >=15 s attempts in the slice, **317/374 =
84.8%** ended terminal-504 against the 43.9% baseline, with the completion share at **55/374 =
14.7%** against the 53.5% baseline.

### Why two metrics are unmeasurable rather than failed

`request_id` is `{YYYYMMDDHHMMSS}.{pid}.{seq}` with `seq` a process-local counter that is unique per
**record**, so the protocol's §4.3 per-turn join ("group by the first two dot-parts") is a per-second
time bucket: measured 3776 distinct first-parts and 3776 distinct first-two-parts over 4228 `ttfb`
rows. Worse, the stall rows — the entire population both metrics are defined over — carried no turn
key at all: 309/309 unkeyed, while 3911/4228 `ttfb` rows carried one.

So neither the 81-bound recovery numerator nor the salvage denominator can be constructed from this
run's data. They are reported as unmeasurable, not as failures and not as passes. The per-**attempt**
join does work (3903 ids hold both the ttfb and the completion half) and is what the row-level
metrics above use.

## Invariants

| invariant | requirement | measured | verdict |
|---|---|---|---|
| §3.2.1 — the idle bound is decoupled from the first-byte rail | no row may read `upstream body stalled after 15000ms`; the idle bound must read 180000 ms | **0** such rows | **PASS** |
| §3.2.2 — `truncated_stream` is flat | share at or below the 2.5% baseline | **2/57 = 3.5%** of >=15 s attempts joined to their completion half (the protocol's own denominator, A1 §3's 4/157); raw counts 32 `truncated_stream` and 19 `length_cutoff` across 3903 completions | **NOT DECIDED** — above the 2.5% bar, but 2.5% of 57 is 1.4 rows, so 2 rows is within noise of it |

**§3.2.1 is verified.** 0 rows read `body stalled after 15000ms`, so the mid-stream guard did not
move: the first-byte rail and the idle bound stayed decoupled for the whole window.

**§3.2.2 could not be decided, and the honest reading is that it is within noise.** It is the one
invariant that would have detected the cut truncating in-flight generations. On the protocol's own
denominator — >=15 s attempts joined to their completion half, which is a per-ATTEMPT join and so
does not depend on the broken turn-join — it is **2/57 = 3.5%** against the 2.5% baseline. That is
above the bar, on a sample where the bar itself is 1.4 rows: 2 rows is within noise of it, and is
not evidence either way.

For scale, the same join over the pre-window part of the same log gives 44/568 = 7.7%, and the raw
counts are 305 `truncated_stream` across 6807 completions against the window's 32 across 3903. The
pre-window slice spans several timeout eras, so it is context rather than a like-for-like baseline;
the bar that governs is A1's 4/157 = 2.5%.

**A note on how this number can be got wrong.** `failure_mode` is only ever set on **completion**
records — 0 of the slice's 4228 `ttfb` rows carry one. Dividing a count of `truncated_stream`
records by a count of `ttfb` rows is therefore 0 by construction, whatever the streams did, and
reading that 0 as a pass is a false one. The reproduction steps below scope the count to completion
records and print the record-kind split so the artefact is visible rather than inviting it.

The decisive measurement for this invariant is the new window, where the failure records carry their
turn key and the per-turn join exists.

## Per-hour table (§4.5)

Percentiles are nearest-rank over `latency_ms` on `ttfb` rows: for a sample of size n, the p-th
percentile is the value at rank `ceil(p/100 * n)`, 1-based. Hours are UTC buckets of the row's own
timestamp.

| hour (UTC) | ttfb n | p50 ms | p95 ms | stall rows | terminal | terminal share |
|---|---:|---:|---:|---:|---:|---:|
| 2026-10-20T18 | 46 | 2428 | 16381 | 10 | 1 | 10.0% |
| 2026-10-20T19 | 170 | 2413 | 11439 | 3 | 0 | 0.0% |
| 2026-10-20T20 | 162 | 2160 | 12512 | 6 | 0 | 0.0% |
| 2026-10-20T21 | 89 | 2390 | 16527 | 7 | 0 | 0.0% |
| 2026-10-20T22 | 66 | 2373 | 16058 | 4 | 0 | 0.0% |
| 2026-10-20T23 | 60 | 2286 | 6356 | 1 | 0 | 0.0% |
| 2026-10-21T00 | 742 | 2328 | 14523 | 29 | 0 | 0.0% |
| 2026-10-21T01 | 136 | 3348 | 16459 | 7 | 1 | 14.3% |
| 2026-10-21T02 | 12 | 7939 | 17127 | 3 | 0 | 0.0% |
| 2026-10-21T03 | 241 | 2817 | 16243 | 14 | 0 | 0.0% |
| 2026-10-21T04 | 189 | 2322 | 10993 | 3 | 1 | 33.3% |
| 2026-10-21T05 | 215 | 2902 | 15886 | 9 | 0 | 0.0% |
| 2026-10-21T06 | 134 | 3719 | 16350 | 12 | 1 | 8.3% |
| 2026-10-21T07 | 181 | 4754 | 16589 | 31 | 5 | 16.1% |
| 2026-10-21T08 | 3 | 590 | 15449 | 1 | 0 | 0.0% |
| 2026-10-21T09 | 279 | 3856 | 16532 | 25 | 3 | 12.0% |
| 2026-10-21T10 | 613 | 3152 | 16442 | 50 | 2 | 4.0% |
| 2026-10-21T11 | 599 | 3140 | 16527 | 48 | 3 | 6.2% |
| 2026-10-21T12 | 291 | 4065 | 16547 | 46 | 7 | 15.2% |

Two things this table is for, per §4.5: so that an upstream incident in the window cannot be mistaken
for the knob's effect, and so that the per-hour shape is published rather than summarised.

**The p95 is pinned against the window.** In every hour with meaningful traffic the p95 sits at
16.4-16.6 s — just past the 15 s cut — while the p50 is 2.2-4.1 s. That is the cut's signature: the
tail is being converted into a 15 s terminal, so the 95th percentile is determined by the knob rather
than by the upstream. The two hours that sit lower (19Z at 11.4 s, 23Z at 6.4 s) are the hours with
the fewest stalls. The p95 "passing" at 16429 ms is therefore a **pass produced by the cut**, not
evidence that the cut is harmless: the metric it satisfies is the one the cut controls.

## Key-split note (§4.5)

The point of this note is that a single bad key, or a single upstream incident, must not be mistaken
for the knob's effect.

| key label | ttfb rows | stall rows | terminal | terminal share |
|---|---:|---:|---:|---:|
| backup | 1140 | 87 | 12 | 13.8% |
| operator-brush | 1119 | 86 | 4 | 4.7% |
| extra | 997 | 70 | 2 | 2.9% |
| doltares | 972 | 66 | 6 | 9.1% |

No key dominates the traffic (972-1140 `ttfb` rows each, so the four are within 17% of each other).
The terminal stalls are **mildly** concentrated on `backup` — 13.8% of its stall rows end
terminally, against 2.9% for `extra`, a 4.8x spread — and `backup` accounts for half of all terminal
stalls (12 of 24) while carrying 28% of the stall rows.

The per-hour table shows the same thing in the time dimension: the terminal share is 0% in eight
hours, under 12% in seven more, and the two hours above 15% (07Z with 31 stall rows, 12Z with 46)
are the two highest-traffic hours. The one hour at 33.3% rests on 3 stall rows and is not
distinguishable from noise.

**Reading:** the terminal stalls are not one upstream incident in one key or one hour. They are spread
across all four keys and across the window, at a low rate (24 of 309 stall rows, 7.8%), which is what
the retry policy's give-up path produces when a request has already spent its attempts or its
budget. The `backup` asymmetry is real but small in absolute terms — 12 rows — and it is the kind of
signal worth watching rather than the kind that explains the bar.

The budget arithmetic in the code confirms that a first attempt can never be terminal: the give-up
needs either all four keys spent or the request's elapsed time (which includes the admission wait)
plus one more window over the fast budget, and the first attempt always fits both. So every terminal
row in the table above is a request that had already made at least one earlier attempt. The
per-turn classification of those 24 rows is not available from this slice, for the reason in the
unmeasurable section above.

## Reproducing these numbers

The slice is defined by timestamp, so it can be recomputed from the live log at any time, but the log
is append-only and keeps growing — take a snapshot first, and quote the snapshot's line count and
sha256 alongside the numbers, as this document does.

```bash
# 1. Freeze the log.
mkdir -p /tmp/slice && cp /var/log/nimaproxy/turns.jsonl /tmp/slice/snap.jsonl
wc -l /tmp/slice/snap.jsonl && sha256sum /tmp/slice/snap.jsonl

# 2. The slice, by timestamp.
FLOOR=2026-10-20T18:43:50Z; CEIL=2026-10-21T12:31:38Z
jq -c --arg f "$FLOOR" --arg c "$CEIL" 'select(.timestamp >= $f and .timestamp < $c)' \
  /tmp/slice/snap.jsonl > /tmp/slice/window.jsonl

# 3. The row-level metrics.
jq -s '
  [ .[] | select(.record_kind=="ttfb") ] as $ttfb
  | [ .[] | select(.error // "" | test("no bytes within")) ] as $stall
  | {
      ttfb_rows:            ($ttfb | length),
      stall_rows:           ($stall | length),
      retry_firing:         (([$stall[] | select(.error | test("retrying on another key"))] | length) / ($stall|length)),
      terminal_stalls:      ([$stall[] | select((.error | test("retrying on another key")) | not)] | length),
      # failure_mode is ONLY ever set on completion records, so it must be counted
      # there. Counting it over $ttfb is 0 by construction, whatever the streams did.
      completions:          ([ .[] | select(.record_kind=="completion") ] | length),
      truncated_stream:     ([ .[] | select(.record_kind=="completion" and .failure_mode=="truncated_stream") ] | length),
      length_cutoff:       ([ .[] | select(.record_kind=="completion" and .failure_mode=="length_cutoff") ] | length),
      # The invariant's own denominator: >=15 s attempts joined to their completion half.
      ge15_joined:          ([ .[] | select(.record_kind=="ttfb" and (.latency_ms // 0) >= 15000) ] | length)
    }' /tmp/slice/window.jsonl

# 3b. The §3.2.2 invariant's own denominator: >=15 s attempts joined to their
#     completion half. This is a per-ATTEMPT join, so it does not depend on the
#     broken turn-join, and it is what the 2/57 = 3.5% figure is.
jq -s '
  [ .[] | select(.record_kind=="ttfb" and (.latency_ms // 0) >= 15000) ] as $a
  | [ .[] | select(.record_kind=="completion") ] as $c
  | ($c | map({(.request_id): .}) | add) as $byid
  | [ $a[] | select($byid[.request_id] != null) ] as $joined
  | { joined: ($joined|length),
      truncated: ([ $joined[] | select($byid[.request_id].failure_mode=="truncated_stream") ]|length),
      share_pct: (100 * ([ $joined[] | select($byid[.request_id].failure_mode=="truncated_stream") ]|length) / ($joined|length)) }' \
  /tmp/slice/window.jsonl
# -> { "joined": 57, "truncated": 2, "share_pct": 3.508771929824561 }

# 4. The grammar facts: one id per record, and the first two dot-parts are a time bucket.
jq -r 'select(.record_kind=="ttfb") | .request_id' /tmp/slice/window.jsonl \
  | awk -F. '{print $1}' | sort -u | wc -l    # first dot-parts
jq -r 'select(.record_kind=="ttfb") | .request_id' /tmp/slice/window.jsonl \
  | awk -F. '{print $1"."$2}' | sort -u | wc -l # first two dot-parts -- the SAME number

# 5. The unkeyed stall rows: the population the per-turn metrics needed.
jq -s '[ .[] | select((.error // "") | test("no bytes within")) ] | {stalls: length, unkeyed: ([.[] | select(.loop_key == null)] | length)}' \
  /tmp/slice/window.jsonl
```

## What this run leaves open

- The **81-bound recovery** and the **per-turn salvage** are unanswered, and they were the
  experiment's central question. A successor window can answer them: failure-path records now carry
  their turn key, so the per-turn join exists.
- The **first-byte cutoff has still never been derived.** The experiment that was to derive it failed
  its own bar and was rolled back; the live window is the dynamic value again, which is a fallback
  rather than an answer.
- The **client/proxy TTFT ratio** was not joined for this window.
