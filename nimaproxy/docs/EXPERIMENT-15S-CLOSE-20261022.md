# EXPERIMENT-15S — close evidence for the stage-4 floor (2026-10-22)

**This is not a results artifact.** It records the close-time evidence for the slice that §11.1
of `EXPERIMENT-15S-WINDOW.md` names, and it states why that slice's nine criteria are **not
scoreable**. The window that the estate is publishing is a different, re-floored window; see
"Which window this is" below. The earlier run's artifact is `EXPERIMENT-15S-RESULTS.md` and is
left untouched.

## Which window this is

| | |
|---|---|
| the slice this evidence covers | floor `2026-10-21T14:50:43Z` &rarr; close `2026-10-22T14:50:43Z` (§11.1's floor and close) |
| binary named as under test by §11.1 | `stage4-20261021`, revision `2e6b6e0` |
| binary that actually served the later rows | `stage6-20261021`, revision `fdef683b`, container started `2026-10-21T18:11:43.607673689Z`, 0 restarts |
| the estate's window | floor `2026-10-21T18:11:43Z` &rarr; close `2026-10-22T18:11:43Z`, artifact `windows/20261022-stage6-24h/RESULTS.md` |

**The §11 slice spans two binaries.** The stage-6 deploy at `2026-10-21T18:11:43Z` is 3 h 21 m
after the §11.1 floor, so 3 h 21 m of the slice was served by stage-4 and the remaining 20 h 39 m
by stage-6. The estate's own close script refuses exactly this shape: `close-window-20261021.sh`
check 0 reads the container's `StartedAt` and exits **4** ("the container started after the floor
&mdash; a binary boundary inside the slice; re-floor and re-run"). `close-window-20261022.sh`
therefore re-floors to `2026-10-21T18:11:43Z` and publishes to
`/opt/nimaproxy-config/windows/20261022-stage6-24h/`, armed by the estate's close timer
(the user unit whose name ends `-close.timer`) at `2026-10-22 18:12:00Z`.

Consequently **the nine §11.2 criteria are not scoreable over this slice**, and this document does
not publish them as a verdict. The only single-binary slice of §11.1's binary is 3.35 h long,
and §10.5 is explicit: extend the window, do not evaluate the criteria on a smaller slice.

## The frozen snapshot

Taken with §11.4's procedure **as it stands in `origin/main` at `8623c5c5`** (PR #54, merged
2026-10-22T14:34:40Z) &mdash; the version with the BEFORE capture above the copy loop, the inode
identity check in place of the size equality, and the source-versus-copied name-set check.

| | |
|---|---|
| snapshot | `snapshot-20261022T145303Z.jsonl` |
| lines | **80954** |
| sha256 | `fc81f70630f8ae1f870740086bcba2ce351d85450ed488275dad2477a253add4` |
| first record | 2026-06-14T06:07:55.478488646Z |
| last record | 2026-10-22T14:52:59.887680109Z |

| file | lines |
|---|---:|
| turns.jsonl.20261022_095148 (rotated, 67,109,329 B, rotated 2026-10-22T09:51:48Z) | 78760 |
| turns.jsonl.LIVE-last (live, 2,367,675 B at 14:52:59Z) | 2194 |
| snapshot-20261022T145303Z.jsonl (concatenation, rotated first, live last) | 80954 |

Two deviations from a textbook close, both recorded rather than hidden:

1. **The snapshot was taken at 14:53:03Z, not at 14:50:43Z.** The first attempt failed at the
   first step &mdash; `mkdir: Permission denied` &mdash; because `/var/log/nimaproxy` is owned by
   uid 10001 and is not writable by the reporting user. The snapshot was therefore written to a
   writable directory, and it contains 2 m 20 s of rows written after the close. The slice is cut
   on the snapshot's own timestamps, so those rows are excluded by the ceiling and the window's
   records are unaffected.
2. **The snapshot's line count and sha256 are of the concatenation**, and the per-file counts
   above are the manifest that shows the two-file rotation case was concatenated rotated-first,
   live-last &mdash; the case §11.4 was rewritten for. The rotation landed at
   `2026-10-22T09:51:48Z`, 4 h 33 min before the close, so the rotation case is the live case.

## The era split, measured

