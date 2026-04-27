## Terminal table and JSON rendering for nimakai.
## v0.14.0: pagination, cursor nav, live filter, help overlay, latency bars.

import std/[strformat, strutils, json, algorithm, options, terminal, math]
import ./[types, metrics, catalog]

# ── Terminal utilities ──────────────────────────────────────────────────────

proc termWidth*(): int =
  ## Get terminal width, defaulting to 120 if detection fails.
  try:
    let w = terminalWidth()
    if w > 0: w else: 120
  except CatchableError:
    120

proc termHeight*(): int =
  ## Get terminal height, defaulting to 40 if detection fails.
  try:
    let h = terminalHeight()
    if h > 0: h else: 40
  except CatchableError:
    40

proc stripAnsi*(s: string): int =
  ## Count visible characters, skipping ANSI escape sequences.
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

# ── Color helpers ────────────────────────────────────────────────────────────

proc colorLatency*(ms: float): string =
  let val = &"{ms:.0f}ms"
  if ms < 500:   return "\e[32m" & val & "\e[0m"
  if ms < 1500:  return "\e[33m" & val & "\e[0m"
  return "\e[31m" & val & "\e[0m"

proc healthIcon*(h: Health): string =
  case h
  of hUp:         "\e[32mUP\e[0m"
  of hTimeout:    "\e[33mTIMEOUT\e[0m"
  of hOverloaded: "\e[31mOVERLOADED\e[0m"
  of hError:      "\e[31mERROR\e[0m"
  of hNoKey:      "\e[33mNO_KEY\e[0m"
  of hNotFound:   "\e[33mNOT_FOUND\e[0m"
  of hPending:    "\e[90mPENDING\e[0m"

proc verdictColor*(v: Verdict): string =
  let s = $v
  case v
  of vPerfect:    "\e[32m"   & s & "\e[0m"
  of vNormal:     "\e[36m"   & s & "\e[0m"
  of vSlow:       "\e[33m"   & s & "\e[0m"
  of vSpiky:      "\e[35m"   & s & "\e[0m"
  of vVerySlow:   "\e[31m"   & s & "\e[0m"
  of vNotFound:   "\e[33m"   & s & "\e[0m"
  of vNotActive:  "\e[31m"   & s & "\e[0m"
  of vUnstable:   "\e[31;1m" & s & "\e[0m"
  of vPending:    "\e[90m"   & s & "\e[0m"

# ── Latency sparkbar (5-char wide) ──────────────────────────────────────────

const BarBlocks = [" ", "\u2581", "\u2582", "\u2583", "\u2584", "\u2585", "\u2586", "\u2587", "\u2588"]

proc latencyBar*(ms: float, maxMs: float = 5000.0): string =
  ## 5-char Unicode bar showing relative latency. Green/yellow/red tinted.
  if ms <= 0: return "\e[90m─────\e[0m"
  let ratio = clamp(ms / maxMs, 0.0, 1.0)
  let filled = int(ratio * 5.0)
  let frac   = int((ratio * 5.0 - filled.float) * 8.0)
  var bar = ""
  for i in 0..<5:
    if i < filled:    bar &= BarBlocks[8]
    elif i == filled: bar &= BarBlocks[max(frac, 0)]
    else:             bar &= BarBlocks[0]
  let color = if ms < 500:  "\e[32m"
              elif ms < 1500: "\e[33m"
              else: "\e[31m"
  color & bar & "\e[0m"

# ── Sorting ─────────────────────────────────────────────────────────────────

proc sortStats*(stats: var seq[ModelStats], col: SortColumn,
                cat: seq[ModelMeta], th: Thresholds = DefaultThresholds) =
  ## Sort stats in place. Favorites always come first.
  stats.sort(proc(a, b: ModelStats): int =
    if a.favorite != b.favorite:
      return if a.favorite: -1 else: 1
    case col
    of scName:      return cmp(a.name, b.name)
    of scAvg:       return cmp(a.avg(), b.avg())
    of scP95:       return cmp(a.p95(), b.p95())
    of scStability:
      let sa = a.stabilityScore(th)
      let sb = b.stabilityScore(th)
      return cmp(sb, sa)  # descending
    of scUptime:
      return cmp(b.uptime(), a.uptime())  # descending
  )

# ── Filter helpers ───────────────────────────────────────────────────────────

