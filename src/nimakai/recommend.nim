## Recommendation engine for optimal model routing.
## Scores models based on latency metrics and model capabilities
## to suggest routing changes for oh-my-opencode categories.

import std/[strutils, strformat, json, options]
import ./[types, metrics, catalog, opencode]

type
  CategoryNeed* = enum
    cnSpeed
    cnQuality
    cnBalance
    cnVision
    cnReliability

  Recommendation* = object
    category*: string
    currentModel*: string
    recommendedModel*: string
    reason*: string
    currentScore*: float
    recommendedScore*: float

proc categorizeNeed*(name: string): CategoryNeed =
  case name
  of "quick", "unspecified-low": cnSpeed
  of "deep", "artistry": cnQuality
  of "ultrabrain": cnReliability
  of "visual-engineering": cnVision
  else: cnBalance

proc defaultWeightsFor*(need: CategoryNeed): CategoryWeights =
  ## Return default scoring weights for a category need.
  case need
  of cnSpeed: CategoryWeights(swe: 0.15, speed: 0.55, ctx: 0.10, stability: 0.20)
  of cnQuality: CategoryWeights(swe: 0.45, speed: 0.10, ctx: 0.25, stability: 0.20)
  of cnReliability: CategoryWeights(swe: 0.25, speed: 0.20, ctx: 0.15, stability: 0.40)
  of cnVision: CategoryWeights(swe: 0.30, speed: 0.20, ctx: 0.20, stability: 0.30)
  of cnBalance: CategoryWeights(swe: 0.30, speed: 0.30, ctx: 0.15, stability: 0.25)

proc scoreModel*(stats: ModelStats, meta: ModelMeta, need: CategoryNeed,
                 th: Thresholds = DefaultThresholds,
                 customWeights: CategoryWeights = CategoryWeights()): float =
  ## Score a model 0-100 for a given category need.
  ## If customWeights has non-zero fields, they override the defaults.
  let defaultW = defaultWeightsFor(need)
  let w = if customWeights.swe + customWeights.speed + customWeights.ctx + customWeights.stability > 0:
            customWeights
          else:
            defaultW

  # SWE score (0-100, already in percent)
  let sweScore = meta.sweScore

  # Speed score (lower latency = higher score)
  let avgMs = stats.avg()
  let speedScore = if avgMs <= 0: 0.0
                   else: clamp(100.0 * (1.0 - avgMs / 5000.0), 0.0, 100.0)

  # Context score (larger = better, normalized to 256k baseline)
  let ctxScore = clamp(meta.ctxSize.float / 262144.0 * 100.0, 0.0, 100.0)

  # Stability score
  let stabScore = stats.stabilityScore(th).float
  let stabNorm = if stabScore < 0: 50.0 else: stabScore

  var score = w.swe * sweScore + w.speed * speedScore +
              w.ctx * ctxScore + w.stability * stabNorm

  # Vision penalty: non-multimodal models get 80% penalty for vision needs
  if need == cnVision and not meta.multimodal:
    score *= 0.20

  # Thinking bonus: 10% for quality/reliability needs
  if meta.thinking and need in {cnQuality, cnReliability}:
    score *= 1.10

  # Output limit penalty: if known and < 8192, penalize quality/reliability
  if meta.outputLimit > 0 and meta.outputLimit < 8192 and need in {cnQuality, cnReliability}:
    score *= 0.70

  # Availability gate: low uptime penalizes score proportionally
  if stats.totalPings > 0:
    let up = stats.uptime()
    score *= (up / 100.0)

  score

proc recommend*(stats: seq[ModelStats], cat: seq[ModelMeta],
                omo: OmoConfig,
                th: Thresholds = DefaultThresholds,
                weightOverrides: seq[tuple[category: string, weights: CategoryWeights]] = @[]): seq[Recommendation] =
  ## Generate routing recommendations for each OMO category.
  for omocat in omo.categories:
    let need = categorizeNeed(omocat.name)

    # Look up custom weights for this category
    var customW = CategoryWeights()
    for wo in weightOverrides:
      if wo.category == omocat.name:
        customW = wo.weights
        break

    var bestModel = ""
    var bestScore = -1.0
    var currentScore = -1.0

    for s in stats:
      let meta = cat.lookupMeta(s.id)
      if meta.isNone: continue
      if s.ringLen == 0: continue # no data

      let score = scoreModel(s, meta.get, need, th, customW)

      if score > bestScore:
        bestScore = score
        bestModel = s.id

      if s.id == omocat.model:
        currentScore = score

    if bestModel.len == 0: continue

    var reason = ""
    if bestModel == omocat.model:
      reason = "already optimal"
    else:
      let currentMeta = cat.lookupMeta(omocat.model)
      let bestMeta = cat.lookupMeta(bestModel)
      var parts: seq[string] = @[]

      # Find the stats for both models
      var bestStats, curStats: ModelStats
      var foundBest, foundCur = false
      for s in stats:
        if s.id == bestModel: bestStats = s; foundBest = true
        if s.id == omocat.model: curStats = s; foundCur = true

      if foundBest and foundCur and bestStats.avg() > 0 and curStats.avg() > 0:
        let diff = ((curStats.avg() - bestStats.avg()) / curStats.avg() * 100).int
        if diff > 5:
          parts.add(&"{diff}% lower avg latency")
        elif diff < -5:
          parts.add(&"{-diff}% higher avg latency")


      let bestStab = if foundBest: bestStats.stabilityScore(th) else: -1
      let curStab = if foundCur: curStats.stabilityScore(th) else: -1
      if bestStab >= 0 and curStab >= 0 and bestStab > curStab + 10:
        parts.add(&"stability {bestStab} vs {curStab}")

      reason = if parts.len > 0: parts.join(", ") else: "higher composite score"

    result.add(Recommendation(
      category: omocat.name,
      currentModel: omocat.model,
      recommendedModel: bestModel,
      reason: reason,
      currentScore: currentScore,
      recommendedScore: bestScore,
    ))

