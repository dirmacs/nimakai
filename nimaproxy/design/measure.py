#!/usr/bin/env python3
"""Measure the SUCCESSOR-WINDOW metrics from a turn-log snapshot.

Usage:
    measure.py SNAPSHOT FLOOR CEIL [--json]
                              [--stats-floor=FILE --stats-ceil=FILE]
                              [--stats-url=URL]

  SNAPSHOT  path to a turn-log snapshot (.jsonl), or to a DIRECTORY holding
            the snapshot parts. When a directory is given, the parts are
            concatenated in time order: rotated parts first (LC_ALL=C name
            order), the live file last. See the ordering note below.
  FLOOR     inclusive lower bound on `timestamp` (e.g. 2026-10-21T18:11:43Z)
  CEIL      exclusive upper bound on `timestamp`
  --json    emit the same numbers as one JSON object on stdout (after the
            human-readable report)
  --stats-floor=FILE --stats-ceil=FILE
            PINNED gateway-counter readings (JSON) taken at the window's floor
            and at its close; the delta is reported. This is the REPRODUCIBLE
            path: re-running this tool on the same snapshot prints the same
            counter numbers, because they come from files, not from a fetch.
  --stats-url=URL  fetch a live /stats reading. NOT reproducible -- the
            numbers then depend on WHEN the analysis ran. Use only for a live
            check; for the artifact, use the pinned files.

The snapshot is a frozen FILE. This tool never reads the live log, never
writes to it, and never rotates it.

THE COUNTER-SIDE POPULATION, and why it is a second population
  The turn log records requests that REACHED THE UPSTREAM. An admission
  rejection -- 503 "gateway overloaded; retry later" (no global permit, or all
  per-key permits held) or 429 "all API keys rate-limited" (no key available) --
  returns to the client BEFORE any log_turn_request call, so it produces NO
  row at all. Measured on the deployed window, 2332 of 3569 client requests
  (56.0%) never reached the upstream and so are invisible to every log-side
  metric. They are visible only in the proxy's own /stats gateway counters.
  The design's sections 0.1 and 2.7 give the full treatment. Note that the
  counters are NOT a partition of request_total: record_request is called once
  per client request, while acquire_gateway_permits is called per attempt
  inside the retry loop, so one request can be refused several times. The
  denominator for a rate is request_total, the CLIENT requests.

ORDERING NOTE (why a directory is concatenated the way it is)
  The logger rotates at 64 MiB and renames the live file to
  `turns.jsonl.<YYYYMMDD_HHMMSS>`. The live name is a lexicographic PREFIX of
  the rotated names, so `cat turns.jsonl*` puts the live file FIRST and the
  window runs BACKWARDS in time -- with exactly the right line count, which is
  why a count check cannot see it. A directory input is therefore concatenated
  rotated-first, live-last, and the tool prints the boundary timestamps of
  every part so a reordering is visible rather than silent.

STREAMING: the file is read line by line; no snapshot is ever fully resident.
The per-turn join is a set of small per-turn state (bounded by the number of
distinct loop keys in the slice), not a copy of the rows.

WHAT THIS TOOL DOES NOT COMPUTE, and will not fake:
  - the client-side TTFT and therefore the client/proxy TTFT ratio. The client
    is not instrumented yet (owner decision 2026-10-21: instrument the client
    side first, join in a later window). The log holds no client clock.
  - any 81-bound recovery figure for a hypothetical cut that is not live. The
    bound is a set of turns defined by a first-byte window that is no longer
    configured; see SUCCESSOR-WINDOW.md section 8 for how the design recovers
    it from the live window instead.
"""

import json
import os
import sys
import datetime
import urllib.request
from collections import Counter, defaultdict

# ---------------------------------------------------------------------------
# population definition constants (printed with every report, so no number can
# be quoted without them)
# ---------------------------------------------------------------------------

STALL_SUBSTRING = "no bytes within"
RETRY_SUBSTRING = "retrying on another key"
CLEAN_FINISH = ("clean_stop", "tool_calls")

# record kinds (turn_log.rs RECORD_KIND_*)
K_TTFB = "ttfb"
K_COMPLETION = "completion"
K_LOOP = "loop"


# ---------------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------------

def eprint(*a):
    print(*a, file=sys.stderr)


def is_err(r):
    e = r.get("error")
    return isinstance(e, str) and e != ""


def is_stall(r):
    e = r.get("error")
    return isinstance(e, str) and STALL_SUBSTRING in e


def is_retrying(r):
    e = r.get("error")
    return isinstance(e, str) and RETRY_SUBSTRING in e


def turn_key_of(r):
    """Canonical, hashable turn key from the `loop_key` OBJECT.

    loop_key is {session_id, loop_seq, round_index}; JSON with sorted keys is
    the canonical form, so two records of the same turn join regardless of
    field order in the file.
    """
    lk = r.get("loop_key")
    if isinstance(lk, dict):
        return json.dumps(lk, sort_keys=True, separators=(",", ":"))
    if lk is None:
        return None
    return str(lk)


def pct(values_sorted, p):
    """Nearest-rank percentile over an already-sorted list.

    The 15 s results artifact used nearest-rank, and this tool must not
    silently change the definition: for n samples the p-th percentile is the
    value at rank ceil(p/100*n), 1-based.
    """
    n = len(values_sorted)
    if n == 0:
        return None
    import math
    rank = max(1, math.ceil(p / 100.0 * n))
    return values_sorted[rank - 1]


