## Model catalog for NVIDIA NIM models.
## Contains compiled-in metadata and user model file loading.

import std/[json, os, options, strutils, algorithm, tables]
import ./types

const BuiltinCatalog*: seq[ModelMeta] = @[
  # S+ tier (SWE-bench >= 70%)
  ModelMeta(id: "minimaxai/minimax-m2.5", name: "MiniMax M2.5", tier: tSPlus, sweScore: 80.2, ctxSize: 204800, thinking: false, multimodal: false),
  ModelMeta(id: "z-ai/glm5", name: "GLM 5", tier: tSPlus, sweScore: 77.8, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "moonshotai/kimi-k2.5", name: "Kimi K2.5", tier: tSPlus, sweScore: 76.8, ctxSize: 131072, thinking: true, multimodal: true),
  ModelMeta(id: "stepfun-ai/step-3.5-flash", name: "Step 3.5 Flash", tier: tSPlus, sweScore: 74.4, ctxSize: 262144, thinking: false, multimodal: false),
  ModelMeta(id: "minimaxai/minimax-m2.1", name: "MiniMax M2.1", tier: tSPlus, sweScore: 74.0, ctxSize: 204800, thinking: false, multimodal: false),
  ModelMeta(id: "z-ai/glm4.7", name: "GLM 4.7", tier: tSPlus, sweScore: 73.8, ctxSize: 204800, thinking: true, multimodal: false),
  ModelMeta(id: "deepseek-ai/deepseek-v3.2", name: "DeepSeek V3.2", tier: tSPlus, sweScore: 73.1, ctxSize: 163840, thinking: false, multimodal: false),
  ModelMeta(id: "mistralai/devstral-2-123b-instruct-2512", name: "Devstral 2 123B", tier: tSPlus, sweScore: 72.2, ctxSize: 262144, thinking: false, multimodal: false),
  ModelMeta(id: "moonshotai/kimi-k2-thinking", name: "Kimi K2 Thinking", tier: tSPlus, sweScore: 71.3, ctxSize: 262144, thinking: true, multimodal: false),
  ModelMeta(id: "qwen/qwen3-coder-480b-a35b-instruct", name: "Qwen3 Coder 480B", tier: tSPlus, sweScore: 70.6, ctxSize: 262144, thinking: false, multimodal: false),
  ModelMeta(id: "qwen/qwen3-235b-a22b", name: "Qwen3 235B", tier: tSPlus, sweScore: 70.0, ctxSize: 131072, thinking: false, multimodal: false),

  # S tier (60-70%)
  ModelMeta(id: "minimaxai/minimax-m2", name: "MiniMax M2", tier: tS, sweScore: 69.4, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "deepseek-ai/deepseek-v3.1-terminus", name: "DeepSeek V3.1 Term", tier: tS, sweScore: 68.4, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "qwen/qwen3-next-80b-a3b-thinking", name: "Qwen3 80B Thinking", tier: tS, sweScore: 68.0, ctxSize: 262144, thinking: true, multimodal: false),
  ModelMeta(id: "qwen/qwen3.5-397b-a17b", name: "Qwen3.5 400B VLM", tier: tS, sweScore: 68.0, ctxSize: 262144, thinking: true, multimodal: true),
  ModelMeta(id: "qwen/qwen3.5-122b-a10b", name: "Qwen3.5 122B MoE", tier: tS, sweScore: 66.0, ctxSize: 262144, thinking: true, multimodal: false),
  ModelMeta(id: "moonshotai/kimi-k2-instruct", name: "Kimi K2 Instruct", tier: tS, sweScore: 65.8, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "moonshotai/kimi-k2-instruct-0905", name: "Kimi K2 0905", tier: tS, sweScore: 65.5, ctxSize: 262144, thinking: false, multimodal: false),
  ModelMeta(id: "qwen/qwen3-next-80b-a3b-instruct", name: "Qwen3 80B Instruct", tier: tS, sweScore: 65.0, ctxSize: 262144, thinking: false, multimodal: false),
  ModelMeta(id: "meta/llama-4-maverick-17b-128e-instruct", name: "Llama 4 Maverick", tier: tS, sweScore: 62.0, ctxSize: 1048576, thinking: false, multimodal: true),
  ModelMeta(id: "deepseek-ai/deepseek-v3.1", name: "DeepSeek V3.1", tier: tS, sweScore: 62.0, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "deepseek-ai/deepseek-r1-0528", name: "DeepSeek R1 0528", tier: tS, sweScore: 63.5, ctxSize: 131072, thinking: true, multimodal: false),
  ModelMeta(id: "openai/gpt-oss-120b", name: "GPT OSS 120B", tier: tS, sweScore: 60.0, ctxSize: 131072, thinking: false, multimodal: false),

  # A+ tier (50-60%)
  ModelMeta(id: "nvidia/llama-3.1-nemotron-70b-instruct", name: "Nemotron 70B", tier: tAPlus, sweScore: 57.0, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "nvidia/nemotron-3-super-120b-a12b", name: "Nemotron 3 Super", tier: tAPlus, sweScore: 59.2, ctxSize: 1048576, thinking: true, multimodal: false),
  ModelMeta(id: "mistralai/mistral-large-3-675b-instruct-2512", name: "Mistral Large 675B", tier: tAPlus, sweScore: 58.0, ctxSize: 262144, thinking: false, multimodal: false),
  ModelMeta(id: "nvidia/llama-3.1-nemotron-ultra-253b-v1", name: "Nemotron Ultra 253B", tier: tAPlus, sweScore: 56.0, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "igenius/colosseum_355b_instruct_16k", name: "Colosseum 355B", tier: tAPlus, sweScore: 52.0, ctxSize: 16384, thinking: false, multimodal: false),
  ModelMeta(id: "qwen/qwq-32b", name: "QwQ 32B", tier: tAPlus, sweScore: 50.0, ctxSize: 131072, thinking: true, multimodal: false),

  # A tier (40-50%)
  ModelMeta(id: "nvidia/llama-3.3-nemotron-super-49b-v1.5", name: "Nemotron Super 49B", tier: tA, sweScore: 49.0, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "mistralai/mistral-medium-3-instruct", name: "Mistral Medium 3", tier: tA, sweScore: 48.0, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "qwen/qwen2.5-coder-32b-instruct", name: "Qwen2.5 Coder 32B", tier: tA, sweScore: 46.0, ctxSize: 32768, thinking: false, multimodal: false),
  ModelMeta(id: "mistralai/magistral-small-2506", name: "Magistral Small", tier: tA, sweScore: 45.0, ctxSize: 32768, thinking: false, multimodal: false),
  ModelMeta(id: "meta/llama-4-scout-17b-16e-instruct", name: "Llama 4 Scout", tier: tA, sweScore: 44.0, ctxSize: 10485760, thinking: false, multimodal: true),
  ModelMeta(id: "meta/llama-3.1-405b-instruct", name: "Llama 3.1 405B", tier: tA, sweScore: 44.0, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "deepseek-ai/deepseek-r1-distill-qwen-32b", name: "R1 Distill 32B", tier: tA, sweScore: 43.9, ctxSize: 131072, thinking: true, multimodal: false),
  ModelMeta(id: "nvidia/nemotron-3-nano-30b-a3b", name: "Nemotron Nano 30B", tier: tA, sweScore: 43.0, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "nvidia/nemotron-4-340b-instruct", name: "Nemotron 4 340B", tier: tA, sweScore: 41.0, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "google/gemma-4-31b-it", name: "Gemma 4 31B", tier: tA, sweScore: 40.0, ctxSize: 262144, thinking: false, multimodal: true),
  ModelMeta(id: "openai/gpt-oss-20b", name: "GPT OSS 20B", tier: tA, sweScore: 42.0, ctxSize: 131072, thinking: false, multimodal: false),

  # A- tier (35-40%)
  ModelMeta(id: "meta/llama-3.3-70b-instruct", name: "Llama 3.3 70B", tier: tAMinus, sweScore: 39.5, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "bytedance/seed-oss-36b-instruct", name: "Seed OSS 36B", tier: tAMinus, sweScore: 38.0, ctxSize: 32768, thinking: false, multimodal: false),
  ModelMeta(id: "deepseek-ai/deepseek-r1-distill-qwen-14b", name: "R1 Distill 14B", tier: tAMinus, sweScore: 37.7, ctxSize: 65536, thinking: true, multimodal: false),
  ModelMeta(id: "meta/llama-3.1-70b-instruct", name: "Llama 3.1 70B", tier: tAMinus, sweScore: 37.0, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "nvidia/llama-3.1-nemotron-51b-instruct", name: "Nemotron 51B", tier: tAMinus, sweScore: 36.0, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "stockmark/stockmark-2-100b-instruct", name: "Stockmark 100B", tier: tAMinus, sweScore: 36.0, ctxSize: 32768, thinking: false, multimodal: false),

  # B+ tier (30-35%)
  ModelMeta(id: "mistralai/codestral-22b-instruct-v0.1", name: "Codestral 22B", tier: tBPlus, sweScore: 34.0, ctxSize: 32768, thinking: false, multimodal: false),
  ModelMeta(id: "mistralai/ministral-14b-instruct-2512", name: "Ministral 14B", tier: tBPlus, sweScore: 34.0, ctxSize: 262144, thinking: false, multimodal: false),
  ModelMeta(id: "mistralai/mixtral-8x22b-instruct-v0.1", name: "Mixtral 8x22B", tier: tBPlus, sweScore: 32.0, ctxSize: 65536, thinking: false, multimodal: false),
  ModelMeta(id: "mistralai/mistral-small-3.1-24b-instruct-2503", name: "Mistral Small 3.1", tier: tBPlus, sweScore: 31.0, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "ibm/granite-34b-code-instruct", name: "Granite 34B Code", tier: tBPlus, sweScore: 30.0, ctxSize: 32768, thinking: false, multimodal: false),

  # B tier (20-30%)
  ModelMeta(id: "mistralai/mistral-large-2-instruct", name: "Mistral Large 2", tier: tB, sweScore: 29.7, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "meta/llama3-70b-instruct", name: "Llama3 70B", tier: tB, sweScore: 25.0, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "google/gemma-3-27b-it", name: "Gemma 3 27B", tier: tB, sweScore: 23.0, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "deepseek-ai/deepseek-r1-distill-llama-8b", name: "R1 Distill 8B", tier: tB, sweScore: 28.2, ctxSize: 32768, thinking: true, multimodal: false),
  ModelMeta(id: "meta/llama-3.2-11b-vision-instruct", name: "Llama 3.2 11B Vision", tier: tB, sweScore: 21.0, ctxSize: 131072, thinking: false, multimodal: true),
  ModelMeta(id: "deepseek-ai/deepseek-r1-distill-qwen-7b", name: "R1 Distill 7B", tier: tB, sweScore: 22.6, ctxSize: 32768, thinking: true, multimodal: false),

  # C tier (<20%)
  ModelMeta(id: "microsoft/phi-3.5-moe-instruct", name: "Phi 3.5 MoE", tier: tC, sweScore: 16.0, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "google/gemma-2-9b-it", name: "Gemma 2 9B", tier: tC, sweScore: 15.0, ctxSize: 8192, thinking: false, multimodal: false),
  ModelMeta(id: "qwen/qwen2.5-coder-7b-instruct", name: "Qwen2.5 Coder 7B", tier: tC, sweScore: 14.0, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "microsoft/phi-4-mini-instruct", name: "Phi 4 Mini", tier: tC, sweScore: 14.0, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "google/gemma-3-12b-it", name: "Gemma 3 12B", tier: tC, sweScore: 13.0, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "nvidia/nvidia-nemotron-nano-9b-v2", name: "Nemotron Nano 9B v2", tier: tC, sweScore: 12.0, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "google/gemma-2-27b-it", name: "Gemma 2 27B", tier: tC, sweScore: 12.0, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "microsoft/phi-3.5-mini-instruct", name: "Phi 3.5 Mini", tier: tC, sweScore: 12.0, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "microsoft/phi-3.5-vision-instruct", name: "Phi 3.5 Vision", tier: tC, sweScore: 10.0, ctxSize: 131072, thinking: false, multimodal: true),
  ModelMeta(id: "microsoft/phi-3-vision-128k-instruct", name: "Phi 3 Vision", tier: tC, sweScore: 10.0, ctxSize: 131072, thinking: false, multimodal: true),
  ModelMeta(id: "microsoft/phi-3-medium-128k-instruct", name: "Phi 3 Medium 128k", tier: tC, sweScore: 10.0, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "nvidia/mistral-nemo-minitron-8b-8k-instruct", name: "Nemo Minitron 8B", tier: tC, sweScore: 9.0, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "microsoft/phi-3-medium-4k-instruct", name: "Phi 3 Medium 4k", tier: tC, sweScore: 9.0, ctxSize: 4096, thinking: false, multimodal: false),
  ModelMeta(id: "meta/llama3-8b-instruct", name: "Llama3 8B", tier: tC, sweScore: 8.0, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "microsoft/phi-3-small-128k-instruct", name: "Phi 3 Small 128k", tier: tC, sweScore: 7.0, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "nvidia/llama3-chatqa-1.5-70b", name: "ChatQA 1.5 70B", tier: tC, sweScore: 7.0, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "deepseek-ai/deepseek-coder-6.7b-instruct", name: "DeepSeek Coder 6.7B", tier: tC, sweScore: 6.0, ctxSize: 16384, thinking: false, multimodal: false),
  ModelMeta(id: "microsoft/phi-3-small-8k-instruct", name: "Phi 3 Small 8k", tier: tC, sweScore: 5.0, ctxSize: 8192, thinking: false, multimodal: false),
  ModelMeta(id: "meta/llama-3.2-1b-instruct", name: "Llama 3.2 1B", tier: tC, sweScore: 3.0, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "google/gemma-3n-e4b-it", name: "Gemma 3n E4B", tier: tC, sweScore: 3.0, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "google/gemma-3-1b-it", name: "Gemma 3 1B", tier: tC, sweScore: 2.0, ctxSize: 32768, thinking: false, multimodal: false),
  ModelMeta(id: "google/gemma-2-2b-it", name: "Gemma 2 2B", tier: tC, sweScore: 2.0, ctxSize: 131072, thinking: false, multimodal: false),
  ModelMeta(id: "google/gemma-3n-e2b-it", name: "Gemma 3n E2B", tier: tC, sweScore: 2.0, ctxSize: 131072, thinking: false, multimodal: false),
]

