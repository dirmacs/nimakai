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
# Build context: the repo root; the crate is copied from nimaproxy/.
# Multi-stage cargo-chef: dependencies are cooked from a manifest-only recipe,
# so the dependency layer is cached across source edits and only the final
# `cargo build` recompiles nimaproxy itself.

ARG RUST_IMAGE=rust:1.98-bookworm
ARG RUNTIME_IMAGE=debian:bookworm-slim

FROM ${RUST_IMAGE} AS chef
RUN cargo install cargo-chef --locked
WORKDIR /workspace/nimaproxy

FROM chef AS planner
# The planner needs the whole crate so `cargo chef prepare` can resolve the
# manifest (lib + both bins + the [[test]] targets that reference tests/).
COPY nimaproxy/ /workspace/nimaproxy/
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
ENV CARGO_TERM_COLOR=always
# nimaproxy uses reqwest with rustls-tls (default-features = false); there is
# no openssl-sys in the graph, so no system TLS library is required to build.
COPY --from=planner /workspace/nimaproxy/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/workspace/nimaproxy/target \
    cargo chef cook --release --locked --recipe-path recipe.json

COPY nimaproxy/ /workspace/nimaproxy/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/workspace/nimaproxy/target \
    cargo build --release --locked --bin nimaproxy \
    && mkdir -p /artifacts \
    && cp target/release/nimaproxy /artifacts/nimaproxy

FROM ${RUNTIME_IMAGE} AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --create-home --uid 10001 --shell /usr/sbin/nologin nimaproxy

COPY --from=builder /artifacts/nimaproxy /usr/local/bin/nimaproxy
RUN mkdir -p /etc/nimaproxy && chown -R nimaproxy:nimaproxy /etc/nimaproxy
USER nimaproxy

# listen default 127.0.0.1:8080; the config's `listen` / --port override it.
EXPOSE 8080
ENTRYPOINT ["nimaproxy", "--config", "/etc/nimaproxy/nimaproxy.toml"]
