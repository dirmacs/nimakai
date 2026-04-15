## Rust FFI bridge for high-performance concurrent HTTP pinging.
## Uses reqwest + rustls + tokio under the hood.

import std/[json, strutils]
import ./types

const rustLib = "libnimrust_ping." & (when defined(linux): "so" else: "dylib")

proc rust_ping_batch(models_csv: cstring, api_key: cstring,
                     timeout: cuint): cstring {.cdecl, dynlib: rustLib, importc.}
proc rust_discover_models(api_key: cstring,
                          timeout: cuint): cstring {.cdecl, dynlib: rustLib, importc.}
proc rust_free_string(s: cstring) {.cdecl, dynlib: rustLib, importc.}

proc rustPingBatch*(apiKey: string, models: seq[string],
                    timeout: int): seq[PingResult] =
  ## Ping all models concurrently via Rust. Returns results in same order.
  let csv = models.join(",")
  let raw = rust_ping_batch(csv.cstring, apiKey.cstring, timeout.cuint)
  if raw.isNil: return @[]
  let js = $raw
  rust_free_string(raw)

  let parsed = parseJson(js)
  for item in parsed["results"]:
    let code = item["status_code"].getInt()
    let ms = item["latency_ms"].getFloat()
    let errMsg = item["error"].getStr()
    let health = if code == 200: hUp
                 elif code in [401, 403]: hNoKey
                 elif code in [404, 410]: hNotFound
                 elif code == 429: hOverloaded
                 elif code in [502, 503]: hOverloaded
                 elif code >= 400: hError
                 elif errMsg.toLowerAscii().contains("timeout"): hTimeout
                 else: hError
    result.add(PingResult(
      health: health,
      ms: ms,
      statusCode: code,
      errorMsg: errMsg,
      timestamp: 0.0,
    ))

proc rustDiscoverModels*(apiKey: string, timeout: int = 15): string =
  ## Fetch /v1/models via Rust. Returns raw JSON body.
  let raw = rust_discover_models(apiKey.cstring, timeout.cuint)
  if raw.isNil: return "{}"
  result = $raw
  rust_free_string(raw)
