## Tests for recommendation history tracking.

import std/[unittest, os, json, strutils]
import nimakai/[types, recommend, rechistory]

suite "appendRecHistory":
  test "writes JSONL entry":
    let path = getTempDir() / "nimakai_test_rechist.jsonl"
    defer:
      if fileExists(path): removeFile(path)

    let recs = @[Recommendation(
      category: "quick",
      currentModel: "old/model",
      recommendedModel: "new/model",
      reason: "faster",
      currentScore: 40.0,
      recommendedScore: 80.0,
    )]
    appendRecHistory(recs, rounds = 3, applied = false, path = path)

    check fileExists(path)
    let content = readFile(path)
    let data = parseJson(content.strip())
    check data.hasKey("ts")
    check data["rounds"].getInt() == 3
    check data["applied"].getBool() == false
    check data["categories"].len == 1
    check data["categories"][0]["category"].getStr() == "quick"

  test "appends multiple entries":
    let path = getTempDir() / "nimakai_test_rechist_multi.jsonl"
    defer:
      if fileExists(path): removeFile(path)

    let recs = @[Recommendation(
      category: "deep", currentModel: "a", recommendedModel: "b",
      reason: "better", currentScore: 30.0, recommendedScore: 70.0)]
    appendRecHistory(recs, rounds = 1, path = path)
    appendRecHistory(recs, rounds = 2, applied = true, path = path)

    var lineCount = 0
    for line in lines(path):
      if line.strip().len > 0: inc lineCount
    check lineCount == 2

  test "includes agent recommendations":
    let path = getTempDir() / "nimakai_test_rechist_agents.jsonl"
    defer:
      if fileExists(path): removeFile(path)

    let recs = @[Recommendation(
      category: "quick", currentModel: "a", recommendedModel: "b",
      reason: "r", currentScore: 1.0, recommendedScore: 2.0)]
    let agentRecs = @[Recommendation(
      category: "coder", currentModel: "c", recommendedModel: "d",
      reason: "ar", currentScore: 3.0, recommendedScore: 4.0)]
    appendRecHistory(recs, agentRecs, rounds = 1, path = path)

    let data = parseJson(readFile(path).strip())
    check data["agents"].len == 1
    check data["agents"][0]["agent"].getStr() == "coder"

suite "loadRecHistory":
  test "loads entries":
    let path = getTempDir() / "nimakai_test_rechist_load.jsonl"
    defer:
      if fileExists(path): removeFile(path)

    let recs = @[Recommendation(
      category: "deep", currentModel: "a", recommendedModel: "b",
      reason: "better", currentScore: 30.0, recommendedScore: 70.0)]
    appendRecHistory(recs, rounds = 3, applied = true, path = path)

    let entries = loadRecHistory(days = 30, path = path)
    check entries.len == 1
    check entries[0].rounds == 3
    check entries[0].applied == true
    check entries[0].recommendations.len == 1
    check entries[0].recommendations[0].category == "deep"

  test "returns empty for nonexistent file":
    let entries = loadRecHistory(path = "/tmp/nonexistent_rechist_12345.jsonl")
    check entries.len == 0

  test "round-trip append then load":
    let path = getTempDir() / "nimakai_test_rechist_rt.jsonl"
    defer:
      if fileExists(path): removeFile(path)

    let recs = @[
      Recommendation(category: "quick", currentModel: "a",
                     recommendedModel: "b", reason: "faster",
                     currentScore: 40.0, recommendedScore: 80.0),
      Recommendation(category: "deep", currentModel: "c",
                     recommendedModel: "c", reason: "already optimal",
                     currentScore: 70.0, recommendedScore: 70.0),
    ]
    appendRecHistory(recs, rounds = 5, applied = false, path = path)

    let entries = loadRecHistory(days = 30, path = path)
    check entries.len == 1
    check entries[0].recommendations.len == 2
    check entries[0].recommendations[0].recommendedModel == "b"
    check entries[0].recommendations[1].reason == "already optimal"

  test "skips malformed lines":
    let path = getTempDir() / "nimakai_test_rechist_bad.jsonl"
    defer:
      if fileExists(path): removeFile(path)

    writeFile(path, "not json\n")
    let recs = @[Recommendation(
      category: "quick", currentModel: "a", recommendedModel: "b",
      reason: "r", currentScore: 1.0, recommendedScore: 2.0)]
    appendRecHistory(recs, rounds = 1, path = path)

    let entries = loadRecHistory(days = 30, path = path)
    check entries.len == 1

  test "applied tracking preserved":
    let path = getTempDir() / "nimakai_test_rechist_applied.jsonl"
    defer:
      if fileExists(path): removeFile(path)

    let recs = @[Recommendation(
      category: "quick", currentModel: "a", recommendedModel: "b",
      reason: "r", currentScore: 1.0, recommendedScore: 2.0)]
    appendRecHistory(recs, rounds = 1, applied = false, path = path)
    appendRecHistory(recs, rounds = 2, applied = true, path = path)

    let entries = loadRecHistory(days = 30, path = path)
    check entries.len == 2
    check entries[0].applied == false
    check entries[1].applied == true

  test "agent recommendations round-trip":
    let path = getTempDir() / "nimakai_test_rechist_agent_rt.jsonl"
    defer:
      if fileExists(path): removeFile(path)

    let agentRecs = @[Recommendation(
      category: "coder", currentModel: "x", recommendedModel: "y",
      reason: "better", currentScore: 50.0, recommendedScore: 90.0)]
    appendRecHistory(@[], agentRecs, rounds = 1, path = path)

    let entries = loadRecHistory(days = 30, path = path)
    check entries.len == 1
    check entries[0].agentRecommendations.len == 1
    check entries[0].agentRecommendations[0].category == "coder"
