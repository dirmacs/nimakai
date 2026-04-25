## Core types, enums, and constants for nimakai.

import std/strutils

const
  Version* = "0.13.2"
  GitCommit* = staticExec("git rev-parse --short HEAD 2>/dev/null || echo unknown").strip()
  BuildDate* = CompileDate & " " & CompileTime
  BaseURL* = "https://integrate.api.nvidia.com/v1/chat/completions"
  DefaultTimeout* = 15
  DefaultInterval* = 5
  MaxSamples* = 100

type
  Health* = enum
    hPending = "PENDING"
    hUp = "UP"
    hTimeout = "TIMEOUT"
    hOverloaded = "OVERLOADED"
    hError = "ERROR"
    hNoKey = "NO_KEY"
    hNotFound = "NOT_FOUND"

  Verdict* = enum
    vPending = "Pending"
    vPerfect = "Perfect"
    vNormal = "Normal"
    vSlow = "Slow"
    vSpiky = "Spiky"
    vVerySlow = "Very Slow"
    vNotFound = "Not Found"
    vNotActive = "Not Active"
    vUnstable = "Unstable"

  Tier* = enum
    tSPlus = "S+"
    tS = "S"
    tAPlus = "A+"
    tA = "A"
    tAMinus = "A-"
    tBPlus = "B+"
    tB = "B"
    tC = "C"

  PingResult* = object
    health*: Health
    ms*: float
    statusCode*: int
    errorMsg*: string
    timestamp*: float

  ModelStats* = object
    id*: string
    name*: string
    ring*: array[MaxSamples, float]
    ringLen*: int
    ringPos*: int
    totalPings*: int
    successPings*: int
    lastMs*: float
    lastHealth*: Health
    favorite*: bool

  ModelMeta* = object
    id*: string
    name*: string
    tier*: Tier
    sweScore*: float
    ctxSize*: int
    outputLimit*: int
    thinking*: bool
    multimodal*: bool

  ThroughputResult* = object
    ttft*: float       ## time to first token (ms)
    totalMs*: float    ## total response time (ms)
    tokenCount*: int   ## tokens generated
    tokPerSec*: float  ## throughput

  ProxyHealth* = object
    status*: string
    activeKeys*: int
    routingEnabled*: bool
    racingEnabled*: bool

  ProxyModelStats* = object
    model*: string
    avgMs*: float
    p95Ms*: float
    total*: int
    success*: int
    successRate*: float
    sampleCount*: int
    consecutiveFailures*: int
    degraded*: bool

  ProxyKeyStats* = object
    label*: string
    keyHint*: string
    active*: bool
    cooldownSecsRemaining*: int

  ProxyStats* = object
    models*: seq[ProxyModelStats]
    keys*: seq[ProxyKeyStats]
    racingModels*: seq[string]
    racingMaxParallel*: int
    racingTimeoutMs*: int

  Thresholds* = object
    perfectAvg*: float
    perfectP95*: float
    normalAvg*: float
    normalP95*: float
    slowAvg*: float
    verySlowAvg*: float
    spikeMs*: float

  CategoryWeights* = object
    swe*: float
    speed*: float
    ctx*: float
    stability*: float

  SortColumn* = enum
    scName = "name"
    scAvg = "avg"
    scP95 = "p95"
    scStability = "stability"
    scTier = "tier"
    scUptime = "uptime"

  Subcommand* = enum
    smBenchmark = "benchmark"
    smCatalog = "catalog"
    smRecommend = "recommend"
    smHistory = "history"
    smTrends = "trends"
    smOpencode = "opencode"
    smWatch = "watch"
    smCheck = "check"
    smDiscover = "discover"
    smProxy = "proxy"
    smFetch = "fetch"

  ProxyAction* = enum
    paStart = "start"
    paStop = "stop"
    paStatus = "status"

  Config* = object
    models*: seq[string]
    once*: bool
    interval*: int
    timeout*: int
    jsonOutput*: bool
    quiet*: bool
    noHistory*: bool
    dryRun*: bool
    apiKey*: string
    subcommand*: Subcommand
    tierFilter*: string
    sortColumn*: SortColumn
    useOpencode*: bool
    rounds*: int
    applySync*: bool
    rollback*: bool
    recHistory*: bool
    throughput*: bool
    alertThreshold*: float
    failIfDegraded*: bool
    days*: int
    profile*: string
    thresholds*: Thresholds
    categoryWeights*: seq[tuple[category: string, weights: CategoryWeights]]
    proxyAction*: ProxyAction
    proxyConfigPath*: string
    proxyPort*: int

const DefaultThresholds* = Thresholds(
  perfectAvg: 400.0,
  perfectP95: 800.0,
  normalAvg: 1000.0,
  normalP95: 2000.0,
  slowAvg: 2000.0,
  verySlowAvg: 5000.0,
  spikeMs: 3000.0,
)

proc padRight*(s: string, width: int): string =
  ## Pad string to width, truncating if too long.
  if s.len >= width: s[0..<width]
  else: s & ' '.repeat(width - s.len)

proc padLeft*(s: string, width: int): string =
  ## Left-pad string to width, truncating if too long.
  if s.len >= width: s[0..<width]
  else: ' '.repeat(width - s.len) & s

proc addSample*(stats: var ModelStats, ms: float) =
  ## Add a latency sample to the ring buffer.
  stats.ring[stats.ringPos] = ms
  stats.ringPos = (stats.ringPos + 1) mod MaxSamples
  if stats.ringLen < MaxSamples:
    inc stats.ringLen

proc samples*(stats: ModelStats): seq[float] =
  ## Extract current ring buffer contents as a seq.
  result = newSeq[float](stats.ringLen)
  for i in 0..<stats.ringLen:
    result[i] = stats.ring[i]

proc tierFamily*(t: Tier): char =
  ## Get the tier family letter (S, A, B, C).
  case t
  of tSPlus, tS: 'S'
  of tAPlus, tA, tAMinus: 'A'
  of tBPlus, tB: 'B'
  of tC: 'C'

proc tierOrd*(t: Tier): int =
  ## Numeric ordering for tiers (lower = better).
  case t
  of tSPlus: 0
  of tS: 1
  of tAPlus: 2
  of tA: 3
  of tAMinus: 4
  of tBPlus: 5
  of tB: 6
  of tC: 7
