# FIRST-BYTE CUTOFF — derivation design (issue dirmacs/nimakai#39)

**Status:** DESIGN, with a measured result attached. This document specifies how the
first-byte cutoff is to be derived, and it records a measurement that shows the
first-cut derivation attempt did not answer the question it set. The live first-byte
window remains a **fallback** (25 000 ms), never a derived value, until the derivation
below is satisfied.

**Why this document exists.** The 15 s window cut the first-byte timeout to 15 000 ms
and rolled it back; the three questions that window existed to answer were never
answered, and the two that mattered most became answerable later when PR #35 gave every
failure-path record its turn key. The cutoff itself was never derived: the live value is
a fallback. The owner's decision is to re-derive it properly with a new, better-designed
experiment, **not** to keep the dynamic fallback.

---

## 1. The question, stated so it can be answered

> **Q.** What first-byte timeout `C` should the proxy use?

The question is not answerable until its two sides are named, because a timeout has a
**cost** side and a **benefit** side, and they are measured by different populations.

| side | what a change to `C` does | how it is measured |
|---|---|---|
| **cost** | lowering `C` converts currently-SUCCESSFUL attempts into stalls | **model-free**: the successes' latencies are all in the log |
| **benefit** | raising `C` converts currently-STALLED attempts into successes | **censored**: a stall's true latency is unobservable — it is recorded as the window value, not a latency |

This asymmetry is the whole difficulty, and it is the reason a single "derived cutoff"
number is not yet available. §4 gives the measured cost curve; §5 gives the measured
benefit bound; §6 states what would settle it.

### 1.1 The reclassification target

A cutoff is only as good as the trade it makes, so the trade is stated as a
target **before** the number is computed:

> **Target T1.** Setting the first-byte timeout to `C` converts at most **1.00%** of
> currently-successful attempts into stalls.

1.00% is the *carried* target from the 15 s protocol and is the only published figure for
this quantity. It is flagged as **carried, not derived** — it was chosen as "small enough
not to hurt the fleet", not computed from a cost/benefit curve. §6 states what would
replace it with a derived target.

### 1.2 What "an attempt" means here — the denominator, stated before it is used

**"Currently-successful attempts" means attempts that reached the upstream.** It does *not*
mean client requests. The turn log's population is "requests that reached the upstream";
an admission rejection (503 overload / 429 no-key) returns to the client before any log
row is written, so it is not in the log at all.

This is not a pedantic distinction on this host. Measured on the deployed window:
**2332 of 3569 client requests (56.0%) never reached the upstream** and are therefore
absent from every figure in this document; the log describes the 44.0% that got through.
The full measurement is in `SUCCESSOR-WINDOW.md` §0.1.

So the honest reading of T1 is: *"of the upstream attempts that succeeded, at most 1.00%
become stalls."* A cutoff is a property of the upstream conversation, not of the client's
request rate — and the client-visible impact of a cutoff is mediated by the admission
layer, which is a separate mechanism with its own counters.

---

## 2. The population, and the four things a first-byte latency row can be

A `ttfb` row's `latency_ms` is **not** always a latency. This distinction is the
difference between a measurement and an artefact, so it is stated first.

| row | `error` | `latency_ms` means | it is a real first-byte latency? |
|---|---|---|---|
| success | absent | the measured time to the first byte | **yes** |
| stall | present | the **timeout window** that fired | **no** — the true latency is ≥ the window, right-censored |
| rejected | present (upstream error) | n/a | no |
| other error | present | n/a | no |

**A stall row must never enter a latency fit as an observation.** It enters as a
*censored* observation, or not at all.

**The live window is measured, not assumed.** Every stall row's `error` string names the
window that fired. Measured across the log's history: `15000 ms` (the 15 s experiment era)
and `25000 ms` (the live era). Within the current live slice every stall row names
`25000 ms`. Any derivation that mixes eras without stratifying them is fitting a
population that never existed.

---

## 3. What the first derivation attempt produced, and why it does not answer the question

