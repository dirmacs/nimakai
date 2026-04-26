## nimakai — NVIDIA NIM model latency benchmarker
## https://github.com/dirmacs/nimakai

import std/[os, strformat, strutils, times, options, json, sequtils]
import std/posix
import posix/termios as term_mod
import nimakai/[types, catalog, display, config, history, metrics,
                opencode, recommend, rechistory, sync, watch, discovery, cli,
                rustffi, proxyffi, update]

# --- Terminal raw mode for interactive sorting ---

var origTermios: Termios
var rawModeEnabled = false

proc disableRawMode()

proc sigintHandler(sig: cint) {.noconv.} =
  disableRawMode()
  quit(0)

proc enableRawMode() =
  discard tcGetAttr(0.cint, addr origTermios)
  var raw = origTermios
  raw.c_lflag = raw.c_lflag and not (ICANON or ECHO)
  raw.c_cc[VMIN] = '\0'
  raw.c_cc[VTIME] = '\0'
  discard tcSetAttr(0.cint, TCSANOW, addr raw)
  rawModeEnabled = true
  discard signal(SIGINT, sigintHandler)

proc disableRawMode() =
  if rawModeEnabled:
    discard tcSetAttr(0.cint, TCSANOW, addr origTermios)
    rawModeEnabled = false

proc tryReadKey(): char =
  var buf: array[1, char]
  let n = read(0.cint, addr buf[0], 1)
  if n > 0: buf[0] else: '\0'

# --- Main ---

proc editDistance(a, b: string): int =
  ## Levenshtein edit distance for fuzzy matching.
  let m = a.len
  let n = b.len
  var d = newSeq[seq[int]](m + 1)
  for i in 0..m:
    d[i] = newSeq[int](n + 1)
    d[i][0] = i
  for j in 0..n:
    d[0][j] = j
  for i in 1..m:
    for j in 1..n:
      let cost = if a[i-1] == b[j-1]: 0 else: 1
      d[i][j] = min(d[i-1][j] + 1, min(d[i][j-1] + 1, d[i-1][j-1] + cost))
  d[m][n]

proc validateModels(models: seq[string], cat: seq[ModelMeta]) =
  ## Warn about model IDs not found in catalog, suggest closest matches.
  for m in models:
    var found = false
    for c in cat:
      if c.id == m:
        found = true
        break
    if not found:
      var bestDist = int.high
      var bestMatch = ""
      let mLower = m.toLowerAscii()
      for c in cat:
        let d = editDistance(mLower, c.id.toLowerAscii())
        if d < bestDist:
          bestDist = d
          bestMatch = c.id
      var msg = &"\e[33mWarning: '{m}' not found in catalog"
      if bestMatch.len > 0 and bestDist <= m.len div 2:
        msg &= &" — did you mean '{bestMatch}'?"
      msg &= "\e[0m"
      stderr.writeLine msg

proc computePageSize(): int =
  ## Rows available for model list: terminal height minus header/footer lines.
  max(5, termHeight() - 8)

