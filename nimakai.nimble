# Package
version       = "0.13.0"
author        = "bkataru"
description   = "NVIDIA NIM model latency benchmarker"
license       = "MIT"
srcDir        = "src"
bin           = @["nimakai"]

# Dependencies
requires "nim >= 2.0.0"
requires "malebolgia >= 0.1.0"

# Build config
task build, "Build nimakai":
  exec "nim c -d:ssl -d:release --opt:size -o:nimakai src/nimakai.nim"

task proxy, "Build nimaproxy (Rust key-rotation proxy)":
  exec "cargo build --release --manifest-path=nimaproxy/Cargo.toml"
  exec "cp nimaproxy/target/release/nimaproxy ."

task test, "Run tests":
  exec "nim c -d:ssl --path:src -r tests/test_types.nim"
  exec "nim c -d:ssl --path:src -r tests/test_metrics.nim"
  exec "nim c -d:ssl --path:src -r tests/test_display.nim"
  exec "nim c -d:ssl --path:src -r tests/test_ping.nim"
  exec "nim c -d:ssl --path:src -r tests/test_catalog.nim"
  exec "nim c -d:ssl --path:src -r tests/test_config.nim"
  exec "nim c -d:ssl --path:src -r tests/test_opencode.nim"
  exec "nim c -d:ssl --path:src -r tests/test_recommend.nim"
  exec "nim c -d:ssl --path:src -r tests/test_sync.nim"
  exec "nim c -d:ssl --path:src -r tests/test_history.nim"
  exec "nim c -d:ssl --path:src -r tests/test_rechistory.nim"
  exec "nim c -d:ssl --path:src -r tests/test_watch.nim"
  exec "nim c -d:ssl --path:src -r tests/test_integration.nim"
  exec "nim c -d:ssl --path:src -r tests/test_discovery.nim"
  exec "nim c -d:ssl --path:src -r tests/test_cli.nim"
