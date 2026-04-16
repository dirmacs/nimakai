# Justfile for nimakai
# Self-contained build: binary + .so files in same directory, no LD_LIBRARY_PATH needed

# Default recipe
default: build

# Build everything: Rust libs + Nim binary with RUNPATH
build:
    # Build rustping (release)
    cd rustping && cargo build --release
    # Build nimaproxy (release)
    cd nimaproxy && cargo build --release
    # Copy .so files to project root (same dir as nimakai binary)
    cp rustping/target/release/libnimrust_ping.so .
    cp nimaproxy/target/release/libnimaproxy.so .
    # Build nimakai (picks up RUNPATH from nim.cfg)
    nimble build

# Build Rust libraries only
build-rust:
    cd rustping && cargo build --release
    cd nimaproxy && cargo build --release
    cp rustping/target/release/libnimrust_ping.so .
    cp nimaproxy/target/release/libnimaproxy.so .

# Build Nim binary only
build-nim:
    nimble build

# Run nimakai (no LD_LIBRARY_PATH needed - RUNPATH=$ORIGIN is set in nim.cfg)
run:
    ./nimakai --once --sort avg

# Run continuous benchmark
watch:
    ./nimakai

# Run tests
test:
    nimble test

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