proc runBenchmark(cfg: Config, cat: seq[ModelMeta], favorites: seq[string]) =
  var stats: seq[ModelStats] = @[]
  for m in cfg.models:
    let meta = cat.lookupMeta(m)
    let name = if meta.isSome: meta.get.name else: m
    var s = ModelStats(id: m, name: name, lastHealth: hPending)
    if m in favorites: s.favorite = true
    stats.add(s)

  var sortCol   = cfg.sortColumn
  var round     = 0
  var pager     = cfg.pagination
  var filterSt  = cfg.filter
  var cursorRow = 0  # 0-indexed cursor in the current visible+paged view
  let interactive = not cfg.once and not cfg.jsonOutput and isatty(0.cint) != 0

  # State machine modes
  var showHelp   = false
  var showDetail = false
  var detailIdx  = -1  # absolute index in stats[] for detail view

  if interactive:
    enableRawMode()
    pager.pageSize = computePageSize()

  try:
    while true:
      inc round

      # Rust-accelerated concurrent ping
      var modelIds: seq[string] = @[]
      for s in stats: modelIds.add(s.id)
      let pingResults = rustPingBatch(cfg.apiKey, modelIds, cfg.timeout)

      for i in 0..<min(pingResults.len, stats.len):
        let pr = pingResults[i]
        stats[i].totalPings += 1
        stats[i].lastMs     = pr.ms
        stats[i].lastHealth = pr.health
        if pr.health == hUp:
          stats[i].successPings += 1
          stats[i].addSample(pr.ms)

      # Sort before display
      sortStats(stats, sortCol, cat, cfg.thresholds)

      if cfg.jsonOutput:
        printJson(stats, round, cat, cfg.thresholds)
      else:
        stdout.write "\e[2J\e[H"
        if showHelp:
          printHelp()
        elif showDetail and detailIdx >= 0 and detailIdx < stats.len:
          printModelDetail(stats[detailIdx], cat, cfg.thresholds)
        else:
          # Compute page size each render (handles terminal resize)
          if interactive:
            pager.pageSize = computePageSize()
          # Clamp cursor to visible list bounds
          let visible = filterStats(stats, filterSt.query)
          let visLen = visible.len
          if visLen > 0:
            cursorRow = max(0, min(cursorRow, visLen - 1))
          # Clamp page
          if pager.enabled and pager.pageSize > 0:
            pager.page = clampPage(pager.page, visLen, pager.pageSize)
          printTable(stats, round, cat, sortCol, cfg.thresholds,
                     pager, filterSt, if interactive: cursorRow else: -1)

      # Persist to history
      if not cfg.noHistory:
        appendRound(stats, round)

      if cfg.once:
        break

      # Wait for interval, processing interactive input
      let deadline = epochTime() + cfg.interval.float
      while epochTime() < deadline:
        if interactive:
          let key = tryReadKey()
          if key == '\0':
            sleep(30)
          else:
            # Dismiss overlays first
            if showHelp or showDetail:
              showHelp   = false
              showDetail = false
              detailIdx  = -1
              # Force immediate redraw
              let deadline2 = epochTime() + 0.05
              while epochTime() < deadline2: sleep(10)
              break

            # Filter mode: capture printable chars
            if filterSt.active:
              if key == '\e':
                filterSt.active = false
                filterSt.query  = ""
                cursorRow = 0
              elif key == '\x7f' or key == '\b':  # backspace
                if filterSt.query.len > 0:
                  filterSt.query = filterSt.query[0..^2]
                  cursorRow = 0
              elif key >= ' ' and key <= '~':  # printable ASCII
                filterSt.query &= key
                cursorRow = 0
              break

            # Normal mode key dispatch
            case key
            of 'a', 'A': sortCol = scAvg
            of 'p', 'P': sortCol = scP95
            of 's', 'S': sortCol = scStability
            of 'n', 'N': sortCol = scName
            of 'u', 'U': sortCol = scUptime
            of 'j':
              # cursor down
              let vis = filterStats(stats, filterSt.query)
              cursorRow = min(cursorRow + 1, max(0, vis.len - 1))
              # If paginated, advance page when cursor exits page
              if pager.enabled and pager.pageSize > 0:
                let (s, e) = pageSlice(vis.len, pager.page, pager.pageSize)
                if cursorRow >= e and pager.page < pageCount(vis.len, pager.pageSize) - 1:
                  inc pager.page
                  cursorRow = e
            of 'k':
              # cursor up
              cursorRow = max(cursorRow - 1, 0)
              if pager.enabled and pager.pageSize > 0:
                let (s, _) = pageSlice(filterStats(stats, filterSt.query).len, pager.page, pager.pageSize)
                if cursorRow < s and pager.page > 0:
                  dec pager.page
                  cursorRow = s - 1
            of '\r', '\n':  # Enter — detail view
              let vis = filterStats(stats, filterSt.query)
              if pager.enabled and pager.pageSize > 0:
                let (s, _) = pageSlice(vis.len, pager.page, pager.pageSize)
                let absRow = s + cursorRow
                if absRow < vis.len:
                  # Find index in stats
                  for si, st in stats:
                    if st.id == vis[absRow].id:
                      detailIdx = si
                      break
              else:
                if cursorRow < vis.len:
                  for si, st in stats:
                    if st.id == vis[cursorRow].id:
                      detailIdx = si
                      break
              if detailIdx >= 0:
                showDetail = true
            of 't', 'T':
              pager.enabled = not pager.enabled
              if pager.enabled:
                pager.pageSize = computePageSize()
                pager.page = 0
              cursorRow = 0
            of '<', '[':  # prev page
              if pager.enabled and pager.page > 0:
                dec pager.page
                cursorRow = 0
            of '>', ']':  # next page
              if pager.enabled:
                let vis = filterStats(stats, filterSt.query)
                let maxPage = pageCount(vis.len, pager.pageSize) - 1
                if pager.page < maxPage:
                  inc pager.page
                  cursorRow = 0
            of '/':
              filterSt.active = true
            of '\e':  # Esc: clear filter
              filterSt.active = false
              filterSt.query  = ""
              cursorRow = 0
            of '?':
              showHelp = true
            of '1'..'9':
              let vis = filterStats(stats, filterSt.query)
              let idx = ord(key) - ord('1')
              let absIdx =
                if pager.enabled and pager.pageSize > 0:
                  let (s, _) = pageSlice(vis.len, pager.page, pager.pageSize)
                  s + idx
                else:
                  idx
              if absIdx < vis.len:
                # Find in stats and toggle
                for si, st in stats:
                  if st.id == vis[absIdx].id:
                    stats[si].favorite = not stats[si].favorite
                    break
                var favs: seq[string] = @[]
                for st in stats:
                  if st.favorite: favs.add(st.id)
                saveFavorites("", favs)
            of 'q', 'Q':
              disableRawMode()
              quit(0)
            else: discard
            break  # redraw after any key
        else:
          sleep(50)
  finally:
    disableRawMode()