def share_pct(num, den):
    if not den:
        return None
    return round(100.0 * num / den, 2)


# ---------------------------------------------------------------------------
# part concatenation (directory input)
# ---------------------------------------------------------------------------

def parts_of(logdir):
    """Rotated parts in LC_ALL=C order, live file LAST, by explicit name."""
    names = [n for n in os.listdir(logdir)
             if n.startswith("turns.jsonl") and os.path.isfile(os.path.join(logdir, n))]
    rotated = sorted([n for n in names if n != "turns.jsonl"])
    live = [n for n in names if n == "turns.jsonl"]
    return [os.path.join(logdir, n) for n in rotated] + [os.path.join(logdir, n) for n in live]


def part_bounds(path):
    """(first timestamp, last timestamp, lines) by streaming."""
    first = last = None
    lines = 0
    with open(path, "r", errors="replace") as fh:
        for line in fh:
            if not line.strip():
                continue
            try:
                ts = json.loads(line).get("timestamp")
            except Exception:
                continue
            if ts is None:
                continue
            if first is None:
                first = ts
            last = ts
            lines += 1
    return first, last, lines


# ---------------------------------------------------------------------------
# the pass itself
# ---------------------------------------------------------------------------

def measure(snapshot, floor, ceil, report):
    # ---- pass-1 state: row-level counts, the per-attempt id index, and the
    #      per-turn join state. Nothing holds a row it does not need.
    kinds = Counter()
    n_lines = 0
    n_bad = 0

    ttfb_lat = []              # ttfb latency_ms (p50/p95)
    ttfb_rows = 0
    stall_rows = 0
    stall_retrying = 0
    stall_terminal = 0
    error_rows = 0
    error_keyed = 0

    # per-attempt join: request_id -> {kind flags + failure_mode}
    # (memory is O(distinct ids in slice), which is what any id join costs)
    by_id = {}

    # per-turn join state, keyed by canonical loop_key
    turn_attempts = Counter()          # ttfb attempts per turn
    turn_stall_attempts = Counter()    # stall attempts per turn
    turn_stall_keyed = Counter()
    turn_clean = {}                    # turn -> (failure_mode, ts) of its earliest clean completion
    turn_any_completion = Counter()
    turn_error_attempts = Counter()
    turn_first_ts = {}
    turn_last_ts = {}
    turn_first_stall_ts = {}
    turn_last_completion_ts = {}
    turn_ge15_attempts = Counter()     # attempts with latency_ms >= 15000

    unkeyed_ttfb = 0
    keyed_ttfb = 0
    unkeyed_stall = 0

    finish_modes = Counter()
    length_cutoff = 0
    truncated_stream = 0
    ge15_attempts = 0
    max_tokens_vals = Counter()
    status_counter = Counter()
    loop_keyed_rows = 0

    open_inputs = parts_of(snapshot) if os.path.isdir(snapshot) else [snapshot]
    slice_rows = []

    if report:
        print("=" * 78)
        print("POPULATION DEFINITION")
        print("=" * 78)
        print("  source file(s):")
        for p in open_inputs:
            print("    %s" % p)
        print("  timestamp filter: floor <= timestamp < ceil")
        print("    floor = %s" % floor)
        print("    ceil  = %s" % ceil)
        print("  record kinds counted: %s (all rows are classified by `record_kind`;" % (K_TTFB,))
        print("    rows written before the field existed default to \"ttfb\", per turn_log.rs)")
        print("  join keys:")
        print("    per-attempt : full `request_id` (ttfb half <-> completion half of one attempt)")
        print("    per-turn    : `loop_key` object {session_id, loop_seq, round_index},")
        print("                  canonicalised as JSON with sorted keys")
        print("  boundaries of the parts, in concatenation order:")
        for p in open_inputs:
            f, l, n = part_bounds(p)
            print("    %-42s %s .. %s  (%d rows)" % (os.path.basename(p), f, l, n))
        print()

    for path in open_inputs:
        with open(path, "r", errors="replace") as fh:
            for line in fh:
                if not line.strip():
                    continue
                try:
                    r = json.loads(line)
                except Exception:
                    n_bad += 1
                    continue
                n_lines += 1
                ts = r.get("timestamp") or ""
                if not (floor <= ts < ceil):
                    continue
                kind = r.get("record_kind") or K_TTFB
                kinds[kind] += 1
                slice_rows.append(r)
                tk = turn_key_of(r)
                lat = r.get("latency_ms") or 0
                rid = r.get("request_id")

                if kind == K_LOOP:
                    # loop summaries are not model calls; turn_log.rs's
                    # denominator rule says so explicitly. They are counted
                    # for reconciliation only, and are NOT folded into the
                    # per-turn join's turn population.
                    loop_keyed_rows += 1
                    continue

                if kind == K_TTFB:
                    ttfb_rows += 1
                    ttfb_lat.append(lat)
                    if tk:
                        keyed_ttfb += 1
                    else:
                        unkeyed_ttfb += 1
                    if lat >= 15000:
                        ge15_attempts += 1
                    if is_stall(r):
                        stall_rows += 1
                        if is_retrying(r):
                            stall_retrying += 1
                        else:
                            stall_terminal += 1
                        if tk:
                            turn_stall_keyed[tk] += 1
                        else:
                            unkeyed_stall += 1
                    status_counter[r.get("status_code")] += 1
                    mt = r.get("request_max_tokens")
                    if mt is not None:
                        max_tokens_vals[mt] += 1

                if kind == K_COMPLETION:
                    fm = r.get("failure_mode")
                    finish_modes[fm] += 1
                    if fm == "truncated_stream":
                        truncated_stream += 1
                    elif fm == "length_cutoff":
                        length_cutoff += 1

                if is_err(r):
                    error_rows += 1
                    if tk:
                        error_keyed += 1

                # ---- per-attempt index
                if rid:
                    slot = by_id.get(rid)
                    if slot is None:
                        slot = {"ttfb": 0, "completion": 0, "loop": 0,
                                "failure_mode": None, "ttfb_error": False,
                                "ttfb_row": None}
                        by_id[rid] = slot
                    if kind == K_TTFB:
                        slot["ttfb"] += 1
                        slot["ttfb_row"] = r
                        if is_err(r):
                            slot["ttfb_error"] = True
                    elif kind == K_COMPLETION:
                        slot["completion"] += 1
                        if fm is not None:
                            slot["failure_mode"] = fm
                    else:
                        slot["loop"] += 1

                # ---- per-turn state
                if tk:
                    if kind == K_TTFB:
                        turn_attempts[tk] += 1
                        if is_stall(r):
                            turn_stall_attempts[tk] += 1
                            if ts and (tk not in turn_first_stall_ts or ts < turn_first_stall_ts[tk]):
                                turn_first_stall_ts[tk] = ts
                        if is_err(r):
                            turn_error_attempts[tk] += 1
                        if lat >= 15000:
                            turn_ge15_attempts[tk] += 1
                        if ts:
                            if tk not in turn_first_ts or ts < turn_first_ts[tk]:
                                turn_first_ts[tk] = ts
                            if tk not in turn_last_ts or ts > turn_last_ts[tk]:
                                turn_last_ts[tk] = ts
                    if kind == K_COMPLETION:
                        turn_any_completion[tk] += 1
                        if ts and (tk not in turn_last_completion_ts or ts > turn_last_completion_ts[tk]):
                            turn_last_completion_ts[tk] = ts
                        if fm in CLEAN_FINISH:
                            if tk not in turn_clean or (ts or "") < (turn_clean[tk][1] or ""):
                                turn_clean[tk] = (fm, ts)

    ttfb_lat.sort()

    # ---- the 81-bound, as the log can express it under the LIVE window.
    # The 15 s protocol's bound was "the attempts a 15000 ms window converts
    # from success into stall". The live window is dynamic (25000 ms), so the
    # live analogue of that set is: ttfb attempts whose first-byte latency
    # falls in the window the knob would have cut -- [15000, 25000) ms -- and
    # which therefore a 15 s cut would have converted. Their per-turn recovery
    # is what the successor window measures.
    bound_band_lo, bound_band_hi = 15000, 25000
    band_turns = set()
    band_attempts = 0
    # a second pass over the small per-turn structures is not needed: the band
    # is identified from ttfb rows, which we kept only in aggregate above.
    # Re-read for the band population (streaming, cheap, no full residency).
    for path in open_inputs:
        with open(path, "r", errors="replace") as fh:
            for line in fh:
                if not line.strip():
                    continue
                try:
                    r = json.loads(line)
                except Exception:
                    continue
                if r.get("record_kind") != K_TTFB:
                    continue
                ts = r.get("timestamp") or ""
                if not (floor <= ts < ceil):
                    continue
                lat = r.get("latency_ms") or 0
                # The band is the set a 15000 ms cut would convert FROM SUCCESS
                # into stall, so it is identified from SUCCESS rows ONLY. A stall
                # row's latency_ms is the window that fired, not a first-byte
                # latency, and under a 15 s window such a row's value (15xxx)
                # falls INSIDE [15000, 25000) - so a test that does not exclude
                # stalls explicitly would count already-failed attempts as
                # conversions. In the live 25 s window the two tests agree (the
                # stall value is 25000+), but the definition must not depend on
                # the window's value.
                err = r.get("error")
                if isinstance(err, str) and STALL_SUBSTRING in err:
                    continue
                if bound_band_lo <= lat < bound_band_hi:
                    band_attempts += 1
                    tk = turn_key_of(r)
                    if tk:
                        band_turns.add(tk)

    band_turns_with_stall = [t for t in band_turns if turn_stall_attempts.get(t)]
    band_turns_with_stall_keyed = [t for t in band_turns if turn_stall_keyed.get(t)]

    # ---- per-turn salvage and recovery over the stall population.
    # "Recovery" and "salvage" are ORDERED quantities: the completing attempt
    # must come after the turn's first stall, otherwise a clean completion
    # recorded before the stall would be miscounted as a recovery from it.
    def clean_after_stall(t):
        c = turn_clean.get(t)
        if not c:
            return False
        return (c[1] or "") >= (turn_first_stall_ts.get(t) or "")

    def completion_after_stall(t):
        last = turn_last_completion_ts.get(t)
        if last is None:
            return False
        return last >= (turn_first_stall_ts.get(t) or "")

    stall_turns = [t for t, c in turn_stall_attempts.items() if c > 0]
    stall_turns_keyed = [t for t, c in turn_stall_keyed.items() if c > 0]
    salvage_turns = [t for t in stall_turns if clean_after_stall(t)]
    salvage_turns_keyed = [t for t in stall_turns_keyed if clean_after_stall(t)]
    recovered_turns = [t for t in stall_turns if completion_after_stall(t)]

    # ---- the 81-bound band's own recovery, on the same ordered predicate
    band_turns_recovered = [t for t in band_turns if clean_after_stall(t)]
    band_turns_with_stall_recovered = [t for t in band_turns_with_stall if clean_after_stall(t)]

    # ---- id-join integrity
    ttfb_ids = {i for i, v in by_id.items() if v["ttfb"]}
    comp_ids = {i for i, v in by_id.items() if v["completion"]}
    no_comp = ttfb_ids - comp_ids
    both_halves = ttfb_ids & comp_ids
    ids_3plus = {i for i, v in by_id.items() if (v["ttfb"] + v["completion"] + v["loop"]) > 2}
    # The THIEF signature: two completions under one id. No legitimate path
    # writes two completions under one id, so this is the sharp instrument.
    ids_2plus_completions = {
        i for i, v in by_id.items() if v["completion"] > 1
    }

    # ---- The ELIGIBLE population. A completion row is written ONLY on the
    #      streaming path, so a failed / rejected / non-streaming attempt can
    #      never hold a completion half. The gate is therefore computed over
    #      ids whose ttfb half was a SUCCESSFUL attempt; a raw coverage over
    #      all ttfb ids tracks the traffic mix, not the join's health, and its
    #      ceiling is set by construction (measured: 89.7-92.7%).
    def _completion_less_class(i):
        """Why does this id have no completion half? Four causes, one of which
        is a lost id."""
        t = by_id[i].get("ttfb_row")
        if t is None:
            return "unknown"
        if t.get("error"):
            return "failure"
        if (t.get("status_code") not in (200, None)):
            return "rejected"
        if t.get("success") is False:
            return "unsuccessful"
        if (t.get("request_tokens") is not None
                or t.get("response_tokens") is not None):
            return "non_streaming"
        return "streaming_orphan"

    no_comp_by_class = {}
    for i in no_comp:
        c = _completion_less_class(i)
        no_comp_by_class[c] = no_comp_by_class.get(c, 0) + 1

    # ELIGIBLE ids: successful streaming attempts. These are the only ids a
    # completion half is POSSIBLE for.
    eligible_ids = set()
    for i in ttfb_ids:
        t = by_id[i].get("ttfb_row")
        if t is None:
            continue
        if t.get("error"):
            continue
        if (t.get("status_code") not in (200, None)):
            continue
        if t.get("success") is False:
            continue
        if (t.get("request_tokens") is not None
                or t.get("response_tokens") is not None):
            continue
        eligible_ids.add(i)

    eligible_with_completion = eligible_ids & comp_ids
    lost_eligible = eligible_ids - comp_ids

    # ---- MATURE vs IN-FLIGHT. A completion half is written by a detached task
    #      bounded by `idle_window * 20 + 60 s` (3660 s at the live 180 s idle
    #      window), so a successful streaming attempt younger than that bound
    #      may simply still be in flight. Counting it would make the gate
    #      measure the CLOSE TIME rather than the traffic.
    IN_FLIGHT_GRACE_S = 3660
    slice_last_ts = max((r.get("timestamp") or "" for r in slice_rows), default="")
    def _age_s(i):
        t = by_id[i].get("ttfb_row")
        if not t or not slice_last_ts:
            return None
        try:
            a = datetime.datetime.fromisoformat(
                (t.get("timestamp") or "").replace("Z", "+00:00"))
            b = datetime.datetime.fromisoformat(
                slice_last_ts.replace("Z", "+00:00"))
        except Exception:
            return None
        return (b - a).total_seconds()

    lost_mature = set()
    lost_in_flight = set()
    for i in lost_eligible:
        age = _age_s(i)
        if age is not None and age < IN_FLIGHT_GRACE_S:
            lost_in_flight.add(i)
        else:
            lost_mature.add(i)

    # G3's denominator excludes the IN-FLIGHT ids for the same reason G4 counts
    # only mature ones: an in-flight id has not yet had its chance to receive a
    # completion half. Including them would make the coverage measure the
    # CLOSE TIME rather than the traffic.
    mature_eligible = eligible_ids - lost_in_flight
    mature_eligible_with_completion = mature_eligible & comp_ids

    # ---- ge15 per-attempt join (the invariant denominator). A streaming pass
    #      keeps the id slot free of latency while the denominator stays exact.
    ge15_joined_ids = set()
    ge15_trunc = 0
    for path in open_inputs:
        with open(path, "r", errors="replace") as fh:
            for line in fh:
                if not line.strip():
                    continue
                try:
                    r = json.loads(line)
                except Exception:
                    continue
                if r.get("record_kind") != K_TTFB:
                    continue
                ts = r.get("timestamp") or ""
                if not (floor <= ts < ceil):
                    continue
                if (r.get("latency_ms") or 0) < 15000:
                    continue
                rid = r.get("request_id")
                if not rid:
                    continue
                slot = by_id.get(rid)
                if slot and slot["completion"]:
                    ge15_joined_ids.add(rid)
                    if slot["failure_mode"] == "truncated_stream":
                        ge15_trunc += 1

    m = {
        "population": {
            "snapshot": open_inputs,
            "floor": floor,
            "ceil": ceil,
            "lines_read": n_lines,
            "unparseable_lines": n_bad,
            "rows_in_slice": sum(kinds.values()),
            "record_kinds": dict(kinds),
            "loop_summary_rows_carrying_a_turn_key": loop_keyed_rows,
            "turn_keys_distinct": len(set(turn_attempts) | set(turn_any_completion)),
        },
        "row_level": {
            "ttfb_rows": ttfb_rows,
            "ttfb_p50_ms": pct(ttfb_lat, 50),
            "ttfb_p95_ms": pct(ttfb_lat, 95),
            "stall_rows": stall_rows,
            "stall_rows_retrying": stall_retrying,
            "retry_firing_pct": share_pct(stall_retrying, stall_rows),
            "terminal_stalls": stall_terminal,
            "terminal_stall_share_pct": share_pct(stall_terminal, stall_rows),
            "completions": kinds[K_COMPLETION],
            "loop_rows": kinds[K_LOOP],
            "error_rows": error_rows,
            "error_rows_with_turn_key": error_keyed,
            "error_rows_without_turn_key": error_rows - error_keyed,
            "unkeyed_ttfb_rows": unkeyed_ttfb,
            "keyed_ttfb_rows": keyed_ttfb,
            "unkeyed_stall_rows": unkeyed_stall,
            "failure_modes": {str(k): v for k, v in finish_modes.items()},
            "truncated_stream": truncated_stream,
            "length_cutoff": length_cutoff,
            "status_codes": {str(k): v for k, v in status_counter.items()},
            "ge15_ttfb_attempts": ge15_attempts,
            "request_max_tokens_distribution": {str(k): v for k, v in sorted(max_tokens_vals.items())},
        },
        "ge15_invariant": {
            "denominator": ">=15000 ms ttfb attempts joined to their completion half (per-attempt join on request_id)",
            "ge15_attempts": ge15_attempts,
            "joined": len(ge15_joined_ids),
            "truncated": ge15_trunc,
            "share_pct": share_pct(ge15_trunc, len(ge15_joined_ids)),
        },
        "id_join_integrity": {
            "denominator": ("eligible ids = distinct request_id whose ttfb half was a "
                            "SUCCESSFUL attempt; only these can carry a completion half "
                            "(a completion row is written on the streaming path only)"),
            "ttfb_ids": len(ttfb_ids),
            "completion_ids": len(comp_ids),
            "ids_holding_both_halves": len(both_halves),
            "ttfb_ids_with_no_completion_half": len(no_comp),
            "no_completion_by_class": dict(sorted(no_comp_by_class.items())),
            "eligible_ids": len(eligible_ids),
            "eligible_ids_with_completion_half": len(eligible_with_completion),
            "mature_eligible_ids": len(mature_eligible),
            "mature_eligible_with_completion_half": len(mature_eligible_with_completion),
            "integrity_pct_eligible_with_both_halves": share_pct(
                len(mature_eligible_with_completion), len(mature_eligible)),
            "lost_successful_ids_mature": len(lost_mature),
            "lost_successful_ids_in_flight": len(lost_in_flight),
            "in_flight_grace_seconds": IN_FLIGHT_GRACE_S,
            "ids_carrying_2plus_completion_records": len(ids_2plus_completions),
            "ids_carrying_3plus_records": len(ids_3plus),
            "gate": {
                "G1_error_rows_without_a_turn_key": error_rows - error_keyed,
                "G2_ids_carrying_2plus_completion_records": len(ids_2plus_completions),
                "G3_integrity_pct_mature_eligible_with_both_halves": share_pct(
                    len(mature_eligible_with_completion), len(mature_eligible)),
                "G4_lost_successful_ids_mature": len(lost_mature),
            },
        },
        "per_turn": {
            "denominator": "turns (loop_key) with >=1 ttfb attempt in the slice",
            "turns_total": len(turn_attempts),
            "turns_with_a_stall_attempt": len(stall_turns),
            "turns_with_a_keyed_stall_attempt": len(stall_turns_keyed),
            "stall_attempts_total": sum(turn_stall_attempts.values()),
            "stall_attempts_keyed": sum(turn_stall_keyed.values()),
            "error_attempts_total": sum(turn_error_attempts.values()),
            "turns_with_a_clean_completion": len(turn_clean),
            "salvage_turns": len(salvage_turns),
            "salvage_turns_keyed_population": len(salvage_turns_keyed),
            "salvage_rate_pct": share_pct(len(salvage_turns), len(stall_turns)),
            "salvage_rate_keyed_pct": share_pct(len(salvage_turns_keyed), len(stall_turns_keyed)),
            "recovered_turns": len(recovered_turns),
            "recovery_rate_pct": share_pct(len(recovered_turns), len(stall_turns)),
        },
        "bound_band": {
            "definition": "ttfb attempts with %d <= latency_ms < %d ms: the set a 15000 ms first-byte window would have converted from success into stall" % (bound_band_lo, bound_band_hi),
            "attempts_in_band": band_attempts,
            "turns_with_an_attempt_in_band": len(band_turns),
            "turns_with_a_band_attempt_and_a_stall_attempt": len(band_turns_with_stall),
            "turns_with_a_keyed_band_attempt_and_a_stall_attempt": len(band_turns_with_stall_keyed),
            "turns_with_a_band_attempt_that_reached_a_clean_completion": len(band_turns_recovered),
            "band_turns_that_stalled_and_reached_a_clean_completion": len(band_turns_with_stall_recovered),
            "recovery_pct_of_band_turns": share_pct(len(band_turns_recovered), len(band_turns)),
            "recovery_pct_of_band_turns_with_a_stall": share_pct(len(band_turns_with_stall_recovered), len(band_turns_with_stall)),
        },
        "not_computable": {
            "client_ttft": "the client clock is not instrumented in this window; the log holds no client-side first-token time, so the client/proxy TTFT ratio cannot be computed here",
            "client_proxy_ratio_p95": None,
            "bound_under_a_hypothetical_cut": "the 81-bound for a 15 s cut that is not live cannot be measured directly; the band population above is its live-window analogue",
        },
    }

    if report:
        emit(m)
    return m


