## Tests for CLI argument parsing (parseArgs).

import std/unittest
import nimakai/[types, cli]

suite "parseArgs defaults":
  test "empty params returns defaults":
    let cfg = parseArgs(@[])
    check cfg.subcommand == smBenchmark
    check cfg.once == false
    check cfg.jsonOutput == false
    check cfg.quiet == false
    check cfg.noHistory == false
    check cfg.dryRun == false
    check cfg.interval == DefaultInterval
    check cfg.timeout == DefaultTimeout
    check cfg.models.len == 0
    check cfg.tierFilter == ""
    check cfg.sortColumn == scAvg
    check cfg.useOpencode == false
    check cfg.rounds == 3
    check cfg.applySync == false
    check cfg.rollback == false

suite "parseArgs boolean flags":
  test "--once":
    let cfg = parseArgs(@["--once"])
    check cfg.once == true

  test "-1 short flag":
    let cfg = parseArgs(@["-1"])
    check cfg.once == true

  test "--json":
    let cfg = parseArgs(@["--json"])
    check cfg.jsonOutput == true

  test "-j short flag":
    let cfg = parseArgs(@["-j"])
    check cfg.jsonOutput == true

  test "--quiet":
    let cfg = parseArgs(@["--quiet"])
    check cfg.quiet == true

  test "-q short flag":
    let cfg = parseArgs(@["-q"])
    check cfg.quiet == true

  test "--no-history":
    let cfg = parseArgs(@["--no-history"])
    check cfg.noHistory == true

  test "--dry-run":
    let cfg = parseArgs(@["--dry-run"])
    check cfg.dryRun == true

  test "--apply":
    let cfg = parseArgs(@["--apply"])
    check cfg.applySync == true

  test "--rollback":
    let cfg = parseArgs(@["--rollback"])
    check cfg.rollback == true

  test "--opencode":
    let cfg = parseArgs(@["--opencode"])
    check cfg.useOpencode == true

  test "multiple boolean flags":
    let cfg = parseArgs(@["--once", "--json", "--quiet", "--no-history"])
    check cfg.once == true
    check cfg.jsonOutput == true
    check cfg.quiet == true
    check cfg.noHistory == true

suite "parseArgs value flags":
  test "--models with single model":
    let cfg = parseArgs(@["--models=qwen/qwen3.5-122b-a10b"])
    check cfg.models == @["qwen/qwen3.5-122b-a10b"]

  test "--models with comma-separated list":
    let cfg = parseArgs(@["-m:model-a,model-b,model-c"])
    check cfg.models.len == 3
    check cfg.models[0] == "model-a"
    check cfg.models[1] == "model-b"
    check cfg.models[2] == "model-c"

  test "--models trims whitespace":
    let cfg = parseArgs(@["-m:model-a , model-b"])
    check cfg.models == @["model-a", "model-b"]

  test "--models skips empty entries":
    let cfg = parseArgs(@["-m:model-a,,model-b"])
    check cfg.models == @["model-a", "model-b"]

  test "--interval":
    let cfg = parseArgs(@["--interval=10"])
    check cfg.interval == 10

  test "-i short flag":
    let cfg = parseArgs(@["-i:20"])
    check cfg.interval == 20

  test "--interval invalid value keeps default":
    let cfg = parseArgs(@["--interval=abc"])
    check cfg.interval == DefaultInterval

  test "--timeout":
    let cfg = parseArgs(@["--timeout=30"])
    check cfg.timeout == 30

  test "-t short flag":
    let cfg = parseArgs(@["-t:5"])
    check cfg.timeout == 5

  test "--rounds":
    let cfg = parseArgs(@["--rounds=5"])
    check cfg.rounds == 5

  test "-r short flag":
    let cfg = parseArgs(@["-r:10"])
    check cfg.rounds == 10

  test "--tier":
    let cfg = parseArgs(@["--tier=S"])
    check cfg.tierFilter == "S"

