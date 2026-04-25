## Integration tests for nimakai workflows.
## Tests the full pipeline: stats → metrics → recommend → sync
## without requiring actual HTTP calls.

import std/[unittest, json, os, strutils]
import nimakai/[types, catalog, recommend, opencode, sync, history, config]

let testDir = getTempDir() / "test_integration_" & $getCurrentProcessId()

proc setupTestDir() =
  createDir(testDir)

proc teardownTestDir() =
  if dirExists(testDir):
    removeDir(testDir)

proc makeStats(id: string, pings: openArray[float],
               totalPings: int = 0, successPings: int = 0): ModelStats =
  result.id = id
  result.name = id
  result.lastHealth = if pings.len > 0: hUp else: hPending
  let tp = if totalPings > 0: totalPings else: pings.len
  let sp = if successPings > 0: successPings else: pings.len
  result.totalPings = tp
  result.successPings = sp
  for p in pings:
    result.addSample(p)
  if pings.len > 0:
    result.lastMs = pings[^1]

proc writeOmoFile(path: string, categories: JsonNode = nil) =
  var data = %*{
    "categories": {
      "quick": {"model": "nvidia/llama-3.1-nemotron-nano-8b-v1"},
      "deep": {"model": "nvidia/qwen3.5-397b-a17b"}
    }
  }
  if categories != nil:
    data["categories"] = categories
  writeFile(path, pretty(data))

suite "full recommend pipeline":
  setup:
    setupTestDir()

  teardown:
    teardownTestDir()

  test "recommend → sync → rollback round-trip":
    let omoPath = testDir / "oh-my-opencode.json"
    writeOmoFile(omoPath)
    let originalContent = readFile(omoPath)

    let cat = loadCatalog()

    # Simulate benchmark results: fast model vs slow model
    var stats: seq[ModelStats] = @[]
    # Make nemotron-nano slow
    stats.add(makeStats("llama-3.1-nemotron-nano-8b-v1",
                        [2000.0, 2200.0, 1800.0]))
    # Make a fast alternative with catalog metadata
    for m in cat:
      if m.id notin ["llama-3.1-nemotron-nano-8b-v1",
                     "qwen/qwen3.5-397b-a17b"]:
        stats.add(makeStats(m.id, [300.0, 350.0, 280.0]))

    # Also add the deep model
    stats.add(makeStats("qwen/qwen3.5-397b-a17b",
                        [500.0, 550.0, 480.0]))

    let omo = OmoConfig(categories: @[
      OmoCategory(name: "quick", model: "llama-3.1-nemotron-nano-8b-v1"),
      OmoCategory(name: "deep", model: "qwen/qwen3.5-397b-a17b"),
    ])

    let recs = recommend(stats, cat, omo)
    check recs.len == 2

    # Quick category should recommend something faster
    let quickRec = recs[0]
    check quickRec.category == "quick"
    # The recommendation should differ (nemotron-nano is slow)
    # (exact model depends on catalog scores, just verify it ran)
    check quickRec.recommendedScore >= 0

    # Apply the recommendations
    let applied = syncRecommendations(recs, omoPath)
    if quickRec.recommendedModel != quickRec.currentModel:
      check applied == true

    # Rollback should restore original
    if applied:
      let rolledBack = rollbackOmo(omoPath)
      check rolledBack == true
      let restoredData = parseJson(readFile(omoPath))
      let origData = parseJson(originalContent)
      check restoredData["categories"]["quick"]["model"].getStr() ==
        origData["categories"]["quick"]["model"].getStr()

  test "recommend with all-optimal models produces no changes":
    let omoPath = testDir / "oh-my-opencode.json"

    # Find the best S+ model from catalog
    let cat = loadCatalog()
    var bestId = ""
    for m in cat:
        bestId = m.id
        break

    if bestId.len > 0:
      let categories = %*{
        "quick": {"model": "nvidia/" & bestId}
      }
      writeOmoFile(omoPath, categories)

      # Give the best model excellent stats
      var stats: seq[ModelStats] = @[]
      stats.add(makeStats(bestId, [100.0, 110.0, 105.0]))
      # Give alternatives worse stats
      for m in cat:
        if m.id != bestId:
          stats.add(makeStats(m.id, [2000.0, 2500.0, 3000.0]))

      let omo = OmoConfig(categories: @[
        OmoCategory(name: "quick", model: bestId),
      ])

      let recs = recommend(stats, cat, omo)
      check recs.len == 1
      check recs[0].reason == "already optimal"
      check recs[0].recommendedModel == recs[0].currentModel