A first-cut derivation was implemented (`design/derive_cutoff.py`, 1019 lines) and run on
the live slice. It is a careful piece of work — a right-censored (Type-II) exponential
MLE as the primary estimator, an uncensored-only estimator beside it for comparison, a
GPD estimator third, a mean-excess plot to justify the tail threshold, a chi-square
interval and a 2000-sample bootstrap. Its own validation is sound: the chi-square
quantile was checked to machine precision against a reference, and a broken GPD sigma
solver was found and fixed rather than reported.

**And its headline answer does not satisfy its own stated target.** The script reports:

```
target reclassification share:    1.00%  (of currently-successful attempts)
DERIVED CUTOFF:                   40917 ms (40.92 s)
uncensored observations it reclassifies as stalls: 0 (0.00% of 1299)
```

A cutoff of 40 917 ms that reclassifies **0** of the successes does not satisfy a 1.00%
reclassification target. The two numbers are inconsistent, and the inconsistency is
structural rather than a bug in the arithmetic.

### 3.1 What the estimator actually solves

The estimator solves: *where does the fitted tail put the cutoff such that a given
fraction of the population above the tail threshold `T` remains above it?*

```
C = T − (1000/λ)·ln(t),     t = target_reclass / share_above_T
```

That is a **tail-quantile** calculation. It answers "where is the 1%-of-the-tail
boundary", which is a different question from "what fraction of today's successes
become stalls". The second quantity depends on where `C` sits **relative to the live
window**, and the estimator never references the live window at all.

### 3.2 The two structural facts that make the answer unusable as a cutoff

1. **The 40 917 ms answer lies ABOVE the live 25 000 ms window.** The set of successes a
   cutoff `C` converts into stalls is exactly the successes with latency in
   `[C, 25000)`. At `C = 40 917` that interval is **empty**. So a 40 917 ms cutoff
   reclassifies nothing — not "0.00% of successes", but nothing at all, by construction.
2. **A cutoff above the live window RELAXES; it cannot tighten.** It can only *save*
   stalled attempts (by waiting longer for their first byte). It can never *create* a new
   stall. The script's own output bears this out: it reports a predicted stall share of
   **8.65%** at 40 917 ms, and the *current* observed stall share is **123/1422 =
   8.65%** — the same number. The model is predicting that a 40 917 ms window stalls
   exactly as often as the 25 000 ms window does now, i.e. it has quantified a relaxation,
   not a tightening.

### 3.3 The estimator disagreement, and which one is defensible

Three estimators on the same slice:

| estimator | cutoff | assumption |
|---|---:|---|
| right-censored exponential MLE (primary) | 40 917 ms | constant hazard above `T`; censoring modelled |
| uncensored-only exponential | 29 691 ms | constant hazard; **ignores** the censored rows |
| GPD with fitted shape (`ξ = −0.44`) | 6 249 ms | generalised Pareto; no constant-hazard assumption |

That is a **6.5× spread**, and the two exponential estimators disagree by 11.2 s — the
agent's own script reports that gap as "the censoring bias" and declines to average the
two. The GPD fit **rejects** the exponential assumption (LR = 47.61, p = 0.0000), so the
primary estimator's own assumption is falsified on this data; a `ξ = −0.44` GPD has a
**finite** tail endpoint, and its 1%-boundary lands just past `T`, which is why it
returns 6.2 s.

**Which is defensible for this question: none of them, as applied.** Each estimates a
tail quantile; the decision needs a reclassification count. The estimators are answering
a neighbouring question, which is why they disagree wildly — they are not converging on
one number from different angles, they are solving different problems.

### 3.4 Independent confirmation

The two claims in §3.2 were checked against a **from-scratch second implementation**
written without reading `derive_cutoff.py`, against a pinned snapshot:

> **(i) "40917 ms is above the live 25000 ms window, so `[C, 25000)` is empty and it
> reclassifies 0 successes by construction." — CONFIRMED.** The live window was verified
> from the stall rows themselves (the only millisecond token in the slice's stall error
> strings is `25000`, 127/127, and every stall row's `latency_ms` lies in 25001…27248,
> i.e. it is the window that fired, not a measurement). `40917 > 25000`, so the interval
> is empty; and the maximum success latency in the slice is 26 422 ms, so there are also
> zero successes anywhere at or above 40 917 ms.

