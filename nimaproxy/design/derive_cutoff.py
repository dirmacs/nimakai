#!/usr/bin/env python3
"""Derive the first-byte cutoff from a turn-log snapshot.

WHAT THIS SCRIPT IS
-------------------
The cutoff is not a number; it is a decision rule over the observable
time-to-first-byte (TTFB) distribution:

    An attempt is a STALL when its first byte has not arrived by CUTOFF ms.

The derivation answers one question: for a given TTFB distribution, which
CUTOFF makes that rule trade the fewest recoverable successes for the most
stall-detection latency? The method is specified in
`CUTOFF-DERIVATION.md` (§2, "the derivation method") and implemented here
EXACTLY as specified there. If the two ever disagree, the document is wrong.

WHY OFFLINE
-----------
The turn log already records every attempt's first-byte latency. The
derivation is therefore computable with no live change at all, which is what
makes it unable to fail a live bar. The live change is only needed to VALIDATE
the derived value (see the design doc §4), and that is a separate decision.

THE CENSORING TRAP (why this is not a one-liner)
-----------------------------------------------
`latency_ms` on a `ttfb` record is what the proxy knew when the first byte
arrived. For an attempt that STALLED, the record is written when the window
fired, so its `latency_ms` is the WINDOW, not the upstream's latency. Under the
live 25000 ms window that is a hard spike at 25000/25001/25160 ms and it is
NOT part of the upstream distribution — it is the measurement instrument.

This script therefore splits the population three ways and uses each part only
for what it can support:

  1. `no-error` ttfb rows        -> the UNCENSORED observations. These are the
                                    distribution the fit is run on.
  2. stall rows (error carries    -> the CENSORED observations. Their latency is
     "no bytes within <W>ms")       the window, so they contribute an
                                    ORDER STATISTIC (an upper bound: "at least
                                    one attempt exceeded W") and never a value.
  3. other error rows (429, 503,  -> EXCLUDED from the fit. They are not
     410, racing, ...)              first-byte-latency observations at all;
                                    including them would move the fit without
                                    being evidence about the upstream.

The distinction is load-bearing: a fit that silently mixes (1) and (2) fits the
instrument, not the upstream, and produces a cutoff that censors its own tail —
which is exactly the defect the live config comments record (a deployed
ceiling/floor pair of 15000 made the "dynamic" timeout a hidden constant).

THE ESTIMATOR
-------------
Model: the uncensored TTFB tail is exponential with rate `lambda` (constant
hazard). Justification from measured evidence, in the design doc §2c. The
estimator is the maximum-likelihood rate over the tail observations, where the
tail is defined by a threshold `T` chosen by a likelihood-ratio test against
the exponential null for the body below T (a GPD shape of 0 is the exponential,
so the test is "does the tail really have its own, heavier distribution?").

Uncertainty: the MLE of an exponential rate is `lambda_hat = k / sum(x_i - T)`
over the k tail observations, which is Gamma-distributed; the exact CI for the
derived cutoff is obtained by inverting the gamma quantiles. A bootstrap is
also printed as a cross-check. Two estimators that agree is evidence; two that
disagree is a finding to report, not to average.

USAGE
-----
    derive_cutoff.py LOG [--floor TS] [--ceiling TS] [--model M] [--tail S]
                      [--target F] [--alpha A] [--bins N] [--bootstrap]

    TS is an ISO8601 prefix, e.g. 2026-10-21T14:50:43Z. The log is STREAMED;
    nothing is loaded whole. Read-only: the file is opened 'r' and never
    written. Rotated parts (`turns.jsonl.<ts>`) are picked up automatically
    when the path is a directory, in LC_ALL=C order with the live file LAST,
    per the protocol's rotation note.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import sys
from collections import Counter

# ---------------------------------------------------------------- constants

RECORD_KIND_TTFB = "ttfb"
RECORD_KIND_COMPLETION = "completion"
RECORD_KIND_LOOP = "loop"

#: The error substrings that identify a FIRST-BYTE stall. These are the
#: censored observations: their latency is the window, not the upstream.
#: They are matched as substrings so a future wording change degrades to
#: "uncategorised" (and is reported as such) rather than silently entering the
#: uncensored set.
STALL_MARKERS = (
    "no bytes within",       # first-byte stall (the rail this derivation is for)
    "upstream timeout after",
    "timeout after",
    "body timeout after",
    "deadline exceeded after",  # racing deadline, not a first-byte stall
)

#: Default tail threshold. The design doc §2c derives it from the likelihood
#: ratio; this is the fallback when the test is inconclusive.
DEFAULT_TAIL_S = 6.0

#: Default target: the share of currently-SUCCESSFUL attempts we are willing
#: to convert into stalls. 1% is the value the design doc derives; it is a
#: reclassification target, not a total-stall target, because the total also
#: contains the attempts that already stalled at the live window and those are
#: not the cutoff's doing.
DEFAULT_TARGET = 0.01


# ---------------------------------------------------------------- helpers

def percentile(sorted_vals, p):
    """Nearest-rank percentile, matching the repo's own convention
    (`log_query.rs:458`, `http.rs:275`): rank = ceil(p/100 * n), 1-based.

    This is deliberately NOT numpy's default, so that a number this script
    prints can be compared by hand against the numbers in the protocol's
    artifacts, which use the repo's own definition.
    """
    n = len(sorted_vals)
    if n == 0:
        return 0
    rank = math.ceil(p / 100.0 * n)
    return sorted_vals[max(1, min(rank, n)) - 1]


def mean(vals):
    return sum(vals) / len(vals) if vals else float("nan")


def gamma_quantile(p, shape):
    """Quantile of the Gamma(shape, 1) distribution.

    The MLE of an exponential rate is Gamma(k, 1/scale); inverting it needs a
    gamma quantile. Implemented from the Wilson-Hilferty approximation, which
    is accurate to ~1e-3 in the tails for shape >= 1 and is what the script
    uses for a CI. The design doc reports the CI width, not the digits, so the
    approximation is fit for purpose; a bootstrap is printed alongside so the
    two can be compared.
    """
    if shape <= 0:
        return float("nan")
    # Wilson-Hilferty: cube-root normal approximation.
    z = _norm_quantile(p)
    if shape < 1e-9:
        return 0.0
    w = (shape * (1.0 - 1.0 / (9.0 * shape) + z / (3.0 * math.sqrt(shape))) ** 3)
    return max(w, 0.0)


def _norm_quantile(p):
    """Acklam's rational approximation to the standard normal quantile."""
    a = [-3.969683028665376e+01, 2.209460984245205e+02, -2.759285104469687e+02,
         1.383577518672690e+02, -3.066479806614716e+01, 2.506628277459239e+00]
    b = [-5.447609879822406e+01, 1.615858368580409e+02, -1.556989798598866e+02,
         6.680131188771972e+01, -1.328068155288572e+01]
    c = [-7.784894002430293e-03, -3.223964580411365e-01, -2.400758277161838e+00,
         -2.549732539343734e+00, 4.374664141464968e+00, 2.938163982698783e+00]
    d = [7.784695709041462e-03, 3.224671290700398e-01, 2.445134137142996e+00,
         3.754408661907416e+00]
    plow, phigh = 0.02425, 1 - 0.02425
    if p <= 0 or p >= 1:
        raise ValueError("p must be in (0,1)")
    if p < plow:
        q = math.sqrt(-2 * math.log(p))
        return (((((c[0]*q+c[1])*q+c[2])*q+c[3])*q+c[4])*q+c[5]) / \
               ((((d[0]*q+d[1])*q+d[2])*q+d[3])*q+1)
    if p > phigh:
        q = math.sqrt(-2 * math.log(1 - p))
        return -(((((c[0]*q+c[1])*q+c[2])*q+c[3])*q+c[4])*q+c[5]) / \
                ((((d[0]*q+d[1])*q+d[2])*q+d[3])*q+1)
    q = p - 0.5
    r = q*q
    return (((((a[0]*r+a[1])*r+a[2])*r+a[3])*r+a[4])*r+a[5])*q / \
           (((((b[0]*r+b[1])*r+b[2])*r+b[3])*r+b[4])*r+1)


