## Terminal table and JSON rendering for nimakai.

import std/[strformat, strutils, json, algorithm, options, terminal]
import ./[types, metrics, catalog]

proc termWidth*(): int =
  ## Get terminal width, defaulting to 120 if detection fails.
  try:
    let w = terminalWidth()
    if w > 0: w else: 120
  except CatchableError:
    120

proc stripAnsi*(s: string): int =
  var i = 0
  var count = 0
  while i < s.len:
    if s[i] == '\e':
      while i < s.len and s[i] != 'm': inc i
      inc i
    else:
      inc count
      inc i
  count

proc padRightAnsi*(s: string, width: int): string =
  let visible = stripAnsi(s)
  if visible >= width: s
  else: s & ' '.repeat(width - visible)

proc padLeftAnsi*(s: string, width: int): string =
  let visible = stripAnsi(s)
  if visible >= width: s
  else: ' '.repeat(width - visible) & s

proc colorLatency*(ms: float): string =
  let val = &"{ms:.0f}ms"
  if ms < 500: return "\e[32m" & val & "\e[0m"
  if ms < 1500: return "\e[33m" & val & "\e[0m"
  return "\e[31m" & val & "\e[0m"

proc healthIcon*(h: Health): string =
  case h
  of hUp: "\e[32mUP\e[0m"
  of hTimeout: "\e[33mTIMEOUT\e[0m"
  of hOverloaded: "\e[31mOVERLOADED\e[0m"
  of hError: "\e[31mERROR\e[0m"
  of hNoKey: "\e[33mNO_KEY\e[0m"
  of hNotFound: "\e[33mNOT_FOUND\e[0m"
  of hPending: "\e[90mPENDING\e[0m"

proc verdictColor*(v: Verdict): string =
  let s = $v
  case v
  of vPerfect: "\e[32m" & s & "\e[0m"
  of vNormal: "\e[36m" & s & "\e[0m"
  of vSlow: "\e[33m" & s & "\e[0m"
  of vSpiky: "\e[35m" & s & "\e[0m"
  of vVerySlow: "\e[31m" & s & "\e[0m"
  of vNotFound: "\e[33m" & s & "\e[0m"
  of vNotActive: "\e[31m" & s & "\e[0m"
  of vUnstable: "\e[31;1m" & s & "\e[0m"
  of vPending: "\e[90m" & s & "\e[0m"


proc sortStats*(stats: var seq[ModelStats], col: SortColumn,
                cat: seq[ModelMeta], th: Thresholds = DefaultThresholds) =
  ## Sort stats in place. Favorites always come first.
  stats.sort(proc(a, b: ModelStats): int =
    # Favorites pinned to top
    if a.favorite != b.favorite:
      return if a.favorite: -1 else: 1
    case col
    of scName:
      return cmp(a.name, b.name)
    of scAvg:
      return cmp(a.avg(), b.avg())
    of scP95:
      return cmp(a.p95(), b.p95())
    of scStability:
      let sa = a.stabilityScore(th)
      let sb = b.stabilityScore(th)
      return cmp(sb, sa) # descending
    of scUptime:
      return cmp(b.uptime(), a.uptime()) # descending
  )

proc printTable*(stats: seq[ModelStats], round: int,
                 cat: seq[ModelMeta], sortCol: SortColumn,
                 th: Thresholds = DefaultThresholds) =
  let tw = termWidth()
  # Adapt model name column: fixed columns need ~82 chars, rest goes to model name
  let fixedCols = 10 + 10 + 10 + 10 + 6 + 2 + 12 + 12 + 7 + 2 # prefix
  let nameWidth = max(15, min(45, tw - fixedCols))
  let sepWidth = min(tw - 4, nameWidth + fixedCols - 2)

  let hdr = &"\e[1m nimakai v{Version}\e[0m  \e[90mround {round} | NVIDIA NIM latency benchmark | sort: {sortCol}\e[0m"
  echo ""
  echo hdr
  echo ""

  let header = "  " &
    padRight("MODEL", nameWidth) &
    padLeft("LATEST", 10) &
    padLeft("AVG", 10) &
    padLeft("P95", 10) &
    padLeft("JITTER", 10) &
    padLeft("STAB", 6) &
    "  " & padRight("HEALTH", 12) &
    padRight("VERDICT", 12) &
    padLeft("UP%", 7)
  echo "\e[1;90m" & header & "\e[0m"
  echo "\e[90m  " & "-".repeat(sepWidth) & "\e[0m"

  for s in stats:
    let meta = cat.lookupMeta(s.id)
    let displayName = s.id  # Standardize on model ID
    let prefix = if s.favorite: "* " else: "  "
    var line = prefix & padRight(displayName, nameWidth)


    if s.ringLen > 0:
      line &= padLeftAnsi(colorLatency(s.lastMs), 10)
      line &= padLeftAnsi(colorLatency(s.avg()), 10)
      line &= padLeftAnsi(colorLatency(s.p95()), 10)
      line &= padLeftAnsi(&"\e[90m{s.jitter():.0f}ms\e[0m", 10)
    else:
      line &= padLeft("-", 10)
      line &= padLeft("-", 10)
      line &= padLeft("-", 10)
      line &= padLeft("-", 10)

    # Stability score
    let stab = s.stabilityScore(th)
    if stab >= 0:
      let stabColor = if stab >= 80: "\e[32m"
                      elif stab >= 50: "\e[33m"
                      else: "\e[31m"
      line &= padLeftAnsi(stabColor & $stab & "\e[0m", 6)
    else:
      line &= padLeft("-", 6)

    line &= "  " & padRightAnsi(healthIcon(s.lastHealth), 12)
    line &= padRightAnsi(verdictColor(s.verdict(th)), 12)

    let up = &"{s.uptime():.0f}%"
    if s.uptime() >= 90: line &= padLeftAnsi("\e[32m" & up & "\e[0m", 7)
    elif s.uptime() >= 50: line &= padLeftAnsi("\e[33m" & up & "\e[0m", 7)
    else: line &= padLeftAnsi("\e[31m" & up & "\e[0m", 7)

    echo line

  echo ""
  echo "\e[90m  sort: [A]vg [P]95 [S]tability [N]ame [U]ptime | [Q]uit\e[0m"
  echo ""

proc printJson*(stats: seq[ModelStats], round: int, cat: seq[ModelMeta],
                th: Thresholds = DefaultThresholds) =
  var results = newJArray()
  for s in stats:
    let meta = cat.lookupMeta(s.id)
    let sweScore = if meta.isSome: meta.get.sweScore else: 0.0

    results.add(%*{
      "model": s.id,
      "name": s.name,
      "swe_score": sweScore,
      "latest_ms": if s.ringLen > 0: s.lastMs else: 0.0,
      "avg_ms": s.avg(),
      "p50_ms": s.p50(),
      "p95_ms": s.p95(),
      "p99_ms": s.p99(),
      "min_ms": s.minMs(),
      "max_ms": s.maxMs(),
      "jitter_ms": s.jitter(),
      "spike_rate": s.spikeRate(th.spikeMs),
      "stability_score": s.stabilityScore(th),
      "health": $s.lastHealth,
      "verdict": $s.verdict(th),
      "uptime_pct": s.uptime(),
      "total_pings": s.totalPings,
      "success_pings": s.successPings,
      "favorite": s.favorite,
    })
  let output = %*{"round": round, "results": results}
  echo $output
