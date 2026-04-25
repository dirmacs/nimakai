## Fetch and update model catalog from NVIDIA NIM API.

import std/[httpclient, json, strutils, net, algorithm, os, options, random]
import ./types, ./catalog

type
  FetchResult* = object
    totalFetched*: int
    newModels*: seq[ModelMeta]
    existingModels*: seq[string]
    apiError*: string

proc inferTierFromModelId*(modelId: string): Tier
proc inferCtxSizeFromModelId*(modelId: string): int
proc containsIgnoreCase*(s: string, substr: string): bool
proc estimateSweScore*(tier: Tier): float
proc inferOutputLimit*(tier: Tier): int



proc fetchModelsFromAPI*(apiKey: string, timeout: int = 15): FetchResult =
  ## Fetch available models from NVIDIA NIM API's /v1/models endpoint.
  let sslCtx = newContext(verifyMode = CVerifyPeer)
  let client = newHttpClient(timeout = timeout * 1000, sslContext = sslCtx)
  client.headers = newHttpHeaders({
    "Authorization": "Bearer " & apiKey
  })

  try:
    let resp = client.get(BaseURL & "/../models")
    let code = parseInt($resp.code)
    if code != 200:
      client.close()
      result.apiError = "API returned status code " & $code
      return

    let body = parseJson(resp.body)
    var discoveredIds: seq[string] = @[]
    
    if body.hasKey("data"):
      let data = body["data"]
      result.totalFetched = data.len()
      
      # Get existing catalog for comparison
      let existingCatalog = loadCatalog()
      var existingIds: seq[string] = @[]
      for m in existingCatalog:
        existingIds.add(m.id)
      
      for item in data:
        let modelId = item["id"].getStr()
        discoveredIds.add(modelId)
        
        # Check if this is a new model
        if modelId notin existingIds:
          var meta: ModelMeta
          meta.id = modelId
          meta.name = item{"name"}.getStr(modelId)
          
          # Determine tier based on model naming patterns and known models
          meta.tier = inferTierFromModelId(modelId)
          
          # Estimate context window from model name
          meta.ctxSize = inferCtxSizeFromModelId(modelId)
          
          # Check for thinking models
          meta.thinking = containsIgnoreCase(modelId, "thinking") or 
                          containsIgnoreCase(meta.name, "thinking") or
                          containsIgnoreCase(modelId, "reasoning") or
                          containsIgnoreCase(meta.name, "reasoning")
          
          # Check for multimodal models  
          meta.multimodal = containsIgnoreCase(modelId, "vision") or
                           containsIgnoreCase(meta.name, "vision") or
                           containsIgnoreCase(modelId, "vl") or
                           containsIgnoreCase(meta.name, "vl") or
                           containsIgnoreCase(meta.name, "multimodal")
          
          meta.sweScore = estimateSweScore(meta.tier)
          meta.outputLimit = inferOutputLimit(meta.tier)
          
          result.newModels.add(meta)
        else:
          result.existingModels.add(modelId)
      
      client.close()
    else:
      client.close()
      result.apiError = "No 'data' field in API response"
      
  except CatchableError as e:
    try: client.close()
    except CatchableError: discard
    result.apiError = "Error fetching models: " & e.msg

proc inferTierFromModelId*(modelId: string): Tier =
  ## Infer model tier based on ID patterns and known model families.
  ## This is a heuristic - for new models without SWE-bench scores.
  let id = toLowerAscii(modelId)
  
  # Known high-tier model families
  if "glm-5" in id or "glm5" in id:
    return tSPlus
  if "qwen3" in id and ("480b" in id or "235b" in id):
    return tSPlus
  if "deepseek-v3.2" in id or "deepseek-r1-0528" in id:
    return tSPlus
  if "kimi-k2.5" in id or "k2.5" in id:
    return tSPlus
  if "minimax-m2.5" in id or "minimax-m2.1" in id:
    return tSPlus
  if "step-3.5" in id:
    return tSPlus
  
  if "glm4.7" in id:
    return tSPlus
  if "qwen3-coder-480b" in id:
    return tSPlus
  
  # S tier models
  if "qwen3" in id and "122b" in id:
    return tS
  if "kimi-k2" in id and "instruct" in id:
    return tS
  if "mistral-large-3" in id:
    return tAPlus  # Not quite S tier
  if "llama-4-maverick" in id:
    return tS
  if "deepseek-r1" in id:
    return tS
  if "gpt-oss-120b" in id:
    return tS
  
  # A+ and A tier models
  if "nemotron-ultra" in id:
    return tAPlus
  if "nemotron-70b" in id:
    return tAPlus
  if "mistral-large-3" in id:
    return tAPlus
  if "llama-3.3-nemotron-super" in id:
    return tA
  if "llama-3.3-70b" in id:
    return tAMinus
  if "llama-3.1-70b" in id:
    return tAMinus
  if "llama-3.1-405b" in id:
    return tA
  
  # B tier and below
  if "gemma-3-27b" in id:
    return tB
  if "mixtral-8x22b" in id:
    return tBPlus
  if "mistral-small-3.1" in id:
    return tBPlus
  
  # Default to B tier for new models without clear indicators
  return tB

proc inferCtxSizeFromModelId*(modelId: string): int =
  ## Estimate context window size from model ID.
  ## Defaults to 131072 (128k) if not specified.
  let id = toLowerAscii(modelId)
  
  # Check for explicit context sizes
  if "16k" in id or "16384" in id:
    return 16384
  if "32k" in id or "32768" in id:
    return 32768
  if "64k" in id or "65536" in id:
    return 65536
  if "128k" in id or "131072" in id:
    return 131072
  if "256k" in id or "262144" in id:
    return 262144
  if "1m" in id or "1048576" in id:
    return 1048576
  if "10m" in id or "10485760" in id:
    return 10485760
  
  # Default to 128k
  return 131072

