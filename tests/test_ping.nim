import std/unittest
import nimakai/[types, ping]

suite "ThroughputResult defaults":
  test "default ThroughputResult has zero values":
    let r = ThroughputResult()
    check r.ttft == 0.0
    check r.totalMs == 0.0
    check r.tokenCount == 0
    check r.tokPerSec == 0.0

  test "ThroughputResult fields can be set":
    let r = ThroughputResult(ttft: 100.0, totalMs: 500.0,
                              tokenCount: 32, tokPerSec: 80.0)
    check r.ttft == 100.0
    check r.tokenCount == 32
    check r.tokPerSec == 80.0

suite "classifyHealth":
  test "200 returns UP":
    check classifyHealth(200, "") == hUp

  test "401 returns NO_KEY":
    check classifyHealth(401, "") == hNoKey

  test "403 returns NO_KEY":
    check classifyHealth(403, "") == hNoKey

  test "404 returns NOT_FOUND":
    check classifyHealth(404, "") == hNotFound

  test "410 returns NOT_FOUND":
    check classifyHealth(410, "") == hNotFound

  test "429 returns OVERLOADED":
    check classifyHealth(429, "") == hOverloaded

  test "502 returns OVERLOADED":
    check classifyHealth(502, "") == hOverloaded

  test "503 returns OVERLOADED":
    check classifyHealth(503, "") == hOverloaded

  test "500 returns ERROR":
    check classifyHealth(500, "") == hError

  test "0 with timeout message returns TIMEOUT":
    check classifyHealth(0, "Connection timed out") == hTimeout

  test "0 with timeout variant returns TIMEOUT":
    check classifyHealth(0, "request timeout exceeded") == hTimeout

  test "0 with generic error returns ERROR":
    check classifyHealth(0, "connection refused") == hError

  test "0 with empty message returns ERROR":
    check classifyHealth(0, "") == hError