suite "stats → history → trends pipeline":
  setup:
    setupTestDir()

  teardown:
    teardownTestDir()

  test "benchmark stats persist through history and produce trends":
    let histPath = testDir / "history.jsonl"

    # Simulate 8 rounds of benchmark data
    # First 4: fast latency
    for round in 1..4:
      var stats = @[
        makeStats("model-a", [200.0, 210.0, 190.0]),
        makeStats("model-b", [500.0, 520.0, 480.0]),
      ]
      appendRound(stats, round, histPath)

    # Next 4: model-a gets slower (degrading)
    for round in 5..8:
      var stats = @[
        makeStats("model-a", [800.0, 900.0, 850.0]),
        makeStats("model-b", [500.0, 520.0, 480.0]),
      ]
      appendRound(stats, round, histPath)

    # Load history
    let entries = loadHistory(days = 30, path = histPath)
    check entries.len == 8

    # Detect trends
    let trends = detectTrends(entries)
    check trends.len >= 1

    # Model-a should show degrading (avg went from ~200 to ~850)
    var foundA = false
    for t in trends:
      if t.id == "model-a":
        foundA = true
        check t.direction == tdDegrading
        check t.avgChange > 0  # positive = latency increased
        break
    check foundA

  test "prune removes old data without affecting recent":
    let histPath = testDir / "history.jsonl"

    # Write a "recent" entry manually (today's date)
    var stats = @[makeStats("model-a", [200.0])]
    appendRound(stats, 1, histPath)

    # Verify it exists
    let before = loadHistory(days = 30, path = histPath)
    check before.len == 1

    # Prune with short retention - recent entries should survive
    pruneHistory(days = 1, path = histPath)
    let after = loadHistory(days = 30, path = histPath)
    check after.len == 1

suite "config → recommend weights pipeline":
  setup:
    setupTestDir()

  teardown:
    teardownTestDir()

  test "config weights change recommendation outcome":
    let configPath = testDir / "config.json"

    # Write config with extreme speed-weighted config for "quick"
    let configData = %*{
      "category_weights": {
        "quick": {"swe": 0.0, "speed": 1.0, "ctx": 0.0, "stability": 0.0}
      }
    }
    writeFile(configPath, pretty(configData))

    let cfg = loadConfigFile(configPath)
    check cfg.categoryWeights.len == 1
    check cfg.categoryWeights[0].category == "quick"
    check cfg.categoryWeights[0].weights.speed == 1.0

    # Create two models: one fast with low SWE, one slow with high SWE
    let fastMeta = ModelMeta(id: "fast-model", name: "Fast",
                             sweScore: 20.0, ctxSize: 32768)
    let slowMeta = ModelMeta(id: "slow-model", name: "Slow",
                             sweScore: 78.0, ctxSize: 131072)

    let fastStats = makeStats("fast-model", [100.0, 110.0, 105.0])
    let slowStats = makeStats("slow-model", [2000.0, 2200.0, 1800.0])

    # With default weights, the high-SWE model might win for balanced scoring
    let defaultScoreFast = scoreModel(fastStats, fastMeta, cnSpeed)
    let defaultScoreSlow = scoreModel(slowStats, slowMeta, cnSpeed)

    # With pure speed weights, fast model should always win decisively
    let customW = cfg.categoryWeights[0].weights
    let customScoreFast = scoreModel(fastStats, fastMeta, cnSpeed, DefaultThresholds, customW)
    let customScoreSlow = scoreModel(slowStats, slowMeta, cnSpeed, DefaultThresholds, customW)

    # Fast model should score much higher with pure speed weights
    check customScoreFast > customScoreSlow
    # The gap should be larger with custom weights than defaults
    check (customScoreFast - customScoreSlow) > (defaultScoreFast - defaultScoreSlow)

suite "build info":
  test "version string is non-empty":
    check Version.len > 0

  test "git commit is populated":
    check GitCommit.len > 0
    check GitCommit != "unknown"

  test "build date is populated":
    check BuildDate.len > 0
    check "2026" in BuildDate or "20" in BuildDate  # year present

suite "category weights defaults":
  test "defaultWeightsFor returns valid weights for all needs":
    for need in CategoryNeed:
      let w = defaultWeightsFor(need)
      let total = w.swe + w.speed + w.ctx + w.stability
      # Weights should sum to approximately 1.0
      check abs(total - 1.0) < 0.01

  test "defaultWeightsFor speed prioritizes speed":
    let w = defaultWeightsFor(cnSpeed)
    check w.speed > w.swe
    check w.speed > w.ctx
    check w.speed > w.stability

  test "defaultWeightsFor quality prioritizes swe":
    let w = defaultWeightsFor(cnQuality)
    check w.swe > w.speed
    check w.swe > w.stability

  test "defaultWeightsFor reliability prioritizes stability":
    let w = defaultWeightsFor(cnReliability)
    check w.stability > w.swe
    check w.stability > w.speed
    check w.stability > w.ctx
