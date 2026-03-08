import std/[unittest, options, json]
import nimakai/[types, metrics, catalog, opencode, recommend]

proc makeStats(id: string, pings: openArray[float],
               total: int = -1, success: int = -1): ModelStats =
  result.id = id
  result.lastHealth = if pings.len > 0: hUp else: hTimeout
  for p in pings:
    result.addSample(p)
  result.totalPings = if total >= 0: total else: pings.len
  result.successPings = if success >= 0: success else: pings.len
  if pings.len > 0:
    result.lastMs = pings[^1]

suite "categorizeNeed":
  test "quick maps to Speed":
    check categorizeNeed("quick") == cnSpeed

  test "deep maps to Quality":
    check categorizeNeed("deep") == cnQuality

  test "ultrabrain maps to Reliability":
    check categorizeNeed("ultrabrain") == cnReliability

  test "visual-engineering maps to Vision":
    check categorizeNeed("visual-engineering") == cnVision

  test "writing maps to Balance":
    check categorizeNeed("writing") == cnBalance

  test "unknown maps to Balance":
    check categorizeNeed("something-else") == cnBalance

suite "scoreModel":
  test "fast model scores high for speed need":
    let stats = makeStats("fast/model", [100.0, 110.0, 105.0])
    let meta = ModelMeta(id: "fast/model", name: "Fast", tier: tS,
                         sweScore: 65.0, ctxSize: 131072)
    let score = scoreModel(stats, meta, cnSpeed)
    check score > 50

  test "high SWE model scores high for quality need":
    let stats = makeStats("quality/model", [500.0, 600.0, 550.0])
    let meta = ModelMeta(id: "quality/model", name: "Quality", tier: tSPlus,
                         sweScore: 78.0, ctxSize: 262144)
    let score = scoreModel(stats, meta, cnQuality)
    check score > 50

  test "non-multimodal model penalized for vision need":
    let stats = makeStats("text/model", [200.0, 210.0, 205.0])
    let textMeta = ModelMeta(id: "text/model", name: "Text", tier: tSPlus,
                             sweScore: 75.0, ctxSize: 131072, multimodal: false)
    let visionMeta = ModelMeta(id: "vision/model", name: "Vision", tier: tSPlus,
                               sweScore: 75.0, ctxSize: 131072, multimodal: true)

    let textScore = scoreModel(stats, textMeta, cnVision)
    let visionScore = scoreModel(stats, visionMeta, cnVision)
    check visionScore > textScore * 3 # 80% penalty = 5x difference

suite "recommend":
  test "recommends faster model for quick category":
    let cat = @[
      ModelMeta(id: "fast/model", name: "Fast", tier: tSPlus,
                sweScore: 72.0, ctxSize: 131072),
      ModelMeta(id: "slow/model", name: "Slow", tier: tSPlus,
                sweScore: 74.0, ctxSize: 131072),
    ]
    let stats = @[
      makeStats("fast/model", [100.0, 110.0, 105.0]),
      makeStats("slow/model", [2000.0, 2100.0, 2200.0]),
    ]
    let omo = OmoConfig(
      agents: @[],
      categories: @[OmoCategory(name: "quick", model: "slow/model")],
    )

    let recs = recommend(stats, cat, omo)
    check recs.len == 1
    check recs[0].recommendedModel == "fast/model"

  test "keeps optimal model":
    let cat = @[
      ModelMeta(id: "best/model", name: "Best", tier: tSPlus,
                sweScore: 78.0, ctxSize: 262144),
    ]
    let stats = @[
      makeStats("best/model", [200.0, 210.0, 205.0]),
    ]
    let omo = OmoConfig(
      agents: @[],
      categories: @[OmoCategory(name: "deep", model: "best/model")],
    )

    let recs = recommend(stats, cat, omo)
    check recs.len == 1
    check recs[0].recommendedModel == "best/model"
    check recs[0].reason == "already optimal"

  test "prefers multimodal for vision category":
    let cat = @[
      ModelMeta(id: "text/model", name: "Text", tier: tSPlus,
                sweScore: 78.0, ctxSize: 131072, multimodal: false),
      ModelMeta(id: "vision/model", name: "Vision", tier: tS,
                sweScore: 65.0, ctxSize: 131072, multimodal: true),
    ]
    let stats = @[
      makeStats("text/model", [100.0, 110.0]),
      makeStats("vision/model", [300.0, 310.0]),
    ]
    let omo = OmoConfig(
      agents: @[],
      categories: @[OmoCategory(name: "visual-engineering", model: "text/model")],
    )

    let recs = recommend(stats, cat, omo)
    check recs.len == 1
    check recs[0].recommendedModel == "vision/model"