proc lookupMeta*(catalog: seq[ModelMeta], id: string): Option[ModelMeta] =
  for m in catalog:
    if m.id == id: return some(m)
  none(ModelMeta)

proc filterByTier*(catalog: seq[ModelMeta], tierLetter: string): seq[ModelMeta] =
  if tierLetter.len == 0: return catalog
  let letter = tierLetter.toUpperAscii()
  for m in catalog:
    if $m.tier.tierFamily == letter or $m.tier == letter:
      result.add(m)

proc catalogModelIds*(catalog: seq[ModelMeta]): seq[string] =
  for m in catalog:
    result.add(m.id)

proc loadUserModels*(path: string = ""): seq[ModelMeta] =
  ## Load additional models from user's models.json file.
  ## User models override built-in entries with the same ID.
  let p = if path.len > 0: path
          else: getHomeDir() / ".config" / "nimakai" / "models.json"
  if not fileExists(p): return @[]
  try:
    let data = parseJson(readFile(p))
    var modelsJson: JsonNode
    if data.kind == JObject and data.hasKey("models"):
      modelsJson = data["models"]
    else:
      modelsJson = data
    if modelsJson.kind == JArray:
      for item in modelsJson:
        var m: ModelMeta
        m.id = item["id"].getStr()
        m.name = item{"name"}.getStr(m.id)
        let tierStr = item{"tier"}.getStr("B")
        case tierStr
        of "S+": m.tier = tSPlus
        of "S": m.tier = tS
        of "A+": m.tier = tAPlus
        of "A": m.tier = tA
        of "A-": m.tier = tAMinus
        of "B+": m.tier = tBPlus
        of "B": m.tier = tB
        of "C": m.tier = tC
        else: m.tier = tB
        m.sweScore = item{"sweScore"}.getFloat(0.0)
        m.ctxSize = item{"ctxSize"}.getInt(131072)
        m.thinking = item{"thinking"}.getBool(false)
        m.multimodal = item{"multimodal"}.getBool(false)
        result.add(m)
  except CatchableError:
    discard