def emit(m):
    def row(label, value, den=None):
        if den is not None:
            print("  %-52s %-14s  (denominator: %s)" % (label, value, den))
        else:
            print("  %-52s %s" % (label, value))

    print("=" * 78)
    print("ROW-LEVEL METRICS")
    print("=" * 78)
    r = m["row_level"]
    row("ttfb rows", r["ttfb_rows"])
    row("ttfb p50 / p95 (ms)", "%s / %s" % (r["ttfb_p50_ms"], r["ttfb_p95_ms"]), "ttfb rows")
    row("stall rows (error contains %r)" % STALL_SUBSTRING, r["stall_rows"])
    row("retry firing", "%s%%" % r["retry_firing_pct"], "stall rows")
    row("terminal stalls (stall, no retry string)", r["terminal_stalls"], "stall rows")
    row("completions", r["completions"])
    row("loop rows (reconciliation only)", r["loop_rows"])
    row("error rows", r["error_rows"])
    row("error rows carrying a turn key", r["error_rows_with_turn_key"], "error rows")
    row("error rows WITHOUT a turn key", r["error_rows_without_turn_key"], "error rows")
    row("unkeyed stall rows", r["unkeyed_stall_rows"], "stall rows")
    row(">=15000 ms ttfb attempts", r["ge15_ttfb_attempts"], "ttfb rows")
    row("truncated_stream", r["truncated_stream"], "completion rows")
    row("length_cutoff", r["length_cutoff"], "completion rows")
    print()
    print("  failure modes over completion rows: %s" % r["failure_modes"])
    print("  status codes over ttfb rows: %s" % r["status_codes"])
    print("  request_max_tokens distribution over ttfb rows: %s" % r["request_max_tokens_distribution"])
    print()

    print("=" * 78)
    print("PER-ATTEMPT JOIN AND THE >=15 s INVARIANT")
    print("=" * 78)
    g = m["ge15_invariant"]
    row(">=15 s attempts (denominator A)", g["ge15_attempts"], "ttfb rows")
    row("of those, joined to a completion half", g["joined"], ">=15 s attempts")
    row("truncated_stream on the joined set", g["truncated"], "%d joined" % g["joined"])
    row("truncated share", "%s%%" % g["share_pct"], "joined >=15 s attempts")
    print()

    print("=" * 78)
    print("ID-JOIN INTEGRITY (issue #44 / PR #48: the request-id race)")
    print("=" * 78)
    print("  A completion row is written on the STREAMING path only, so a failed,")
    print("  rejected or non-streaming attempt can never carry one. The gate is")
    print("  therefore computed over the ELIGIBLE ids, and the raw total is only")
    print("  ever reported with its classes.")
    i = m["id_join_integrity"]
    row("distinct ttfb request_ids", i["ttfb_ids"])
    row("distinct completion request_ids", i["completion_ids"])
    row("ids holding BOTH halves", i["ids_holding_both_halves"], "distinct ttfb request_ids")
    row("ttfb ids with NO completion half", i["ttfb_ids_with_no_completion_half"], "distinct ttfb request_ids")
    print("    by class: %s" % i["no_completion_by_class"])
    print()
    print("  -- the ELIGIBLE population (the gate's denominator) --")
    row("eligible ids (successful streaming attempts)", i["eligible_ids"], "distinct ttfb request_ids")
    row("eligible ids WITH a completion half", i["eligible_ids_with_completion_half"], "eligible ids")
    row("lost successful ids, MATURE (> %ds old)" % i["in_flight_grace_seconds"], i["lost_successful_ids_mature"], "eligible ids")
    row("lost successful ids, still IN FLIGHT", i["lost_successful_ids_in_flight"], "eligible ids")
    print("    (an in-flight id is not a defect: the detached completion writer may")
    print("     still be waiting, bounded by idle_window*20+60s)")
    row("mature eligible ids (the gate's denominator)", i["mature_eligible_ids"], "eligible ids")
    row("mature eligible ids WITH a completion half", i["mature_eligible_with_completion_half"], "mature eligible ids")
    row("MATURE ELIGIBLE join coverage", "%s%%" % i["integrity_pct_eligible_with_both_halves"], "mature eligible ids")
    print()
    print("  -- the race's two signatures --")
    row("ids carrying 2+ completion records (THIEF)", i["ids_carrying_2plus_completion_records"], "distinct request_ids")
    row("ids carrying 3+ records (structural)", i["ids_carrying_3plus_records"], "distinct request_ids")
    print()
    print("  -- THE GATE --")
    g4 = i["gate"]
    row("G1 error rows without a turn key (must be 0)", g4["G1_error_rows_without_a_turn_key"])
    row("G2 ids with 2+ completions (must be 0)", g4["G2_ids_carrying_2plus_completion_records"])
    row("G3 mature eligible join coverage (>= 99.9%)", "%s%%" % g4["G3_integrity_pct_mature_eligible_with_both_halves"])
    row("G4 mature lost successful ids (must be 0)", g4["G4_lost_successful_ids_mature"])
    print()

    print("=" * 78)
    print("PER-TURN JOIN (loop_key object, canonicalised)")
    print("=" * 78)
    p = m["per_turn"]
    row("turns with >=1 ttfb attempt", p["turns_total"])
    row("turns with >=1 stall attempt (DENOMINATOR)", p["turns_with_a_stall_attempt"], "turns total")
    row("  of which the stall attempt carried its turn key", p["turns_with_a_keyed_stall_attempt"], "turns with a stall attempt")
    row("stall attempts total", p["stall_attempts_total"])
    row("stall attempts carrying a turn key", p["stall_attempts_keyed"], "stall attempts total")
    row("error attempts total", p["error_attempts_total"])
    row("turns reaching a clean completion", p["turns_with_a_clean_completion"], "turns total")
    row("SALVAGE: stalled turns reaching a clean completion", p["salvage_turns"], "turns with a stall attempt")
    row("salvage rate", "%s%%" % p["salvage_rate_pct"], "turns with a stall attempt")
    row("  same rate on the keyed-stall population only", "%s%%" % p["salvage_rate_keyed_pct"], "turns with a keyed stall attempt")
    row("RECOVERY: stalled turns with any completion half", p["recovered_turns"], "turns with a stall attempt")
    row("recovery rate (any completion)", "%s%%" % p["recovery_rate_pct"], "turns with a stall attempt")
    print()

    print("=" * 78)
    print("THE 81-BOUND BAND (live-window analogue)")
    print("=" * 78)
    b = m["bound_band"]
    print("  definition: %s" % b["definition"])
    row("attempts in the band", b["attempts_in_band"], "ttfb rows")
    row("turns with an attempt in the band", b["turns_with_an_attempt_in_band"])
    row("  of those, turns that also stalled", b["turns_with_a_band_attempt_and_a_stall_attempt"], "band turns")
    row("  of those, turns with a KEYED band attempt and a stall", b["turns_with_a_keyed_band_attempt_and_a_stall_attempt"], "band turns")
    row("band turns reaching a clean completion", b["turns_with_a_band_attempt_that_reached_a_clean_completion"], "band turns")
    row("band turns that stalled AND reached a clean completion", b["band_turns_that_stalled_and_reached_a_clean_completion"], "band turns that stalled")
    row("band recovery rate", "%s%%" % b["recovery_pct_of_band_turns"], "band turns")
    row("band recovery rate (stalled band turns only)", "%s%%" % b["recovery_pct_of_band_turns_with_a_stall"], "band turns that stalled")
    print()

    print("=" * 78)
    print("NOT COMPUTABLE FROM THIS LOG")
    print("=" * 78)
    n = m["not_computable"]
    print("  client-side TTFT: %s" % n["client_ttft"])
    print("  client/proxy TTFT ratio p95: %s" % n["client_proxy_ratio_p95"])
    print("  %s" % n["bound_under_a_hypothetical_cut"])
    print()