suite "parseArgs sort options":
  test "--sort avg":
    let cfg = parseArgs(@["--sort=avg"])
    check cfg.sortColumn == scAvg

  test "--sort p95":
    let cfg = parseArgs(@["--sort=p95"])
    check cfg.sortColumn == scP95

  test "--sort stability":
    let cfg = parseArgs(@["--sort=stability"])
    check cfg.sortColumn == scStability

  test "--sort tier":
    let cfg = parseArgs(@["--sort=tier"])
    check cfg.sortColumn == scTier

  test "--sort name":
    let cfg = parseArgs(@["--sort=name"])
    check cfg.sortColumn == scName

  test "--sort uptime":
    let cfg = parseArgs(@["--sort=uptime"])
    check cfg.sortColumn == scUptime

  test "--sort shorthand a":
    let cfg = parseArgs(@["--sort=a"])
    check cfg.sortColumn == scAvg

  test "--sort shorthand s":
    let cfg = parseArgs(@["--sort=s"])
    check cfg.sortColumn == scStability

  test "--sort case insensitive":
    let cfg = parseArgs(@["--sort=AVG"])
    check cfg.sortColumn == scAvg

  test "--sort unknown keeps default":
    let cfg = parseArgs(@["--sort=xyz"])
    check cfg.sortColumn == scAvg

suite "parseArgs subcommands":
  test "catalog as first arg":
    let cfg = parseArgs(@["catalog"])
    check cfg.subcommand == smCatalog

  test "recommend as first arg":
    let cfg = parseArgs(@["recommend"])
    check cfg.subcommand == smRecommend

  test "history as first arg":
    let cfg = parseArgs(@["history"])
    check cfg.subcommand == smHistory

  test "trends as first arg":
    let cfg = parseArgs(@["trends"])
    check cfg.subcommand == smTrends

  test "opencode as first arg":
    let cfg = parseArgs(@["opencode"])
    check cfg.subcommand == smOpencode

  test "no subcommand defaults to benchmark":
    let cfg = parseArgs(@["--once"])
    check cfg.subcommand == smBenchmark

  # Item 14: subcommand after flags (was broken when only params[0] was checked)
  test "catalog after --once flag":
    let cfg = parseArgs(@["--once", "catalog"])
    check cfg.subcommand == smCatalog
    check cfg.once == true

  test "catalog after --json flag":
    let cfg = parseArgs(@["--json", "catalog"])
    check cfg.subcommand == smCatalog
    check cfg.jsonOutput == true

  test "recommend after -m flag":
    let cfg = parseArgs(@["-m:model-a", "recommend"])
    check cfg.subcommand == smRecommend
    check cfg.models == @["model-a"]

  test "recommend with --rounds and --apply":
    let cfg = parseArgs(@["recommend", "--rounds=5", "--apply"])
    check cfg.subcommand == smRecommend
    check cfg.rounds == 5
    check cfg.applySync == true

  test "recommend after multiple flags":
    let cfg = parseArgs(@["--quiet", "--json", "recommend", "--rounds=3"])
    check cfg.subcommand == smRecommend
    check cfg.quiet == true
    check cfg.jsonOutput == true
    check cfg.rounds == 3

  test "catalog with --tier":
    let cfg = parseArgs(@["catalog", "--tier=S"])
    check cfg.subcommand == smCatalog
    check cfg.tierFilter == "S"

  test "catalog with --tier before subcommand":
    let cfg = parseArgs(@["--tier=A", "catalog"])
    check cfg.subcommand == smCatalog
    check cfg.tierFilter == "A"

suite "parseArgs combined scenarios":
  test "full recommend command":
    let cfg = parseArgs(@["recommend", "--rounds=5", "--apply", "--quiet"])
    check cfg.subcommand == smRecommend
    check cfg.rounds == 5
    check cfg.applySync == true
    check cfg.quiet == true

  test "benchmark with all options":
    let cfg = parseArgs(@[
      "--once", "--json", "-m:model-a,model-b",
      "--interval=10", "--timeout=30",
      "--tier=S", "--sort=p95",
      "--quiet", "--no-history"
    ])
    check cfg.once == true
    check cfg.jsonOutput == true
    check cfg.models == @["model-a", "model-b"]
    check cfg.interval == 10
    check cfg.timeout == 30
    check cfg.tierFilter == "S"
    check cfg.sortColumn == scP95
    check cfg.quiet == true
    check cfg.noHistory == true

  test "recommend with dry-run and apply":
    let cfg = parseArgs(@["recommend", "--apply", "--dry-run"])
    check cfg.subcommand == smRecommend
    check cfg.applySync == true
    check cfg.dryRun == true

  test "opencode subcommand with flags":
    let cfg = parseArgs(@["--json", "opencode"])
    check cfg.subcommand == smOpencode
    check cfg.jsonOutput == true
