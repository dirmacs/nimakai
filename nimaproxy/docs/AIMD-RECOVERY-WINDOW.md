# AIMD RECOVERY WINDOW — design (pre-registration input)

**Status:** DESIGN. Nothing here is a result; every number is either a baseline quoted from a
measured reading or a threshold with its derivation stated.

**What this window is for.** The proxy sheds a large share of client requests before they reach the
upstream. Two independent failure modes produce that, and a per-key AIMD rule can only reach one
of them. This document says which is which, because a window that cannot tell them apart would
report a flat headline while the failure mode flipped underneath it.

## 1. The measured state this window starts from

Read live from the proxy's own counters and `/stats`, during the 24 h window that closed on
2026-10-22. This table is an **episode**, not the standing state: the pool collapsed hard for a
bounded period and recovered. It is also a **25 000 ms-era** measurement — see §7 for why the AIMD window's baseline is re-derived at its own floor.

| quantity | value | source |
|---|---|---|
| per-key AIMD window | **1 of a configured 4**, on every key, **at 06:55–07:00** | `/stats` `max_in_flight` vs `configured_max_in_flight` |
| effective window sum | **4 of 16 at 06:55–07:00; 11 of 16 at 10:07; 12 of 16 at 10:3x** | `/stats`; dry-run capture 10:07, live reads 06:55 and 10:3x |
| upstream 429 rate | **18.47 %** (836 / 4525) | counter delta `rate_limit_count / upstream_attempts` |
| client requests admission-rejected | **47.5 %** | counter delta `(overload_rejects + no_key_rejects) / request_total` |
| active keys | 0–2 of 4 across 40 samples over 160 s | `/stats` |

**The 429 rate is a COUNTER, not a turn-log figure.** The upstream-429 path writes no log row: it
records the counter, benches the key, prints to stderr and moves to the next key. Any analysis that
looks for 429s in the turn log will find none, and a turn log cannot see admission rejections at
all — which is why the counter side is the only instrument for this change.

**The configured per-key ceiling is INERT.** `record_rate_limited` halves from the window's
current value, and every window is pinned at 1, so raising or lowering `max_in_flight_per_key`
changes almost nothing today: the live window is 1, not the ceiling. That is stated here so a
reader does not mistake the ceiling for a lever.

## 2. The even per-key 429 histogram does NOT establish an account-level limit

The per-key 429 histogram, read from the proxy's own stderr:

```
    256 key 2 rate-limited      247 key 3 rate-limited
    246 key 0 rate-limited      240 key 1 rate-limited
```

**Even within 6 % across all four keys — and that evenness is NON-DISCRIMINATING.** The proxy
picks keys round-robin, and every 429 fans out to the *next* key. A per-key upstream limit and an
account-level limit therefore produce the SAME even histogram: whichever key is picked first takes
the 429, is benched, and the retry lands on a different key that shares the same upstream quota.
The histogram cannot separate the two hypotheses.

**So the account-level premise is UNPROVEN, and the account-scope penalty design built on it is
rejected.** A penalty scoped to the account would count one penalty for the whole episode, while
the per-key scope counts one per key; under the even-histogram regime the latter is strictly
stronger, so choosing the account scope would have *weakened* the throttle while claiming to be
the principled fix.

What the histogram does establish is a weaker, still-actionable fact: **the 429s are spread across
all four keys rather than concentrated on one**, so there is no single key to quarantine. That is
consistent with both hypotheses and is all this document relies on.

## 2a. The shipped correction: one AIMD penalty per 429 event

The code change that ships with this design fixes the part that WAS provable — the number of
penalties each upstream event costs, and the operator's visibility into it:

- **One AIMD penalty per 429 event.** The racing path recorded the penalty per leg *and* applied
  `mark_rate_limited` again after the join, so a single 429 leg halved the key's window twice
  (4 → 2 → 1) and pinned it at the floor. The post-join step is now bench-only, so one leg halves
  the window exactly once (4 → 2). The halving rule itself is unchanged; only the duplicate
  penalty is gone.
- **A bench-only key-pool operation.** `KeyPool::bench_key(idx, secs)` applies a
  `BenchReason::RateLimit` cooldown without touching the AIMD window, the 429 ratio, or the
  recovery credit. `KeyPool::mark_rate_limited(idx, secs)` remains the combined operation
  (one penalty + one bench) used by the sequential chat, fallback, and legacy HTTP paths. The
  configured bench cap stays a **maximum**: it can shorten a requested bench, never extend one.
  An **active auth quarantine survives** a later rate-limit bench: a 401/403 is credential-level
  evidence that a transient 429 cannot contradict, and without the guard the bench could shorten a
  900 s quarantine to the bench cap and relabel it `RateLimit` in `/stats` and `/health`. The
  AIMD penalty count is unaffected — a 429 still costs exactly one penalty.