proc containsIgnoreCase*(s: string, substr: string): bool =
  ## Check if substring exists (case-insensitive).
  toLowerAscii(s).contains(toLowerAscii(substr))

proc estimateSweScore*(tier: Tier): float =
  ## Estimate SWE-bench score based on tier.
  case tier
  of tSPlus: 70.0 + rand(10.0)  # 70-80 range
  of tS: 60.0 + rand(10.0)      # 60-70 range
  of tAPlus: 55.0 + rand(5.0)   # 55-60 range
  of tA: 40.0 + rand(10.0)      # 40-50 range
  of tAMinus: 35.0 + rand(5.0)  # 35-40 range
  of tBPlus: 30.0 + rand(5.0)   # 30-35 range
  of tB: 20.0 + rand(10.0)      # 20-30 range
  of tC: 5.0 + rand(10.0)       # 5-15 range

proc inferOutputLimit*(tier: Tier): int =
  ## Estimate output token limit based on tier.
  case tier
  of tSPlus, tS, tAPlus, tA, tAMinus:
    return 8192
  of tBPlus, tB:
    return 4096
  of tC:
    return 2048

proc updateUserModels*(newModels: seq[ModelMeta], 
                       filePath: string = ""): int =
  ## Add new models to user's models.json file.
  ## Returns number of models added.
  let path = if filePath.len > 0: filePath
             else: getHomeDir() / ".config" / "nimakai" / "models.json"
  
  var existingModels: seq[ModelMeta] = @[]
  if fileExists(path):
    try:
      let data = parseJson(readFile(path))
      for item in data:
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
        existingModels.add(m)
    except CatchableError:
      discard
  
  # Add new models that aren't already in the file
  var modelsToAdd: seq[ModelMeta] = @[]
  for nm in newModels:
    var found = false
    for em in existingModels:
      if em.id == nm.id:
        found = true
        break
    if not found:
      modelsToAdd.add(nm)
  
  if modelsToAdd.len > 0:
    # Combine existing and new models
    var allModels = existingModels
    for m in modelsToAdd:
      allModels.add(m)
    
    # Write back to file
    createDir(path.splitPath().head)
    var jsonArr = newJArray()
    for m in allModels:
      jsonArr.add(%*{
        "id": m.id,
        "name": m.name,
        "tier": $m.tier,
        "sweScore": m.sweScore,
        "ctxSize": m.ctxSize,
        "thinking": m.thinking,
        "multimodal": m.multimodal,
      })
    
    writeFile(path, $(%*{"models": jsonArr}))
  
  return modelsToAdd.len

proc printFetchResults*(result: FetchResult, jsonOutput: bool = false) =
  ## Print fetch results in either table or JSON format.
  if jsonOutput:
    var newModelsJson = newJArray()
    for m in result.newModels:
      let ctxStr = if m.ctxSize >= 1048576: $(m.ctxSize div 1048576) & "M"
                   elif m.ctxSize >= 1024: $(m.ctxSize div 1024) & "k"
                   else: $m.ctxSize
      newModelsJson.add(%*{
        "id": m.id,
        "name": m.name,
        "tier": $m.tier,
        "swe_score": m.sweScore,
        "ctx_size": m.ctxSize,
        "ctx_display": ctxStr,
        "thinking": m.thinking,
        "multimodal": m.multimodal,
        "output_limit": m.outputLimit,
      })
    
    let j = %*{
      "total_fetched": result.totalFetched,
      "new_models": newModelsJson,
      "existing_models": result.existingModels,
      "new_count": result.newModels.len,
      "existing_count": result.existingModels.len,
    }
    if result.apiError.len > 0:
      j["error"] = %result.apiError
    echo $j
  else:
    echo ""
    echo "\e[1m nimakai v" & Version & "\e[0m  \e[90mmodel fetch\e[0m"
    echo ""
    
    if result.apiError.len > 0:
      echo "\e[31mError: " & result.apiError & "\e[0m"
      echo ""
      return
    
    echo "Fetched " & $result.totalFetched & " models from NVIDIA API"
    echo ""
    
    if result.newModels.len > 0:
      echo "\e[32m  New models not in catalog (" & $result.newModels.len & "):\e[0m"
      echo ""
      
      for m in result.newModels:
        let tc = case m.tier
        of tSPlus: "\e[32;1m"
        of tS: "\e[32m"
        of tAPlus: "\e[36m"
        of tA: "\e[36m"
        of tAMinus: "\e[33m"
        of tBPlus: "\e[33m"
        of tB: "\e[90m"
        of tC: "\e[90m"
        
        var caps = ""
        if m.thinking: caps &= "think "
        if m.multimodal: caps &= "vision "
        if caps.len == 0: caps = "-"
        
        let ctxStr = if m.ctxSize >= 1048576: $(m.ctxSize div 1048576) & "M"
                     elif m.ctxSize >= 1024: $(m.ctxSize div 1024) & "k"
                     else: $m.ctxSize
        
        echo "  + " & tc & padLeft($m.tier, 5) & "\e[0m  " &
             padRight(m.name, 35) & " " &
             padLeft(ctxStr, 8) & "  " &
             padRight(caps.strip(), 12) & " " &
             "\e[90m" & m.id & "\e[0m"
      echo ""
    else:
      echo "No new models found - catalog is up to date!"
      echo ""
    
    if result.existingModels.len > 0 and result.existingModels.len < 20:
      echo "\e[90m  Already cataloged: " & $result.existingModels.len & " models\e[0m"
      echo ""
