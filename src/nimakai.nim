## nimakai — NVIDIA NIM model latency benchmarker
## https://github.com/bkataru/nimakai

import std/[os, strformat, strutils, times, options, json]
import std/posix
import posix/termios as term_mod
import malebolgia
import nimakai/[types, ping, catalog, display, config, history,
                opencode, recommend, sync, cli]

# --- Terminal raw mode for interactive sorting ---

var origTermios: Termios
var rawModeEnabled = false

proc enableRawMode() =
  discard tcGetAttr(0.cint, addr origTermios)
  var raw = origTermios
  raw.c_lflag = raw.c_lflag and not (ICANON or ECHO)
  raw.c_cc[VMIN] = '\0'
  raw.c_cc[VTIME] = '\0'
  discard tcSetAttr(0.cint, TCSANOW, addr raw)
  rawModeEnabled = true

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

proc runBenchmark(cfg: Config, cat: seq[ModelMeta], favorites: seq[string]) =
  var stats: seq[ModelStats] = @[]
  for m in cfg.models:
    let meta = cat.lookupMeta(m)
    let name = if meta.isSome: meta.get.name else: m
    var s = ModelStats(id: m, name: name, lastHealth: hPending)
    if m in favorites: s.favorite = true
    stats.add(s)

  var sortCol = cfg.sortColumn
  var round = 0
  let interactive = not cfg.once and not cfg.jsonOutput and isatty(0.cint) != 0

  if interactive:
    enableRawMode()

  try:
    while true:
      inc round

      var results = newSeq[PingResult](stats.len)
      for i in 0..<results.len:
        results[i] = PingResult(health: hTimeout, ms: float(cfg.timeout * 1000))

      try:
        var m = createMaster(timeout = initDuration(seconds = cfg.timeout + 5))
        m.awaitAll:
          for i in 0..<stats.len:
            m.spawn doPing(cfg.apiKey, stats[i].id, cfg.timeout) -> results[i]
      except CatchableError as e:
        if not cfg.jsonOutput and not cfg.quiet:
          stderr.writeLine &"\e[33mWarning: ping pool error: {e.msg}\e[0m"

      for i in 0..<stats.len:
        let pr = results[i]
        stats[i].totalPings += 1
        stats[i].lastMs = pr.ms
        stats[i].lastHealth = pr.health
        if pr.health == hUp:
          stats[i].successPings += 1
          stats[i].addSample(pr.ms)

      # Sort before display
      sortStats(stats, sortCol, cat, cfg.thresholds)

      if cfg.jsonOutput:
        printJson(stats, round, cat, cfg.thresholds)
      else:
        if round > 1 or not cfg.once:
          stdout.write "\e[2J\e[H"
        printTable(stats, round, cat, sortCol, cfg.thresholds)

      # Persist to history
      if not cfg.noHistory:
        appendRound(stats, round)

      if cfg.once:
        break

      # Wait for interval, checking for interactive input
      let deadline = epochTime() + cfg.interval.float
      while epochTime() < deadline:
        if interactive:
          let key = tryReadKey()
          case key
          of 'a', 'A': sortCol = scAvg
          of 'p', 'P': sortCol = scP95
          of 's', 'S': sortCol = scStability
          of 't', 'T': sortCol = scTier
          of 'n', 'N': sortCol = scName
          of 'u', 'U': sortCol = scUptime
          of '1'..'9':
            let idx = ord(key) - ord('1')
            if idx < stats.len:
              stats[idx].favorite = not stats[idx].favorite
              # Persist favorites
              var favs: seq[string] = @[]
              for s in stats:
                if s.favorite: favs.add(s.id)
              saveFavorites("", favs)
          of 'q', 'Q':
            disableRawMode()
            quit(0)
          else: discard
        sleep(50)
  finally:
    disableRawMode()

proc runRecommend(cfg: Config, cat: seq[ModelMeta]) =
  if cfg.rollback:
    discard rollbackOmo()
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

    var results = newSeq[PingResult](stats.len)
    for i in 0..<results.len:
      results[i] = PingResult(health: hTimeout, ms: float(cfg.timeout * 1000))

    try:
      var m = createMaster(timeout = initDuration(seconds = cfg.timeout + 5))
      m.awaitAll:
        for i in 0..<stats.len:
          m.spawn doPing(cfg.apiKey, stats[i].id, cfg.timeout) -> results[i]
    except CatchableError as e:
      if not cfg.jsonOutput and not cfg.quiet:
        stderr.writeLine &"\e[33mWarning: ping pool error: {e.msg}\e[0m"

    for i in 0..<stats.len:
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

  if cfg.jsonOutput:
    echo $recommendationsToJson(recs)
  elif cfg.applySync and not cfg.dryRun:
    printRecommendations(recs, cfg.rounds)
    discard syncRecommendations(recs)
  else:
    printRecommendations(recs, cfg.rounds)
    if cfg.dryRun and cfg.applySync:
      stderr.writeLine "\e[90m  (dry-run: changes not applied)\e[0m"

proc main() =
  let cfg = parseArgs()
  let cat = loadCatalog()

  case cfg.subcommand
  of smCatalog:
    var filtered = cat
    if cfg.tierFilter.len > 0:
      filtered = filterByTier(cat, cfg.tierFilter)
    printCatalog(filtered)
    return

  of smHistory:
    printHistory()
    return

  of smTrends:
    printTrends()
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
      # Default: use catalog models filtered by tier
      if cfg.tierFilter.len > 0:
        let filtered = filterByTier(cat, cfg.tierFilter)
        models = catalogModelIds(filtered)
      else:
        # Default subset: S+ and S tier only
        let filtered = filterByTier(cat, "S")
        models = catalogModelIds(filtered)

    if models.len == 0:
      stderr.writeLine "\e[31mError: no models matched the filter\e[0m"
      stderr.writeLine "Use --models, --tier, or --opencode to specify models"
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