- **Upstream `Retry-After` on the legacy HTTP paths.** `/v1/completions` and `/v1/embeddings` used
  to hardcode a 60 s bench; they now parse the upstream's numeric `Retry-After` and fall back to
  60 s when it is missing or malformed, matching the chat, fallback, and racing paths. The shared
  parser accepts exactly RFC 9110 delta-seconds — a non-empty run of ASCII digits — so a signed
  or whitespace-padded spelling such as `+7` is malformed and takes the 60 s fallback rather than
  being accepted by the integer parser.
- **A bounded bench for an unrepresentable duration.** A `Retry-After` value that does not fit in
  `u64`, and therefore cannot be added to the current `Instant`, no longer panics the request
  handler. The effective bench is clamped to 24 hours: long enough to preserve the
  temporary-throttle contract, bounded so the key recovers without operator intervention. The
  configured bench cap still applies first and remains a maximum that only shortens.
- **Observability for the benched-fraction instrument.** `/stats` and `/health` now serialize each
  key's `bench_reason` and `stalls`, and `/stats.gateway` exposes `keys_benched_rate_limit`. This
  is an **instantaneous count** of keys currently benched for rate limiting, excluding auth
  quarantines. Derive the per-key benched **fraction** by sampling that count over the measurement
  window. The per-key `stalls` counter records upstream stalls separately and is **not** an input to
  the benched fraction.
