## Live model discovery from NVIDIA API.
## Compares available models against the built-in catalog.

import std/[httpclient, json, strutils, net, algorithm]
import ./types

type
  DiscoveredModel* = object
    id*: string
    ownedBy*: string
    created*: int

proc discoverModels*(apiKey: string, timeout: int = 15): seq[DiscoveredModel] =
  ## Fetch available models from NVIDIA API's /v1/models endpoint.
  let sslCtx = newContext(verifyMode = CVerifyPeer)
  let client = newHttpClient(timeout = timeout * 1000, sslContext = sslCtx)
  client.headers = newHttpHeaders({
    "Authorization": "Bearer " & apiKey
  })

  try:
    let resp = client.get("https://integrate.api.nvidia.com/v1/models")
    let code = parseInt($resp.code)
    if code != 200:
      client.close()
      return @[]

    let body = parseJson(resp.body)
    if body.hasKey("data"):
      for item in body["data"]:
        result.add(DiscoveredModel(
          id: item["id"].getStr(),
          ownedBy: item{"owned_by"}.getStr(""),
          created: item{"created"}.getInt(0),
        ))
    client.close()
  except CatchableError:
    try: client.close()
    except CatchableError: discard

proc parseDiscoverResponse*(body: string): seq[DiscoveredModel] =
  ## Parse a /v1/models JSON response body into DiscoveredModel objects.
  ## Useful for testing without network access.
  try:
    let data = parseJson(body)
    if data.kind == JObject and data.hasKey("data"):
      for item in data["data"]:
        result.add(DiscoveredModel(
          id: item["id"].getStr(),
          ownedBy: item{"owned_by"}.getStr(""),
          created: item{"created"}.getInt(0),
        ))
  except CatchableError:
    discard

type
  CatalogDiff* = object
    newModels*: seq[string]       ## models in API but not catalog
    missingModels*: seq[string]   ## models in catalog but not API
    matchedModels*: seq[string]   ## models in both

proc diffCatalog*(discovered: seq[DiscoveredModel],
                  catalog: seq[ModelMeta]): CatalogDiff =
  ## Compare discovered models against the built-in catalog.
  var catalogIds: seq[string] = @[]
  for m in catalog:
    catalogIds.add(m.id)

  var discoveredIds: seq[string] = @[]
  for d in discovered:
    discoveredIds.add(d.id)

  for d in discoveredIds:
    if d in catalogIds:
      result.matchedModels.add(d)
    else:
      result.newModels.add(d)

  for c in catalogIds:
    if c notin discoveredIds:
      result.missingModels.add(c)

  result.newModels.sort()
  result.missingModels.sort()
  result.matchedModels.sort()

proc printDiscovery*(discovered: seq[DiscoveredModel],
                     catalog: seq[ModelMeta]) =
  ## Print a formatted comparison of discovered vs cataloged models.
  let diff = diffCatalog(discovered, catalog)

  echo ""
  echo "\e[1m nimakai v" & Version & "\e[0m  \e[90mdiscovery | " &
       $discovered.len & " API models | " & $catalog.len & " cataloged\e[0m"
  echo ""

  if diff.newModels.len > 0:
    echo "\e[32m  New models (not in catalog):\e[0m"
    for m in diff.newModels:
      echo "    + " & m
    echo ""

  if diff.missingModels.len > 0:
    echo "\e[33m  Catalog-only (not in API):\e[0m"
    for m in diff.missingModels:
      echo "    - " & m
    echo ""

  echo "\e[90m  Matched: " & $diff.matchedModels.len & " models in both\e[0m"
  echo ""

proc discoveryToJson*(discovered: seq[DiscoveredModel],
                      catalog: seq[ModelMeta]): string =
  ## Output discovery results as JSON.
  let diff = diffCatalog(discovered, catalog)
  let j = %*{
    "discovered": discovered.len,
    "cataloged": catalog.len,
    "matched": diff.matchedModels.len,
    "new_models": diff.newModels,
    "missing_models": diff.missingModels,
  }
  $j


proc syncFromProxy*(proxyPort: int = 8080, timeoutMs: int = 3000): seq[DiscoveredModel] =
  ## Fetch models from a locally-running nimaproxy /v1/models endpoint.
  ## Returns empty on any error (proxy not running is the common case).
  let client = newHttpClient(timeout = timeoutMs)
  try:
    let url = "http://127.0.0.1:" & $proxyPort & "/v1/models"
    let resp = client.get(url)
    let code = resp.code.int
    if code != 200:
      client.close()
      return @[]
    result = parseDiscoverResponse(resp.body)
    client.close()
  except CatchableError:
    try: client.close()
    except CatchableError: discard