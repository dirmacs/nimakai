.PHONY: test test-ffi test-all clean test-lib test-integration

# Kill orphaned nimaproxy processes left by FFI tests
kill-orphans:
	@for pid in $$(ss -tlnp 2>/dev/null | grep -E ':1910[1-6] ' | sed 's/.*pid=\([0-9]*\).*/\1/' | grep -v '^$$'); do \
		kill -9 $$pid 2>/dev/null; \
	done
	@echo "Orphaned proxy processes cleaned up"

# Run FFI tests sequentially (required — each test spawns a proxy on a fixed port)
test-ffi: kill-orphans
	cargo test ffi_tests -- --test-threads=1

# Run lib unit tests (excluding FFI — those need sequential mode)
test-lib:
	cargo test --lib -- --skip ffi_tests

# Run integration tests
test-integration:
	cargo test --test integration

# Run all non-FFI tests in parallel
test: kill-orphans
	cargo test --lib -- --skip ffi_tests
	cargo test --test integration
	cargo test --test e2e_live
