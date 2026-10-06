# syntax=docker/dockerfile:1.7
# nimakai / nimaproxy — NVIDIA NIM model latency proxy (Rust service binary).
#
# The phase-3 service image carries the `nimaproxy` Rust binary. Runtime config
# is REQUIRED via `--config <path>` and is NEVER baked into the image — the
# local nimaproxy.toml holds a live NVIDIA API key and is gitignored. Mount the
# config at run time:
#
#   docker build -t dirmacs/nimaproxy:phase2 .
#   docker run -v /path/to/nimaproxy.toml:/etc/nimaproxy/nimaproxy.toml:ro \
#       dirmacs/nimaproxy:phase2
#
# Build context: the repo root (/opt/nimakai); the crate is copied from nimaproxy/.

ARG RUST_IMAGE=rust:1.98-bookworm
ARG RUNTIME_IMAGE=debian:bookworm-slim

FROM ${RUST_IMAGE} AS builder
ENV CARGO_TERM_COLOR=always
# nimaproxy depends on openssl-sys (native-tls); build the C library link.
RUN apt-get update && apt-get install -y --no-install-recommends \
      pkg-config libssl-dev ca-certificates \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY nimaproxy/Cargo.toml nimaproxy/Cargo.lock ./nimaproxy/
COPY nimaproxy/src ./nimaproxy/src
WORKDIR /build/nimaproxy
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo build --release --locked --bin nimaproxy \
 && cp target/release/nimaproxy /tmp/nimaproxy

FROM ${RUNTIME_IMAGE} AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates libssl3 \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --create-home --uid 10001 --shell /usr/sbin/nologin nimaproxy

COPY --from=builder /tmp/nimaproxy /usr/local/bin/nimaproxy
RUN mkdir -p /etc/nimaproxy && chown -R nimaproxy:nimaproxy /etc/nimaproxy
USER nimaproxy

# listen default 127.0.0.1:8080; the config's `listen` / --port override it.
EXPOSE 8080
ENTRYPOINT ["nimaproxy", "--config", "/etc/nimaproxy/nimaproxy.toml"]
