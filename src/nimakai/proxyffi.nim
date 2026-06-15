import std/[httpclient, json, options]
import ./types

const proxyLib = "libnimaproxy.so"

proc c_proxy_start(configPath: cstring, port: cuint): cint {.cdecl, dynlib: proxyLib, importc: "proxy_start".}
proc c_proxy_stop(): cint {.cdecl, dynlib: proxyLib, importc: "proxy_stop".}
proc c_proxy_health(): cstring {.cdecl, dynlib: proxyLib, importc: "proxy_health".}
proc c_proxy_stats(): cstring {.cdecl, dynlib: proxyLib, importc: "proxy_stats".}
proc c_proxy_free_string(s: cstring) {.cdecl, dynlib: proxyLib, importc: "proxy_free_string".}

proc proxyStart*(configPath: string, port: int = 0): int =
  let raw = c_proxy_start(configPath.cstring, cuint(port))
  result = int(raw)

proc proxyStop*(): int =
  let raw = c_proxy_stop()
  result = int(raw)

proc child(node: JsonNode; key: string): JsonNode =
  if node.isNil or node.kind != JObject or not node.hasKey(key):
    return nil
  node[key]

proc nodeInt(node: JsonNode; default = 0): int =
  if node.isNil or node.kind == JNull:
    return default
  try:
    case node.kind
    of JInt:
      node.getBiggestInt().int
    of JFloat:
      node.getFloat().int
    else:
      default
  except:
    default

proc jsonInt(node: JsonNode; key: string; default = 0): int =
  nodeInt(child(node, key), default)

proc jsonFloat(node: JsonNode; key: string; default = 0.0): float =
  let n = child(node, key)
  if n.isNil or n.kind == JNull:
    return default
  try:
    case n.kind
    of JFloat, JInt:
      n.getFloat()
    else:
      default
  except:
    default

proc jsonBool(node: JsonNode; key: string; default = false): bool =
  let n = child(node, key)
  if n.isNil or n.kind == JNull:
    return default
  try:
    if n.kind == JBool: n.getBool() else: default
  except:
    default

proc jsonStr(node: JsonNode; key: string; default = ""): string =
  let n = child(node, key)
  if n.isNil or n.kind == JNull:
    return default
  try:
    if n.kind == JString: n.getStr() else: default
  except:
    default

proc jsonArray(node: JsonNode; key: string): JsonNode =
  let n = child(node, key)
  if n.isNil or n.kind != JArray:
    return newJArray()
  n

proc parseStringArray(node: JsonNode; key: string): seq[string] =
  for item in jsonArray(node, key):
    if not item.isNil and item.kind == JString:
      result.add(item.getStr())

proc parseRacingWins(node: JsonNode): seq[ProxyRacingWin] =
  let wins = child(node, "racing_wins")
  if wins.isNil or wins.kind != JObject:
    return @[]
  for model, countNode in wins.pairs:
    result.add(ProxyRacingWin(model: model, wins: nodeInt(countNode)))

proc parseGatewayStats(node: JsonNode): ProxyGatewayStats =
  let gateway = child(node, "gateway")
  result = ProxyGatewayStats(
    requestTotal: jsonInt(gateway, "request_total"),
    directRequests: jsonInt(gateway, "direct_requests"),
    racingRequests: jsonInt(gateway, "racing_requests"),
    upstreamAttempts: jsonInt(gateway, "upstream_attempts"),
    upstreamInFlight: jsonInt(gateway, "upstream_in_flight"),
    maxUpstreamInFlight: jsonInt(gateway, "max_upstream_in_flight"),
    maxInFlightPerKey: jsonInt(gateway, "max_in_flight_per_key"),
    keyWindowCapacity: jsonInt(gateway, "key_window_capacity"),
    keyAvailablePermits: jsonInt(gateway, "key_available_permits"),
    admissionWaitMs: jsonInt(gateway, "admission_wait_ms"),
    overloadRejects: jsonInt(gateway, "overload_rejects"),
    noKeyRejects: jsonInt(gateway, "no_key_rejects"),
    timeoutCount: jsonInt(gateway, "timeout_count"),
    rateLimitCount: jsonInt(gateway, "rate_limit_count"),
    fanoutTotal: jsonInt(gateway, "fanout_total"),
    fanoutSamples: jsonInt(gateway, "fanout_samples"),
    fanoutAvg: jsonFloat(gateway, "fanout_avg"),
    racingWins: parseRacingWins(gateway),
  )

proc parseProxyHealthJson*(js: string): Option[ProxyHealth] =
  try:
    let node = parseJson(js)
    let keysActive = jsonInt(node, "keys_active", jsonInt(node, "active_keys"))
    return some(ProxyHealth(
      status: jsonStr(node, "status"),
      activeKeys: keysActive,
      keysTotal: jsonInt(node, "keys_total"),
      gatewayInFlight: jsonInt(node, "gateway_in_flight"),
      gatewayLimit: jsonInt(node, "gateway_limit"),
      keyWindowCapacity: jsonInt(node, "key_window_capacity"),
      keyAvailablePermits: jsonInt(node, "key_available_permits"),
      admissionWaitMs: jsonInt(node, "admission_wait_ms"),
      routingEnabled: jsonBool(node, "routing_enabled"),
      racingEnabled: jsonBool(node, "racing_enabled"),
      racingMaxParallel: jsonInt(node, "racing_max_parallel"),
      racingTimeoutMs: jsonInt(node, "racing_timeout_ms"),
      racingMaxTotalRequestMs: jsonInt(node, "racing_max_total_request_ms"),
      racingAdaptive: jsonBool(node, "racing_adaptive"),
      racingMinParallel: jsonInt(node, "racing_min_parallel"),
      racingPressureParallel: jsonInt(node, "racing_pressure_parallel"),
      racingDegradedParallel: jsonInt(node, "racing_degraded_parallel"),
      racingLargePromptCharThreshold: jsonInt(node, "racing_large_prompt_char_threshold"),
      racingLargePromptParallel: jsonInt(node, "racing_large_prompt_parallel"),
      racingSoloFallback: jsonBool(node, "racing_solo_fallback"),
    ))
  except:
    return none(ProxyHealth)

