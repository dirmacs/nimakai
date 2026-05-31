import std/[unittest, json, os, strutils]
import nimakai/[sync, recommend]

let testDir = getTempDir() / "test_sync_" & $getCurrentProcessId()

proc setupTestDir() =
  createDir(testDir)

proc teardownTestDir() =
  if dirExists(testDir):
    removeDir(testDir)

proc writeOmoFile(path: string, categories: JsonNode = nil) =
  ## Write a minimal oh-my-opencode.json for testing.
  var data = %*{
    "categories": {
      "quick": {"model": "nvidia/stepfun-ai/step-3.7-flash"},
      "deep": {"model": "nvidia/qwen/qwen3.5-397b-a17b"}
    }
  }
  if categories != nil:
    data["categories"] = categories
  writeFile(path, pretty(data))

proc makeRec(category, current, recommended: string,
             reason: string = "", curScore: float = 50.0,
             recScore: float = 80.0): Recommendation =
  Recommendation(
    category: category,
    currentModel: current,
    recommendedModel: recommended,
    reason: reason,
    currentScore: curScore,
    recommendedScore: recScore,
  )

suite "backupOmo":
  setup:
    setupTestDir()

  teardown:
    teardownTestDir()

  test "creates backup with timestamp format":
    let omoPath = testDir / "oh-my-opencode.json"
    writeOmoFile(omoPath)

    let backupPath = backupOmo(omoPath)

    check fileExists(backupPath)
    check backupPath.startsWith(omoPath & ".bak.")
    # Verify timestamp portion matches YYYYMMDD-HHMMSS-MMM format
    let suffix = backupPath.replace(omoPath & ".bak.", "")
    check suffix.len >= 15  # "20260308-123456-NNN"
    check suffix[8] == '-'
    # Verify backup content matches original
    check readFile(backupPath) == readFile(omoPath)

  test "raises IOError if file missing":
    let omoPath = testDir / "nonexistent.json"
    expect(IOError):
      discard backupOmo(omoPath)

suite "applyRecommendations":
  setup:
    setupTestDir()

  teardown:
    teardownTestDir()

  test "updates category models in JSON":
    let omoPath = testDir / "oh-my-opencode.json"
    writeOmoFile(omoPath)

    let recs = @[
      makeRec("quick", "stepfun-ai/step-3.7-flash",
              "qwen/qwen3.5-397b-a17b"),
    ]
    applyRecommendations(recs, omoPath)

    let data = parseJson(readFile(omoPath))
    check data["categories"]["quick"]["model"].getStr() ==
      "nvidia/qwen/qwen3.5-397b-a17b"

  test "preserves existing structure":
    let omoPath = testDir / "oh-my-opencode.json"
    # Write with extra fields to ensure they survive
    let data = %*{
      "categories": {
        "quick": {"model": "nvidia/old-model", "extra_key": "preserved"},
        "deep": {"model": "nvidia/deep-model"}
      },
      "agents": {"coder": {"model": "nvidia/some-agent"}}
    }
    writeFile(omoPath, pretty(data))

    let recs = @[
      makeRec("quick", "old-model", "new-model"),
    ]
    applyRecommendations(recs, omoPath)

    let result = parseJson(readFile(omoPath))
    # Model updated
    check result["categories"]["quick"]["model"].getStr() == "nvidia/new-model"
    # Extra key preserved
    check result["categories"]["quick"]["extra_key"].getStr() == "preserved"
    # Other category untouched
    check result["categories"]["deep"]["model"].getStr() == "nvidia/deep-model"
    # Agents section preserved
    check result["agents"]["coder"]["model"].getStr() == "nvidia/some-agent"

  test "skips unchanged models":
    let omoPath = testDir / "oh-my-opencode.json"
    writeOmoFile(omoPath)
    let originalContent = readFile(omoPath)

    # Recommendation where current == recommended (no change)
    let recs = @[
      makeRec("quick", "same-model", "same-model"),
    ]
    applyRecommendations(recs, omoPath)

    # File should not be rewritten when no changes
    check readFile(omoPath) == originalContent

  test "raises IOError if file missing":
    let omoPath = testDir / "nonexistent.json"
    let recs = @[makeRec("quick", "old", "new")]
    expect(IOError):
      applyRecommendations(recs, omoPath)

  test "adds new category if not present":
    let omoPath = testDir / "oh-my-opencode.json"
    writeOmoFile(omoPath)

    let recs = @[
      makeRec("newcat", "old-model", "new-model"),
    ]
    applyRecommendations(recs, omoPath)

    let data = parseJson(readFile(omoPath))
    check data["categories"]["newcat"]["model"].getStr() == "nvidia/new-model"
    # Existing categories still present
    check data["categories"].hasKey("quick")
    check data["categories"].hasKey("deep")

  test "prepends nvidia/ prefix to recommended model":
    let omoPath = testDir / "oh-my-opencode.json"
    writeOmoFile(omoPath)

    let recs = @[
      makeRec("quick", "old-model", "bare-model-id"),
    ]
    applyRecommendations(recs, omoPath)

    let data = parseJson(readFile(omoPath))
    check data["categories"]["quick"]["model"].getStr() == "nvidia/bare-model-id"

