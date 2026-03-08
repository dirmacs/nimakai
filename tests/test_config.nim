import std/[unittest, os, json]
import nimakai/[types, config]

suite "loadConfigFile":
  test "returns defaults when file doesn't exist":
    let cfg = loadConfigFile("/tmp/nonexistent-nimakai-config.json")
    check cfg.interval == DefaultInterval
    check cfg.timeout == DefaultTimeout
    check cfg.models.len == 0
    check cfg.tierFilter == ""
    check cfg.thresholds == DefaultThresholds
    check cfg.favorites.len == 0

  test "loads config from file":
    let path = "/tmp/test-nimakai-config.json"
    let data = %*{
      "interval": 3,
      "timeout": 10,
      "models": ["model/a", "model/b"],
      "favorites": ["model/a"],
      "thresholds": {
        "perfect_avg": 300,
        "spike_ms": 2000,
      }
    }
    writeFile(path, $data)
    defer: removeFile(path)

    let cfg = loadConfigFile(path)
    check cfg.interval == 3
    check cfg.timeout == 10
    check cfg.models.len == 2
    check cfg.models[0] == "model/a"
    check cfg.favorites.len == 1
    check cfg.favorites[0] == "model/a"
    check cfg.thresholds.perfectAvg == 300.0
    check cfg.thresholds.spikeMs == 2000.0
    # Unset thresholds keep defaults
    check cfg.thresholds.perfectP95 == 800.0

suite "saveConfigFile":
  test "creates config file":
    let path = "/tmp/test-nimakai-save.json"
    defer:
      if fileExists(path): removeFile(path)
      let dir = parentDir(path)
      # Don't try to remove /tmp

    saveConfigFile(path, favorites = @["model/x"],
                   interval = 7, timeout = 20)
    check fileExists(path)

    let data = parseJson(readFile(path))
    check data["interval"].getInt() == 7
    check data["timeout"].getInt() == 20
    check data["favorites"][0].getStr() == "model/x"

suite "saveFavorites":
  test "updates favorites in existing config":
    let path = "/tmp/test-nimakai-favs.json"
    writeFile(path, $(%*{"interval": 5}))
    defer: removeFile(path)

    saveFavorites(path, @["model/a", "model/b"])

    let data = parseJson(readFile(path))
    check data["interval"].getInt() == 5 # preserved
    check data["favorites"].len == 2
    check data["favorites"][0].getStr() == "model/a"

  test "creates file if not exists":
    let path = "/tmp/test-nimakai-favs-new.json"
    defer:
      if fileExists(path): removeFile(path)

    saveFavorites(path, @["model/x"])

    check fileExists(path)
    let data = parseJson(readFile(path))
    check data["favorites"][0].getStr() == "model/x"

suite "loadConfigFile category_weights":
  test "loads category weights from config":
    let path = "/tmp/test-nimakai-catw.json"
    let data = %*{
      "category_weights": {
        "quick": {"swe": 0.10, "speed": 0.60, "ctx": 0.10, "stability": 0.20}
      }
    }
    writeFile(path, $data)
    defer: removeFile(path)

    let cfg = loadConfigFile(path)
    check cfg.categoryWeights.len == 1
    check cfg.categoryWeights[0].category == "quick"
    check cfg.categoryWeights[0].weights.swe == 0.10
    check cfg.categoryWeights[0].weights.speed == 0.60
    check cfg.categoryWeights[0].weights.ctx == 0.10
    check cfg.categoryWeights[0].weights.stability == 0.20

  test "returns empty weights when not in config":
    let path = "/tmp/test-nimakai-catw-empty.json"
    let data = %*{"interval": 5}
    writeFile(path, $data)
    defer: removeFile(path)

    let cfg = loadConfigFile(path)
    check cfg.categoryWeights.len == 0

  test "loads multiple category weights":
    let path = "/tmp/test-nimakai-catw-multi.json"
    let data = %*{
      "category_weights": {
        "quick": {"swe": 0.10, "speed": 0.60, "ctx": 0.10, "stability": 0.20},
        "deep": {"swe": 0.50, "speed": 0.05, "ctx": 0.25, "stability": 0.20}
      }
    }
    writeFile(path, $data)
    defer: removeFile(path)

    let cfg = loadConfigFile(path)
    check cfg.categoryWeights.len == 2

    # Find each category (JSON key order not guaranteed)
    var foundQuick, foundDeep = false
    for cw in cfg.categoryWeights:
      if cw.category == "quick":
        foundQuick = true
        check cw.weights.swe == 0.10
        check cw.weights.speed == 0.60
        check cw.weights.ctx == 0.10
        check cw.weights.stability == 0.20
      elif cw.category == "deep":
        foundDeep = true
        check cw.weights.swe == 0.50
        check cw.weights.speed == 0.05
        check cw.weights.ctx == 0.25
        check cw.weights.stability == 0.20
    check foundQuick
    check foundDeep

suite "loadProfile":
  test "loads profile from config":
    let path = "/tmp/test-nimakai-profile.json"
    let data = %*{
      "profiles": {
        "work": {"interval": 10, "tier_filter": "S", "rounds": 5}
      }
    }
    writeFile(path, $data)
    defer: removeFile(path)

    let prof = loadProfile("work", path)
    check prof.hasInterval == true
    check prof.interval == 10
    check prof.hasTierFilter == true
    check prof.tierFilter == "S"
    check prof.hasRounds == true
    check prof.rounds == 5
    check prof.hasTimeout == false

  test "unknown profile returns empty overrides":
    let path = "/tmp/test-nimakai-profile-unk.json"
    let data = %*{
      "profiles": {
        "work": {"interval": 10}
      }
    }
    writeFile(path, $data)
    defer: removeFile(path)

    let prof = loadProfile("nonexistent", path)
    check prof.hasInterval == false
    check prof.hasTimeout == false
    check prof.hasTierFilter == false
    check prof.hasRounds == false

  test "partial profile settings":
    let path = "/tmp/test-nimakai-profile-partial.json"
    let data = %*{
      "profiles": {
        "fast": {"timeout": 5}
      }
    }
    writeFile(path, $data)
    defer: removeFile(path)

    let prof = loadProfile("fast", path)
    check prof.hasTimeout == true
    check prof.timeout == 5
    check prof.hasInterval == false
    check prof.hasTierFilter == false

  test "missing profiles section returns empty":
    let path = "/tmp/test-nimakai-profile-noprof.json"
    let data = %*{"interval": 5}
    writeFile(path, $data)
    defer: removeFile(path)

    let prof = loadProfile("any", path)
    check prof.hasInterval == false