proc parseProxyStatsJson*(js: string): Option[ProxyStats] =
  try:
    let node = parseJson(js)
    var models: seq[ProxyModelStats] = @[]
    for m in jsonArray(node, "models"):
      models.add(ProxyModelStats(
        model: jsonStr(m, "model"),
        avgMs: jsonFloat(m, "avg_ms"),
        p95Ms: jsonFloat(m, "p95_ms"),
        total: jsonInt(m, "total"),
        success: jsonInt(m, "success"),
        successRate: jsonFloat(m, "success_rate"),
        sampleCount: jsonInt(m, "sample_count"),
        consecutiveFailures: jsonInt(m, "consecutive_failures"),
        degraded: jsonBool(m, "degraded"),
      ))
    var keys: seq[ProxyKeyStats] = @[]
    for k in jsonArray(node, "keys"):
      keys.add(ProxyKeyStats(
        label: jsonStr(k, "label"),
        keyHint: jsonStr(k, "key_hint"),
        active: jsonBool(k, "active"),
        cooldownSecsRemaining: jsonInt(k, "cooldown_secs_remaining"),
        inFlight: jsonInt(k, "in_flight"),
        maxInFlight: jsonInt(k, "max_in_flight"),
        configuredMaxInFlight: jsonInt(k, "configured_max_in_flight"),
      ))
    return some(ProxyStats(
      models: models,
      keys: keys,
      gateway: parseGatewayStats(node),
      racingModels: parseStringArray(node, "racing_models"),
      racingEnabled: jsonBool(node, "racing_enabled"),
      racingMaxParallel: jsonInt(node, "racing_max_parallel"),
      racingTimeoutMs: jsonInt(node, "racing_timeout_ms"),
      racingMaxTotalRequestMs: jsonInt(node, "racing_max_total_request_ms"),
      racingAdaptive: jsonBool(node, "racing_adaptive"),
      racingMinParallel: jsonInt(node, "racing_min_parallel"),
      racingPressureParallel: jsonInt(node, "racing_pressure_parallel"),
      racingDegradedParallel: jsonInt(node, "racing_degraded_parallel"),
      racingLargePromptCharThreshold: jsonInt(node, "racing_large_prompt_char_threshold"),
      racingLargePromptParallel: jsonInt(node, "racing_large_prompt_parallel"),
      racingSoloFallback: jsonBool(node, "racing_solo_fallback"),
      racingFastModels: parseStringArray(node, "racing_fast_models"),
      racingFallbackModels: parseStringArray(node, "racing_fallback_models"),
    ))
  except:
    return none(ProxyStats)

proc proxyHealth*(): Option[ProxyHealth] =
  let raw = c_proxy_health()
  if raw.isNil:
    return none(ProxyHealth)
  defer: c_proxy_free_string(raw)
  parseProxyHealthJson($raw)

proc safeProxyHealth*(): Option[ProxyHealth] =
  ## Wraps proxyHealth() to swallow dynlib errors when libnimaproxy.so absent.
  try: proxyHealth()
  except CatchableError: none(ProxyHealth)

proc proxyStats*(): Option[ProxyStats] =
  let raw = c_proxy_stats()
  if raw.isNil:
    return none(ProxyStats)
  defer: c_proxy_free_string(raw)
  parseProxyStatsJson($raw)

proc safeProxyStats*(): Option[ProxyStats] =
  ## Wraps proxyStats() to swallow dynlib errors when libnimaproxy.so absent.
  try: proxyStats()
  except CatchableError: none(ProxyStats)

proc fetchProxyHttpJson(port: int; path: string): Option[string] =
  try:
    let client = newHttpClient(timeout = 3000)
    defer: client.close()
    let url = "http://127.0.0.1:" & $port & path
    let resp = client.request(url, httpMethod = HttpGet)
    if resp.code.int >= 200 and resp.code.int < 300:
      return some(resp.body)
  except CatchableError:
    discard
  none(string)

proc proxyHttpHealth*(port: int = 8080): Option[ProxyHealth] =
  let raw = fetchProxyHttpJson(port, "/health")
  if raw.isNone:
    return none(ProxyHealth)
  parseProxyHealthJson(raw.get)

proc proxyHttpStats*(port: int = 8080): Option[ProxyStats] =
  let raw = fetchProxyHttpJson(port, "/stats")
  if raw.isNone:
    return none(ProxyStats)
  parseProxyStatsJson(raw.get)

proc proxyHealthWithHttpFallback*(port: int = 8080): Option[ProxyHealth] =
  let httpHealth = proxyHttpHealth(port)
  if httpHealth.isSome:
    return httpHealth
  safeProxyHealth()

proc proxyStatsWithHttpFallback*(port: int = 8080): Option[ProxyStats] =
  let httpStats = proxyHttpStats(port)
  if httpStats.isSome:
    return httpStats
  safeProxyStats()