proc runRecommend(cfg: Config, cat: seq[ModelMeta]) =
  if cfg.rollback:
    discard rollbackOmo()
    return

  if cfg.recHistory:
    printRecHistory()
    return

  if cfg.apiKey.len == 0:
    stderr.writeLine "\e[31mError: NVIDIA_API_KEY required for benchmarking\e[0m"
    quit(1)

  # Determine models to benchmark from OMO config
  let omo = parseOmoConfig()
  var modelSet: seq[string] = @[]
  for c in omo.categories:
    if c.model notin modelSet:
      modelSet.add(c.model)
  # Include agent models
  for a in omo.agents:
    if a.model notin modelSet:
      modelSet.add(a.model)
  # Also benchmark all catalog models that could be alternatives
  for m in cat:
    if m.id notin modelSet:
      modelSet.add(m.id)

  if not cfg.jsonOutput and not cfg.quiet:
    stderr.writeLine &"\e[1m nimakai\e[0m v{Version}"
    stderr.writeLine &"\e[90m  recommend mode | {cfg.rounds} rounds | {modelSet.len} models\e[0m"

  var stats: seq[ModelStats] = @[]
  for m in modelSet:
    let meta = cat.lookupMeta(m)
    let name = if meta.isSome: meta.get.name else: m
    stats.add(ModelStats(id: m, name: name, lastHealth: hPending))

  # Run benchmark rounds
  for round in 1..cfg.rounds:
    if not cfg.jsonOutput and not cfg.quiet:
      stderr.write &"\r\e[90m  round {round}/{cfg.rounds}...\e[0m"

    # Rust-accelerated concurrent ping
    var modelIds: seq[string] = @[]
    for s in stats: modelIds.add(s.id)
    let results = rustPingBatch(cfg.apiKey, modelIds, cfg.timeout)

    for i in 0..<min(results.len, stats.len):
      let pr = results[i]
      stats[i].totalPings += 1
      stats[i].lastMs = pr.ms
      stats[i].lastHealth = pr.health
      if pr.health == hUp:
        stats[i].successPings += 1
        stats[i].addSample(pr.ms)

    if round < cfg.rounds:
      sleep(2000) # brief pause between rounds

  if not cfg.jsonOutput and not cfg.quiet:
    stderr.writeLine "\r\e[90m  benchmarking complete.     \e[0m"

  let recs = recommend(stats, cat, omo, cfg.thresholds, cfg.categoryWeights)
  let agentRecs = recommendAgents(stats, cat, omo, cfg.thresholds, cfg.categoryWeights)

  if cfg.jsonOutput:
    var j = recommendationsToJson(recs)
    let aj = agentRecommendationsToJson(agentRecs)
    j["agent_recommendations"] = aj["agent_recommendations"]
    echo $j
  elif cfg.applySync and not cfg.dryRun:
    printRecommendations(recs, cfg.rounds)
    printAgentRecommendations(agentRecs, cfg.rounds)
    discard syncRecommendations(recs)
  else:
    printRecommendations(recs, cfg.rounds)
    printAgentRecommendations(agentRecs, cfg.rounds)
    if cfg.dryRun and cfg.applySync:
      stderr.writeLine "\e[90m  (dry-run: changes not applied)\e[0m"

  # Persist to recommendation history
  let applied = cfg.applySync and not cfg.dryRun
  appendRecHistory(recs, agentRecs, cfg.rounds, applied)