proc loadCatalog*(userModelsPath: string = ""): seq[ModelMeta] =
  ## Load the full catalog: built-in models + user overrides.
  result = BuiltinCatalog
  let userModels = loadUserModels(userModelsPath)
  for um in userModels:
    var found = false
    for i in 0..<result.len:
      if result[i].id == um.id:
        result[i] = um
        found = true
        break
    if not found:
      result.add(um)

proc buildCatalogIndex*(catalog: seq[ModelMeta]): Table[string, ModelMeta] =
  ## Build an O(1) lookup table from model ID to metadata.
  result = initTable[string, ModelMeta]()
  for m in catalog:
    result[m.id] = m

proc lookupMeta*(index: Table[string, ModelMeta], id: string): Option[ModelMeta] =
  ## O(1) lookup by model ID from a pre-built index.
  if id in index:
    some(index[id])
  else:
    none(ModelMeta)

proc printCatalogJson*(catalog: seq[ModelMeta]) =
  ## Print the catalog as JSON.
  var arr = newJArray()
  for m in catalog:
    let ctxStr = if m.ctxSize >= 1048576: $(m.ctxSize div 1048576) & "M"
                 elif m.ctxSize >= 1024: $(m.ctxSize div 1024) & "k"
                 else: $m.ctxSize
    arr.add(%*{
      "id": m.id,
      "name": m.name,
      "tier": $m.tier,
      "swe_score": m.sweScore,
      "ctx_size": m.ctxSize,
      "ctx_display": ctxStr,
      "output_limit": m.outputLimit,
      "thinking": m.thinking,
      "multimodal": m.multimodal,
    })
  echo $(%*{"models": arr, "count": catalog.len})

