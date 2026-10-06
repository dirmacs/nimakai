# syntax=docker/dockerfile:1.7
#
# nimakai — Nim agent runtime; the deployable service is nimaproxy (a Rust
# sub-project under nimaproxy/ — a standalone NVIDIA NIM key-rotation reverse
# proxy). The nimakai Nim binary is a benchmark CLI, not the production service;
# this image builds and runs nimaproxy.
#
# nimaproxy reads nimaproxy.toml (see nimaproxy/nimaproxy.toml.example). Mount
# your config at /etc/nimaproxy/nimaproxy.toml. The default config listens on
# 127.0.0.1:8080, which is unreachable from outside a container — this image
# ships a config that binds 0.0.0.0:8080 unless you mount your own.
#
# Build:  docker build -t dirmacs/nimaproxy:phase2 .
# Run:    docker run --rm -p 8080:8080 \
#                   -v $PWD/nimaproxy.toml:/etc/nimaproxy/nimaproxy.toml:ro \
#                   dirmacs/nimaproxy:phase2

# Builder: compile the nimaproxy Rust binary. Build context is the nimakai repo
# root; the Rust crate lives in nimaproxy/.
FROM rust:1-bookworm AS builder

WORKDIR /build
COPY nimaproxy ./nimaproxy

RUN cargo build --release --manifest-path nimaproxy/Cargo.toml --bin nimaproxy

# Runtime: slim bookworm, non-root.
FROM debian:bookworm-slim AS runtime

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl && \
    rm -rf /var/lib/apt/lists/*

RUN useradd --system --uid 10001 --create-home nimaproxy

COPY --from=builder /build/nimaproxy/target/release/nimaproxy /usr/local/bin/nimaproxy

# Ship a container-ready default config that binds 0.0.0.0 (the example binds
# 127.0.0.1, unreachable from outside the container). Operators mount their own
# nimaproxy.toml over this to configure keys/target.
RUN mkdir -p /etc/nimaproxy && \
    printf 'listen = "0.0.0.0:8080"\n' > /etc/nimaproxy/nimaproxy.toml && \
    chown -R nimaproxy:nimaproxy /etc/nimaproxy

USER nimaproxy

EXPOSE 8080

# nimaproxy serves GET /health (key-pool status) per its README — use it as the
# liveness probe.
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD curl -fsS -o /dev/null http://127.0.0.1:8080/health || exit 1

CMD ["nimaproxy", "--config", "/etc/nimaproxy/nimaproxy.toml"]