proc runWatch(cfg: Config, cat: seq[ModelMeta]) =
  if cfg.apiKey.len == 0:
    stderr.writeLine "\e[31mError: NVIDIA_API_KEY required\e[0m"
    quit(1)

  let omo = parseOmoConfig()
  var modelSet: seq[string] = @[]
  for c in omo.categories:
    if c.model notin modelSet:
      modelSet.add(c.model)
  for a in omo.agents:
    if a.model notin modelSet:
      modelSet.add(a.model)

  if modelSet.len == 0:
    stderr.writeLine "\e[31mError: no OMO-routed models found\e[0m"
    quit(1)

  if not cfg.quiet:
    stderr.writeLine &"\e[1m nimakai\e[0m v{Version}"
    stderr.writeLine &"\e[90m  watch mode | {modelSet.len} OMO models | {cfg.interval}s interval\e[0m"

  var stats: seq[ModelStats] = @[]
  for m in modelSet:
    let meta = cat.lookupMeta(m)
    let name = if meta.isSome: meta.get.name else: m
    stats.add(ModelStats(id: m, name: name, lastHealth: hPending))

  var prevStats: seq[ModelStats] = @[]
  var round = 0
  let stabThreshold = if cfg.alertThreshold > 0: cfg.alertThreshold else: 50.0

  while true:
    inc round
    prevStats = stats

    # Rust-accelerated concurrent ping
    var watchModelIds: seq[string] = @[]
    for s in stats: watchModelIds.add(s.id)
    let results = rustPingBatch(cfg.apiKey, watchModelIds, cfg.timeout)

    for i in 0..<min(results.len, stats.len):
      let pr = results[i]
      stats[i].totalPings += 1
      stats[i].lastMs = pr.ms
      stats[i].lastHealth = pr.health
      if pr.health == hUp:
        stats[i].successPings += 1
        stats[i].addSample(pr.ms)

    sortStats(stats, cfg.sortColumn, cat, cfg.thresholds)

    if not cfg.jsonOutput:
      if round > 1:
        stdout.write "\e[2J\e[H"
      printTable(stats, round, cat, cfg.sortColumn, cfg.thresholds)

    # Check alerts
    if round > 1:
      let alerts = checkAlerts(stats, prevStats, cfg.thresholds, stabThreshold)
      for alert in alerts:
        printAlert(alert)

    if not cfg.noHistory:
      appendRound(stats, round)

    if cfg.once:
      break

    sleep(cfg.interval * 1000)