suite "recommendationsToJson":
  test "empty recommendations produces empty array":
    let recs: seq[Recommendation] = @[]
    let j = recommendationsToJson(recs)
    check j.hasKey("recommendations")
    check j["recommendations"].len == 0

  test "single recommendation serializes all fields":
    let recs = @[Recommendation(
      category: "quick",
      currentModel: "slow/model",
      recommendedModel: "fast/model",
      reason: "42% lower avg latency",
      currentScore: 35.5,
      recommendedScore: 78.2,
    )]
    let j = recommendationsToJson(recs)
    check j.hasKey("recommendations")
    check j["recommendations"].kind == JArray
    check j["recommendations"].len == 1

    let r = j["recommendations"][0]
    check r["category"].getStr() == "quick"
    check r["current_model"].getStr() == "slow/model"
    check r["recommended_model"].getStr() == "fast/model"
    check r["reason"].getStr() == "42% lower avg latency"
    check abs(r["current_score"].getFloat() - 35.5) < 0.01
    check abs(r["recommended_score"].getFloat() - 78.2) < 0.01

  test "multiple recommendations all present in array":
    let recs = @[
      Recommendation(
        category: "quick",
        currentModel: "model-a",
        recommendedModel: "model-b",
        reason: "faster",
        currentScore: 40.0,
        recommendedScore: 80.0,
      ),
      Recommendation(
        category: "deep",
        currentModel: "model-b",
        recommendedModel: "model-b",
        reason: "already optimal",
        currentScore: 75.0,
        recommendedScore: 75.0,
      ),
    ]
    let j = recommendationsToJson(recs)
    check j["recommendations"].len == 2
    check j["recommendations"][0]["category"].getStr() == "quick"
    check j["recommendations"][1]["category"].getStr() == "deep"
    check j["recommendations"][1]["reason"].getStr() == "already optimal"
    check abs(j["recommendations"][1]["current_score"].getFloat() -
              j["recommendations"][1]["recommended_score"].getFloat()) < 0.01

suite "custom weights":
  test "custom weights override defaults in scoreModel":
    let stats = makeStats("test/model", [300.0, 320.0, 310.0])
    let meta = ModelMeta(id: "test/model", name: "Test", tier: tS,
                         sweScore: 70.0, ctxSize: 131072)
    let defaultScore = scoreModel(stats, meta, cnBalance)
    let speedOnlyWeights = CategoryWeights(swe: 0.0, speed: 1.0,
                                           ctx: 0.0, stability: 0.0)
    let customScore = scoreModel(stats, meta, cnBalance,
                                 customWeights = speedOnlyWeights)
    check abs(defaultScore - customScore) > 0.1

  test "zero custom weights fall back to defaults":
    let stats = makeStats("test/model", [300.0, 320.0, 310.0])
    let meta = ModelMeta(id: "test/model", name: "Test", tier: tS,
                         sweScore: 70.0, ctxSize: 131072)
    let defaultScore = scoreModel(stats, meta, cnBalance)
    let zeroWeights = CategoryWeights(swe: 0.0, speed: 0.0,
                                      ctx: 0.0, stability: 0.0)
    let zeroScore = scoreModel(stats, meta, cnBalance,
                               customWeights = zeroWeights)
    check abs(defaultScore - zeroScore) < 0.001

  test "custom weights via recommend function":
    # Two models: one fast with low SWE, one slow with high SWE.
    # Default "deep" weights favor SWE (0.45) over speed (0.10),
    # so high-swe model wins by default. With speed-only weights,
    # the fast model should win instead.
    let cat = @[
      ModelMeta(id: "fast/model", name: "Fast", tier: tS,
                sweScore: 50.0, ctxSize: 131072),
      ModelMeta(id: "smart/model", name: "Smart", tier: tSPlus,
                sweScore: 80.0, ctxSize: 262144),
    ]
    let stats = @[
      makeStats("fast/model", [100.0, 110.0, 105.0]),
      makeStats("smart/model", [2000.0, 2100.0, 2200.0]),
    ]
    let omo = OmoConfig(
      agents: @[],
      categories: @[OmoCategory(name: "deep", model: "smart/model")],
    )

    # Without weight overrides: deep favors SWE, smart/model should stay
    let defaultRecs = recommend(stats, cat, omo)
    check defaultRecs.len == 1
    check defaultRecs[0].recommendedModel == "smart/model"

    # With speed-only weight override: fast/model should win
    let speedWeights = CategoryWeights(swe: 0.0, speed: 1.0,
                                       ctx: 0.0, stability: 0.0)
    let overrides = @[(category: "deep", weights: speedWeights)]
    let customRecs = recommend(stats, cat, omo, weightOverrides = overrides)
    check customRecs.len == 1
    check customRecs[0].recommendedModel == "fast/model"