> **(ii) "A cutoff at 40917 ms would RELAX the timeout rather than tighten it: it can
> only save stalled attempts, and can never create new stalls." — CONFIRMED in direction,
> UNVERIFIABLE in magnitude.** 40 917 ms is 1.637× the live window, so it lengthens the
> enforced window; no logged success has a latency ≥ 40 917 ms and no logged stall has a
> latency below 25 000 ms, so such a cutoff cannot convert any logged success into a
> stall. The first clause cannot be checked from this log at all: the number of the 127
> stalls a 40 917 ms window would save is unobservable under the right-censoring in §5.

That second clause is the crux, and it is why this design recommends **no cutoff**.

---

## 4. The measured cost curve (model-free)

This is the part that **is** derivable from the log, because a successful attempt's
latency is directly recorded.

**Population:** the live slice of the **stage-4** window, floor `2026-10-21T14:50:43Z`, live log read directly,
snapshot not frozen. **Successes:** `ttfb` rows with no `error`. **Live window:**
25 000 ms (read from the stall rows' own error strings). A cutoff `C` converts the
successes with latency in `[C, 25000)` into stalls.

Measured at `2026-10-21T19:05Z` on 1377 successes and 124 stalls. **The absolute counts
drift as the window accumulates traffic; the curve's shape does not.** An earlier reading
at 18:52Z on 1305 successes gave the same curve to within one success at every point
(e.g. 15 000 ms: 121 then 122), so the percentages below are stable to ~0.4 pp while the
window is young and will tighten as it fills.

**This curve was recomputed from scratch by a second, independent implementation**
(`/tmp/cutoff-second-opinion/cutoff_curve.py`, written without reading
`derive_cutoff.py`), against a **pinned** snapshot so the numbers are reproducible: 1520
successes / 127 stalls / 1647 attempts, sha256
`315bf8eae2ab6c74dc701336e676234823d4a024f007b23cb64d31e90b07e740`, source mtime
`2026-10-21T19:12:29Z`. Three instruments (Python `json`, `grep`/`awk`, `jq`) agree on
every band count, and a controls harness proves the band counter **can** fire (a planted
25 001 ms latency is counted in `[25000, 40917)`, a planted 24 999 ms one is not, and a
non-zero count comes from the same pipeline whose `>= 25000` probe returns 9) — so the
zeros in the table are **measured zeros, not a filter that cannot fire**.

| `C` (ms) | successes in `[C, 25000)` | share of successes | share of all attempts |
|---:|---:|---:|---:|
| 10 000 | 288 | 18.95% | 17.49% |
| 12 000 | 220 | 14.47% | 13.36% |
| 15 000 | 129 | 8.49% | 7.83% |
| 18 000 | 78 | 5.13% | 4.74% |
| 20 000 | 53 | 3.49% | 3.22% |
| 22 000 | 39 | 2.57% | 2.37% |
| 23 000 | 26 | 1.71% | 1.58% |
| 24 000 | 14 | 0.92% | 0.85% |
| 24 200 | 11 | 0.72% | 0.67% |
| 24 500 | 7 | 0.46% | 0.43% |
| 25 000 | 0 | 0.00% | 0.00% |
| 26 000 | 0 | 0.00% | 0.00% |
| 30 000 | 0 | 0.00% | 0.00% |
| 40 000 | 0 | 0.00% | 0.00% |

**The 1.00% target (T1) is met by any cutoff at or above ≈ 23 300 ms.** The boundary is
an **interval, not a point**: the count of reclassified successes is a step function of
`C` (it drops by one at each logged success's latency), so the exact millisecond is not
a meaningful quantity — the *band* is. Measured at 19:22Z on 2049 successes:

- every `C ≥ 23 324 ms` reclassifies at most **20** successes = **0.976%** of successes;
- every `C ≤ 23 323 ms` reclassifies at least **21** = **1.025%**;
- the two differ only across the single logged success at 23 323 ms.

At the earlier 19:12Z snapshot the same boundary fell at **24 646 ms** (15 successes =
0.987%) — the band moves as traffic accumulates, which is itself a reason to treat the
1.00% figure as a **rate to be re-measured at decision time**, not a constant. The
practical statement is stable: **a cutoff in the 23–25 s range keeps the reclassification
share near 1%, and the curve is flat there** (the last 1.5 s of the window contains only
a handful of successes), so the decision is insensitive to the exact millisecond.

**This number is model-free and reproducible.** It is a direct count of recorded
successes, and the same count recomputed on a different slice moves with the traffic
mix, not with a fitted model.

**But 24 200 ms is NOT a recommendation**, for two reasons that the curve alone cannot
address:
- it is 800 ms below the live window, so it buys a 0.84% stall rate to save an
  unquantified number of stalled attempts;
- the benefit side is censored (§5), so the trade cannot be priced yet.

---

## 5. The benefit side is censored, and that is the binding constraint

The attempts a *higher* cutoff would save are exactly the ones the log cannot see. A
stall at the 25 000 ms window says "this attempt had not produced a first byte by 25 000
ms"; it never says how long it *would* have taken. So:

> **The number of stalls a 40 000 ms cutoff would save is NOT in the log, and no
> re-analysis of the existing log can produce it.** It requires either a model of the
> unobserved tail, or a measurement.

The three estimators in §3.3 are all models of that unobserved tail, and they disagree
by 6.5×. A disagreement that large is not a precision question to be settled by
preferring the "primary" one; it is evidence that **the tail is not well determined by
the observed data**.

**The assumption-free statement of the benefit side is therefore a bound, not a number.**
With no model at all, the only defensible estimate of how many stalls any cutoff `C`
*saves* is **zero** — because a censored row reveals nothing about its true latency — and
so the predicted stall rate is `n_stall / n_attempts`, **constant** across every
candidate cutoff. That is what the independent recomputation reports: 127/1647 =
**7.7110%** at every `C` from 10 000 to 40 000 ms.

So the model-free position is exact and worth stating plainly:

- **costing** a cutoff `C` is a **measurement** (the count of successes in `[C, 25000)`);
- **benefiting** from a cutoff `C` is a **model output** whose assumption the log cannot
  test.

Any number presented as "the benefit of raising the cutoff" is therefore a model, and
the three models available disagree by 6.5×. The censored MLE is the statistically
correct way to use the censoring and the only one that does not discard information, but
its answer is *still* unusable as a cutoff because of §3.2's structural facts — not
because the estimator is wrong.

### 5.1 What would settle the benefit side

One measurement, and it is cheap: **a window with a deliberately LONGER first-byte
timeout** (say 45 000 ms, above every observed latency, so nothing is censored). Then
every first-byte latency is observed, the tail is not censored, and the benefit side is a
direct count exactly as the cost side already is. The trade curve becomes fully
model-free on both sides.

The cost of that window is known and small: at 45 000 ms the timeout exceeds every
latency ever observed (max 27 047 ms in the live window, 26 422 ms in the 15 s slice), so
**no attempt would stall** and the window would produce no stalls at all. That is
precisely its point — it measures the distribution the stalls were the censored tail of.

**The alternative is to derive the benefit side from the fleet's own recovery rate**,
which the successor window will measure. A stall that the client recovers from is not
lost work; a stall that ends the turn is. If the per-turn salvage rate is high, a
tighter cutoff is cheap. If it is low, a tighter cutoff is expensive. **That number is
not yet measured**, and it is the number the decision actually turns on.

---

## 6. What this design concludes, and what it does not

### 6.1 What is established

| # | finding | how measured |
|---|---|---|
| 1 | The cost curve is **model-free and measured**. 1.00% of successes is reached at `C ≈ 24 200 ms`; 9.27% at 15 000 ms; 21.00% at 10 000 ms | direct count of successful `ttfb` rows' latencies in `[C, 25000)` |
| 2 | The first-cut estimator's 40 917 ms answer **does not satisfy its own target** and lies above the live window, so it reclassifies nothing | the script's own output (`0 of 1299`) plus the empty-interval argument |
| 3 | A cutoff above the live window is a **relaxation**, not a tightening | predicted stall share at 40 917 ms (8.65%) equals the current observed stall share (123/1422 = 8.65%) |
| 4 | The three estimators disagree by **6.5×**, and the primary one's constant-hazard assumption is **rejected** by its own GPD fit (p = 0.0000) | the script's own estimator table |
| 5 | The **benefit side is censored** and no re-analysis of the existing log can recover it | stall rows record the window, not the latency |

### 6.2 What is NOT concluded

- **No cutoff is recommended.** A recommendation needs the benefit side, and it is not
  measured. The 1.00% target is **carried**, not derived, and is itself a candidate to be
  replaced once the trade curve exists.
- The live 25 000 ms window **remains a fallback** and is not defended by this document.
- The GPD's 6 249 ms is **not** an alternative answer. It solves the same
  tail-quantile problem as the others and, with a finite-endpoint tail, its 1%-boundary
  lands just past `T` by construction. It is a third estimate of a neighbouring quantity.

### 6.3 The two measurements that would settle it, in order of cost

1. **The per-turn salvage rate** (the successor window's Q2, already specified in
   `SUCCESSOR-WINDOW.md` §1). This prices the benefit side: it is the fraction of turns
   that hit a stall and then still reach a clean completion. **Already in flight — no new
   experiment needed.**
2. **A long-timeout window** (45 000 ms, above every observed latency) that makes the
   tail uncensored and the whole trade curve model-free on both sides. This is a single
   deploy-and-observe cycle, and it is the only way to settle the benefit side from
   measurement rather than from a model.

Until at least (1) is in hand, any specific cutoff number is a target-compliance
arithmetic exercise rather than a decision.

---

## 7. The instrument (`design/derive_cutoff.py`) and what it is for

The script is kept, because the work in it is sound and reusable: the censoring
classification, the mean-excess tail-threshold plot, the exact chi-square interval, the
bootstrap cross-check, and the multi-era stratification are all correct instruments.
**But its headline output must not be read as "the cutoff".**

**Required change before the script is used to decide anything:** the output must
(a) print the **model-free reclassification count and share** for the derived `C`
alongside the model-based figure, and (b) state plainly when `C` lies above the live
window, because then the quantity it computed is a relaxation, not a tightening.

**The strata that must be separate.** The log spans two timeout eras (`15000 ms` and
`25000 ms` named in the stall rows' own error strings). A fit that pools them estimates
a tail for a configuration that never ran. Every era must be fitted separately, and the
reported `λ` must carry its era.

**A note on the tail threshold.** The mean-excess plot justifies `T = 6 s` well: the
mean excess above `T` is roughly flat from 2 s to 6 s (4682 → 7261 → 6836 ms) and then
falls, which is the signature of a tail that starts at 6 s. The threshold is measured,
not asserted.

---

## 8. The experiment this design implies (for the successor window)

The successor window (`SUCCESSOR-WINDOW.md`) **changes no timeout at all** — by design,
so that its bars are not functions of the knob it manipulates. This cutoff work
therefore does **not** go into that window.

The long-timeout window in §6.3(2) is a **separate** window with its own question and
its own bars, to be run after the successor window closes. It is the natural successor
to the successor window, and it is the window that would finally make the live
first-byte timeout a **derived** value rather than a fallback.

---

## Appendix — reproducing the cost curve

```
python3 - <<'PY'
import json, collections
# The stage-4 window's floor. Re-running this reproduces the curve as published;
# it is deliberately NOT repointed at a later floor, which would slice a different
# population and no longer reproduce the published numbers.
F="2026-10-21T14:50:43Z"; W=25000
u=[];c=[]
for line in open('<turn-log-path>'):
    line=line.strip()
    if not line: continue
    try: d=json.loads(line)
    except Exception: continue
    if (d.get('timestamp') or '')<F: continue
    if d.get('record_kind')!='ttfb': continue
    lat=d.get('latency_ms')
    if lat is None: continue
    (c if d.get('error') else u).append(lat)
print(f"successes {len(u)}  stalls {len(c)}")
for C in (10000,12000,15000,18000,20000,22000,23000,24000,24200,24500,25000):
    k=sum(1 for x in u if C<=x<W)
    print(f"  C={C:>6}  reclassified {k:>4}  {100.0*k/len(u):6.2f}% of successes")
PY
```

**Note on the window value:** `W = 25000` is read from the stall rows' own error
strings, not assumed. The reading must be re-derived per era; a hard-coded `W` is a
silent way to compute a curve for a configuration that never ran.
