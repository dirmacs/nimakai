## Fetch and update model catalog from NVIDIA NIM API.
import std/[httpclient, json, strutils, net, os]
import ./types, ./catalog

type
  FetchResult* = object
    totalFetched*: int
    newModels*: seq[ModelMeta]
    existingModels*: seq[string]
    allModels*: seq[string]
    apiError*: string

proc containsIgnoreCase*(s: string, substr: string): bool =
  ## Check if substring exists (case-insensitive).
  toLowerAscii(s).contains(toLowerAscii(substr))

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

proc fetchModelsFromAPI*(apiKey: string, timeout: int = 15): FetchResult =
  ## Fetch available models from NVIDIA NIM API.
  let sslCtx = newContext(verifyMode = CVerifyPeer)
  let client = newHttpClient(timeout = timeout * 1000, sslContext = sslCtx)
  client.headers = newHttpHeaders({
    "Authorization": "Bearer " & apiKey
  })

  try:
    let resp = client.get(ModelsURL)
    let code = resp.code.int
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

          meta.sweScore = 0.0
          meta.outputLimit = 0

          result.newModels.add(meta)
        else:
          result.existingModels.add(modelId)

      # Deduplicate model IDs before assigning to allModels
      var uniqueIds: seq[string] = @[]
      for id in discoveredIds:
        if id notin uniqueIds:
          uniqueIds.add(id)
      result.allModels = uniqueIds
      client.close()
    else:
      client.close()
      result.apiError = "No 'data' field in API response"

  except CatchableError as e:
    try: client.close()
    except CatchableError: discard
    result.apiError = "Error fetching models: " & e.msg

proc updateUserModels*(newModels: seq[ModelMeta],
  filePath: string = ""): int =
  ## Add new models to user's models.json file.
  ## Returns number of models added.
  let path = if filePath.len > 0: filePath
  else: getHomeDir() / ".config" / "nimakai" / "models.json"

  var existingModels: seq[ModelMeta] = @[]
  if fileExists(path):
    try:
      let root = parseJson(readFile(path))
      let data = if root.hasKey("models"): root["models"]
      else: root
      for item in data:
        var m: ModelMeta
        m.id = item["id"].getStr()
        m.name = item{"name"}.getStr(m.id)
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
        "swe_score": m.sweScore,
        "ctx_size": m.ctxSize,
        "ctx_display": ctxStr,
        "thinking": m.thinking,
        "multimodal": m.multimodal,
      })

    let j = %*{
      "total_fetched": result.totalFetched,
      "new_models": newModelsJson.len,
      "new_models_list": newModelsJson,
    }
    echo $(j)
  else:
    echo ""
    echo "\e[1m nimakai\e[0m v" & Version & "\e[0m \e[90mfetch results\e[0m"
    echo ""
    echo " \e[1mTotal models fetched:\e[0m " & $result.totalFetched
    echo " \e[1mNew models found:\e[0m " & $result.newModels.len
    echo ""

    if result.newModels.len > 0:
      echo "\e[1mNew models:\e[0m"
      echo ""
      let header = " " &
        padRight("MODEL", 35) &
        padLeft("CTX", 8) &
        " " &
        padRight("CAPS", 12) &
        padRight("ID", 45)
      echo "\e[1;90m" & header & "\e[0m"
      echo "\e[90m " & "-".repeat(100) & "\e[0m"

      for m in result.newModels:
        let ctxStr = if m.ctxSize >= 1048576: $(m.ctxSize div 1048576) & "M"
        elif m.ctxSize >= 1024: $(m.ctxSize div 1024) & "k"
        else: $m.ctxSize

        var caps = ""
        if m.thinking: caps &= "think "
        if m.multimodal: caps &= "vision "
        if caps.len == 0: caps = "-"

        echo " " &
          padRight(m.name, 35) &
          padLeft(ctxStr, 8) &
          " " &
          padRight(caps.strip(), 12) &
          "\e[90m" & padRight(m.id, 45) & "\e[0m"
        echo ""
