## Tests for model discovery and catalog diff.

import std/[unittest, json]
import nimakai/[types, discovery]

suite "parseDiscoverResponse":
  test "parses valid response":
    let body = $(%*{
      "data": [
        {"id": "model/a", "owned_by": "org-a", "created": 1000},
        {"id": "model/b", "owned_by": "org-b", "created": 2000},
      ]
    })
    let models = parseDiscoverResponse(body)
    check models.len == 2
    check models[0].id == "model/a"
    check models[0].ownedBy == "org-a"
    check models[0].created == 1000
    check models[1].id == "model/b"

  test "returns empty for invalid JSON":
    let models = parseDiscoverResponse("not json")
    check models.len == 0

  test "returns empty for missing data key":
    let body = $(%*{"models": []})
    let models = parseDiscoverResponse(body)
    check models.len == 0

  test "handles missing optional fields":
    let body = $(%*{
      "data": [{"id": "model/c"}]
    })
    let models = parseDiscoverResponse(body)
    check models.len == 1
    check models[0].id == "model/c"
    check models[0].ownedBy == ""
    check models[0].created == 0

suite "diffCatalog":
  let catalog = @[
    ModelMeta(id: "model/a", name: "Model A", sweScore: 65.0, ctxSize: 131072),
    ModelMeta(id: "model/b", name: "Model B", sweScore: 45.0, ctxSize: 32768),
    ModelMeta(id: "model/c", name: "Model C", sweScore: 25.0, ctxSize: 32768),
  ]

  test "all matched when identical":
    let discovered = @[
      DiscoveredModel(id: "model/a"),
      DiscoveredModel(id: "model/b"),
      DiscoveredModel(id: "model/c"),
    ]
    let diff = diffCatalog(discovered, catalog)
    check diff.matchedModels.len == 3
    check diff.newModels.len == 0
    check diff.missingModels.len == 0

  test "detects new models not in catalog":
    let discovered = @[
      DiscoveredModel(id: "model/a"),
      DiscoveredModel(id: "model/new"),
    ]
    let diff = diffCatalog(discovered, catalog)
    check diff.newModels == @["model/new"]
    check diff.matchedModels == @["model/a"]
    check diff.missingModels.len == 2  # model/b and model/c

  test "detects missing catalog models":
    let discovered = @[DiscoveredModel(id: "model/a")]
    let diff = diffCatalog(discovered, catalog)
    check diff.matchedModels == @["model/a"]
    check diff.missingModels.len == 2  # model/b and model/c
    check "model/b" in diff.missingModels
    check "model/c" in diff.missingModels

  test "empty discovered means all catalog missing":
    let diff = diffCatalog(@[], catalog)
    check diff.matchedModels.len == 0
    check diff.newModels.len == 0
    check diff.missingModels.len == 3

  test "empty catalog means all discovered are new":
    let discovered = @[
      DiscoveredModel(id: "model/x"),
      DiscoveredModel(id: "model/y"),
    ]
    let diff = diffCatalog(discovered, @[])
    check diff.newModels.len == 2
    check diff.matchedModels.len == 0
    check diff.missingModels.len == 0


suite "parseDiscoverResponse fuzz and edge cases":
  test "returns empty for empty string input":
    let models = parseDiscoverResponse("")
    check models.len == 0

  test "returns empty when root is a JSON array (no data key)":
    let models = parseDiscoverResponse("[]")
    check models.len == 0

  test "returns empty when data is empty array":
    let body = $(%*{"data": []})
    let models = parseDiscoverResponse(body)
    check models.len == 0

  test "entry with empty string id is preserved":
    # id explicitly set to empty string
    let body = $(%*{"data": [{"id": "", "owned_by": "org-x"}]})
    let models = parseDiscoverResponse(body)
    check models.len == 1
    check models[0].id == ""
    check models[0].ownedBy == "org-x"

  test "handles extra unknown fields gracefully":
    let body = $(%*{
      "data": [{"id": "model/z", "owned_by": "org", "created": 42, "extra": "ignored"}]
    })
    let models = parseDiscoverResponse(body)
    check models.len == 1
    check models[0].id == "model/z"
    check models[0].created == 42

  test "returns empty for truncated/malformed JSON":
    let models = parseDiscoverResponse("{\"data\": [{\"id\"")
    check models.len == 0

  test "large batch parses all entries":
    var items = newJArray()
    for i in 0..<50:
      items.add(%*{"id": "model/" & $i, "owned_by": "org", "created": i})
    let body = $(%*{"data": items})
    let models = parseDiscoverResponse(body)
    check models.len == 50
    check models[0].id == "model/0"
    check models[49].id == "model/49"

suite "diffCatalog edge cases":
  test "duplicated discovered IDs: matched twice, catalog-missing stays zero":
    let cat2 = @[ModelMeta(id: "model/a", name: "A", sweScore: 50.0, ctxSize: 131072)]
    let disc2 = @[
      DiscoveredModel(id: "model/a"),
      DiscoveredModel(id: "model/a"),
    ]
    let diff2 = diffCatalog(disc2, cat2)
    check diff2.missingModels.len == 0
    check diff2.newModels.len == 0

  test "single model in both returns one matched":
    let cat3 = @[ModelMeta(id: "solo", name: "Solo", sweScore: 0.0, ctxSize: 131072)]
    let disc3 = @[DiscoveredModel(id: "solo")]
    let diff3 = diffCatalog(disc3, cat3)
    check diff3.matchedModels == @["solo"]
    check diff3.newModels.len == 0
    check diff3.missingModels.len == 0