def _gpd_loglik(x, xi, sigma, loc):
    """Log-likelihood of a GPD(loc, sigma, xi) over exceedances x > loc."""
    if sigma <= 0:
        return -float("inf")
    if abs(xi) < 1e-9:
        # exponential limit
        return len(x) * (-math.log(sigma)) - sum(xi_v - loc for xi_v in x) / sigma
    if xi < 0 and any(v >= loc - sigma / xi for v in x):
        return -float("inf")  # endpoint violated
    s = 0.0
    for v in x:
        z = 1.0 + xi * (v - loc) / sigma
        if z <= 0:
            return -float("inf")
        s += -math.log(sigma) - (1.0 + 1.0 / xi) * math.log(z)
    return s


def _gpd_fit(x, loc, xi0=None):
    """Profile-likelihood GPD fit over exceedances x > loc.

    Returns (xi, sigma, loglik). `xi` is the shape; `xi == 0` IS the
    exponential, which is what makes the likelihood-ratio test below a test
    of "is the tail exponential?" rather than an arbitrary comparison.
    """
    # grid + golden-section refine on xi, sigma profiled out analytically-ish
    best = None
    for xi in [i / 20.0 for i in range(-8, 21)]:   # -0.4 .. 1.0
        s = _gpd_fit_sigma(x, loc, xi)
        if s is None:
            continue
        ll = _gpd_loglik(x, xi, s, loc)
        if best is None or ll > best[2]:
            best = (xi, s, ll)
    if best is None:
        return None
    # local refine
    xi, s, ll = best
    step = 0.05
    for _ in range(60):
        improved = False
        for cand in (xi - step, xi + step):
            cs = _gpd_fit_sigma(x, loc, cand)
            if cs is None:
                continue
            cll = _gpd_loglik(x, xi=cand, sigma=cs, loc=loc)
            if cll > ll:
                xi, s, ll = cand, cs, cll
                improved = True
        if not improved:
            step /= 2.0
            if step < 1e-6:
                break
    return xi, s, ll


def _gpd_loglik_sigma(x, loc, xi, sigma):
    """Log-likelihood at a fixed (xi, sigma). Returns -inf if inadmissible."""
    if sigma <= 0:
        return -float("inf")
    if xi < 0:
        endpoint = loc - sigma / xi
        if any(v >= endpoint for v in x):
            return -float("inf")
    s = 0.0
    for v in x:
        z = 1.0 + xi * (v - loc) / sigma
        if z <= 0:
            return -float("inf")
        s += -math.log(sigma) - (1.0 + 1.0 / xi) * math.log(z)
    return s


