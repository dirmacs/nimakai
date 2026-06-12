## Tests for NVIDIA model fetch/update helpers.

import std/unittest
import nimakai/update

suite "benchmarkable model filtering":
  test "keeps chat completion candidates and rejects non-chat endpoints":
    let ids = @[
      "stepfun-ai/step-3.7-flash",
      "nvidia/embed-qa-4",
      "nvidia/nemoretriever-parse",
      "nvidia/example-reward-model",
      "nvidia/riva-translate-4b-instruct",
      "nvidia/ai-synthetic-video-detector",
      "qwen/qwen3.5-397b-a17b",
    ]

    check filterBenchmarkableModelIds(ids) == @[
      "stepfun-ai/step-3.7-flash",
      "qwen/qwen3.5-397b-a17b",
    ]

  test "deduplicates while preserving first-seen order":
    let ids = @[
      "moonshotai/kimi-k2.6",
      "nvidia/embed-qa-4",
      "moonshotai/kimi-k2.6",
      "mistralai/mistral-medium-3.5-128b",
    ]

    check filterBenchmarkableModelIds(ids) == @[
      "moonshotai/kimi-k2.6",
      "mistralai/mistral-medium-3.5-128b",
    ]

  test "empty or whitespace-only ids are not benchmarkable":
    check not isBenchmarkableModelId("")
    check not isBenchmarkableModelId("   ")

  test "catalog-like chat models are benchmarkable":
    check isBenchmarkableModelId("deepseek-ai/deepseek-v4-pro")
    check isBenchmarkableModelId("nvidia/nemotron-3-ultra-550b-a55b")
    check isBenchmarkableModelId("minimaxai/minimax-m3")
    check isBenchmarkableModelId("stepfun-ai/step-3.7-flash")