proc printCatalog*(catalog: seq[ModelMeta]) =
  ## Print the catalog as a formatted table.
  echo ""
  echo "\e[1m nimakai v" & Version & "\e[0m  \e[90mmodel catalog | " & $catalog.len & " NVIDIA NIM models\e[0m"
  echo ""

  let header = "  " &
    padRight("MODEL", 35) &
    padLeft("TIER", 5) &
    padLeft("SWE%", 7) &
    padLeft("CTX", 8) &
    "  " &
    padRight("CAPS", 12) &
    padRight("ID", 45)
  echo "\e[1;90m" & header & "\e[0m"
  echo "\e[90m  " & "-".repeat(112) & "\e[0m"

  var sorted = catalog
  sorted.sort(proc(a, b: ModelMeta): int =
    let ta = tierOrd(a.tier)
    let tb = tierOrd(b.tier)
    if ta != tb: return ta - tb
    if a.sweScore != b.sweScore: return (if a.sweScore > b.sweScore: -1 else: 1)
    return cmp(a.name, b.name)
  )

  for m in sorted:
    let tc = case m.tier
      of tSPlus: "\e[32;1m"
      of tS: "\e[32m"
      of tAPlus: "\e[36m"
      of tA: "\e[36m"
      of tAMinus: "\e[33m"
      of tBPlus: "\e[33m"
      of tB: "\e[90m"
      of tC: "\e[90m"

    let ctxStr = if m.ctxSize >= 1048576: $(m.ctxSize div 1048576) & "M"
                 elif m.ctxSize >= 1024: $(m.ctxSize div 1024) & "k"
                 else: $m.ctxSize

    var caps = ""
    if m.thinking: caps &= "think "
    if m.multimodal: caps &= "vision "
    if caps.len == 0: caps = "-"

    echo "  " &
      padRight(m.name, 35) &
      tc & padLeft($m.tier, 5) & "\e[0m" &
      padLeft($m.sweScore & "%", 7) &
      padLeft(ctxStr, 8) &
      "  " &
      padRight(caps.strip(), 12) &
      "\e[90m" & padRight(m.id, 45) & "\e[0m"

  echo ""