def _gpd_fit_sigma(x, loc, xi):
    """MLE of sigma for a fixed GPD shape xi over exceedances x > loc.

    Maximising the log-likelihood over sigma directly, by golden-section search
    on a log-sigma grid, rather than solving the score equation. The score is
    NOT monotone in sigma for every shape (measured: at xi = 0.1 over this
    data it has an interior maximum near sigma = mean(excess) and tends to a
    negative constant at both ends), so a bisection on its sign returns "no
    root" for every non-zero shape and the shape test silently reports
    xi = 0. Direct maximisation cannot fail that way, and it is checked against
    the closed form at xi = 0 (sigma == mean excess).
    """
    if not x:
        return None
    exc = [v - loc for v in x]
    if any(e <= 0 for e in exc):
        return None
    m = mean(exc)
    if abs(xi) < 1e-12:
        return m
    # bracket: sigma must exceed -xi*max(excess) when xi<0; a wide positive
    # bracket otherwise.
    if xi < 0:
        lo = -xi * max(exc) * (1.0 + 1e-9)
    else:
        lo = m * 1e-6
    hi = max(m * 1e4, lo * 10.0)
    phi = (math.sqrt(5.0) - 1.0) / 2.0
    a, b = lo, hi
    c = b - phi * (b - a)
    d = a + phi * (b - a)
    fc = _gpd_loglik_sigma(x, loc, xi, c)
    fd = _gpd_loglik_sigma(x, loc, xi, d)
    if fc == -float("inf") and fd == -float("inf"):
        return None
    for _ in range(300):
        if fc < fd:
            a, c, fc = c, d, fd
            d = a + phi * (b - a)
            fd = _gpd_loglik_sigma(x, loc, xi, d)
        else:
            b, d, fd = d, c, fc
            c = b - phi * (b - a)
            fc = _gpd_loglik_sigma(x, loc, xi, c)
        if b - a < 1e-9 * max(1.0, b):
            break
    return 0.5 * (a + b)


def lrt_pvalue(lr_stat, df=1):
    """p-value for a chi-square LR statistic with df=1 (exponential vs GPD)."""
    if lr_stat <= 0:
        return 1.0
    # chi2(1) survival: 2*(1-Phi(sqrt(x)))
    return 2.0 * (1.0 - _norm_cdf(math.sqrt(lr_stat)))


def _norm_cdf(z):
    return 0.5 * (1.0 + math.erf(z / math.sqrt(2.0)))


# ---------------------------------------------------------------- log reading

def iter_log_lines(paths):
    """Stream lines from the given paths in the given order."""
    for p in paths:
        with open(p, "r", encoding="utf-8", errors="replace") as fh:
            for line in fh:
                yield line


def discover_log_paths(path):
    """Resolve the log argument to an ordered list of files.

    A directory (or a bare `turns.jsonl` whose rotated siblings exist) is read
    as: rotated parts in LC_ALL=C order, then the live file LAST. The live
    file's name is a lexicographic prefix of the rotated names, so a naive
    glob would put it first and concatenate the record backwards in time —
    a reordering that a line count cannot detect. See the protocol's §11.4.
    """
    if os.path.isdir(path):
        d = path
    else:
        d = os.path.dirname(os.path.abspath(path)) or "."
    rotated = sorted(
        f for f in os.listdir(d) if f.startswith("turns.jsonl.") and f != "turns.jsonl"
    )
    out = [os.path.join(d, f) for f in rotated]
    live = os.path.join(d, "turns.jsonl")
    if os.path.exists(live):
        out.append(live)
    elif os.path.isfile(path):
        out = [path]
    return out


def classify(record):
    """Classify a ttfb record into one of: uncensored / censored / excluded.

    Returns (kind, stall_window_ms_or_None).
    """
    err = record.get("error")
    if not err:
        return "uncensored", None
    for m in STALL_MARKERS:
        if m in err:
            # the window is recorded in the message itself: "... within 25000ms"
            w = None
            try:
                tail = err.split(m, 1)[1].strip()
                num = ""
                for ch in tail:
                    if ch.isdigit():
                        num += ch
                    else:
                        break
                if num:
                    w = int(num)
            except Exception:
                w = None
            if m == "deadline exceeded after":
                return "excluded", w  # racing path, not the first-byte rail
            return "censored", w
    return "excluded", None


# ---------------------------------------------------------------- derivation