def read_gateway_counters(stats_url):
    """Read the proxy's gateway counters from /stats.

    These are the requests the TURN LOG CANNOT SEE: admission rejections
    (503 overload / 429 no-key) return to the client before any
    log_turn_request call, so they produce no row at all. The counters are
    cumulative since process start, so the caller must delta them.

    Only /stats carries them. /metrics, /gateway/metrics and /prometheus are
    404 on this proxy - probed on the live instance.
    """
    try:
        with urllib.request.urlopen(stats_url, timeout=10) as fh:
            d = json.loads(fh.read().decode("utf-8", "replace"))
    except Exception as e:
        return {"error": "could not read %s: %s" % (stats_url, e)}
    g = d.get("gateway") or {}
    out = {"source": stats_url, "read_at": datetime.datetime.now(
        datetime.timezone.utc).isoformat()}
    for k in ("upstream_attempts", "overload_rejects", "no_key_rejects",
              "rate_limit_count", "max_upstream_in_flight",
              "max_in_flight_per_key", "admission_wait_ms"):
        if k in g:
            out[k] = g[k]
    if "overload_rejects" in out and "no_key_rejects" in out:
        out["admission_rejects_total"] = out["overload_rejects"] + out["no_key_rejects"]
    if out.get("upstream_attempts"):
        out["admission_rejects_per_upstream_attempt"] = round(
            100.0 * out.get("admission_rejects_total", 0) / out["upstream_attempts"], 4)
    return out


