import std/[json, options]
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

proc proxyHealth*(): Option[ProxyHealth] =
  let raw = c_proxy_health()
  if raw.isNil:
    return none(ProxyHealth)
  defer: c_proxy_free_string(raw)
  let js = $raw
  try:
    let node = parseJson(js)
    return some(ProxyHealth(
      status: node{"status"}.getStr(),
      activeKeys: node{"active_keys"}.getInt(),
      routingEnabled: node{"routing_enabled"}.getBool(),
      racingEnabled: node{"racing_enabled"}.getBool(),
    ))
  except:
    return none(ProxyHealth)

proc proxyStats*(): Option[ProxyStats] =
  let raw = c_proxy_stats()
  if raw.isNil:
    return none(ProxyStats)
  defer: c_proxy_free_string(raw)
  let js = $raw
  try:
    let node = parseJson(js)
    var models: seq[ProxyModelStats] = @[]
    for m in node{"models"}:
      models.add(ProxyModelStats(
        model: m{"model"}.getStr(),
        avgMs: m{"avg_ms"}.getFloat(),
        p95Ms: m{"p95_ms"}.getFloat(),
        total: m{"total"}.getBiggestInt().int,
        success: m{"success"}.getBiggestInt().int,
        successRate: m{"success_rate"}.getFloat(),
        sampleCount: m{"sample_count"}.getBiggestInt().int,
        consecutiveFailures: m{"consecutive_failures"}.getBiggestInt().int,
        degraded: m{"degraded"}.getBool(),
      ))
    var keys: seq[ProxyKeyStats] = @[]
    for k in node{"keys"}:
      keys.add(ProxyKeyStats(
        label: k{"label"}.getStr(),
        keyHint: k{"key_hint"}.getStr(),
        active: k{"active"}.getBool(),
        cooldownSecsRemaining: k{"cooldown_secs_remaining"}.getBiggestInt().int,
      ))
    var racingModels: seq[string] = @[]
    for rm in node{"racing_models"}:
      racingModels.add(rm.getStr())
    return some(ProxyStats(
      models: models,
      keys: keys,
      racingModels: racingModels,
      racingMaxParallel: node{"racing_max_parallel"}.getBiggestInt().int,
      racingTimeoutMs: node{"racing_timeout_ms"}.getBiggestInt().int,
    ))
  except:
    return none(ProxyStats)