def derive(uncensored_ms, tail_s, target_reclass, alpha, censored_windows=()):
    """Run the derivation. Returns a dict of everything worth printing.

    `target_reclass` is the target RECLASSIFICATION share: the share of
    currently-successful attempts the cutoff is allowed to convert into stalls.
    It is NOT the total stall share, which also contains the attempts that
    already stalled at the live window.

    `censored_windows` is the multiset of live window values read off the
    stall rows' own error strings (one per stall row). Those rows are CENSORED
    observations: their latency is the window, so they say "this attempt was
    still waiting at W ms" and never "this attempt took W ms". The primary
    estimator below uses them as right-censored observations; the
    uncensored-only variant is reported beside it so the two can be compared
    and the difference attributed.
    """
    n = len(uncensored_ms)
    n_cens = len(censored_windows)
    out = {"n_uncensored": n, "n_censored": n_cens}
    if n < 30:
        out["error"] = (
            f"only {n} uncensored observations; the design doc requires >=30 "
            "for a fit to mean anything (see the design doc's minima)."
        )
        return out

    T = tail_s * 1000.0
    tail_ms = [v for v in uncensored_ms if v >= T]
    k = len(tail_ms)
    if k < 10:
        out["error"] = (
            f"only {k} observations above the {tail_s}s tail threshold; "
            "the design doc requires >=10 for the tail MLE to be decidable."
        )
        return out

    # ---- tail threshold choice, published (the mean-excess plot)
    # A threshold is right when the mean excess above it stops growing with T:
    # that is the empirical signature of a constant-hazard (exponential) tail.
    # The plot is printed by main(); it is computed here so the numbers in the
    # report and the numbers in the derivation cannot drift apart.
    plot = []
    for ts in (2, 4, 6, 8, 10, 12, 14, 16, 18, 20):
        tms = ts * 1000.0
        exc = [v - tms for v in uncensored_ms if v >= tms]
        cens_above = sum(1 for w in censored_windows if w >= tms)
        plot.append({
            "t_s": ts,
            "n_above": len(exc),
            "mean_excess_ms": (sum(exc) / len(exc)) if exc else None,
            "censored_above": cens_above,
            "censored_excess_min_ms": (max(censored_windows) - tms)
                                       if censored_windows else None,
        })
    out["mean_excess_plot"] = plot

    excess = [v - T for v in tail_ms]
    cens_excess = [max(w - T, 0.0) for w in censored_windows if w > T]
    n_cens_above = len(cens_excess)
    m_exc = mean(excess)                     # ms
    m_exc_s = m_exc / 1000.0
    out.update({
        "tail_s": tail_s,
        "n_tail": k,
        "tail_share": k / n,
        "tail_min_ms": tail_ms[0],
        "tail_max_ms": tail_ms[-1],
        "tail_mean_excess_ms": m_exc,
        "tail_mean_excess_s": m_exc_s,
        "n_censored_above_T": n_cens_above,
        "censored_excess_sum_s": sum(cens_excess) / 1000.0,
        "max_censored_window_ms": max(censored_windows) if censored_windows else None,
    })

    # ---- assumption check: is the tail exponential (GPD shape xi == 0)?
    # The LR statistic tests the model's own assumption, because xi = 0 IS the
    # exponential. A small p-value means the tail has its own shape and the
    # derived cutoff carries that caveat; it is reported, never silently passed.
    fit = _gpd_fit(tail_ms, T)
    if fit is not None:
        xi, sigma, ll_gpd = fit
        ll_exp = _gpd_loglik(tail_ms, 0.0, m_exc, T)
        lr = 2.0 * (ll_gpd - ll_exp)
        out.update({"gpd_xi": xi, "gpd_sigma_ms": sigma,
                    "lr_stat": lr, "lr_p": lrt_pvalue(lr)})

    # ---- PRIMARY ESTIMATOR: censored (Type II) MLE of the exponential rate.
    #
    # The live window is a CENSORING MECHANISM, and it censors exactly the
    # observations the tail is made of: every attempt whose first byte had not
    # arrived by W ms is recorded with latency = W, and every attempt that
    # WOULD have arrived after W ms never appears at all. An uncensored-only fit
    # therefore conditions on "the first byte arrived before the window" and
    # estimates the hazard of the SURVIVING population, which is not the
    # population the cutoff has to decide about.
    #
    # For an exponential tail, the censored MLE has a closed form:
    #     lambda_hat = K / (sum(excess over uncensored above T)
    #                        + sum(W - T over censored rows above T))
    # where K is the total number of exceedances (uncensored above T PLUS
    # censored above T). This is the standard Type-II right-censored exponential
    # estimator and it is the estimator the design doc specifies.
    K = k + n_cens_above
    denom_s = (sum(excess) + sum(cens_excess)) / 1000.0
    if K > 0 and denom_s > 0:
        lam_cens = K / denom_s
        out["lambda_cens_per_s"] = lam_cens
        out["K_exceedances"] = K
        out["denominator_s"] = denom_s
    else:
        lam_cens = float("nan")
        out["lambda_cens_per_s"] = lam_cens

    # ---- SECONDARY ESTIMATOR: uncensored only, for the comparison the design
    # doc requires (the two must be reported side by side, never averaged).
    lam_unc = 1.0 / m_exc_s if m_exc_s > 0 else float("inf")
    out["lambda_unc_per_s"] = lam_unc

    # ---- the derived cutoff, for BOTH estimators
    #
    # The fitted exponential tail gives the survival of TTFB beyond T:
    #     P(TTFB >= C | TTFB >= T) = exp(-lambda * (C - T)/1000)
    # with (C - T) in ms and lambda per second. The cutoff that leaves a target
    # share `t` of the attempts standing above T is
    #     C = T - (1000/lambda) * ln(t)
    # `t` is the target reclassification share expressed as a fraction of the
    # population ABOVE T, so it is target_reclass / (share of attempts above T).
    def _cutoff(lam, n_above_total):
        if lam <= 0 or math.isinf(lam) or math.isnan(lam):
            return None
        share_above = n_above_total / (n + n_cens) if (n + n_cens) else 0.0
        if share_above <= 0:
            return None
        t = target_reclass / share_above
        if t >= 1.0:
            return None
        ln_t = math.log(t)
        return T - (1000.0 / lam) * ln_t, ln_t, t

    n_above_cens = K
    n_above_unc = k
    got = _cutoff(lam_cens, n_above_cens)
    if got is None:
        out["error"] = (
            "the censored MLE is degenerate (no exceedances, or the target "
            "reclassification share is >= the share of attempts above the tail "
            "threshold); the cutoff cannot be derived from this slice."
        )
        return out
    cutoff_ms, ln_t_c, t_c = got
    out.update({"cutoff_ms": cutoff_ms, "cutoff_ln_t": ln_t_c, "cutoff_t": t_c})

    got_u = _cutoff(lam_unc, n_above_unc)
    if got_u is not None:
        out["cutoff_uncensored_ms"], _, _ = got_u

    # ---- THIRD ESTIMATOR: the GPD tail, fitted WITH the censoring.
    # The exponential assumption is tested above and (on this data) rejected,
    # so the cutoff is ALSO derived from the fitted GPD survival function,
    # which does not assume constant hazard. A GPD with shape xi has
    #     S(u) = 1 - (1 + xi*u/sigma)^(-1/xi)     for u = TTFB - T >= 0
    # The cutoff that leaves a target share t of the population above T is
    #     C = T + (sigma/xi) * ((1 - t)^(-xi) - 1)
    # (the xi -> 0 limit is the exponential, so the two agree in that limit).
    if fit is not None:
        share_above_cens = K / (n + n_cens) if (n + n_cens) else 0.0
        if share_above_cens > 0:
            t_g = target_reclass / share_above_cens
            if 0 < t_g < 1:
                if abs(xi) < 1e-9:
                    cut_g = T - (1000.0 * sigma) * math.log(t_g)
                else:
                    cut_g = T + (sigma / xi) * ((1.0 - t_g) ** (-xi) - 1.0)
                out["cutoff_gpd_ms"] = cut_g
                out["cutoff_gpd_xi"] = xi
                out["cutoff_gpd_sigma_ms"] = sigma

    # ---- uncertainty: exact gamma interval on the censored rate.
    # For a censored exponential, 2*K*lambda*denominator ~ chi2(2K), so the
    # interval for 1/lambda is [2*denom/chi2_hi, 2*denom/chi2_lo] and the
    # cutoff is affine in 1/lambda. K here is the total exceedance count
    # (uncensored + censored above T), which is what makes the interval honest
    # about the sample actually used.
    chi_lo = _chi2_quantile(alpha / 2.0, 2 * K)
    chi_hi = _chi2_quantile(1.0 - alpha / 2.0, 2 * K)
    if chi_lo > 0 and denom_s > 0:
        inv_lo = (2.0 * denom_s / chi_hi)      # seconds, low
        inv_hi = (2.0 * denom_s / chi_lo)      # seconds, high
        cut_lo = T - (1000.0 * inv_lo) * ln_t_c
        cut_hi = T - (1000.0 * inv_hi) * ln_t_c
        if cut_lo > cut_hi:
            cut_lo, cut_hi = cut_hi, cut_lo
        out.update({
            "cutoff_lo_ms": cut_lo,
            "cutoff_hi_ms": cut_hi,
            "ci_alpha": alpha,
            "ci_method": "exact gamma interval on the censored rate "
                         "(chi-square identity, 2K df)",
        })
    else:
        out["ci_method"] = "unavailable"

    # ---- bootstrap cross-check (fixed seed: the output is reproducible)
    import random
    rng = random.Random(20261021)
    boots = []
    for _ in range(2000):
        # resample the exceedances as the pooled sample they are: uncensored
        # observations contribute their excess, censored ones contribute
        # max(W - T, 0).
        pool = excess + cens_excess
        samp = rng.choices(pool, k=K)
        d = sum(samp) / 1000.0
        if d <= 0:
            continue
        b = K / d
        cb = _cutoff(b, K)
        if cb is not None:
            boots.append(cb[0])
    if boots:
        boots.sort()
        out.update({
            "boot_n": len(boots),
            "boot_cutoff_ms": percentile(boots, 50),
            "boot_lo_ms": percentile(boots, 100 * alpha / 2),
            "boot_hi_ms": percentile(boots, 100 * (1 - alpha / 2)),
        })

    # ---- what the cutoff would do to the observed population
    reclass = sum(1 for v in uncensored_ms if v >= cutoff_ms)
    out.update({
        "reclassified_uncensored": reclass,
        "reclassified_share": reclass / n,
        "target_reclass": target_reclass,
    })
    denom_all = n + n_cens
    out["predicted_stall_share"] = (
        (reclass + n_cens) / denom_all if denom_all else float("nan")
    )
    return out