proc printRecommendations*(recs: seq[Recommendation], rounds: int) =
  echo ""
  echo &"\e[1m nimakai v{Version}\e[0m  \e[90mrecommendations based on {rounds} rounds\e[0m"
  echo ""

  echo "\e[1;90m  " & padRight("CATEGORY", 22) & padRight("CURRENT", 32) &
       padRight("RECOMMENDED", 32) & padRight("REASON", 40) & "\e[0m"
  echo "\e[90m  " & "-".repeat(126) & "\e[0m"

  for r in recs:
    let recDisplay = if r.recommendedModel == r.currentModel: "(no change)"
                     else: r.recommendedModel
    let recColor = if r.recommendedModel == r.currentModel: "\e[90m"
                   else: "\e[32m"
    echo "  " & padRight(r.category, 22) &
         padRight(r.currentModel, 32) &
         recColor & padRight(recDisplay, 32) & "\e[0m" &
         "\e[90m" & r.reason & "\e[0m"

  echo ""

proc recommendationsToJson*(recs: seq[Recommendation]): JsonNode =
  var arr = newJArray()
  for r in recs:
    arr.add(%*{
      "category": r.category,
      "current_model": r.currentModel,
      "recommended_model": r.recommendedModel,
      "reason": r.reason,
      "current_score": r.currentScore,
      "recommended_score": r.recommendedScore,
    })
  %*{"recommendations": arr}

proc classifyAgentNeed*(agent: OmoAgent): CategoryNeed =
  ## Classify an OMO agent's need based on its parameters.
  let nameLower = agent.name.toLowerAscii()
  if "multimodal" in nameLower or "visual" in nameLower:
    return cnVision
  if agent.thinking and agent.maxTokens > 16384:
    return cnQuality
  if agent.thinking and agent.maxTokens <= 16384:
    return cnReliability
  if agent.maxTokens > 0 and agent.maxTokens <= 4096 or
     (not agent.thinking and agent.maxTokens == 0):
    return cnSpeed
  cnBalance

proc recommendAgents*(stats: seq[ModelStats], cat: seq[ModelMeta],
                      omo: OmoConfig,
                      th: Thresholds = DefaultThresholds,
                      weightOverrides: seq[tuple[category: string, weights: CategoryWeights]] = @[]): seq[Recommendation] =
  ## Generate routing recommendations for each OMO agent.
  for agent in omo.agents:
    let need = classifyAgentNeed(agent)

    var customW = CategoryWeights()
    for wo in weightOverrides:
      if wo.category == agent.name:
        customW = wo.weights
        break

    var bestModel = ""
    var bestScore = -1.0
    var currentScore = -1.0

    for s in stats:
      let meta = cat.lookupMeta(s.id)
      if meta.isNone: continue
      if s.ringLen == 0: continue

      let score = scoreModel(s, meta.get, need, th, customW)

      if score > bestScore:
        bestScore = score
        bestModel = s.id

      if s.id == agent.model:
        currentScore = score

    if bestModel.len == 0: continue

    var reason = ""
    if bestModel == agent.model:
      reason = "already optimal"
    else:
      reason = "higher composite score for " & $need

    result.add(Recommendation(
      category: agent.name,
      currentModel: agent.model,
      recommendedModel: bestModel,
      reason: reason,
      currentScore: currentScore,
      recommendedScore: bestScore,
    ))

proc printAgentRecommendations*(recs: seq[Recommendation], rounds: int) =
  if recs.len == 0: return

  echo ""
  echo &"\e[1m nimakai v{Version}\e[0m  \e[90magent recommendations based on {rounds} rounds\e[0m"
  echo ""

  echo "\e[1;90m  " & padRight("AGENT", 22) & padRight("CURRENT", 32) &
       padRight("RECOMMENDED", 32) & padRight("REASON", 40) & "\e[0m"
  echo "\e[90m  " & "-".repeat(126) & "\e[0m"

  for r in recs:
    let recDisplay = if r.recommendedModel == r.currentModel: "(no change)"
                     else: r.recommendedModel
    let recColor = if r.recommendedModel == r.currentModel: "\e[90m"
                   else: "\e[32m"
    echo "  " & padRight(r.category, 22) &
         padRight(r.currentModel, 32) &
         recColor & padRight(recDisplay, 32) & "\e[0m" &
         "\e[90m" & r.reason & "\e[0m"

  echo ""

proc agentRecommendationsToJson*(recs: seq[Recommendation]): JsonNode =
  var arr = newJArray()
  for r in recs:
    arr.add(%*{
      "agent": r.category,
      "current_model": r.currentModel,
      "recommended_model": r.recommendedModel,
      "reason": r.reason,
      "current_score": r.currentScore,
      "recommended_score": r.recommendedScore,
    })
  %*{"agent_recommendations": arr}