| era | slice | ttfb | completion | loop | p50 ms | p95 ms | elapsed |
|---|---|---:|---:|---:|---:|---:|---:|
| stage-4 (§11.1's binary) | 14:50:43Z &rarr; 18:11:43Z | 1143 | 1025 | 163 | 3787 | 26502 | 3.35 h |
| stage-6 | 18:11:43Z &rarr; 14:50:43Z | 6068 | 5344 | 2358 | 4733 | 26578 | 20.65 h |
| the §11 slice as a whole | 14:50:43Z &rarr; 14:50:43Z | 7211 | 6369 | 2521 | 4606 | 26558 | 24.00 h |

Boundary evidence rather than an assertion: 13 rows (9 `ttfb`, 4 `completion`) fall within
&plusmn;30 s of the container start. The last row at or before `18:11:43.607673689Z` is a
`completion` at `18:11:33.837434229Z`; the first row after it is a `ttfb` at `18:11:50.400374297Z`.
The 16.6 s gap is the restart, and no row straddles the boundary.

## The minima, as measured (§4.4)

| Minimum | Bar | stage-4 era (3.35 h) | stage-6 era (20.65 h) | §11 slice (24.00 h) |
|---|---|---|---|---|
| &gt;=24 h post-restart | 24 h | **NO** | **NO** | YES |
| &gt;=300 `ttfb` rows | 300 | YES (1143) | YES (6068) | YES (7211) |
| &gt;=20 stall attempts | 20 | YES (105) | YES (702) | YES (807) |

Neither era meets the 24 h minimum, which is why neither can be scored. The §11 slice meets all
three &mdash; and is still not scoreable, because it is not one window.

## The nine criteria, measured per era

These are measurements, not verdicts: neither era is a scoreable window. The stage-6 column is
what the estate's armed close will score over its full 24 h; the numbers here stop at the
14:50:43Z close.

**stage-4 era** (the §11.1 binary, 3.35 h &mdash; not scoreable, the 24 h minimum is unmet):

| # | criterion | bar | measured | verdict |
|---|---|---|---|---|
| 1 | ttfb p95 | &lt;=19000 ms pass / &gt;21000 ms fail | 26502 ms (p50 3787 ms, n=1143) | **FAIL** |
| 2 | ttfb p50 | reported, no bar | 3787 ms | **REPORTED** |
| 3 | terminal-504 share and completion share | &lt;=43.9% and completion share &gt;=53.5% | 105 terminal-504 of 214 attempts &gt;=15 s = 49.07%; completion share 50.93% (109 clean of 214) | **FAIL (terminal FAIL, completion FAIL)** |
| 4 | truncated_stream share | &lt;=2.5% of the &gt;=15 s attempts with a completion half | 0 of 109 = 0.00% | **PASS** |
| 5 | per-turn recovery rate | &gt;=25% of the turns with &gt;=1 stall attempt | 80 of 84 turns = 95.24% | **PASS** |
| 6 | per-turn salvage rate | &gt;=9.5% of the turns with &gt;=1 stall attempt | 72 of 84 turns = 85.71% | **PASS** |
| 7 | request-side vs response-side tool-call count | the two must not be conflated | request-side max 350 (sum 108972); response-side max 6 (sum 1303) | **REPORTED side by side** |
| 8 | the step-5 cap is effective | no row exceeds 32768 and the maximum effective request_max_tokens equals 32768 | max effective 32768; 0 rows over 32768 | **PASS** |
| 9 | the breaker is not tripped inside the cap | no breaker trip on a response within 32768 tokens | not observable in the turn log &mdash; see below | **NOT MEASURED** |

**stage-6 era** (20.65 h of the estate's 24 h window &mdash; partial, the 24 h minimum is unmet):

| # | criterion | bar | measured | verdict |
|---|---|---|---|---|
| 1 | ttfb p95 | &lt;=19000 ms pass / &gt;21000 ms fail | 26578 ms (p50 4733 ms, n=6068) | **FAIL** |
| 2 | ttfb p50 | reported, no bar | 4733 ms | **REPORTED** |
| 3 | terminal-504 share and completion share | &lt;=43.9% and completion share &gt;=53.5% | 702 terminal-504 of 1372 attempts &gt;=15 s = 51.17%; completion share 48.03% (659 clean of 1372) | **FAIL (terminal FAIL, completion FAIL)** |
| 4 | truncated_stream share | &lt;=2.5% of the &gt;=15 s attempts with a completion half | 10 of 669 = 1.49% | **PASS** |
| 5 | per-turn recovery rate | &gt;=25% of the turns with &gt;=1 stall attempt | 376 of 527 turns = 71.35% | **PASS** |
| 6 | per-turn salvage rate | &gt;=9.5% of the turns with &gt;=1 stall attempt | 350 of 527 turns = 66.42% | **PASS** |
| 7 | request-side vs response-side tool-call count | the two must not be conflated | request-side max 380 (sum 318412); response-side max 8 (sum 5027) | **REPORTED side by side** |
| 8 | the step-5 cap is effective | no row exceeds 32768 and the maximum effective request_max_tokens equals 32768 | max effective 32768; 0 rows over 32768 | **PASS** |
| 9 | the breaker is not tripped inside the cap | no breaker trip on a response within 32768 tokens | not observable in the turn log &mdash; see below | **NOT MEASURED** |

**the §11 slice as a whole** (mixed binaries &mdash; shown only so the mixed numbers are on the
record, not as a verdict):

| # | criterion | bar | measured | verdict |
|---|---|---|---|---|
| 1 | ttfb p95 | &lt;=19000 ms pass / &gt;21000 ms fail | 26558 ms (p50 4606 ms, n=7211) | **FAIL** |
| 2 | ttfb p50 | reported, no bar | 4606 ms | **REPORTED** |
| 3 | terminal-504 share and completion share | &lt;=43.9% and completion share &gt;=53.5% | 807 terminal-504 of 1586 attempts &gt;=15 s = 50.88%; completion share 48.42% (768 clean of 1586) | **FAIL (terminal FAIL, completion FAIL)** |
| 4 | truncated_stream share | &lt;=2.5% of the &gt;=15 s attempts with a completion half | 10 of 778 = 1.29% | **PASS** |
| 5 | per-turn recovery rate | &gt;=25% of the turns with &gt;=1 stall attempt | 456 of 611 turns = 74.63% | **PASS** |
| 6 | per-turn salvage rate | &gt;=9.5% of the turns with &gt;=1 stall attempt | 422 of 611 turns = 69.07% | **PASS** |
| 7 | request-side vs response-side tool-call count | the two must not be conflated | request-side max 380 (sum 427384); response-side max 8 (sum 6330) | **REPORTED side by side** |
| 8 | the step-5 cap is effective | no row exceeds 32768 and the maximum effective request_max_tokens equals 32768 | max effective 32768; 0 rows over 32768 | **PASS** |
| 9 | the breaker is not tripped inside the cap | no breaker trip on a response within 32768 tokens | not observable in the turn log &mdash; see below | **NOT MEASURED** |

## Three definitions a reader must not misread

**The stall filter has two wordings.** §11.4's canonical command is
`error~no bytes within count`, which counts 803 rows in the §11 slice. It does not match
the other timeout wording, `upstream timeout after 25000ms` (4 rows), so the union is
807. The terminal-504 count in bar 3 is 807, which equals the union, not the
canonical filter's number.

**"Terminal" and "stall" are two different populations.** Bar 3's denominator is latency-based:
every `ttfb` attempt with `latency_ms >= 15000` (1586 rows in the §11 slice), which in this
window includes slow successes inside the dynamic window's [15 s, 25 s) band. The per-hour
"stall rows" column is error-based (the timeout wordings). The bars themselves
(&lt;=43.9% terminal-504, &gt;=53.5% completion share) are **A1 §3's pre-experiment baseline,
carried from §5.1 trigger 8**, measured while the 15 s override was absent &mdash; that is, under
the same dynamic-25 s first-byte window this run uses, over the same latency-based
"&gt;=15 s attempts" denominator §11.2 restates. The 84.8% / 14.7% figures are the **15 s run's own
measurements**, not the baseline's. So bar 3 is like-for-like in regime, and 50.88% terminal
(51.17% in stage-6) with a 48.42% completion share (48.03% in stage-6) is a genuine FAIL of both
clauses. The caveat that does apply: A1's baseline window spans 22Z&ndash;09Z with zero-traffic
hours, so it is not a diurnal-24 h slice like this one.

**These are §11-protocol bars, not the estate's Q1/Q2/Q3.** The terminal-504 finding above is
therefore a handover to the estate, whose armed close scores the successor window's Q1/Q2/Q3
from `measure.py` &mdash; a different metric set with different key names, not a trigger of that
window.

**Bar 9 is not measurable from the turn log.** The log's completion vocabulary is
`tool_calls`, `clean_stop`, `length_cutoff`, `truncated_stream` &mdash; there is no breaker label
and no breaker field, so "no breaker trip" cannot be read off this log. Reporting zero breaker
references would be a proxy presented as the metric, which §11.4 forbids. `length_cutoff` is a
different class (the upstream hit its token limit) and is not the breaker.

## Per-hour table (stage-6 era)

Percentiles are nearest-rank over `latency_ms` on `ttfb` rows: for a sample of size n, the p-th
percentile is the value at rank `ceil(p/100 * n)`, 1-based. Hours are UTC buckets of the row's
own timestamp. The final hour is **partial** &mdash; the slice ends at 14:50:43Z, so the 14Z
bucket covers 50 minutes, not 60.

Column definitions, so the two counts are not read as one: **"stall rows"** is error-based over
`ttfb` rows &mdash; the canonical `no bytes within` wording **plus** the
`upstream timeout after 25000ms` variant (the union, 807 rows over the whole §11 slice).
**"terminal share"** is `stall rows` divided by **that hour's `ttfb n`**, not by the hour's stall
rows. The prior artifact's column of the same name divides by the hour's stall rows, so the two
tables carry different quantities under one name and must not be compared cell by cell.

| hour (UTC) | ttfb n | p50 ms | p95 ms | stall rows | terminal | terminal share (of ttfb n) |
|---|---:|---:|---:|---:|---:|---:|
| 2026-10-21T18 | 380 | 3076 | 25074 | 19 | 19 | 5.0% |
| 2026-10-21T19 | 674 | 2521 | 21880 | 26 | 26 | 3.9% |
| 2026-10-21T20 | 584 | 2620 | 21744 | 17 | 17 | 2.9% |
| 2026-10-21T21 | 511 | 2543 | 17474 | 13 | 13 | 2.5% |
| 2026-10-21T22 | 304 | 2519 | 15012 | 8 | 8 | 2.6% |
| 2026-10-21T23 | 134 | 2706 | 18768 | 3 | 3 | 2.2% |
| 2026-10-22T00 | 66 | 2829 | 26470 | 6 | 6 | 9.1% |
| 2026-10-22T01 | 67 | 4787 | 26652 | 7 | 7 | 10.4% |
| 2026-10-22T02 | 64 | 5126 | 26911 | 7 | 7 | 10.9% |
| 2026-10-22T03 | 67 | 6063 | 26895 | 8 | 8 | 11.9% |
| 2026-10-22T04 | 66 | 6558 | 26929 | 6 | 6 | 9.1% |
| 2026-10-22T05 | 266 | 6273 | 26458 | 33 | 33 | 12.4% |
| 2026-10-22T06 | 533 | 8446 | 26518 | 63 | 63 | 11.8% |
| 2026-10-22T07 | 533 | 6379 | 26435 | 33 | 33 | 6.2% |
| 2026-10-22T08 | 492 | 7909 | 26667 | 40 | 40 | 8.1% |
| 2026-10-22T09 | 285 | 10292 | 26670 | 44 | 44 | 15.4% |
| 2026-10-22T10 | 180 | 8193 | 26670 | 23 | 23 | 12.8% |
| 2026-10-22T11 | 78 | 7031 | 26810 | 10 | 10 | 12.8% |
| 2026-10-22T12 | 108 | 16002 | 26965 | 32 | 32 | 29.6% |
| 2026-10-22T13 | 372 | 20758 | 26955 | 144 | 144 | 38.7% |
| 2026-10-22T14 | 304 | 25940 | 27090 | 160 | 160 | 52.6% |

## Reproducing these numbers

Every number above is recomputable from the one frozen snapshot, by timestamp, with the
commands below. The snapshot's identity is `snapshot-20261022T145303Z.jsonl` &mdash; 80954 lines, sha256 `fc81f70630f8ae1f870740086bcba2ce351d85450ed488275dad2477a253add4`.

```bash
SNAP=/var/tmp/snapshot-20261022T145303Z.jsonl
wc -l "$SNAP"
sha256sum "$SNAP"

# The §11 slice (floor -> close), and each era.
for pair in "mixed 2026-10-21T14:50:43Z 2026-10-22T14:50:43Z" \
            "stage4 2026-10-21T14:50:43Z 2026-10-21T18:11:43Z" \
            "stage6 2026-10-21T18:11:43Z 2026-10-22T14:50:43Z"; do
  set -- $pair; NAME=$1; FLOOR=$2; CEIL=$3
  jq -c --arg f "$FLOOR" --arg c "$CEIL" 'select(.timestamp >= $f and .timestamp < $c)' \
    "$SNAP" > /tmp/$NAME-slice.jsonl
  wc -l /tmp/$NAME-slice.jsonl
done

# The row-level metrics for one slice.
jq -s '
  def nr($a; $p): ($a | sort) as $s | $s[((($p/100)*($s|length))|ceil) - 1];
  [ .[] | select(.record_kind=="ttfb") ] as $ttfb
  | [ .[] | select(.record_kind=="completion") ]
    | map({key: .request_id, value: .}) | from_entries as $comp
  | [ $ttfb[] | select((.latency_ms|tonumber) >= 15000) ] as $ge15
  | [ $ge15[] | .request_id ] as $ids
  | [ $ids[] as $id | select($comp[$id] != null) | $comp[$id] ] as $joined
  | [ $joined[] | select(.failure_mode == "truncated_stream") ] as $trunc
  | [ $joined[] | select(.failure_mode != "truncated_stream" and .success == true) ] as $clean
  | {
      ttfb_rows:          ($ttfb | length),
      p50_ms:             nr([$ttfb[] | .latency_ms|tonumber]; 50),
      p95_ms:             nr([$ttfb[] | .latency_ms|tonumber]; 95),
      ge15_attempts:      ($ge15 | length),
      terminal_504:       ([$ge15[] | select((.status_code|tostring)=="504")] | length),
      terminal_share_pct: (([$ge15[] | select((.status_code|tostring)=="504")] | length) * 100 / ($ge15|length)),
      with_completion:    ($joined | length),
      clean_completions:  ($clean | length),
      truncated:          ($trunc | length),
      completion_share_pct:(($clean | length) * 100 / ($ge15|length)),
      truncated_share_pct:(($trunc | length) * 100 / ($joined|length)),
      stall_canonical:    ([$ttfb[] | select((.error//"") | test("no bytes within"))] | length),
      stall_variant:      ([$ttfb[] | select((.error//"") | test("timeout after"))] | length),
      cap_max_rtt:        ([$ttfb[] | select(.responding_model=="stepfun-ai/step-5-preview") | .request_max_tokens|tonumber] | max),
      cap_rows_over_32768:([$ttfb[] | select((.request_max_tokens|tonumber) > 32768)] | length)
    }
' /tmp/mixed-slice.jsonl

# The era boundary: the last row at or before the container start, and the first after.
jq -r 'select(.timestamp <= "2026-10-21T18:11:43.607673689Z") | .timestamp' "$SNAP" | tail -1
jq -r 'select(.timestamp >  "2026-10-21T18:11:43.607673689Z") | .timestamp' "$SNAP" | head -1
```

The per-turn metrics (bars 5 and 6) need the turn key and the stall population, and both are
defined by the protocol rather than chosen here:

- **Turn key** &mdash; `SUCCESSOR-WINDOW.md` §2.4: "`loop_key` is `{session_id, loop_seq,
  round_index}`. It is canonicalised as JSON with sorted keys." So the turn is the **full
  triple**, not `session_id` alone. In the §11 slice all 16,100 rows carry a `loop_key`, and
  368 of its 392 distinct `session_id` values hold more than one triple (up to 2,291), so
  grouping by `session_id` merges turns the protocol separates: 392 sessions vs **8,665**
  distinct triples.
- **Stall population** &mdash; `T_stall`, the set of turns containing at least one `ttfb` attempt
  whose `error` matches the canonical `no bytes within` wording (§11.4's own command). The
  latency-based `latency_ms >= 15000` predicate is **bar 3's** denominator and must not be
  substituted here.

```python
CANON = "no bytes within"
CLEAN = {"clean_stop", "tool_calls"}   # failure_mode, not finish_reason

first_stall, last_comp, first_clean = {}, {}, {}
for r in slice_rows:
    lk = r["loop_key"]
    turn = (lk["session_id"], lk["loop_seq"], lk["round_index"])
    ts = r["timestamp"]
    if r["record_kind"] == "ttfb" and CANON in (r.get("error") or ""):
        if turn not in first_stall or ts < first_stall[turn]:
            first_stall[turn] = ts
    if r["record_kind"] == "completion":
        if turn not in last_comp or ts > last_comp[turn]:
            last_comp[turn] = ts
        if (r.get("failure_mode") or "") in CLEAN:
            if turn not in first_clean or ts < first_clean[turn]:
                first_clean[turn] = ts

stall_turns = list(first_stall)
# both are ORDERED: the completion must come at/after the turn's first stall.
# bar 5 uses the LAST completion half; bar 6 uses the EARLIEST clean completion
# (measure.py keeps the first clean, not the last).
recovered = [t for t in stall_turns if last_comp.get(t, "") >= first_stall[t]]
salvaged  = [t for t in stall_turns if first_clean.get(t, "") >= first_stall[t]]
assert set(salvaged) <= set(recovered)   # recovery is a superset of salvage
```

Two corrections this block records against the first published version of this document, so a
reader comparing the two can see exactly what changed:

1. **The turn key.** The first version grouped by `loop_key.session_id` alone, which merges
   turns the protocol separates. On the correct triple the mixed-slice `T_stall` is **611**
   turns, not the 160 the first version published.
2. **The stall predicate.** The first version pre-filtered attempts by
   `latency_ms >= 15000` &mdash; bar 3's latency denominator &mdash; instead of the error-based
   stall wording §11.2 defines for `T_stall`. That is why the first version's turn counts
   reproduced as 32 / 128 / 160 under the latency predicate and 132 under the error predicate
   on the `session_id` key, while the protocol-correct combination gives 84 / 527 / 611.
3. **Which clean completion counts as the salvage.** `measure.py` keeps the turn's **earliest**
   clean completion (`turn_clean` keeps first), not the last, so bar 6 above uses the earliest.
   The two readings disagree on **23 turns** in the mixed slice (7 in stage-4, 16 in stage-6):
   turns whose earliest clean completion *precedes* the first stall while a later clean
   completion follows it. Taking the latest instead would report 445 salvaged of 611 (72.83%)
   rather than the protocol's 422 (69.07%). Both readings are published here so the choice is
   visible rather than implicit; the tables carry the protocol's earliest-clean reading.

Bars 5 and 6 still **PASS** in every era on the corrected definitions, so the correction changes
the denominators rather than the verdicts &mdash; but the first version's numbers were computed
on definitions the protocol does not define, and are superseded by the table above.

Two independent implementations (a Python pass and the jq above) were cross-checked against the
**previous** window's published numbers: both reproduce its population (374), terminal-504 count
(317), joined rows (57), clean completions (55), truncations (2), terminal share (84.8%),
completion share (14.7%), truncated share (3.5%), p50 (2748 ms), p95 (16429 ms) and stall-row
count (309) exactly. The definitions used here are therefore the protocol's own.

## What this run leaves open

- The nine §11.2 criteria are unmeasured for any scoreable window: the stage-4 era is 3.35 h and
  the stage-6 era is 20.65 h at this close, and the estate's armed close at 18:12Z scores the
  re-floored 24 h window from the same log.
- The p95 sits at the dynamic 25 s bound in every era, so bar 1's band (&lt;=19000 pass /
  &gt;21000 fail) is a statement about the bound under test, not about the upstream. The previous
  run's p95 of 16429 ms was the 15 s cut's signature in the same way.
- The §11 slice's p95 (26558 ms) must not be quoted as a window result: it is a mix of two
  binaries' traffic.