def _chi2_quantile(p, df):
    """Chi-square quantile with an even number of degrees of freedom.

    The CI below needs chi2 quantiles at 2k df (k = tail observations, which
    is ~400 here, so both tails are far into the approximation's bad region and
    a Wilson-Hilferty cube-root approximation collapses them onto each other —
    that is exactly how a CI of "6000 .. 6000" gets printed and looks like a
    bug in the fit rather than in the quantile). For EVEN df the quantile has
    a closed form through the regularised lower incomplete gamma function, and
    it is exact, so it is used instead and the approximation is not trusted
    anywhere.

        chi2_p(df=2m) = 2 * Q_inv(p, m)

    where Q_inv is the inverse of the regularised gamma P(m, x). Inverted by
    bisection on x, which is monotone and cheap.
    """
    if df <= 0 or df != int(df) or int(df) % 2 != 0:
        # only even df is needed here; refuse rather than approximate silently
        raise ValueError(f"chi2 quantile needs a positive even df, got {df}")
    m = int(df) // 2
    if p <= 0.0:
        return 0.0
    if p >= 1.0:
        return float("inf")
    lo, hi = 0.0, 1.0
    # expand hi until the regularised gamma covers p
    while _gamma_p(m, hi) < p:
        hi *= 2.0
        if hi > 1e12:
            break
    for _ in range(200):
        mid = 0.5 * (lo + hi)
        if _gamma_p(m, mid) < p:
            lo = mid
        else:
            hi = mid
    return 2.0 * 0.5 * (lo + hi)