proc runCheck(cfg: Config, cat: seq[ModelMeta]) =
  if cfg.apiKey.len == 0:
    stderr.writeLine "\e[31mError: NVIDIA_API_KEY required\e[0m"
    quit(1)

  let omo = parseOmoConfig()
  var modelSet: seq[string] = @[]
  for c in omo.categories:
    if c.model notin modelSet:
      modelSet.add(c.model)
  for a in omo.agents:
    if a.model notin modelSet:
      modelSet.add(a.model)

  if modelSet.len == 0:
    stderr.writeLine "\e[31mError: no OMO-routed models found\e[0m"
    quit(1)

  if not cfg.quiet:
    stderr.writeLine &"\e[1m nimakai\e[0m v{Version}"
    stderr.writeLine &"\e[90m  check mode | {cfg.rounds} rounds | {modelSet.len} models\e[0m"

  var stats: seq[ModelStats] = @[]
  for m in modelSet:
    let meta = cat.lookupMeta(m)
    let name = if meta.isSome: meta.get.name else: m
    stats.add(ModelStats(id: m, name: name, lastHealth: hPending))

  for round in 1..cfg.rounds:
    if not cfg.quiet:
      stderr.write &"\r\e[90m  round {round}/{cfg.rounds}...\e[0m"

    # Rust-accelerated concurrent ping
    var checkModelIds: seq[string] = @[]
    for s in stats: checkModelIds.add(s.id)
    let results = rustPingBatch(cfg.apiKey, checkModelIds, cfg.timeout)

    for i in 0..<min(results.len, stats.len):
      let pr = results[i]
      stats[i].totalPings += 1
      stats[i].lastMs = pr.ms
      stats[i].lastHealth = pr.health
      if pr.health == hUp:
        stats[i].successPings += 1
        stats[i].addSample(pr.ms)

    if round < cfg.rounds:
      sleep(2000)

  if not cfg.quiet:
    stderr.writeLine "\r\e[90m  check complete.         \e[0m"

  # Build JSON summary
  var degraded = false
  var results = newJArray()
  for s in stats:
    let up = s.uptime()
    let stab = s.stabilityScore(cfg.thresholds)
    let isDegraded = s.lastHealth != hUp or up < 50 or (stab >= 0 and stab < 50)
    if isDegraded: degraded = true
    results.add(%*{
      "model": s.id,
      "health": $s.lastHealth,
      "avg_ms": s.avg(),
      "uptime_pct": up,
      "stability": stab,
      "degraded": isDegraded,
    })

  let output = %*{
    "status": if degraded: "degraded" else: "healthy",
    "rounds": cfg.rounds,
    "models": results,
  }
  echo $output

  if degraded and cfg.failIfDegraded:
    quit(1)

