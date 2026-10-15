# ── Build stage ────────────────────────────────────────────────────────────
# Pinned to the same 1.99 toolchain the lockfile was resolved against on this
# host (rust:1.99-bookworm is present locally). --locked is used because no
# manifest rewriting happens in-container, so the lock stays valid.
FROM rust:1.99-bookworm AS builder

WORKDIR /build

# Copy manifests first for dependency-layer caching.
COPY Cargo.toml Cargo.lock ./
# Dummy targets so `cargo fetch`/build metadata resolves without full sources.
# Both bins are declared in Cargo.toml and must exist for metadata.
RUN mkdir -p src \
    && echo 'fn main() {}' > src/main.rs \
    && echo 'fn main() {}' > src/log_query.rs \
    && echo '' > src/lib.rs

# Real sources. Copy the whole crate — target/ is excluded by .dockerignore.
COPY src ./src
# Tests live in the crate and are referenced by [[test]] targets; ship them so
# manifest resolution is complete (they don't run in the image).
COPY tests ./tests

# Build only the binaries we ship; --locked pins the resolved dependency set.
# cdylib/staticlib lib target is built too but only the bin artifacts are used.
RUN cargo build --release --locked --bins \
    && install -Dm755 target/release/nimaproxy /out/nimaproxy \
    && install -Dm755 target/release/nimaproxy-query /out/nimaproxy-query

# ── Runtime stage ──────────────────────────────────────────────────────────
FROM debian:bookworm-slim

# Non-root service user.
#
# The uid is PINNED to 10001 rather than allocated by `useradd -r`. The
# deployment bind-mounts /var/log/nimaproxy owned by 10001:999 (set when the
# container was first cut over), so a build that allocates a different uid
# silently loses write access and turn logging disables itself at startup
# with "Permission denied". That is not hypothetical: builds of this same
# Dockerfile produced uid 10001 at one time and 999 at another, because
# `useradd -r` takes the next free system uid from the BUILD host's passwd,
# which is not stable across hosts.
RUN groupadd -g 999 nimaproxy \
    && useradd -r -u 10001 -g 999 -s /usr/sbin/nologin -M nimaproxy

# Runtime deps: CA certs for TLS to upstream NIM (reqwest + rustls), curl for
# the healthcheck. Keep it minimal.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# Binaries + default config.
COPY --from=builder /out/nimaproxy /usr/local/bin/nimaproxy
COPY --from=builder /out/nimaproxy-query /usr/local/bin/nimaproxy-query
COPY nimaproxy.toml.example /etc/nimaproxy.toml

# Runtime state dirs: log dir (matches [logging].path default) and a writable
# /run/nimaproxy for the pid file. Owned by the service user.
RUN mkdir -p /var/log/nimaproxy /run/nimaproxy \
    && chown -R nimaproxy:nimaproxy /var/log/nimaproxy /run/nimaproxy /etc/nimaproxy.toml

USER nimaproxy

ENV NIMAPROXY_PID_FILE=/run/nimaproxy/nimaproxy.pid

EXPOSE 8080

# Default entry: config file. Port override via `--port` still works:
#   docker run ... dirmacs/nimaproxy:phase2 --config /etc/nimaproxy.toml --port 9090
# Note: the in-container service binds 127.0.0.1 by default (config), so it is
# reachable from inside the container. To expose it on the host, set
# listen = "0.0.0.0:8080" in the mounted config and publish the port.
HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8080/health || exit 1

ENTRYPOINT ["nimaproxy"]
CMD ["--config", "/etc/nimaproxy.toml"]