def _flatten_counters(d):
    """Accept either shape of a /stats reading.

    A full /stats response nests the counters under "gateway"; a captured
    subset may be flat. Both must work, or the close script's floor file and
    the ceiling file will silently disagree about shape and the delta will be
    nonsense.
    """
    if not isinstance(d, dict):
        return {}
    g = d.get("gateway")
    if isinstance(g, dict):
        out = dict(g)
        out["read_at"] = d.get("read_at") or g.get("read_at")
        return out
    return dict(d)


def counter_delta(floor_read, ceil_read):
    """Delta the cumulative gateway counters across the window.

    The counters are cumulative since process start, so a raw reading charges
    the window with the PREVIOUS process's rejects. The floor reading must have
    been taken at the window's floor and the ceiling reading at its close.
    """
    if not floor_read or not ceil_read:
        return {"error": "both a floor and a ceiling counter reading are required"}
    if "error" in floor_read or "error" in ceil_read:
        return {"error": floor_read.get("error") or ceil_read.get("error")}
    fr = _flatten_counters(floor_read)
    cr = _flatten_counters(ceil_read)
    d = {}
    for k in ("request_total", "upstream_attempts", "overload_rejects",
              "no_key_rejects", "rate_limit_count", "timeout_count"):
        if k in fr and k in cr:
            d[k] = cr[k] - fr[k]
    d["admission_rejects_total"] = d.get("overload_rejects", 0) + d.get("no_key_rejects", 0)
    # The denominator is request_total (CLIENT requests), not
    # upstream_attempts. A single client request can be retried, and the
    # proxy's own internal retry loop re-enters admission, so the number of
    # admission rejects can EXCEED the number of upstream attempts - measured
    # 2280 rejects against 1501 upstream attempts, a 151.9% "rate" that is an
    # artefact of the wrong denominator, not a real rate. Against
    # request_total (3472) the same 2280 is 65.7%, which is a rate.
    if d.get("request_total"):
        d["admission_rejects_per_client_request"] = round(
            100.0 * d["admission_rejects_total"] / d["request_total"], 4)
    d["floor_read_at"] = fr.get("read_at")
    d["ceil_read_at"] = cr.get("read_at")
    d["floor_raw"] = {k: fr.get(k) for k in
                      ("request_total", "upstream_attempts", "overload_rejects",
                       "no_key_rejects", "rate_limit_count", "timeout_count")}
    d["ceil_raw"] = {k: cr.get(k) for k in
                     ("request_total", "upstream_attempts", "overload_rejects",
                      "no_key_rejects", "rate_limit_count", "timeout_count")}
    return d


