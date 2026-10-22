#!/usr/bin/env python3
"""Export the client-side TTFT records for the successor window's Q3.

Owner decision 4 (2026-10-21): instrument the CLIENT side first, so the
client/proxy TTFT ratio can be joined in the next window.

WHAT THIS IS. An EXPORT, not a new measurement. The client (an omp agent
session) already records, per assistant message, the time to its own first
token (`message.ttft`, in milliseconds), the provider, the model, and the
usage. This script reads those records and writes them out in the shape the
window's design specifies (SUCCESSOR-WINDOW.md section 7.1), filtered to the
proxy's provider, so the window has a client-side population to join.

THE JOIN KEY, and why it is what it is. The client's own `message.responseId`
is the response body's `id` - the upstream's id, relayed by the proxy. The
proxy's log `request_id` is a DIFFERENT id (YYYYMMDDHHMMSS.pid.seq) that the
proxy only publishes in a response HEADER, which the transcript does not
record. So the client and the log currently share no id.

MEASURED: of 37472 assistant records with provider == "nimaproxy" across the
session corpus, ZERO carry a proxy-format request id in any structural field;
a corpus-wide scan for that shape returns 47 hits, and every one is an
assistant's own prose discussing the id format.

The fix is a turn-log field carrying the body's `id` (the proxy already parses
it on both the streaming and the non-streaming path - sse_tap.rs:348 for
streams, and the assembled body for non-streams). When that field exists, the
join is EXACT:

    client.responseId == turn_log.response_body_id  ->  turn_log.request_id
    ->  the ttfb row's latency_ms

Until it exists, this script emits `client_response_id` and the window must
join on the weaker `session_id` + time window, and REPORT that weaker join's
match rate beside the p95 (a p95 over a partial match is not the population's
p95).

Usage:
    export-client-ttft.py [--sessions DIR] [--since RFC3339]
                           [--until RFC3339] [--provider NAME]
                           [--out FILE] [--json] [--limit N]

    --sessions DIR   the session corpus root (REQUIRED: where the corpus
                     lives is deployment-specific, so it is never baked in)
    --since          emit only records at or after this instant (inclusive)
    --until          emit only records before this instant (exclusive)
    --provider       only this provider (default: nimaproxy)
    --out            write the JSONL here (default: stdout)
    --json           print a summary object instead of the records
    --limit          stop after N records (for a smoke test)

The output is append-only JSONL, one object per client turn, with the fields
named in the design. It is written to a file the window can read; the window's
close reads it alongside the log snapshot and computes the ratio.

WHAT THIS DOES NOT DO, deliberately:
  - it does not modify the transcripts, the proxy, or the log;
  - it does not invent a join. The `client_response_id` it emits is the id the
    client ACTUALLY recorded, and if that id is not in the log, the join
    reports a match rate rather than pretending to be exact;
  - it does not filter by "the response came from the proxy" by model name. It
    filters by `provider`, which is the field the client actually sets, so a
    non-proxy provider is excluded rather than silently averaged in.
"""

import argparse
import datetime
import glob
import json
import os
import sys


def parse_ts(s):
    if not s:
        return None
    try:
        return datetime.datetime.fromisoformat(s.replace("Z", "+00:00"))
    except Exception:
        return None


def record_instant(msg):
    """The instant to range-filter on.

    `message.timestamp` is epoch MILLISECONDS (e.g. 1791154329956), while
    `message.completedAt` is also epoch ms and `completedAt` is None for a
    failed request. The assistant message's own `timestamp` (ISO, on the outer
    record) is the wall clock of the record. Prefer the message's epoch-ms
    timestamp, fall back to the outer one.
    """
    for v in (msg.get("timestamp"), msg.get("completedAt")):
        if isinstance(v, (int, float)) and v > 0:
            return datetime.datetime.fromtimestamp(v / 1000.0,
                                                    datetime.timezone.utc)
    return None


def session_files(root):
    """Every session transcript under root, newest first.

    The corpus is per-session files under <root>/<cwd-slug>/<ts>_<uuid>.jsonl.
    `agent.db` indexes them (threads.rollout_path) but reading the files
    directly is simpler and does not need the db to be unlocked.
    """
    files = glob.glob(os.path.join(root, "**", "*.jsonl"), recursive=True)
    files = [f for f in files if os.path.isfile(f)]
    files.sort(key=lambda f: os.path.getmtime(f), reverse=True)
    return files