- **Comparable upstream-stall accounting.** Chat, racing, fallback, `/v1/completions`, and
  `/v1/embeddings` increment the per-key `stalls` counter when an admitted upstream attempt's
  effective wait for usable bytes elapses without completion. This includes send, first-byte, and
  body timeouts, the mid-stream idle rail, and the proxy's remaining request deadline when it cuts
  the wait short. The counter covers every admitted wait it names, including the four waits that
  were previously invisible: a committed SSE response that stalls under the mid-stream idle rail
  (counted once, when the guard's abort is observed), a chat 400 error body that times out, a
  racing 4xx/5xx error body that times out, and a fallback attempt whose remaining request
  deadline expires before body collection. A plain body I/O error, a clean stream end, and an
  upstream stream error are not stalls and are not counted.

The correction leaves every **operator-facing default unchanged** — the adaptive recovery rule, the
success bar, and the bench cap keep their existing defaults, and the per-key window *rule* is
untouched. It does change one default-code path: on the racing path a 429 leg now costs one AIMD
penalty instead of two, because the duplicate post-race application is gone. It also stops the
legacy paths from overriding the upstream's own wait, and makes the benched fraction measurable.

## 3. The bench is the dominant term, and it is self-imposed

Every one of the **989** bench events carried **60 s**. On the legacy `/v1/completions` and
`/v1/embeddings` paths that 60 s was **hardcoded**, so those observations cannot distinguish
"the upstream sent no `Retry-After`" from "the proxy ignored the header the upstream did send".
The shipped correction in §2a removes that ambiguity on the legacy paths; the racing and chat
paths already parsed the header.

What the reading does establish is that **the proxy's own default dominated the bench duration**,
and the bench was therefore a large term: roughly one 429 every 20 s, each benching a key for
60 s. That is consistent with the measured 0–2 of 4 keys active, and with a benched key being
the reason a request is refused.

**The recovery interlock.** A 429 both halves a key's window *and* benches it. A benched key is
skipped by key selection, so it serves no traffic, earns no successes — and successes are the
only thing that can grow a window. A benched key therefore earns nothing while benched; what
breaks the interlock is that the credit it banked *before* the bench is decayed rather than
zeroed, so the residue survives the bench instead of being destroyed by it. Any recovery change
that still destroys the banked credit on each 429 leaves the state exactly where it is.

## 4. The two changes, and what each can achieve

**Change A — the bench cap** (`rate_limit_bench_cap_secs`). A **maximum** on how long a bench
lasts: it can only *shorten* a bench the upstream asked to be long, never lengthen one it asked to
be short. `None` (the default) is today's behaviour exactly, so the binary can be deployed inert.

**This is the lever that reaches the 429 half**, because the bench duration is a term the proxy
controls on every path — it does not depend on whether the upstream's limit is per key or per
account. Its risk is real and stated: a key released sooner is handed more requests while the
upstream is still over its limit, which can earn more 429s. The measurement below is built to see
that.

**Change B — the recovery rule** (`aimd_pressure_adaptive`). The credit the key has already
banked is DECAYED on a 429 rather than zeroed, so it survives the bench; and the cost of the
next slot falls as that key's 429 pressure rises, via the pressure-derived bar. Credit still
comes from successes — a benched key earns none — so it is the *residue*, not new credit, that
breaks the interlock. `false` (the default) is today's behaviour exactly.

**This lever reaches the 503 half** — faster recovery frees permits, so `overload_rejects` should
fall. It is explicitly **not** claimed to reduce the upstream 429 rate: it restores capacity the
proxy withheld, and more concurrency against the same upstream quota can only increase 429
pressure. §2 shows the even histogram cannot be used to argue otherwise in either direction.

**Neither change touches the AIMD penalty rule.** The multiplicative halving remains the safety
mechanism that protects the upstream, and it stays exactly as it is — one 429 event, one halving.
What §2a removes is a *duplicate* application of that same rule on the racing path, not the rule
itself.

## 5. The two halves move in OPPOSITE directions

Faster recovery frees permits → the **503 half** (`overload_rejects`) should fall. But more
in-flight per key, sooner → the **upstream-429 half** can rise. **A single "share of client
requests rejected" can therefore stay flat while the failure mode flips from 503 to 429.**

So the window must report three deltas separately:

- `request_total`
- `overload_rejects`
- `rate_limit_count`

and, because admission-rejected requests produce **no turn-log rows**, the counter side is the
only instrument that can see this change at all. The counters themselves carry no per-key
breakdown; per-key state comes from `/stats` and `/health` (§2a, §6), and the per-key 429
histogram comes from the proxy's stderr.

## 6. The instruments, and the bar

Reported per key and over time:

- the **per-key window sum** — the quantity that must move (4 of 16 during the 06:55–07:00 episode, recovered to 11–12 of 16 by 10:07);
- the **per-key benched fraction of time** — the term Change A acts on. Take
  `/stats.gateway.keys_benched_rate_limit` as an instantaneous sample and repeat it over the
  window. Per-key `bench_reason` attributes each benched key to a rate limit (§2a). The per-key
  `stalls` counter is a separate upstream-stall count and is not part of this term;
- the three counter deltas of section 5;
- the per-key 429 histogram of section 2, reported as a **descriptive** observation, not as a
  discriminator between a per-key and an account-level upstream limit.

**The bar must distinguish "the lever moved but capacity did not" from "nothing moved."** If the
bench is the binding constraint, a working recovery change can leave effective capacity flat. So:

| outcome | reading | action |
|---|---|---|
| benched fraction falls **and** effective capacity rises **and** the 503 half falls | both levers reached their targets | the change works; keep it on |
| benched fraction falls **but** capacity is flat **and** the 429 half rises | the bench was released faster than the upstream could absorb | the bench cap is too aggressive; tune it, and keep the recovery change |
| neither the benched fraction nor the window sum moves | the levers did not act | the change did not work; do not ship it |
| the 429 histogram stops being even | the 429s are no longer spread across the pool | descriptive only — re-derive the hypothesis, but do **not** treat this as proof of a limit scope; §2 shows the histogram cannot discriminate |

**Population:** the whole window. **Minimum:** the same minimum the successor design applies, so a
thin population is reported as unmeasured rather than scored.

## 7. Sequencing — this is floor 2 of 2

The `response_body_id` change is **observationally neutral**: it adds a log column and changes no
behaviour. It deploys at the **next** window's floor, so that window measures the **old** AIMD
behaviour and the Q1/Q2 pre-registration stays valid.

**This change deploys at a later floor, of its own, and needs its own window.** Merging it before
the first floor is taken would put it inside the window that was supposed to measure the old
behaviour, and confound the Q1/Q2 population exactly the way the 15 s window confounded its bars.

The 45 000 ms long-timeout window — the blocked benefit side of the cutoff question — runs **before**
this window, not after it (R3, 2026-10-22: the benefit side is measured first, on the floor binary,
so no cutoff value is published until BOTH the Q1 verdict and a measured benefit side exist; R4 holds
this change's deploy behind the 45 s run). The sequence is floor → 45 s benefit window → AIMD window.

**Baseline consequence.** §1's baseline (window sum 4 of 16 in the episode, 18.47 % 429 rate,
47.5 % admission-rejected) was measured in the 25 000 ms timeout era. A 45 s timeout converts
25 s stalls into successes, so it changes the attempt population, the per-key concurrency
pressure, the 429 rate and the admission-reject share. The AIMD window's baseline MUST therefore
be re-derived at its own floor from that deploy's stats-floor.json; before/after is
within-window, never §1's 25 s-era numbers against 45 s-era measurements.

## 8. What this change does NOT do

- It does not reduce the upstream's aggregate request rate; only the provider's quota, more keys,
  or serving more client-side from the admission queue can do that.
- It does not make the per-key ceiling meaningful; with every window pinned at 1, the ceiling is
  not the binding constraint.
- It does not change the 429 penalty *rule* — one event still halves a key's window once, and that
  side is what protects the upstream. §2a removes only the racing path's duplicate application of
  the rule, so the count of applications matches the count of events.
- It does not make the two reject classes move together, and the window is built on the assumption
  that they do not.