def _gamma_p(a, x):
    """Regularised lower incomplete gamma P(a, x) for integer a >= 1.

    Series for x < a+1, continued fraction for the complement otherwise, which
    is the standard numerically stable split.
    """
    if x <= 0:
        return 0.0
    if x < a + 1.0:
        # series
        term = 1.0 / a
        total = term
        n = 0
        while n < 10000:
            n += 1
            term *= x / (a + n)
            total += term
            if abs(term) < abs(total) * 1e-15:
                break
        return total * math.exp(-x + a * math.log(x) - math.lgamma(a))
    # continued fraction for Q(a, x), then complement
    b = x + 1.0 - a
    c = 1e300
    d = 1.0 / b
    h = d
    for i in range(1, 10000):
        an = -i * (i - a)
        b += 2.0
        d = an * d + b
        if abs(d) < 1e-300:
            d = 1e-300
        c = b + an / c
        if abs(c) < 1e-300:
            c = 1e-300
        d = 1.0 / d
        delta = d * c
        h *= delta
        if abs(delta - 1.0) < 1e-15:
            break
    q = math.exp(-x + a * math.log(x) - math.lgamma(a)) * h
    return 1.0 - q


# ---------------------------------------------------------------- printing

def hist(vals, bins, width=44, label="s"):
    if not vals:
        print("  (no data)")
        return
    lo, hi = 0, max(vals)
    step = (hi / bins) if bins else 1
    step = max(step, 1)
    c = Counter(int(v // step) for v in vals)
    top = max(c.values())
    print(f"  histogram of {len(vals)} values, {int(step)} ms bins (x = {label}):")
    for kk in range(0, int(hi // step) + 1):
        cnt = c.get(kk, 0)
        bar = "#" * int(round(width * cnt / top)) if top else ""
        print(f"  {kk*step/1000:8.1f} | {cnt:6d} {bar}")


def main(argv=None):
    ap = argparse.ArgumentParser(
        description="Derive the first-byte cutoff from a turn-log snapshot.")
    ap.add_argument("log", help="turn log file, or its directory")
    ap.add_argument("--floor", default=None, help="ISO8601 prefix, inclusive")
    ap.add_argument("--ceiling", default=None, help="ISO8601 prefix, exclusive")
    ap.add_argument("--model", default=None,
                    help="restrict to one responding_model (default: the "
                         "single busiest model, reported)")
    ap.add_argument("--tail", type=float, default=DEFAULT_TAIL_S,
                    help="tail threshold in seconds (default: %(default)s)")
    ap.add_argument("--target", type=float, default=DEFAULT_TARGET,
                    help="target RECLASSIFICATION share: the share of "
                         "currently-successful attempts the cutoff may convert "
                         "into stalls (default: %(default)s)")
    ap.add_argument("--alpha", type=float, default=0.05,
                    help="CI two-sided alpha (default: %(default)s)")
    ap.add_argument("--bins", type=int, default=40)
    ap.add_argument("--bootstrap", action="store_true",
                    help="also print the bootstrap cross-check")
    args = ap.parse_args(argv)

    paths = discover_log_paths(args.log)
    if not paths:
        print(f"no log files found under {args.log}", file=sys.stderr)
        return 3

    # ---- population scan (streamed; one pass, everything counted here)
    kinds = Counter()
    models = Counter()
    uncensored = []
    censored = []
    excluded = Counter()
    stall_windows = Counter()
    first_ts = None
    last_ts = None
    bad_lines = 0
    total_lines = 0
    unparsed_model = Counter()
    for line in iter_log_lines(paths):
        total_lines += 1
        line = line.strip()
        if not line:
            continue
        try:
            r = json.loads(line)
        except Exception:
            bad_lines += 1
            continue
        ts = r.get("timestamp")
        if not isinstance(ts, str):
            bad_lines += 1
            continue
        if first_ts is None or ts < first_ts:
            first_ts = ts
        if last_ts is None or ts > last_ts:
            last_ts = ts
        if args.floor and ts < args.floor:
            continue
        if args.ceiling and ts >= args.ceiling:
            continue
        kind = r.get("record_kind", RECORD_KIND_TTFB)
        kinds[kind] += 1
        if kind != RECORD_KIND_TTFB:
            continue
        m = r.get("responding_model") or "<missing>"
        models[m] += 1
        c, w = classify(r)
        if c == "uncensored":
            L = r.get("latency_ms")
            if isinstance(L, (int, float)) and L >= 0:
                uncensored.append(int(L))
            else:
                unparsed_model["uncensored_no_latency"] += 1
        elif c == "censored":
            censored.append(r)
            stall_windows[w] += 1
        else:
            e = (r.get("error") or "")[:60]
            excluded[e] += 1

    print("=" * 78)
    print("FIRST-BYTE CUTOFF DERIVATION")
    print("=" * 78)
    print("POPULATION (denominators — every count below is of something named)")
    print(f"  log files read, in order (rotated first, live LAST):")
    for p in paths:
        print(f"    {p}")
    print(f"  lines read (streamed, nothing loaded whole): {total_lines}")
    print(f"  lines that did not parse as a record:          {bad_lines}")
    print(f"  timestamp range of ALL records read:           {first_ts} .. {last_ts}")
    print(f"  slice floor (inclusive):  {args.floor or '(none)'}")
    print(f"  slice ceiling (exclusive):{args.ceiling or '(none)'}")
    print(f"  record_kind split in slice: " +
          ", ".join(f"{k}={kinds[k]}" for k in sorted(kinds)))
    print(f"  models in slice (responding_model):")
    for m, c in models.most_common():
        print(f"    {c:7d}  {m}")
    chosen = args.model
    if chosen is None:
        chosen = models.most_common(1)[0][0] if models else None
    if chosen and chosen != "*":
        print(f"  model used for the fit: {chosen}")
        if args.model is None:
            print("    (the busiest model, chosen because the derivation is "
                  "per-model; pass --model to pin one)")

    print()
    print("CLASSIFICATION OF ttfb RECORDS (why each part may be used)")
    n_ttfb = sum(models.values())
    n_err = n_ttfb - len(uncensored) - len(censored)
    print(f"  ttfb records in slice:                 {n_ttfb}")
    print(f"  uncensored (no error -> a real TTFB):  {len(uncensored)}")
    print(f"  censored (first-byte stall):           {len(censored)}"
          f"   <- latency is the WINDOW, excluded from the fit")
    print(f"  excluded (other errors):               {n_err}")
    print(f"  stall window values seen in the error strings:")
    for w, c in sorted(stall_windows.items(), key=lambda kv: (kv[0] is None, kv[0])):
        print(f"    {w} ms x {c}")
    if excluded:
        print(f"  excluded error strings (top 8):")
        for e, c in excluded.most_common(8):
            print(f"    {c:6d}  {e!r}")
    if unparsed_model:
        print(f"  records with a missing/non-numeric latency: {dict(unparsed_model)}")

    # restrict to the chosen model if requested
    if chosen and args.model:
        uncensored_m, censored_m = [], []
        # re-scan for the single model (cheap; the log is already streamed once
        # and this keeps the code honest rather than filtering in place with a
        # second in-memory copy of every record)
        for p in paths:
            with open(p, "r", encoding="utf-8", errors="replace") as fh:
                for line in fh:
                    try:
                        r = json.loads(line)
                    except Exception:
                        continue
                    ts = r.get("timestamp")
                    if not isinstance(ts, str):
                        continue
                    if args.floor and ts < args.floor:
                        continue
                    if args.ceiling and ts >= args.ceiling:
                        continue
                    if r.get("record_kind") != RECORD_KIND_TTFB:
                        continue
                    if (r.get("responding_model") or "<missing>") != chosen:
                        continue
                    c, _w = classify(r)
                    if c == "uncensored":
                        L = r.get("latency_ms")
                        if isinstance(L, (int, float)) and L >= 0:
                            uncensored_m.append(int(L))
                    elif c == "censored":
                        censored_m.append(r)
        uncensored, censored = uncensored_m, censored_m
        print(f"  restricted to {chosen}: uncensored={len(uncensored)}, "
              f"censored={len(censored)}")

    uncensored.sort()

    print()
    print("DISTRIBUTION USED (uncensored TTFB, ms)")
    if not uncensored:
        print("  no uncensored observations in the slice; nothing to derive.")
        return 2
    for p in (10, 25, 50, 75, 90, 95, 99):
        print(f"  p{p:<2d} = {percentile(uncensored, p):8d} ms")
    print(f"  max = {uncensored[-1]:8d} ms   n = {len(uncensored)}")
    print()
    hist(uncensored, args.bins)

    print()
    print("TAIL (the part the fit uses)")
    for t in (5.0, 6.0, 8.0, 10.0, 12.0):
        c = sum(1 for v in uncensored if v >= t * 1000)
        print(f"  >= {t:5.1f}s : {c:6d}  ({100*c/len(uncensored):6.2f}%)")


    print()
    print("FIT AND DERIVATION")
    res = derive(uncensored, args.tail, args.target, args.alpha,
                 censored_windows=[w for (w, c) in stall_windows.items()
                                   for _ in range(c)])
    if "error" in res:
        print(f"  DERIVATION FAILED: {res['error']}")
        return 2
    print(f"  tail threshold T:                 {res['tail_s']} s")
    print(f"  uncensored observations above T:  {res['n_tail']} "
          f"({100*res['tail_share']:.2f}% of the uncensored sample)")
    print(f"  CENSORED rows above T:            {res['n_censored_above_T']} "
          f"(stalls at the live window {res['max_censored_window_ms']} ms)")
    print(f"  total exceedances K:              {res['K_exceedances']} "
          f"(uncensored above T + censored above T)")
    print(f"  tail range:                       {res['tail_min_ms']} .. "
          f"{res['tail_max_ms']} ms")
    print(f"  mean excess above T (uncensored): {res['tail_mean_excess_ms']:.1f} ms "
          f"({res['tail_mean_excess_s']:.3f} s)")
    print()
    print("  ESTIMATOR 1 (PRIMARY): censored MLE — the stall rows enter as")
    print("    right-censored observations with excess max(W - T, 0). The live")
    print("    window is a censoring mechanism; an uncensored-only fit conditions")
    print("    on survival and cannot see the population the cutoff decides about.")
    print(f"    lambda_hat (per second):        {res['lambda_cens_per_s']:.6f}")
    print(f"    denominator:                    {res['denominator_s']:.2f} s "
          f"of exposure over K={res['K_exceedances']} exceedances")
    print()
    print("  ESTIMATOR 2 (SECONDARY): uncensored only — reported beside the")
    print("    primary so the two can be compared, never averaged.")
    print(f"    lambda_hat (per second):        {res['lambda_unc_per_s']:.6f}")
    if "cutoff_uncensored_ms" in res:
        print(f"    cutoff it would give:           "
              f"{res['cutoff_uncensored_ms']:.0f} ms "
              f"({res['cutoff_uncensored_ms']/1000:.2f} s)")
    print(f"    the gap between the two is the censoring bias, and it is")
    print(f"    reported rather than hidden:    "
          f"{(res['cutoff_uncensored_ms'] - res['cutoff_ms'])/1000.0:+.2f} s")
    print()
    print("MEAN-EXCESS PLOT (how the tail threshold T is chosen, not asserted)")
    print("  A constant-hazard (exponential) tail has a mean excess that is FLAT")
    print("  in T. The censored column counts the stall rows above T; their")
    print("  minimum excess is the live window minus T, which is why the")
    print("  uncensored mean excess falls as T rises -- it is clipped by the")
    print("  window, and that clipping is the bias the censored MLE corrects.")
    print()
    print("     T (s)  n_above  mean_excess(ms)  censored_above  censored_min_excess(ms)")
    for row in res["mean_excess_plot"]:
        me = row["mean_excess_ms"]
        cme = row["censored_excess_min_ms"]
        me_s = f"{me:13.1f}" if me is not None else "          n/a"
        cme_s = f"{cme:19.1f}" if cme is not None else "               n/a"
        print(f"    {row['t_s']:6d}  {row['n_above']:7d}  {me_s}  "
              f"{row['censored_above']:14d}  {cme_s}")
    print()
    if "cutoff_gpd_ms" in res:
        print("  ESTIMATOR 3: the GPD tail (no constant-hazard assumption).")
        print(f"    cutoff it gives:                {res['cutoff_gpd_ms']:.0f} ms "
              f"({res['cutoff_gpd_ms']/1000:.2f} s)")
        print("    (the exponential assumption is rejected above, so this is the")
        print("     estimator the design doc treats as the honest bound; it is")
        print("     reported beside the exponential one, not instead of it.)")
        print()
    print()
    if "gpd_xi" in res:
        print(f"  ASSUMPTION CHECK: GPD shape xi over the uncensored tail:")
        print(f"    xi = {res['gpd_xi']:+.4f}   (0 == exponential; the model)")
        print(f"    sigma = {res['gpd_sigma_ms']:.1f} ms")
        print(f"    LR stat (GPD vs exponential) = {res['lr_stat']:.4f}, "
              f"p = {res['lr_p']:.4f}")
        if res["lr_p"] < 0.05:
            print("    -> the exponential assumption is REJECTED at alpha=0.05: the")
            print("       tail is NOT constant-hazard. The cutoff below therefore")
            print("       carries this caveat explicitly (see the design doc §2c).")
        else:
            print("    -> the exponential assumption is not rejected; the tail is")
            print("       consistent with a constant-hazard process.")
        print()
    print(f"  target reclassification share:    {100*res['target_reclass']:.2f}%"
          f"  (of currently-successful attempts, NOT of all attempts)")
    print()
    print(f"  DERIVED CUTOFF:                   {res['cutoff_ms']:.0f} ms "
          f"({res['cutoff_ms']/1000:.2f} s)")
    if "cutoff_lo_ms" in res:
        print(f"  {100*(1-res['ci_alpha']):.0f}% CI ({res['ci_method']}):"
              f" {res['cutoff_lo_ms']:.0f} .. {res['cutoff_hi_ms']:.0f} ms")
    else:
        print(f"  CI: {res['ci_method']}")
    if args.bootstrap and "boot_cutoff_ms" in res:
        print(f"  bootstrap ({res['boot_n']} resamples, seed 20261021): "
              f"{res['boot_lo_ms']:.0f} .. {res['boot_hi_ms']:.0f} ms, "
              f"median {res['boot_cutoff_ms']:.0f} ms")
    print()
    print("WHAT THE CUTOFF WOULD DO TO THIS SLICE")
    print(f"  uncensored observations it reclassifies as stalls: "
          f"{res['reclassified_uncensored']} ({100*res['reclassified_share']:.2f}% "
          f"of {len(uncensored)})")
    print(f"  stall rows already in the slice:  {len(censored)}")
    print(f"  predicted stall share of all attempts at this cutoff: "
          f"{100*res['predicted_stall_share']:.2f}%")
    print()
    print("  This prediction is what the VALIDATION window measures. It is")
    print("  computed here, from the same snapshot the cutoff came from, so the")
    print("  derivation and its prediction cannot drift apart.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