proc filterStats*(stats: seq[ModelStats], query: string): seq[ModelStats] =
  ## Return models whose id or name contains query (case-insensitive).
  if query.len == 0: return stats
  let q = query.toLowerAscii()
  for s in stats:
    if q in s.id.toLowerAscii() or q in s.name.toLowerAscii():
      result.add(s)

proc highlightQuery*(s: string, query: string): string =
  ## Wrap occurrences of query in s with bold-yellow highlight.
  if query.len == 0: return s
  let q = query.toLowerAscii()
  let sl = s.toLowerAscii()
  var result2 = ""
  var i = 0
  while i < s.len:
    let pos = sl.find(q, i)
    if pos < 0:
      result2 &= s[i..^1]
      break
    result2 &= s[i..<pos]
    result2 &= "\e[1;33m" & s[pos..<pos+q.len] & "\e[0m"
    i = pos + q.len
  result2

# ── Page legend ──────────────────────────────────────────────────────────────

proc pageLegend*(pager: PagerState, total: int): string =
  ## Compact page indicator string, e.g. "page 2/5 ".
  if not pager.enabled or pager.pageSize <= 0: return ""
  let pages = pageCount(total, pager.pageSize)
  &"page {pager.page + 1}/{pages} "

# ── Main TUI table ───────────────────────────────────────────────────────────