proc runFetch(cfg: Config, cat: seq[ModelMeta]) =
  if cfg.apiKey.len == 0:
    stderr.writeLine "\e[31mError: NVIDIA_API_KEY required\e[0m"
    quit(1)

  if not cfg.quiet:
    stderr.writeLine "\e[1m nimakai\e[0m v{Version}"
    stderr.writeLine "\e[90m  fetch mode | querying NVIDIA API for latest models\e[0m"
    stderr.writeLine ""

  # Check API key format
  if not cfg.apiKey.startsWith("nvapi-"):
    if not cfg.quiet:
      stderr.writeLine "\e[33mWarning: API key should start with 'nvapi-'\e[0m"

  let result = fetchModelsFromAPI(cfg.apiKey, cfg.timeout)
  
  if result.apiError.len > 0:
    stderr.writeLine "\e[31mError fetching models: " & result.apiError & "\e[0m"
    quit(1)
  
  # Update user models.json with new models
  let numAdded = updateUserModels(result.newModels)
  
  # Print results
  printFetchResults(result, cfg.jsonOutput)
  
  if numAdded > 0 and not cfg.jsonOutput:
    stderr.writeLine "\e[32m✓ Added " & $numAdded & " new models to ~/.config/nimakai/models.json\e[0m"
    stderr.writeLine ""
    stderr.writeLine "Next steps:"
    stderr.writeLine "  - Review new models in your configuration"
    stderr.writeLine "  - Run 'nimakai catalog' to see all available models"
    stderr.writeLine "  - Use '--models' flag to include new models in benchmarks"

proc runProxy(cfg: Config) =
  case cfg.proxyAction
  of paStart:
    if cfg.proxyConfigPath.len == 0:
      stderr.writeLine "\e[31mError: --proxy-config <path> required for proxy start\e[0m"
      quit(1)
    if not fileExists(cfg.proxyConfigPath):
      stderr.writeLine &"\e[31mError: config file not found: {cfg.proxyConfigPath}\e[0m"
      quit(1)
    let port = if cfg.proxyPort > 0: cfg.proxyPort else: 0
    let ret = proxyStart(cfg.proxyConfigPath, port)
    if ret == 0:
      let portStr = if cfg.proxyPort > 0: $cfg.proxyPort else: "config default"
      stdout.writeLine &"nimaproxy started (config: {cfg.proxyConfigPath}, port: {portStr})"
    else:
      stderr.writeLine &"\e[31mError: proxy_start returned {ret} (already running? bad config?)\e[0m"
      quit(1)

  of paStop:
    let ret = proxyStop()
    if ret == 0:
      stdout.writeLine "nimaproxy stopped"
    else:
      stderr.writeLine "\e[33mNote: nimaproxy was not running\e[0m"

  of paStatus:
    let healthOpt = proxyHealth()
    let statsOpt = proxyStats()
    if cfg.jsonOutput:
      var j = newJObject()
      if healthOpt.isSome:
        let h = healthOpt.get
        j["health"] = %*{
          "status": h.status,
          "active_keys": h.activeKeys,
          "routing_enabled": h.routingEnabled,
          "racing_enabled": h.racingEnabled,
        }
      if statsOpt.isSome:
        let s = statsOpt.get
        j["stats"] = %*{
          "models": s.models.mapIt(%*{
            "model": it.model,
            "avg_ms": it.avgMs,
            "p95_ms": it.p95Ms,
            "total": it.total,
            "success": it.success,
            "success_rate": it.successRate,
            "degraded": it.degraded,
          }),
          "keys": s.keys.mapIt(%*{
            "label": it.label,
            "key_hint": it.keyHint,
            "active": it.active,
            "cooldown_secs_remaining": it.cooldownSecsRemaining,
          }),
          "racing_models": s.racingModels,
        }
      echo $j
    else:
      if healthOpt.isNone:
        stdout.writeLine "\e[33mnimaproxy is not running\e[0m"
        return
      let h = healthOpt.get
      echo ""
      echo &"\e[1m  nimaproxy status\e[0m"
      echo &"  status           : \e[92m{h.status}\e[0m"
      echo &"  active keys      : {h.activeKeys}"
      echo &"  routing enabled  : {h.routingEnabled}"
      echo &"  racing enabled   : {h.racingEnabled}"
      if statsOpt.isSome:
        let s = statsOpt.get
        echo ""
        if s.models.len > 0:
          echo &"  \e[1mmodel latency stats ({s.models.len} tracked)\e[0m"
          for m in s.models:
            let deg = if m.degraded: &" \e[31m[DEGRADED]\e[0m" else: ""
            let avg = if m.avgMs > 0: &"{m.avgMs:.0f}ms avg" else: "no samples"
            echo &"    {m.model:<50} {avg:>15}{deg}"
        if s.keys.len > 0:
          echo ""
          echo &"  \e[1mkey pool ({s.keys.len} keys)\e[0m"
          for k in s.keys:
            let act = if k.active: "\e[92mactive\e[0m" else: &"\e[33mcooldown {k.cooldownSecsRemaining}s\e[0m"
            echo &"    {k.label:<15} {act:>20}  hint={k.keyHint}"
      echo ""

