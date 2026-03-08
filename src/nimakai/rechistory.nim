## Recommendation history tracking.
## Location: ~/.local/share/nimakai/recommendations.jsonl

import std/[json, os, times, strutils]
import ./[types, recommend]

proc defaultRecHistoryPath*(): string =
  getHomeDir() / ".local" / "share" / "nimakai" / "recommendations.jsonl"

type
  RecHistoryEntry* = object
    ts*: string
    rounds*: int
    applied*: bool
    recommendations*: seq[Recommendation]
    agentRecommendations*: seq[Recommendation]

proc appendRecHistory*(recs: seq[Recommendation],
                       agentRecs: seq[Recommendation] = @[],
                       rounds: int = 0,
                       applied: bool = false,
                       path: string = "") =
  ## Append a recommendation result to the history file.
  let p = if path.len > 0: path else: defaultRecHistoryPath()
  let dir = parentDir(p)
  if not dirExists(dir):
    createDir(dir)

  var catArr = newJArray()
  for r in recs:
    catArr.add(%*{
      "category": r.category,
      "current": r.currentModel,
      "recommended": r.recommendedModel,
      "reason": r.reason,
      "current_score": r.currentScore,
      "recommended_score": r.recommendedScore,
    })

  var agentArr = newJArray()
  for r in agentRecs:
    agentArr.add(%*{
      "agent": r.category,
      "current": r.currentModel,
      "recommended": r.recommendedModel,
      "reason": r.reason,
      "current_score": r.currentScore,
      "recommended_score": r.recommendedScore,
    })

  let entry = %*{
    "ts": now().utc.format("yyyy-MM-dd'T'HH:mm:ss'Z'"),
    "rounds": rounds,
    "applied": applied,
    "categories": catArr,
    "agents": agentArr,
  }

  let f = open(p, fmAppend)
  f.writeLine($entry)
  f.close()

proc loadRecHistory*(days: int = 30, path: string = ""): seq[RecHistoryEntry] =
  ## Load recommendation history entries from the last `days` days.
  let p = if path.len > 0: path else: defaultRecHistoryPath()
  if not fileExists(p): return @[]

  let cutoff = now().utc - initDuration(days = days)
  let cutoffStr = cutoff.format("yyyy-MM-dd'T'HH:mm:ss'Z'")

  try:
    for line in lines(p):
      if line.strip().len == 0: continue
      try:
        let data = parseJson(line)
        let ts = data["ts"].getStr()
        if ts < cutoffStr: continue

        var entry: RecHistoryEntry
        entry.ts = ts
        entry.rounds = data{"rounds"}.getInt(0)
        entry.applied = data{"applied"}.getBool(false)

        if data.hasKey("categories"):
          for r in data["categories"]:
            entry.recommendations.add(Recommendation(
              category: r{"category"}.getStr(),
              currentModel: r{"current"}.getStr(),
              recommendedModel: r{"recommended"}.getStr(),
              reason: r{"reason"}.getStr(),
              currentScore: r{"current_score"}.getFloat(),
              recommendedScore: r{"recommended_score"}.getFloat(),
            ))

        if data.hasKey("agents"):
          for r in data["agents"]:
            entry.agentRecommendations.add(Recommendation(
              category: r{"agent"}.getStr(),
              currentModel: r{"current"}.getStr(),
              recommendedModel: r{"recommended"}.getStr(),
              reason: r{"reason"}.getStr(),
              currentScore: r{"current_score"}.getFloat(),
              recommendedScore: r{"recommended_score"}.getFloat(),
            ))

        result.add(entry)
      except CatchableError:
        discard
  except CatchableError:
    discard

proc printRecHistory*(days: int = 30, path: string = "") =
  ## Print formatted recommendation history.
  let entries = loadRecHistory(days, path)
  if entries.len == 0:
    echo "No recommendation history found."
    return

  echo ""
  echo "\e[1m nimakai v" & Version & "\e[0m  \e[90mrecommendation history | last " & $days & " days | " & $entries.len & " entries\e[0m"
  echo ""

  echo "\e[1;90m  " & padRight("TIMESTAMP", 22) & padRight("ROUNDS", 8) &
       padRight("APPLIED", 9) & padRight("CHANGES", 60) & "\e[0m"
  echo "\e[90m  " & "-".repeat(99) & "\e[0m"

  for e in entries:
    var changes: seq[string] = @[]
    for r in e.recommendations:
      if r.recommendedModel != r.currentModel:
        changes.add(r.category & ": " & r.currentModel & " -> " & r.recommendedModel)
    for r in e.agentRecommendations:
      if r.recommendedModel != r.currentModel:
        changes.add(r.category & ": " & r.currentModel & " -> " & r.recommendedModel)

    let appliedStr = if e.applied: "\e[32myes\e[0m" else: "\e[90mno\e[0m"
    let changeStr = if changes.len == 0: "\e[90m(no changes)\e[0m"
                    else: changes[0]

    echo "  " & padRight(e.ts, 22) & padRight($e.rounds, 8) &
         padRight(if e.applied: "yes" else: "no", 9) & changeStr

    for i in 1..<changes.len:
      echo "  " & " ".repeat(39) & changes[i]

  echo ""
