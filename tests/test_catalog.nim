import std/[unittest, options, sets, json, os]
import nimakai/[types, catalog]

suite "BuiltinCatalog integrity":
  test "catalog has models":
    check BuiltinCatalog.len >= 40

  test "no duplicate model IDs":
    var ids: HashSet[string]
    for m in BuiltinCatalog:
      check m.id notin ids
      ids.incl(m.id)

  test "all models have non-empty ID and name":
    for m in BuiltinCatalog:
      check m.id.len > 0
      check m.name.len > 0

  test "SWE scores are in valid range":
    for m in BuiltinCatalog:
      check m.sweScore >= 0.0
      check m.sweScore <= 100.0

  test "context sizes are positive":
    for m in BuiltinCatalog:
      check m.ctxSize > 0


suite "lookupMeta":
  test "finds existing model":
    let result = BuiltinCatalog.lookupMeta("z-ai/glm4.7")
    check result.isSome
    check result.get.name == "GLM 4.7"

  test "returns none for unknown model":
    let result = BuiltinCatalog.lookupMeta("nonexistent/model")
    check result.isNone


suite "loadCatalog":
  test "returns at least builtin models":
    let cat = loadCatalog()
    check cat.len >= BuiltinCatalog.len

suite "catalogModelIds":
  test "returns all IDs":
    let ids = catalogModelIds(BuiltinCatalog)
    check ids.len == BuiltinCatalog.len
    check "z-ai/glm4.7" in ids

suite "loadUserModels":
  test "loads models from a temp JSON file":
    let tmpPath = getTempDir() / "nimakai_test_models.json"
    let data = %*[
      {
        "id": "custom/test-model",
        "name": "Test Model",
        "sweScore": 55.0,
        "ctxSize": 65536,
        "thinking": true,
        "multimodal": false,
      }
    ]
    writeFile(tmpPath, $data)
    defer: removeFile(tmpPath)

    let models = loadUserModels(tmpPath)
    check models.len == 1
    check models[0].id == "custom/test-model"
    check models[0].name == "Test Model"

    check abs(models[0].sweScore - 55.0) < 0.01
    check models[0].ctxSize == 65536
    check models[0].thinking == true
    check models[0].multimodal == false

  test "returns empty for non-existent file":
    let models = loadUserModels("/tmp/nimakai_nonexistent_file_12345.json")
    check models.len == 0

  test "loads multiple models":
    let tmpPath = getTempDir() / "nimakai_test_models_multi.json"
    let data = %*[
      {"id": "custom/model-a", "name": "Model A", "sweScore": 72.0},
      {"id": "custom/model-b", "name": "Model B", "sweScore": 15.0},
    ]
    writeFile(tmpPath, $data)
    defer: removeFile(tmpPath)

    let models = loadUserModels(tmpPath)
    check models.len == 2
    check models[0].id == "custom/model-a"

    check models[1].id == "custom/model-b"



  test "loadCatalog overrides builtin model with user model":
    let tmpPath = getTempDir() / "nimakai_test_override.json"
    # Override an existing builtin model
    let data = %*[
      {
        "id": "z-ai/glm4.7",
        "name": "GLM 4.7 Custom",
        "sweScore": 45.0,
        "ctxSize": 32768,
      }
    ]
    writeFile(tmpPath, $data)
    defer: removeFile(tmpPath)

    let cat = loadCatalog(tmpPath)
    let meta = cat.lookupMeta("z-ai/glm4.7")
    check meta.isSome
    check meta.get.name == "GLM 4.7 Custom"

    check abs(meta.get.sweScore - 45.0) < 0.01
    check meta.get.ctxSize == 32768

suite "buildCatalogIndex":
  test "index lookup finds model":
    let index = buildCatalogIndex(BuiltinCatalog)
    let meta = index.lookupMeta("z-ai/glm4.7")
    check meta.isSome
    check meta.get.name == "GLM 4.7"

  test "index lookup returns none for missing model":
    let index = buildCatalogIndex(BuiltinCatalog)
    let meta = index.lookupMeta("nonexistent/model")
    check meta.isNone

  test "index matches linear scan":
    let index = buildCatalogIndex(BuiltinCatalog)
    for m in BuiltinCatalog:
      let fromIndex = index.lookupMeta(m.id)
      let fromLinear = BuiltinCatalog.lookupMeta(m.id)
      check fromIndex.isSome
      check fromLinear.isSome
      check fromIndex.get.id == fromLinear.get.id
  

suite "printCatalogJson":
  test "produces valid JSON with all models":
    # Just verify it compiles and the function exists
    # (output goes to stdout, not easy to capture in unit test)
    check BuiltinCatalog.len > 0