proc main() =
  let cfg = parseArgs()
  let cat = loadCatalog()

  case cfg.subcommand
  of smCatalog:
    var filtered = cat
    if cfg.jsonOutput:
      printCatalogJson(filtered)
    else:
      printCatalog(filtered)
    return

  of smHistory:
    printHistory(cfg.days)
    return

  of smTrends:
    printTrends(cfg.days)
    return

  of smOpencode:
    let models = parseOpenCodeConfig()
    printOpenCodeModels(models)
    let omo = parseOmoConfig()
    printOmoRouting(omo)
    return

  of smRecommend:
    runRecommend(cfg, cat)
    return

  of smWatch:
    runWatch(cfg, cat)
    return

  of smCheck:
    runCheck(cfg, cat)
    return

  of smDiscover:
    if cfg.apiKey.len == 0:
      stderr.writeLine "\e[31mError: NVIDIA_API_KEY required\e[0m"
      quit(1)
    let rawJson = rustDiscoverModels(cfg.apiKey, cfg.timeout)
    let discovered = parseDiscoverResponse(rawJson)
    if cfg.jsonOutput:
      echo discoveryToJson(discovered, cat)
    else:
      printDiscovery(discovered, cat)
    return

  of smFetch:
    runFetch(cfg, cat)
    return

  of smProxy:
    runProxy(cfg)
    return

  of smBenchmark:
    if cfg.apiKey.len == 0:
      stderr.writeLine "\e[31mError: NVIDIA_API_KEY environment variable not set\e[0m"
      stderr.writeLine "Get your key at https://build.nvidia.com"
      quit(1)

    # Determine model list
    var models = cfg.models
    if cfg.useOpencode:
      let ocModels = parseOpenCodeConfig()
      models = @[]
      for m in ocModels:
        models.add(m.id)
    if models.len == 0:
      # Default: fetch model list from NVIDIA API
      if cfg.apiKey.len == 0:
        stderr.writeLine "\e[31mError: NVIDIA_API_KEY required for default model discovery\e[0m"
        stderr.writeLine "Set NVIDIA_API_KEY or use --models to specify models"
        quit(1)
      let fetchResult = fetchModelsFromAPI(cfg.apiKey, cfg.timeout)
      if fetchResult.apiError.len > 0:
        stderr.writeLine "\e[31mError fetching models: " & fetchResult.apiError & "\e[0m"
        quit(1)
      # Use all fetched model IDs as the default list
      models = fetchResult.allModels

    if models.len == 0:
      stderr.writeLine "\e[31mError: no models matched the filter\e[0m"
      stderr.writeLine "Use --models or --opencode to specify models"
      quit(1)

    if not cfg.quiet:
      validateModels(models, cat)

    var runCfg = cfg
    runCfg.models = models

    if not cfg.jsonOutput and not cfg.quiet:
      stderr.writeLine &"\e[1m nimakai\e[0m v{Version}"
      stderr.writeLine &"\e[90m  {models.len} models | {cfg.interval}s interval | {cfg.timeout}s timeout | concurrent pings\e[0m"

    # Prune old history on startup
    pruneHistory()

    let fileCfg = loadConfigFile()
    runBenchmark(runCfg, cat, fileCfg.favorites)

main()
