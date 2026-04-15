## CLI argument parsing for nimakai.

import std/[os, strutils, strformat, parseopt]
import types, config

proc parseArgs*(params: seq[string]): Config =
  result = Config(
    models: @[],
    once: false,
    interval: DefaultInterval,
    timeout: DefaultTimeout,
    jsonOutput: false,
    quiet: false,
    noHistory: false,
    dryRun: false,
    apiKey: "",
    subcommand: smBenchmark,
    tierFilter: "",
    sortColumn: scAvg,
    useOpencode: false,
    rounds: 3,
    applySync: false,
    rollback: false,
    days: 7,
    thresholds: DefaultThresholds,
    proxyAction: paStart,
    proxyConfigPath: "",
    proxyPort: 0,
  )

  result.apiKey = getEnv("NVIDIA_API_KEY", "")

  # Load config file defaults
  let fileCfg = loadConfigFile()
  if fileCfg.interval != DefaultInterval: result.interval = fileCfg.interval
  if fileCfg.timeout != DefaultTimeout: result.timeout = fileCfg.timeout
  if fileCfg.models.len > 0: result.models = fileCfg.models
  if fileCfg.tierFilter.len > 0: result.tierFilter = fileCfg.tierFilter
  result.thresholds = fileCfg.thresholds
  result.categoryWeights = fileCfg.categoryWeights

  # Scan all non-flag arguments for subcommands
  for arg in params:
    if arg.startsWith("-"): continue
    case arg
    of "catalog": result.subcommand = smCatalog
    of "recommend": result.subcommand = smRecommend
    of "history": result.subcommand = smHistory
    of "trends": result.subcommand = smTrends
    of "opencode": result.subcommand = smOpencode
    of "watch": result.subcommand = smWatch
    of "check": result.subcommand = smCheck
    of "discover": result.subcommand = smDiscover
    of "proxy":
      result.subcommand = smProxy
      # Parse the proxy action (start/stop/status) — must be the next non-flag arg
      let args = params
      for i, arg in args:
        if arg == "proxy" and i + 1 < args.len:
          let action = args[i + 1]
          if action == "start": result.proxyAction = paStart
          elif action == "stop": result.proxyAction = paStop
          elif action == "status": result.proxyAction = paStatus
          break
    else: discard

  const shortNoVal = {'1', 'j', 'q'}
  const longNoVal = @["once", "json", "quiet", "no-history", "dry-run",
                       "apply", "rollback", "opencode", "help", "version",
                       "rec-history", "throughput", "fail-if-degraded"]
  var cliSetInterval, cliSetTimeout, cliSetTierFilter, cliSetRounds = false
  var p = initOptParser(params, shortNoVal = shortNoVal, longNoVal = longNoVal,
                        mode = LaxMode)
  while true:
    p.next()
    case p.kind
    of cmdEnd: break
    of cmdShortOption, cmdLongOption:
      case p.key
      of "once", "1": result.once = true
      of "json", "j": result.jsonOutput = true
      of "models", "m":
        result.models = @[]
        for m in p.val.split(','):
          let trimmed = m.strip()
          if trimmed.len > 0: result.models.add(trimmed)
      of "interval", "i":
        try:
          result.interval = parseInt(p.val)
          cliSetInterval = true
        except ValueError: discard
      of "timeout", "t":
        try:
          result.timeout = parseInt(p.val)
          cliSetTimeout = true
        except ValueError: discard
      of "tier":
        result.tierFilter = p.val
        cliSetTierFilter = true
      of "sort":
        case p.val.toLowerAscii()
        of "avg", "a": result.sortColumn = scAvg
        of "p95", "p": result.sortColumn = scP95
        of "stability", "s": result.sortColumn = scStability
        of "tier", "t": result.sortColumn = scTier
        of "name", "n": result.sortColumn = scName
        of "uptime", "u": result.sortColumn = scUptime
        else: discard
      of "opencode": result.useOpencode = true
      of "rounds", "r":
        try:
          result.rounds = parseInt(p.val)
          cliSetRounds = true
        except ValueError: discard
      of "quiet", "q": result.quiet = true
      of "no-history": result.noHistory = true
      of "dry-run": result.dryRun = true
      of "apply": result.applySync = true
      of "rollback": result.rollback = true
      of "rec-history": result.recHistory = true
      of "throughput": result.throughput = true
      of "fail-if-degraded": result.failIfDegraded = true
      of "proxy-config":
        result.proxyConfigPath = p.val
      of "proxy-port":
        try: result.proxyPort = parseInt(p.val)
        except ValueError: discard
      of "alert-threshold":
        try: result.alertThreshold = parseFloat(p.val)
        except ValueError: discard
      of "days", "d":
        try: result.days = parseInt(p.val)
        except ValueError: discard
      of "profile":
        result.profile = p.val
      of "help", "h":
        echo &"""
nimakai v{Version} - NVIDIA NIM latency benchmarker

Usage: nimakai [command] [options]

Commands:
  (default)              Continuous benchmark
  catalog                List all known models with metadata
  recommend              Benchmark and recommend routing changes
  watch                  Monitor OMO-routed models with alerts
  check                  CI health check with exit codes
  discover               Compare API models against catalog
  history                Show historical benchmark data
  trends                 Show latency trend analysis
  opencode               Show models from opencode.json
  proxy                  Manage embedded nimaproxy (start|stop|status)

Proxy Options:
  --proxy-config <path>  Path to nimaproxy.toml config (required for start)
  --proxy-port <port>    Override listen port

Proxy Examples:
  nimakai proxy start --proxy-config nimaproxy.toml
  nimakai proxy stop
  nimakai proxy status

Options:
  --once, -1             Single round, then exit
  --models, -m <list>    Comma-separated model IDs
  --interval, -i <sec>   Ping interval (default: {DefaultInterval}s)
  --timeout, -t <sec>    Request timeout (default: {DefaultTimeout}s)
  --json, -j             Output JSON
  --tier <S|A|B|C>       Filter models by tier family
  --sort <col>           Sort: avg, p95, stability, tier, name, uptime
  --opencode             Use models from opencode.json
  --rounds, -r <n>       Benchmark rounds for recommend (default: 3)
  --apply                Apply recommendations to oh-my-opencode.json
  --rollback             Rollback oh-my-opencode.json from backup
  --quiet, -q            Suppress stderr status messages
  --no-history           Don't write to history file
  --dry-run              Preview recommend changes without applying
  --rec-history          Show recommendation history
  --throughput           Measure output token throughput
  --alert-threshold <n>  Alert threshold for watch mode (default: 50)
  --fail-if-degraded     Exit 1 if any model is degraded (check mode)
  --days, -d <n>         Days of history to show (default: 7)
  --profile <name>       Load named profile from config
  --help, -h             Show this help
  --version, -v          Show version

Interactive keys (continuous mode):
  A/P/S/T/N/U            Sort by avg/p95/stability/tier/name/uptime
  1-9                    Toggle favorite on Nth model
  Q                      Quit

Environment:
  NVIDIA_API_KEY         API key for NVIDIA NIM

Examples:
  nimakai --once
  nimakai catalog --tier S
  nimakai -m qwen/qwen3.5-122b-a10b,qwen/qwen3.5-397b-a17b
  nimakai recommend --rounds 5 --apply
  nimakai --opencode --json
"""
        quit(0)
      of "version", "v":
        echo &"nimakai v{Version} ({GitCommit}, {BuildDate})"
        quit(0)
      else:
        stderr.writeLine &"Unknown option: {p.key}"
        quit(1)
    of cmdArgument:
      # Subcommand args already handled above
      discard

  # Apply profile overrides after CLI parsing.
  # Profile values are applied only when the CLI didn't explicitly set them.
  if result.profile.len > 0:
    let prof = loadProfile(result.profile)
    if prof.hasInterval and not cliSetInterval:
      result.interval = prof.interval
    if prof.hasTimeout and not cliSetTimeout:
      result.timeout = prof.timeout
    if prof.hasTierFilter and not cliSetTierFilter:
      result.tierFilter = prof.tierFilter
    if prof.hasRounds and not cliSetRounds:
      result.rounds = prof.rounds

proc parseArgs*(): Config =
  parseArgs(commandLineParams())