def main(argv):
    args = [a for a in argv[1:] if not a.startswith("--")]
    as_json = "--json" in argv[1:]
    opts = [a for a in argv[1:] if a.startswith("--")]
    if len(args) != 3:
        eprint(__doc__)
        return 3
    snapshot, floor, ceil = args
    if not os.path.exists(snapshot):
        eprint("no such snapshot: %s" % snapshot)
        return 3
    m = measure(snapshot, floor, ceil, report=not as_json)

    # ---- the counter-side population: the requests the log cannot see.
    # --stats-floor and --stats-ceil are JSON files holding a /stats reading
    # taken at the window's floor and at its close.
    floor_stats = ceil_stats = None
    stats_url = None
    for o in opts:
        if o.startswith("--stats-floor="):
            floor_stats = o.split("=", 1)[1]
        elif o.startswith("--stats-ceil="):
            ceil_stats = o.split("=", 1)[1]
        elif o.startswith("--stats-url="):
            stats_url = o.split("=", 1)[1]
    if floor_stats and ceil_stats:
        try:
            with open(floor_stats) as fh:
                fr = json.load(fh)
        except Exception as e:
            fr = {"error": "could not read %s: %s" % (floor_stats, e)}
        try:
            with open(ceil_stats) as fh:
                cr = json.load(fh)
        except Exception as e:
            cr = {"error": "could not read %s: %s" % (ceil_stats, e)}
        m["counter_side"] = counter_delta(fr, cr)
    elif stats_url:
        m["counter_side"] = read_gateway_counters(stats_url)
        m["counter_side"]["reproducible"] = False
        m["counter_side"]["warning"] = (
            "this is a LIVE fetch, so the numbers depend on WHEN this tool ran, "
            "not on the window's ceiling. For the artifact, capture the reading "
            "into the snapshot directory at the close and pass it with "
            "--stats-floor/--stats-ceil instead.")
    else:
        m["counter_side"] = {
            "note": ("no counter reading supplied. The turn log CANNOT see the "
                     "admission-rejected requests (503 overload / 429 no-key): "
                     "they return to the client before any log_turn_request "
                     "call, so they produce no row. Pass --stats-floor=FILE and "
                     "--stats-ceil=FILE (JSON files holding a /stats reading) to "
                     "report that population, or --stats-url=URL for a live read. "
                     "See the design's section 0.1 for why this population "
                     "exists and section 2.7 for the metrics and the "
                     "denominator. NO fallback figure is printed here on "
                     "purpose: a measurement tool must not print a number it did "
                     "not just measure, because an unmeasured constant would be "
                     "re-printed by every future run as if it were current."),
        }
    if as_json:
        print(json.dumps(m, indent=2, sort_keys=True, default=str))
    else:
        print()
        print("=" * 78)
        print("COUNTER-SIDE POPULATION (the requests the turn log CANNOT see)")
        print("=" * 78)
        cs = m.get("counter_side") or {}
        if "note" in cs:
            print("  %s" % cs["note"])
        else:
            for k in ("request_total", "upstream_attempts", "overload_rejects",
                      "no_key_rejects", "rate_limit_count", "timeout_count",
                      "admission_rejects_total",
                      "admission_rejects_per_client_request",
                      "floor_read_at", "ceil_read_at"):
                if k in cs:
                    print("  %-42s %s" % (k, cs[k]))
            print("  (these are DELTAS across the window; the counters are")
            print("   cumulative since process start)")
            print("  (admission_rejects_per_client_request is the rate; the")
            print("   denominator is request_total, the CLIENT requests)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