def extract(path, provider, since, until, limit=None):
    """Yield the client TTFT records from one transcript."""
    out = []
    try:
        fh = open(path, errors="replace")
    except OSError:
        return out
    with fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            # The prefilter must NOT require a `"ttft"` key to be present. A
            # request that FAILED before producing a token has NO ttft field
            # at all - and those are exactly the records the window's ratio
            # must not drop silently. Requiring the key excluded them: a
            # first version of this script reported 0 failed-before-a-token
            # records over 35034 emitted, while a direct search found 12 in
            # the newest files alone.
            if '"provider"' not in line:
                continue
            try:
                d = json.loads(line)
            except Exception:
                continue
            if d.get("type") != "message":
                continue
            msg = d.get("message") or {}
            if msg.get("provider") != provider:
                continue
            ttft = msg.get("ttft")
            if ttft is None:
                # A failed request has no first token, so no TTFT. It is
                # still worth emitting, because the client DID experience a
                # failure and the window's ratio must not silently exclude
                # the requests that never produced a token.
                pass
            else:
                try:
                    ttft = float(ttft)
                except Exception:
                    continue
            when = record_instant(msg)
            if since and (when is None or when < since):
                continue
            if until and (when is not None and when >= until):
                continue
            usage = msg.get("usage") or {}
            rec = {
                "client_response_id": msg.get("responseId"),
                "client_ttft_ms": ttft,
                "client_first_token_at": (when.isoformat() if when else None),
                "model": msg.get("model"),
                "provider": msg.get("provider"),
                "api": msg.get("api"),
                "stopReason": msg.get("stopReason"),
                "duration_ms": msg.get("duration"),
                "prompt_tokens": usage.get("input"),
                "completion_tokens": usage.get("output"),
                "cache_read_tokens": usage.get("cacheRead"),
                "total_tokens": usage.get("totalTokens"),
                "error_id": msg.get("errorId"),
                "error_message": msg.get("errorMessage"),
                "session_file": os.path.basename(path),
                "session_id": d.get("id"),
            }
            out.append(rec)
            if limit and len(out) >= limit:
                break
    return out


def main(argv):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--sessions", required=True,
                    help="the session corpus root (required)")
    ap.add_argument("--since", default=None)
    ap.add_argument("--until", default=None)
    ap.add_argument("--provider", default="nimaproxy")
    ap.add_argument("--out", default="-")
    ap.add_argument("--json", action="store_true")
    ap.add_argument("--limit", type=int, default=None)
    a = ap.parse_args(argv[1:])

    since = parse_ts(a.since)
    until = parse_ts(a.until)
    if a.since and since is None:
        eprint("could not parse --since: %r" % a.since)
        return 3
    if a.until and until is None:
        eprint("could not parse --until: %r" % a.until)
        return 3

    files = session_files(a.sessions)
    if not files:
        eprint("no session files under %s" % a.sessions)
        return 3

    records = []
    scanned = 0
    for f in files:
        got = extract(f, a.provider, since, until, a.limit)
        scanned += 1
        records.extend(got)
        if a.limit and len(records) >= a.limit:
            break
    records.sort(key=lambda r: (r.get("client_first_token_at") or ""))

    # ---- the summary, with the denominators named
    n = len(records)
    with_ttft = [r for r in records if r.get("client_ttft_ms") is not None]
    with_id = [r for r in records if r.get("client_response_id")]
    failed = [r for r in records if r.get("client_ttft_ms") is None]
    providers = {}
    for r in records:
        providers[r.get("provider")] = providers.get(r.get("provider"), 0) + 1

    summary = {
        "session_files_scanned": scanned,
        "session_files_total": len(files),
        "provider_filtered": a.provider,
        "since": a.since,
        "until": a.until,
        "records_emitted": n,
        "records_with_a_ttft": len(with_ttft),
        "records_with_a_client_response_id": len(with_id),
        "records_that_failed_before_a_token": len(failed),
        "providers_seen": providers,
        "join_key": "client_response_id == turn_log.response_body_id",
        "join_key_status": (
            "the turn log has NO response_body_id field yet, so this key "
            "cannot be joined to the log. The window must use the weaker "
            "session_id + time-window join and REPORT its match rate beside "
            "the p95. See SUCCESSOR-WINDOW.md section 7.0."),
    }
    if with_ttft:
        vals = sorted(r["client_ttft_ms"] for r in with_ttft)

        def pct(p):
            import math
            k = max(1, math.ceil(p / 100.0 * len(vals)))
            return vals[k - 1]
        summary["client_ttft_ms_p50"] = pct(50)
        summary["client_ttft_ms_p95"] = pct(95)
        summary["client_ttft_ms_min"] = vals[0]
        summary["client_ttft_ms_max"] = vals[-1]

    if a.json:
        print(json.dumps(summary, indent=2, sort_keys=True, default=str))
        return 0

    sink = sys.stdout if a.out == "-" else open(a.out, "a")
    try:
        for r in records:
            sink.write(json.dumps(r, sort_keys=True, default=str) + "\n")
    finally:
        if sink is not sys.stdout:
            sink.close()
    eprint("exported %d records (%d with a ttft) to %s"
           % (n, len(with_ttft), a.out))
    return 0


def eprint(s):
    sys.stderr.write(s + "\n")


if __name__ == "__main__":
    sys.exit(main(sys.argv))