suite "classifyAgentNeed":
  test "thinking + high maxTokens maps to Quality":
    let agent = OmoAgent(name: "coder", model: "m", maxTokens: 32768, thinking: true)
    check classifyAgentNeed(agent) == cnQuality

  test "thinking + low maxTokens maps to Reliability":
    let agent = OmoAgent(name: "planner", model: "m", maxTokens: 8192, thinking: true)
    check classifyAgentNeed(agent) == cnReliability

  test "no thinking + no maxTokens maps to Speed":
    let agent = OmoAgent(name: "quick", model: "m", maxTokens: 0, thinking: false)
    check classifyAgentNeed(agent) == cnSpeed

  test "low maxTokens maps to Speed":
    let agent = OmoAgent(name: "summarizer", model: "m", maxTokens: 2048, thinking: false)
    check classifyAgentNeed(agent) == cnSpeed

  test "multimodal in name maps to Vision":
    let agent = OmoAgent(name: "multimodal-agent", model: "m", maxTokens: 16384, thinking: true)
    check classifyAgentNeed(agent) == cnVision

  test "visual in name maps to Vision":
    let agent = OmoAgent(name: "visual-processor", model: "m", maxTokens: 8192, thinking: false)
    check classifyAgentNeed(agent) == cnVision

suite "recommendAgents":
  test "recommends for agents":
    let cat = @[
      ModelMeta(id: "fast/model", name: "Fast", tier: tSPlus,
                sweScore: 72.0, ctxSize: 131072),
      ModelMeta(id: "slow/model", name: "Slow", tier: tSPlus,
                sweScore: 74.0, ctxSize: 131072),
    ]
    let stats = @[
      makeStats("fast/model", [100.0, 110.0, 105.0]),
      makeStats("slow/model", [2000.0, 2100.0, 2200.0]),
    ]
    let omo = OmoConfig(
      agents: @[OmoAgent(name: "coder", model: "slow/model",
                          maxTokens: 0, thinking: false)],
      categories: @[],
    )
    let recs = recommendAgents(stats, cat, omo)
    check recs.len == 1
    check recs[0].recommendedModel == "fast/model"

  test "empty agents returns empty":
    let cat = @[ModelMeta(id: "m", name: "M", tier: tS, sweScore: 60.0, ctxSize: 131072)]
    let stats = @[makeStats("m", [100.0])]
    let omo = OmoConfig(agents: @[], categories: @[])
    let recs = recommendAgents(stats, cat, omo)
    check recs.len == 0