proc printTable*(stats: seq[ModelStats], round: int,
                 cat: seq[ModelMeta], sortCol: SortColumn,
                 th: Thresholds = DefaultThresholds,
                 pager: PagerState = PagerState(),
                 filterSt: FilterState = FilterState(),
                 cursorRow: int = -1,
                 proxyStatus: Option[ProxyHealth] = none(ProxyHealth)) =
  ## Render the benchmark table with optional pagination, filter highlight,
  ## cursor highlight, and full footer key legend.
  let tw = termWidth()
  # Fixed columns: LATEST(10) AVG(10) P95(10) JITTER(10) STAB(6)
  #                2-gap BAR(7) HEALTH(12) VERDICT(12) UP%(7) prefix(2) = 88
  let fixedCols = 10 + 10 + 10 + 10 + 6 + 2 + 7 + 12 + 12 + 7 + 2
  let nameWidth = max(15, min(50, tw - fixedCols))
  let sepWidth  = min(tw - 4, nameWidth + fixedCols - 2)

  # ── Header ──
  let filterNote = if filterSt.active or filterSt.query.len > 0:
    "  \e[1;33m[filter: " & filterSt.query & (if filterSt.active: "_" else: "") & "]\e[0m"
  else: ""
  echo ""
  echo &"\e[1m nimakai v{Version}\e[0m  " &
       &"\e[90mround {round} | NVIDIA NIM latency | sort: {sortCol}\e[0m" &
       filterNote
  echo ""

  # ── Apply filter ──
  let visible = filterStats(stats, filterSt.query)

  # ── Pagination slice ──
  var showStats: seq[ModelStats]
  if pager.enabled and pager.pageSize > 0:
    let (s, e) = pageSlice(visible.len, pager.page, pager.pageSize)
    showStats = visible[s..<e]
  else:
    showStats = visible

  # ── Column header ──
  let hdr = "  " &
    padRight("MODEL",  nameWidth) &
    padLeft("LATEST",  10) &
    padLeft("AVG",     10) &
    padLeft("P95",     10) &
    padLeft("JITTER",  10) &
    padLeft("STAB",     6) &
    "  " & padRight("BAR", 7) &
    padRight("HEALTH",  12) &
    padRight("VERDICT", 12) &
    padLeft("UP%",       7)
  echo "\e[1;90m" & hdr & "\e[0m"
  echo "\e[90m  " & "-".repeat(sepWidth) & "\e[0m"

  # ── Rows ──
  for i, s in showStats:
    let isCursor = (cursorRow >= 0 and i == cursorRow)
    let rowBg    = if isCursor: "\e[48;5;236m" else: ""
    let rowReset = if isCursor: "\e[0m" else: ""

    let displayName = if filterSt.query.len > 0:
      highlightQuery(s.id, filterSt.query)
    else:
      s.id
    let prefix = if s.favorite: "\e[33m*\e[0m " else: "  "
    var line = rowBg & prefix & padRightAnsi(displayName, nameWidth)

    if s.ringLen > 0:
      line &= padLeftAnsi(colorLatency(s.lastMs), 10)
      line &= padLeftAnsi(colorLatency(s.avg()),  10)
      line &= padLeftAnsi(colorLatency(s.p95()),  10)
      line &= padLeftAnsi(&"\e[90m{s.jitter():.0f}ms\e[0m", 10)
    else:
      line &= padLeft("-", 10)
      line &= padLeft("-", 10)
      line &= padLeft("-", 10)
      line &= padLeft("-", 10)

    let stab = s.stabilityScore(th)
    if stab >= 0:
      let sc = if stab >= 80: "\e[32m"
               elif stab >= 50: "\e[33m"
               else: "\e[31m"
      line &= padLeftAnsi(sc & $stab & "\e[0m", 6)
    else:
      line &= padLeft("-", 6)

    # Latency bar
    let bar = if s.ringLen > 0: latencyBar(s.avg()) else: "\e[90m─────\e[0m"
    line &= "  " & padRightAnsi(bar, 7)

    line &= padRightAnsi(healthIcon(s.lastHealth), 12)
    line &= padRightAnsi(verdictColor(s.verdict(th)), 12)

    let up = &"{s.uptime():.0f}%"
    if   s.uptime() >= 90: line &= padLeftAnsi("\e[32m" & up & "\e[0m", 7)
    elif s.uptime() >= 50: line &= padLeftAnsi("\e[33m" & up & "\e[0m", 7)
    else:                  line &= padLeftAnsi("\e[31m" & up & "\e[0m", 7)

    line &= rowReset
    echo line

  if showStats.len == 0:
    echo "  \e[90m(no models match filter)\e[0m"

  echo ""

  # ── Footer: two-line key legend ──
  # Line 1: sorting + navigation
  echo "\e[90m  sort:[A]vg [P]95 [S]tab [N]ame [U]ptime | " &
       "[j][k] cursor | [Enter] detail | [1-9] fav | [?] help | [Q]uit\e[0m"

  # Line 2: pagination + filter + count
  let pagePart =
    if pager.enabled:
      let pages = if pager.pageSize > 0: pageCount(visible.len, pager.pageSize) else: 1
      "\e[90m[<]/[>] page " & $( pager.page + 1) & "/" & $pages & " | [T] page-off\e[0m"
    else:
      "\e[90m[T] enable pagination\e[0m"

  let filterPart =
    if filterSt.active:
      "\e[1;33m[/] filter: " & filterSt.query & "_  [Esc] clear\e[0m"
    elif filterSt.query.len > 0:
      "\e[33m[/] filter(" & filterSt.query & ")  [Esc] clear\e[0m"
    else:
      "\e[90m[/] filter\e[0m"

  let countPart = "\e[90m" & $visible.len & " model" &
                  (if visible.len == 1: "" else: "s") & "\e[0m"

  echo "  " & pagePart & " | " & filterPart & " | " & countPart
  if proxyStatus.isSome:
    let ph = proxyStatus.get
    let proxyColor = if ph.status == "running": "\e[32m" else: "\e[33m"
    let keysStr = $ph.activeKeys & " key" & (if ph.activeKeys == 1: "" else: "s")
    let routeStr = if ph.routingEnabled: "routing" else: "no-routing"
    let raceStr  = if ph.racingEnabled:  " racing" else: ""
    echo "  " & proxyColor & "[proxy " & ph.status & "]\e[0m " &
         "\e[90m" & keysStr & " | " & routeStr & raceStr & "\e[0m"
  echo ""

# ── Help overlay ─────────────────────────────────────────────────────────────

proc printHelp*() =
  ## Full-screen help overlay. Caller clears screen before/after.
  let tw = termWidth()
  let divLine = "─".repeat(min(tw - 4, 62))
  echo ""
  echo "\e[1;36m  nimakai v" & Version & " — key bindings\e[0m"
  echo "  " & divLine
  echo ""
  echo "\e[1;90m  SORTING\e[0m"
  echo "  A / P / S / N / U   avg · p95 · stability · name · uptime"
  echo ""
  echo "\e[1;90m  CURSOR\e[0m"
  echo "  j / k               move cursor down / up"
  echo "  Enter               open model detail panel"
  echo ""
  echo "\e[1;90m  PAGINATION\e[0m"
  echo "  T                   toggle pagination on/off"
  echo "  < or [              previous page"
  echo "  > or ]              next page"
  echo ""
  echo "\e[1;90m  FILTER\e[0m"
  echo "  /                   enter filter mode (type to search)"
  echo "  Esc                 clear filter / exit filter mode"
  echo "  Backspace           delete last filter character"
  echo ""
  echo "\e[1;90m  FAVORITES\e[0m"
  echo "  1–9                 toggle favorite on Nth visible model"
  echo "  * prefix            marks favorites (always pinned to top)"
  echo ""
  echo "\e[1;90m  OTHER\e[0m"
  echo "  ?                   show this help screen"
  echo "  Q                   quit"
  echo ""
  echo "  " & divLine
  echo "  \e[90mpress any key to return\e[0m"
  echo ""

