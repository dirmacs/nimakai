import std/[unittest, os, json]
import nimakai/opencode

suite "parseOpenCodeConfig":
  test "returns empty for nonexistent file":
    let models = parseOpenCodeConfig("/tmp/nonexistent-opencode.json")
    check models.len == 0

  test "parses NVIDIA models":
    let path = "/tmp/test-opencode.json"
    let data = %*{
      "provider": {
        "nvidia": {
          "npm": "@ai-sdk/openai-compatible",
          "models": {
            "qwen/qwen3.5-397b-a17b": {
              "name": "Qwen 3.5 397B",
              "limit": {
                "context": 262144,
                "output": 16384
              }
            },
            "z-ai/glm-5.1": {
              "name": "GLM 5.1",
              "limit": {
                "context": 131072,
                "output": 131072
              }
            }
          }
        }
      }
    }
    writeFile(path, $data)
    defer: removeFile(path)

    let models = parseOpenCodeConfig(path)
    check models.len == 2

    var foundQwen = false
    var foundGlm = false
    for m in models:
      if m.id == "qwen/qwen3.5-397b-a17b":
        foundQwen = true
        check m.name == "Qwen 3.5 397B"
        check m.ctxSize == 262144
        check m.outputLimit == 16384
      if m.id == "z-ai/glm-5.1":
        foundGlm = true
    check foundQwen
    check foundGlm

suite "parseOmoConfig":
  test "returns empty for nonexistent file":
    let omo = parseOmoConfig("/tmp/nonexistent-omo.json")
    check omo.agents.len == 0
    check omo.categories.len == 0

  test "parses agents and categories":
    let path = "/tmp/test-omo.json"
    let data = %*{
      "agents": {
        "sisyphus": {"model": "nvidia/stepfun-ai/step-3.7-flash"},
        "oracle": {"model": "nvidia/qwen/qwen3.5-397b-a17b"}
      },
      "categories": {
        "quick": {"model": "nvidia/minimaxai/minimax-m2.7"},
        "deep": {"model": "nvidia/qwen/qwen3.5-397b-a17b"}
      }
    }
    writeFile(path, $data)
    defer: removeFile(path)

    let omo = parseOmoConfig(path)
    check omo.agents.len == 2
    check omo.categories.len == 2

    # Verify nvidia/ prefix is stripped
    var foundSisyphus = false
    for a in omo.agents:
      if a.name == "sisyphus":
        foundSisyphus = true
        check a.model == "stepfun-ai/step-3.7-flash"
    check foundSisyphus

    var foundQuick = false
    for c in omo.categories:
      if c.name == "quick":
        foundQuick = true
        check c.model == "minimaxai/minimax-m2.7"
    check foundQuick

  test "handles models without nvidia prefix":
    let path = "/tmp/test-omo-noprefix.json"
    let data = %*{
      "agents": {
        "test": {"model": "some/model"}
      },
      "categories": {}
    }
    writeFile(path, $data)
    defer: removeFile(path)

    let omo = parseOmoConfig(path)
    check omo.agents[0].model == "some/model"

  test "parses thinking flag from NVIDIA chat_template_kwargs":
    let path = "/tmp/test-omo-thinking.json"
    let data = %*{
      "agents": {
        "deepseek": {
          "model": "nvidia/deepseek-ai/deepseek-v4-flash",
          "chat_template_kwargs": {"thinking": true}
        },
        "legacy": {
          "model": "nvidia/qwen/qwen3.5-397b-a17b",
          "chat_template_kwargs": {"enable_thinking": true}
        }
      },
      "categories": {}
    }
    writeFile(path, $data)
    defer: removeFile(path)

    let omo = parseOmoConfig(path)
    check omo.agents.len == 2
    for a in omo.agents:
      check a.thinking