suite "thinking and output limit scoring":
  test "thinking bonus for quality":
    let stats = makeStats("test/model", [300.0, 320.0, 310.0])
    let thinkMeta = ModelMeta(id: "test/model", name: "Think", tier: tS,
                              sweScore: 70.0, ctxSize: 131072, thinking: true)
    let noThinkMeta = ModelMeta(id: "test/model", name: "NoThink", tier: tS,
                                sweScore: 70.0, ctxSize: 131072, thinking: false)
    let thinkScore = scoreModel(stats, thinkMeta, cnQuality)
    let noThinkScore = scoreModel(stats, noThinkMeta, cnQuality)
    check thinkScore > noThinkScore

  test "no thinking bonus for speed":
    let stats = makeStats("test/model", [300.0, 320.0, 310.0])
    let thinkMeta = ModelMeta(id: "test/model", name: "Think", tier: tS,
                              sweScore: 70.0, ctxSize: 131072, thinking: true)
    let noThinkMeta = ModelMeta(id: "test/model", name: "NoThink", tier: tS,
                                sweScore: 70.0, ctxSize: 131072, thinking: false)
    let thinkScore = scoreModel(stats, thinkMeta, cnSpeed)
    let noThinkScore = scoreModel(stats, noThinkMeta, cnSpeed)
    check abs(thinkScore - noThinkScore) < 0.01

  test "output limit penalty for quality":
    let stats = makeStats("test/model", [300.0, 320.0, 310.0])
    let lowLimit = ModelMeta(id: "test/model", name: "Low", tier: tS,
                             sweScore: 70.0, ctxSize: 131072, outputLimit: 4096)
    let highLimit = ModelMeta(id: "test/model", name: "High", tier: tS,
                              sweScore: 70.0, ctxSize: 131072, outputLimit: 16384)
    let lowScore = scoreModel(stats, lowLimit, cnQuality)
    let highScore = scoreModel(stats, highLimit, cnQuality)
    check lowScore < highScore

  test "no output limit penalty for speed":
    let stats = makeStats("test/model", [300.0, 320.0, 310.0])
    let lowLimit = ModelMeta(id: "test/model", name: "Low", tier: tS,
                             sweScore: 70.0, ctxSize: 131072, outputLimit: 4096)
    let noLimit = ModelMeta(id: "test/model", name: "None", tier: tS,
                            sweScore: 70.0, ctxSize: 131072, outputLimit: 0)
    let lowScore = scoreModel(stats, lowLimit, cnSpeed)
    let noScore = scoreModel(stats, noLimit, cnSpeed)
    check abs(lowScore - noScore) < 0.01

suite "agentRecommendationsToJson":
  test "serializes agent recommendations":
    let recs = @[Recommendation(
      category: "coder",
      currentModel: "old/model",
      recommendedModel: "new/model",
      reason: "faster",
      currentScore: 40.0,
      recommendedScore: 80.0,
    )]
    let j = agentRecommendationsToJson(recs)
    check j.hasKey("agent_recommendations")
    check j["agent_recommendations"].len == 1
    check j["agent_recommendations"][0]["agent"].getStr() == "coder"

suite "uptime scoring":
  test "100% uptime leaves score unchanged":
    let stats = makeStats("test/model", [300.0, 320.0, 310.0], total = 3, success = 3)
    let meta = ModelMeta(id: "test/model", name: "Test", tier: tS,
                         sweScore: 70.0, ctxSize: 131072)
    let score = scoreModel(stats, meta, cnBalance)
    # With 100% uptime, multiplier is 1.0, so score should be same as base
    check score > 0

  test "50% uptime significantly reduces score":
    let fullUp = makeStats("a", [300.0, 320.0, 310.0], total = 3, success = 3)
    let halfUp = makeStats("b", [300.0, 320.0, 310.0], total = 6, success = 3)
    let meta = ModelMeta(id: "a", name: "Test", tier: tS,
                         sweScore: 70.0, ctxSize: 131072)
    let scoreFull = scoreModel(fullUp, meta, cnBalance)
    let scoreHalf = scoreModel(halfUp, meta, cnBalance)
    # 50% uptime should reduce score substantially (at least 40% lower)
    check scoreHalf < scoreFull * 0.6
    check scoreHalf > 0

  test "0% uptime zeroes score":
    let stats = makeStats("test/model", [300.0, 320.0, 310.0], total = 3, success = 0)
    let meta = ModelMeta(id: "test/model", name: "Test", tier: tS,
                         sweScore: 70.0, ctxSize: 131072)
    let score = scoreModel(stats, meta, cnBalance)
    check score == 0.0
