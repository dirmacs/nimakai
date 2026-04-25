import std/[strutils, unittest]
import nimakai/[types, display, metrics]

suite "padding":
  test "padRight pads correctly":
    check padRight("hi", 5) == "hi   "

  test "padRight truncates when too long":
    check padRight("hello world", 5) == "hello"

  test "padRight exact width":
    check padRight("hello", 5) == "hello"

  test "padLeft pads correctly":
    check padLeft("hi", 5) == "   hi"

  test "padLeft truncates when too long":
    check padLeft("hello world", 5) == "hello"

suite "stripAnsi":
  test "plain string returns length":
    check stripAnsi("hello") == 5

  test "string with ANSI codes returns visible length":
    check stripAnsi("\e[32mUP\e[0m") == 2

  test "empty string returns 0":
    check stripAnsi("") == 0

  test "multiple ANSI sequences":
    check stripAnsi("\e[1m\e[32mhi\e[0m") == 2

suite "padRightAnsi":
  test "pads based on visible width":
    let s = "\e[32mUP\e[0m"
    let padded = padRightAnsi(s, 10)
    check stripAnsi(padded) == 10

suite "padLeftAnsi":
  test "pads based on visible width":
    let s = "\e[32mUP\e[0m"
    let padded = padLeftAnsi(s, 10)
    check stripAnsi(padded) == 10

suite "colorLatency":
  test "green for < 500ms":
    let c = colorLatency(300.0)
    check "\e[32m" in c

  test "yellow for 500-1500ms":
    let c = colorLatency(800.0)
    check "\e[33m" in c

  test "red for >= 1500ms":
    let c = colorLatency(2000.0)
    check "\e[31m" in c

suite "healthIcon":
  test "UP is green":
    check "\e[32m" in healthIcon(hUp)

  test "TIMEOUT is yellow":
    check "\e[33m" in healthIcon(hTimeout)

  test "ERROR is red":
    check "\e[31m" in healthIcon(hError)

  test "PENDING is dim":
    check "\e[90m" in healthIcon(hPending)

suite "verdictColor":
  test "Perfect is green":
    check "\e[32m" in verdictColor(vPerfect)

  test "Normal is cyan":
    check "\e[36m" in verdictColor(vNormal)

  test "Unstable is bold red":
    check "\e[31;1m" in verdictColor(vUnstable)

  test "Pending is dim":
    check "\e[90m" in verdictColor(vPending)

proc makeDisplayStats(id: string, pings: openArray[float], fav: bool = false): ModelStats =
  result.id = id
  result.name = id
  result.lastHealth = hUp
  result.totalPings = pings.len
  result.successPings = pings.len
  for p in pings:
    result.addSample(p)
  if pings.len > 0:
    result.lastMs = pings[^1]
  result.favorite = fav

suite "sortStats":
  let cat = @[
    ModelMeta(id: "alpha", name: "Alpha", sweScore: 78.0, ctxSize: 131072),
    ModelMeta(id: "beta", name: "Beta", sweScore: 45.0, ctxSize: 131072),
    ModelMeta(id: "gamma", name: "Gamma", sweScore: 65.0, ctxSize: 131072),
  ]
  let th = DefaultThresholds

  test "sort by avg ascending":
    var stats = @[
      makeDisplayStats("beta", [500.0, 600.0]),   # avg 550
      makeDisplayStats("alpha", [100.0, 200.0]),   # avg 150
      makeDisplayStats("gamma", [300.0, 400.0]),   # avg 350
    ]
    sortStats(stats, scAvg, cat, th)
    check stats[0].id == "alpha"
    check stats[1].id == "gamma"
    check stats[2].id == "beta"

  test "sort by name ascending":
    var stats = @[
      makeDisplayStats("gamma", [100.0]),
      makeDisplayStats("alpha", [100.0]),
      makeDisplayStats("beta", [100.0]),
    ]
    sortStats(stats, scName, cat, th)
    check stats[0].id == "alpha"
    check stats[1].id == "beta"
    check stats[2].id == "gamma"


  test "sort by uptime descending":
    var stats = @[
      makeDisplayStats("alpha", [100.0]),
      makeDisplayStats("beta", [100.0]),
      makeDisplayStats("gamma", [100.0]),
    ]
    # Manually set different uptimes
    stats[0].totalPings = 10; stats[0].successPings = 5   # 50%
    stats[1].totalPings = 10; stats[1].successPings = 10  # 100%
    stats[2].totalPings = 10; stats[2].successPings = 8   # 80%
    sortStats(stats, scUptime, cat, th)
    check stats[0].id == "beta"    # 100%
    check stats[1].id == "gamma"   # 80%
    check stats[2].id == "alpha"   # 50%

  test "favorites always come first regardless of sort column":
    var stats = @[
      makeDisplayStats("beta", [500.0, 600.0]),             # avg 550, not fav
      makeDisplayStats("alpha", [100.0, 200.0], fav = true), # avg 150, fav
      makeDisplayStats("gamma", [300.0, 400.0]),             # avg 350, not fav
    ]
    # Sort by avg; alpha has lowest avg AND is favorite, so it should be first
    sortStats(stats, scAvg, cat, th)
    check stats[0].id == "alpha"
    check stats[0].favorite == true

  test "favorite with higher avg still comes first":
    var stats = @[
      makeDisplayStats("alpha", [100.0, 200.0]),              # avg 150, not fav
      makeDisplayStats("beta", [900.0, 1000.0], fav = true),  # avg 950, fav
      makeDisplayStats("gamma", [300.0, 400.0]),               # avg 350, not fav
    ]
    sortStats(stats, scAvg, cat, th)
    check stats[0].id == "beta"     # favorite pinned to top despite high avg
    check stats[0].favorite == true
    check stats[1].id == "alpha"    # lowest avg among non-favorites
    check stats[2].id == "gamma"

  test "sort by p95 ascending":
    var stats = @[
      makeDisplayStats("alpha", [100.0, 200.0, 300.0]),  # lower p95
      makeDisplayStats("beta", [500.0, 600.0, 700.0]),   # higher p95
    ]
    sortStats(stats, scP95, cat, th)
    check stats[0].id == "alpha"
    check stats[1].id == "beta"

  test "sort by stability descending":
    var stats = @[
      makeDisplayStats("alpha", [100.0, 110.0, 105.0]),    # low jitter, good stability
      makeDisplayStats("beta", [100.0, 3000.0, 200.0]),    # high jitter, spiky, worse stability
    ]
    stats[0].totalPings = 3; stats[0].successPings = 3
    stats[1].totalPings = 3; stats[1].successPings = 3
    sortStats(stats, scStability, cat, th)
    # Higher stability score should come first (descending)
    check stats[0].stabilityScore(th) >= stats[1].stabilityScore(th)

suite "termWidth":
  test "returns a positive integer":
    let w = termWidth()
    check w > 0

  test "returns at least 80":
    # Minimum usable terminal width
    let w = termWidth()
    check w >= 80