# ── Model detail panel ───────────────────────────────────────────────────────

proc printModelDetail*(s: ModelStats, cat: seq[ModelMeta],
                       th: Thresholds = DefaultThresholds) =
  ## Full detail panel for one model. Caller clears screen.
  let tw = termWidth()
  let divLine = "─".repeat(min(tw - 4, 62))
  echo ""
  echo &"\e[1;36m  {s.id}\e[0m"
  echo "  " & divLine
  let meta = cat.lookupMeta(s.id)
  if meta.isSome:
    let m = meta.get
    echo &"  name        : {m.name}"
    echo &"  swe bench   : {m.sweScore}%"
    let ctxStr = if m.ctxSize >= 1048576: $(m.ctxSize div 1048576) & "M"
                 elif m.ctxSize >= 1024:  $(m.ctxSize div 1024) & "k"
                 else: $m.ctxSize
    echo &"  context     : {ctxStr} tokens"
    var caps: seq[string] = @[]
    if m.thinking:   caps.add("thinking")
    if m.multimodal: caps.add("multimodal")
    echo "  capabilities: " & (if caps.len > 0: caps.join(", ") else: "text")
  echo ""
  echo "  \e[1;90mLATENCY\e[0m"
  if s.ringLen > 0:
    echo &"  latest      : {colorLatency(s.lastMs)}"
    echo &"  average     : {colorLatency(s.avg())}"
    echo &"  p50         : {colorLatency(s.p50())}"
    echo &"  p95         : {colorLatency(s.p95())}"
    echo &"  p99         : {colorLatency(s.p99())}"
    echo &"  min / max   : {s.minMs():.0f}ms / {s.maxMs():.0f}ms"
    echo &"  jitter      : {s.jitter():.0f}ms stddev"
    echo  "  bar         : " & latencyBar(s.avg())
  else:
    echo "  \e[90mno samples yet\e[0m"
  echo ""
  echo "  \e[1;90mRELIABILITY\e[0m"
  let stab = s.stabilityScore(th)
  echo "  stability   : " & (if stab >= 0: $stab & "/100" else: "-")
  echo &"  uptime      : {s.uptime():.1f}%"
  echo  "  health      : " & healthIcon(s.lastHealth)
  echo  "  verdict     : " & verdictColor(s.verdict(th))
  echo &"  pings       : {s.successPings}/{s.totalPings} success"
  echo  "  favorite    : " & (if s.favorite: "yes" else: "no")
  echo ""
  echo "  " & divLine
  echo "  \e[90mpress any key to return\e[0m"
  echo ""

# ── JSON output ──────────────────────────────────────────────────────────────

proc printJson*(stats: seq[ModelStats], round: int, cat: seq[ModelMeta],
                th: Thresholds = DefaultThresholds) =
  var results = newJArray()
  for s in stats:
    let meta = cat.lookupMeta(s.id)
    let sweScore = if meta.isSome: meta.get.sweScore else: 0.0
    results.add(%*{
      "model":           s.id,
      "name":            s.name,
      "swe_score":       sweScore,
      "latest_ms":       if s.ringLen > 0: s.lastMs else: 0.0,
      "avg_ms":          s.avg(),
      "p50_ms":          s.p50(),
      "p95_ms":          s.p95(),
      "p99_ms":          s.p99(),
      "min_ms":          s.minMs(),
      "max_ms":          s.maxMs(),
      "jitter_ms":       s.jitter(),
      "spike_rate":      s.spikeRate(th.spikeMs),
      "stability_score": s.stabilityScore(th),
      "health":          $s.lastHealth,
      "verdict":         $s.verdict(th),
      "uptime_pct":      s.uptime(),
      "total_pings":     s.totalPings,
      "success_pings":   s.successPings,
      "favorite":        s.favorite,
    })
  echo $(%*{"round": round, "results": results})
