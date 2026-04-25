# Justfile for nimakai
# Self-contained build: binary + .so files in same directory, no LD_LIBRARY_PATH needed

# Config
config       := "/opt/nimakai/nimaproxy.toml"
binary_name  := "nimaproxy"
binary_src   := "nimaproxy/target/release/" + binary_name
binary_dst   := "/usr/local/bin/nimaproxy"
service_name := "nimaproxy"

# Default recipe
default: build

# Build everything: Rust libs + Nim binary with RUNPATH
build: build-rust build-nim

# Build Rust libraries only
build-rust:
	cd rustping && cargo build --release
	cd nimaproxy && cargo build --release
	cp rustping/target/release/libnimrust_ping.so .
	cp nimaproxy/target/release/libnimaproxy.so .

# Build Nim binary only
build-nim:
	nimble build

# ─── nimaproxy service management ───────────────────────────────

# Build nimaproxy release binary (no deploy)
build-proxy:
	cd nimaproxy && cargo build --release

# Deploy nimaproxy: build + stop + kill orphans + copy binary + start + verify
deploy: build-proxy
	sudo systemctl stop {{service_name}} || true
	@sleep 1
	-pkill -9 -f {{binary_dst}} 2>/dev/null || true
	@sleep 1
	cp {{binary_src}} {{binary_dst}}
	sudo systemctl start {{service_name}}
	@sleep 2
	@echo "=== Service status ==="
	sudo systemctl status {{service_name}} --no-pager -l | head -15
	@echo "=== racing_max_parallel ==="
	@curl -s http://127.0.0.1:8080/stats | jq '{racing_max_parallel: .racing_max_parallel, racing_models: (.racing_models | length), keys: (.keys | length)}'

# Hot-restart nimaproxy (no rebuild, just restart the service)
restart:
	sudo systemctl restart {{service_name}}
	@sleep 1
	sudo systemctl status {{service_name}} --no-pager -l | head -10

# Stop nimaproxy service + kill stray processes
stop:
	sudo systemctl stop {{service_name}} || true
	-pkill -9 -f {{binary_dst}} 2>/dev/null || true
	@echo "nimaproxy stopped"

# Start nimaproxy service
start:
	sudo systemctl start {{service_name}}
	@sleep 1
	sudo systemctl status {{service_name}} --no-pager -l | head -10

# Show nimaproxy service status
status:
	sudo systemctl status {{service_name}} --no-pager -l

# Show nimaproxy /stats endpoint (formatted)
stats:
	@curl -s http://127.0.0.1:8080/stats | jq '.'

# Show nimaproxy /stats — models only, sorted by total requests
stats-models:
	@curl -s http://127.0.0.1:8080/stats | jq '.models | sort_by(.total) | reverse'

# Show nimaproxy /stats — racing config only
stats-racing:
	@curl -s http://127.0.0.1:8080/stats | jq '{racing_models, racing_max_parallel, racing_timeout_ms}'

# Show nimaproxy /health endpoint
health:
	@curl -s http://127.0.0.1:8080/health

# Tail nimaproxy service logs (last 50 lines)
logs:
	sudo journalctl -u {{service_name}} -n 50 --no-pager

# Follow nimaproxy service logs
follow:
	sudo journalctl -u {{service_name}} -f

# ─── nimakai (Nim) ──────────────────────────────────────────────

# Run nimakai single round
run:
	./nimakai --once --sort avg

# Run continuous benchmark
watch:
	./nimakai

# ─── testing ────────────────────────────────────────────────────

# Run Nim tests
test:
	nim c -d:ssl --path:src -r tests/test_types.nim
	nim c -d:ssl --path:src -r tests/test_catalog.nim
	nim c -d:ssl --path:src -r tests/test_cli.nim
	nim c -d:ssl --path:src -r tests/test_config.nim
	nim c -d:ssl --path:src -r tests/test_discovery.nim
	nim c -d:ssl --path:src -r tests/test_display.nim
	nim c -d:ssl --path:src -r tests/test_history.nim
	nim c -d:ssl --path:src -r tests/test_integration.nim
	nim c -d:ssl --path:src -r tests/test_metrics.nim
	nim c -d:ssl --path:src -r tests/test_opencode.nim
	nim c -d:ssl --path:src -r tests/test_ping.nim
	nim c -d:ssl --path:src -r tests/test_proxy.nim
	nim c -d:ssl --path:src -r tests/test_rechistory.nim
	nim c -d:ssl --path:src -r tests/test_recommend.nim
	nim c -d:ssl --path:src -r tests/test_sync.nim
	nim c -d:ssl --path:src -r tests/test_watch.nim

# Run nimaproxy Rust tests (lib only, no FFI)
test-proxy:
	cd nimaproxy && cargo test --lib -- --skip ffi_tests

# Run nimaproxy integration tests
test-proxy-integration:
	cd nimaproxy && cargo test --test integration

# Run nimaproxy FFI tests (sequential — each test spawns a proxy on a fixed port)
test-proxy-ffi:
	cd nimaproxy && just test-ffi

# ─── cleanup ────────────────────────────────────────────────────

# Clean build artifacts
clean:
	cargo clean --manifest-path rustping/Cargo.toml
	cargo clean --manifest-path nimaproxy/Cargo.toml
	rm -f libnimrust_ping.so libnimaproxy.so

# Clean Nim cache
cleannim:
	rm -rf nimcache/ src/nimcache/
	rm -f nimakai

# Full clean
distclean: clean cleannim

# Check library dependencies
check-libs:
	@echo "=== Library dependencies ===" && ldd nimakai | grep libnim || echo "(no libnim deps found)"
	@echo "=== RUNPATH ===" && readelf -d nimakai | grep -i runpath