suite "findLatestBackup":
  setup:
    setupTestDir()

  teardown:
    teardownTestDir()

  test "returns latest .bak.* file":
    let omoPath = testDir / "oh-my-opencode.json"
    writeOmoFile(omoPath)

    # Create multiple backups with different timestamps
    let bak1 = omoPath & ".bak.20260101-100000"
    let bak2 = omoPath & ".bak.20260201-100000"
    let bak3 = omoPath & ".bak.20260115-100000"
    writeFile(bak1, "old")
    writeFile(bak2, "newest")
    writeFile(bak3, "middle")

    let latest = findLatestBackup(omoPath)
    check latest == bak2  # Feb is latest by sort

  test "returns empty string if none":
    let omoPath = testDir / "oh-my-opencode.json"
    writeOmoFile(omoPath)

    let latest = findLatestBackup(omoPath)
    check latest == ""

  test "ignores unrelated files":
    let omoPath = testDir / "oh-my-opencode.json"
    writeOmoFile(omoPath)

    # Create a file that does NOT match the .bak.* pattern
    writeFile(testDir / "other-file.bak.20260301-100000", "decoy")
    writeFile(testDir / "oh-my-opencode.json.tmp", "also decoy")

    let latest = findLatestBackup(omoPath)
    check latest == ""

suite "rollbackOmo":
  setup:
    setupTestDir()

  teardown:
    teardownTestDir()

  test "restores from backup":
    let omoPath = testDir / "oh-my-opencode.json"
    let originalData = %*{
      "categories": {"quick": {"model": "nvidia/original-model"}}
    }
    writeFile(omoPath, pretty(originalData))

    # Create a backup
    discard backupOmo(omoPath)

    # Modify the original
    let modifiedData = %*{
      "categories": {"quick": {"model": "nvidia/modified-model"}}
    }
    writeFile(omoPath, pretty(modifiedData))

    # Verify it was modified
    let beforeRollback = parseJson(readFile(omoPath))
    check beforeRollback["categories"]["quick"]["model"].getStr() ==
      "nvidia/modified-model"

    # Rollback
    let ok = rollbackOmo(omoPath)
    check ok == true

    # Verify restored to original
    let afterRollback = parseJson(readFile(omoPath))
    check afterRollback["categories"]["quick"]["model"].getStr() ==
      "nvidia/original-model"

  test "returns false if no backup exists":
    let omoPath = testDir / "oh-my-opencode.json"
    writeOmoFile(omoPath)

    let ok = rollbackOmo(omoPath)
    check ok == false

suite "syncRecommendations":
  setup:
    setupTestDir()

  teardown:
    teardownTestDir()

  test "returns false when no changes":
    let omoPath = testDir / "oh-my-opencode.json"
    writeOmoFile(omoPath)

    # All recommendations have current == recommended (no changes)
    let recs = @[
      makeRec("quick", "same-model", "same-model"),
      makeRec("deep", "also-same", "also-same"),
    ]

    let applied = syncRecommendations(recs, omoPath)
    check applied == false

  test "returns true when changes applied":
    let omoPath = testDir / "oh-my-opencode.json"
    writeOmoFile(omoPath)

    let recs = @[
      makeRec("quick", "old-model", "new-model"),
    ]

    let applied = syncRecommendations(recs, omoPath)
    check applied == true

    # Verify the change was applied
    let data = parseJson(readFile(omoPath))
    check data["categories"]["quick"]["model"].getStr() == "nvidia/new-model"

  test "creates backup before applying":
    let omoPath = testDir / "oh-my-opencode.json"
    writeOmoFile(omoPath)

    let recs = @[
      makeRec("quick", "old-model", "new-model"),
    ]

    discard syncRecommendations(recs, omoPath)

    # A backup file should have been created
    let backup = findLatestBackup(omoPath)
    check backup.len > 0
    check fileExists(backup)
    # Backup should contain the original data (before apply)
    let backupData = parseJson(readFile(backup))
    check backupData["categories"]["quick"]["model"].getStr() ==
      "nvidia/stepfun-ai/step-3.7-flash"

  test "returns false when omo file missing":
    let omoPath = testDir / "nonexistent.json"
    let recs = @[
      makeRec("quick", "old-model", "new-model"),
    ]

    # syncRecommendations catches errors internally and returns false
    let applied = syncRecommendations(recs, omoPath)
    check applied == false
