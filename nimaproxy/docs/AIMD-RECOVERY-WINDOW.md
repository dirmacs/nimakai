# AIMD RECOVERY WINDOW — design (pre-registration input)

**Status:** DESIGN. Nothing here is a result; every number is either a baseline quoted from a
measured reading or a threshold with its derivation stated.

**What this window is for.** The proxy sheds a large share of client requests before they reach the
upstream. Two independent failure modes produce that, and a per-key AIMD rule can only reach one
of them. This document says which is which, because a window that cannot tell them apart would
report a flat headline while the failure mode flipped underneath it.

## 1. The measured state this window starts from

Read live from the proxy's own counters and `/stats`, during the 24 h window that closed on
2026-10-22:

| quantity | value | source |
|---|---|---|
| per-key AIMD window | **1 of a configured 4**, on every key | `/stats` `max_in_flight` vs `configured_max_in_flight` |
| effective window sum | **4 of 16** | `/stats` |
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

## 2. The upstream limit is ACCOUNT-level, not per key

The per-key 429 histogram, read from the proxy's own stderr:

```
    256 key 2 rate-limited      247 key 3 rate-limited
    246 key 0 rate-limited      240 key 1 rate-limited
```

**Even within 6 % across all four keys.** If the upstream limited per key, one key would be
throttled and the others would not. Even distribution across keys that share one quota is the
signature of an **account-level** limit.

**Therefore a per-key AIMD window cannot reduce the 429 rate.** Shrinking one key's window only
hands its load to another key on the same account, which is then throttled too. This is why the
window sum is not the whole story, and why the bar below is not "the 429 rate must fall".

## 3. The bench is the dominant term, and it is self-imposed

Every one of the **989** bench events carried **60 s** — the proxy's own **default** (the upstream's
`Retry-After` header, falling back to 60 s when absent). No other value ever appeared.

So the 60-second bench is **the proxy's choice, not upstream guidance**, and it is a large term:
roughly one 429 every 20 s, each benching a key for 60 s. That is consistent with the measured
0–2 of 4 keys active, and with a benched key being the reason a request is refused.

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

**This is the lever that reaches the 429 half**, because the limit is account-level and the bench
is the proxy's own term. Its risk is real and stated: a key released sooner is handed more requests
while the account is still over its limit, which can earn more 429s. The measurement below is
built to see that.

**Change B — the recovery rule** (`aimd_pressure_adaptive`). The credit the key has already
banked is DECAYED on a 429 rather than zeroed, so it survives the bench; and the cost of the
next slot falls as that key's 429 pressure rises, via the pressure-derived bar. Credit still
comes from successes — a benched key earns none — so it is the *residue*, not new credit, that
breaks the interlock. `false` (the default) is today's behaviour exactly.

**This lever reaches the 503 half** — faster recovery frees permits, so `overload_rejects` should
fall. It is explicitly **not** claimed to reduce the upstream 429 rate, which section 2 shows it
cannot.

**Neither change touches the 429 penalty side.** The multiplicative halving is the safety
mechanism that protects the upstream's per-key limit, and it stays exactly as it is.

## 5. The two halves move in OPPOSITE directions

Faster recovery frees permits → the **503 half** (`overload_rejects`) should fall. But more
in-flight per key, sooner → the **upstream-429 half** can rise. **A single "share of client
requests rejected" can therefore stay flat while the failure mode flips from 503 to 429.**

So the window must report three deltas separately:

- `request_total`
- `overload_rejects`
- `rate_limit_count`

and, because admission-rejected requests produce **no turn-log rows**, the counter side is the
only instrument that can see this change at all. There is no per-key breakdown available to
attribute it.

## 6. The instruments, and the bar

Reported per key and over time:

- the **per-key window sum** — the quantity that must move (4 of 16 today);
- the **per-key benched fraction of time** — the term Change A acts on;
- the three counter deltas of section 5;
- the per-key 429 histogram of section 2, to check whether the even distribution holds.

**The bar must distinguish "the lever moved but capacity did not" from "nothing moved."** If the
bench is the binding constraint, a working recovery change can leave effective capacity flat. So:

| outcome | reading | action |
|---|---|---|
| benched fraction falls **and** effective capacity rises **and** the 503 half falls | both levers reached their targets | the change works; keep it on |
| benched fraction falls **but** capacity is flat **and** the 429 half rises | the bench was released faster than the account could absorb | the bench cap is too aggressive; tune it, and keep the recovery change |
| neither the benched fraction nor the window sum moves | the levers did not act | the change did not work; do not ship it |
| the 429 histogram stops being even | the limit is no longer account-wide | re-derive before acting — the whole design rests on that being true |

**Population:** the whole window. **Minimum:** the same minimum the successor design applies, so a
thin population is reported as unmeasured rather than scored.

## 7. Sequencing — this is floor 2 of 2

The `response_body_id` change is **observationally neutral**: it adds a log column and changes no
behaviour. It deploys at the **next** window's floor, so that window measures the **old** AIMD
behaviour and the Q1/Q2 pre-registration stays valid.

**This change deploys at a later floor, of its own, and needs its own window.** Merging it before
the first floor is taken would put it inside the window that was supposed to measure the old
behaviour, and confound the Q1/Q2 population exactly the way the 15 s window confounded its bars.

The 45 000 ms long-timeout window — the blocked benefit side of the cutoff question — is scheduled
**after** this window, not before it.

## 8. What this change does NOT do

- It does not reduce the upstream's aggregate request rate; only the provider's quota, more keys,
  or serving more client-side from the admission queue can do that.
- It does not make the per-key ceiling meaningful; with every window pinned at 1, the ceiling is
  not the binding constraint.
- It does not change the 429 penalty side, and it must not: that side is what protects the upstream.
- It does not make the two reject classes move together, and the window is built on the assumption
  that they do not.
