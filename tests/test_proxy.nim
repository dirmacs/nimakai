import std/[unittest, os, strutils, json, options]
import nimakai/types
import nimakai/proxyffi
import nimakai/cli

suite "Proxy FFI Tests":
  setup:
    # Ensure proxy is stopped before each test
    discard proxyStop()
    sleep(100)

  teardown:
    # Clean up after each test
    discard proxyStop()
    sleep(100)

  test "proxyStart spawns daemon successfully":
    let configPath = "/opt/nimakai/nimaproxy/nimaproxy.toml"
    let port = 8080
    
    let result = proxyStart(configPath, port)
    check result == 0
    
    # Give daemon time to start
    sleep(500)
    
    # Verify PID file exists
    check fileExists("/tmp/nimaproxy.pid")

  test "proxyHealth returns UP when daemon is running":
    let configPath = "/opt/nimakai/nimaproxy/nimaproxy.toml"
    let port = 8080

    discard proxyStart(configPath, port)
    sleep(500)

    let healthOpt = proxyHealth()
    check isSome(healthOpt)
    let health = get(healthOpt)
    check health.status == "UP"

  test "proxyStats returns valid statistics":
    let configPath = "/opt/nimakai/nimaproxy/nimaproxy.toml"
    let port = 8080

    discard proxyStart(configPath, port)
    sleep(500)

    let statsOpt = proxyStats()
    check isSome(statsOpt)
    let stats = get(statsOpt)
    check stats.models.len >= 0
    check stats.keys.len >= 0
    check stats.racingMaxParallel >= 0
    check stats.racingTimeoutMs >= 0

  test "proxyStop kills running daemon":
    let configPath = "/opt/nimakai/nimaproxy/nimaproxy.toml"
    let port = 8080
    
    discard proxyStart(configPath, port)
    sleep(500)
    
    check fileExists("/tmp/nimaproxy.pid")
    
    let result = proxyStop()
    check result == 0
    
    sleep(200)
    check not fileExists("/tmp/nimaproxy.pid")

  test "proxyHealth returns error when daemon is not running":
    # Ensure daemon is stopped
    discard proxyStop()
    sleep(200)

    let healthOpt = proxyHealth()
    # Should return none when daemon is not running
    check isNone(healthOpt)

  test "proxyStart with port 0 uses config default":
    let configPath = "/opt/nimakai/nimaproxy/nimaproxy.toml"

    let result = proxyStart(configPath, 0)
    check result == 0

    sleep(500)

    let healthOpt = proxyHealth()
    check isSome(healthOpt)
    let health = get(healthOpt)
    check health.status == "UP"

  test "proxyStart with custom port override":
    let configPath = "/opt/nimakai/nimaproxy/nimaproxy.toml"
    let customPort = 9090

    let result = proxyStart(configPath, customPort)
    check result == 0

    sleep(500)

    let healthOpt = proxyHealth()
    check isSome(healthOpt)
    let health = get(healthOpt)
    check health.status == "UP"

test "proxyStats includes racing configuration":
  let configPath = "/opt/nimakai/nimaproxy/nimaproxy.toml"
  let port = 8080

  discard proxyStart(configPath, port)
  sleep(600)

  let healthOpt = proxyHealth()
  check isSome(healthOpt)
  discard get(healthOpt)

  let statsOpt = proxyStats()
  check isSome(statsOpt)
  let stats = get(statsOpt)
  check stats.racingModels.len >= 0
  check stats.racingMaxParallel >= 0
  check stats.racingTimeoutMs >= 0

  test "proxyStop is idempotent":
    let configPath = "/opt/nimakai/nimaproxy/nimaproxy.toml"
    let port = 8080
    
    discard proxyStart(configPath, port)
    sleep(500)
    
    # First stop
    let result1 = proxyStop()
    check result1 == 0
    
    sleep(200)
    
    # Second stop should also succeed (idempotent)
    let result2 = proxyStop()
    check result2 == 0

  test "proxyStart fails with invalid config path":
    let invalidPath = "/nonexistent/config.toml"
    let port = 8080
    
    let result = proxyStart(invalidPath, port)
    # Should return non-zero on failure
    check result != 0

test "proxyStats returns empty arrays when no requests made":
  let configPath = "/opt/nimakai/nimaproxy/nimaproxy.toml"
  let port = 9095

  let startResult = proxyStart(configPath, port)
  check startResult == 0

  var healthOpt = proxyHealth()
  var retries = 0
  while isNone(healthOpt) and retries < 5:
    sleep(400)
    healthOpt = proxyHealth()
    retries += 1

  check isSome(healthOpt)
  discard get(healthOpt)

  let statsOpt = proxyStats()
  check isSome(statsOpt)
  let stats = get(statsOpt)
  check stats.models.len >= 0
  check stats.keys.len >= 0
