#!/usr/bin/env bash
#
# Safe cutover deploy for the live nimaproxy container.
#
# Usage: scripts/deploy.sh <new-tag>
#
#   Builds dirmacs/nimaproxy:<new-tag>, verifies the candidate actually
#   answers on a free port, then cuts over the live container
#   (the running container) to it. The previous image is kept for rollback.
#
# SAFE TO READ WITHOUT RUNNING. This script is never executed by CI; the
# operator runs it by hand. It performs no action until invoked.
#
# Why host networking for the candidate: the binary's --port override binds
# 127.0.0.1 (see nimaproxy/src/main.rs), so inside a bridge-networked
# container the candidate would listen on the *container's* loopback and
# Docker port publishing could never reach it. With --network host the
# container's loopback IS the host's loopback, so curl can verify it.
# The live container needs no --port flag: it uses the mounted config's
# listen address. The declarative spec for the service lives in the infra
# repo (dirmacs/infra, stacks/nimaproxy/) -- see docker-compose.yml.RETIRED.
#
# NEVER added here: `docker system prune`, image deletion, or touching any
# container other than the running container and the short-lived candidate.

set -euo pipefail

CONTAINER="${NIMAPROXY_CONTAINER:-nimaproxy}"
CANDIDATE="${NIMAPROXY_CONTAINER:-nimaproxy}-candidate"
REPO="dirmacs/nimaproxy"
LIVE_PORT="8080"
# Host path of the config, provided by the operator; it holds a live API key
# and is never committed. Exported before running this script.
CONFIG_SRC="${NIMAPROXY_CONFIG_PATH:?set NIMAPROXY_CONFIG_PATH to the host path of nimaproxy.toml}"
LOG_SRC="/var/log/nimaproxy"
PID_FILE_ENV="NIMAPROXY_PID_FILE=/run/nimaproxy/nimaproxy.pid"
STARTUP_TIMEOUT_SECS="30"
MAX_ATTEMPTS="3"

usage() {
    echo "Usage: $0 <new-tag>" >&2
    echo "  <new-tag> must be NEW: it must not equal the tag currently running." >&2
}

if [ "$#" -ne 1 ]; then
    usage
    exit 2
fi
NEW_TAG="$1"
if [ -z "$NEW_TAG" ]; then
    usage
    exit 2
fi
IMAGE="${REPO}:${NEW_TAG}"

for tool in docker curl python3; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "ERROR: required tool '$tool' not found on PATH." >&2
        exit 1
    fi
done

# Run from the repo root regardless of where the operator invoked us.
cd "$(dirname "$0")/.."

# Record what is running now, for the new-tag guard and the rollback line.
OLD_IMAGE=""
if docker inspect "$CONTAINER" >/dev/null 2>&1; then
    OLD_IMAGE="$(docker inspect --format '{{.Config.Image}}' "$CONTAINER")"
fi

# (a) Never build over the tag currently running.
if [ -n "$OLD_IMAGE" ] && [ "$OLD_IMAGE" = "$IMAGE" ]; then
    echo "ERROR: ${IMAGE} is the image currently running in ${CONTAINER}." >&2
    echo "Refusing to overwrite it; pass a NEW tag." >&2
    exit 1
fi

echo "Building ${IMAGE} ..."
# Context is the REPO ROOT and the root Dockerfile, on purpose -- that is the
# recipe every currently-deployed image was built from (it installs exactly one
# binary, /usr/local/bin/nimaproxy, and bakes in no config). nimaproxy/Dockerfile
# is a DIFFERENT recipe: it also emits nimaproxy-query and copies
# nimaproxy.toml.example to /etc/nimaproxy.toml. Building with context
# `nimaproxy/` selects that file implicitly and silently produces an image that
# is not the one running. Keep the context at the repo root.
docker build -f Dockerfile -t "$IMAGE" .

# The candidate gets an isolated log dir so verification traffic never
# touches the live usage data in /var/log/nimaproxy.
CANDIDATE_LOG_DIR="$(mktemp -d)"
cleanup() {
    # Only ever the candidate and its scratch dir. The live container is
    # NOT referenced here, so no failure path can stop or remove it.
    docker rm -f "$CANDIDATE" >/dev/null 2>&1 || true
    if [ -n "${CANDIDATE_LOG_DIR:-}" ]; then
        rm -rf "$CANDIDATE_LOG_DIR"
    fi
}
trap cleanup EXIT

# Free port chosen by the kernel: bind port 0, read back the assigned port,
# release it. Never a guessed number (8099 was already taken once and the
# guess produced a completely misleading 404 from another container).
pick_free_port() {
    python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'
}

# Print the HTTP status code for a path, or 000 when unreachable.
http_code() {
    curl -s -o /dev/null -w '%{http_code}' --max-time 5 "http://127.0.0.1:$1$2" || true
}

# (b) Start the candidate with host networking and verify it ANSWERS.
# Retried with a fresh kernel-picked port in case the picked port is stolen
# in the pick-to-bind window.
ATTEMPT="0"
VERIFIED_PORT=""
while [ "$ATTEMPT" -lt "$MAX_ATTEMPTS" ]; do
    ATTEMPT="$((ATTEMPT + 1))"
    PORT="$(pick_free_port)"
    if [ "$PORT" = "$LIVE_PORT" ]; then
        echo "Attempt ${ATTEMPT}/${MAX_ATTEMPTS}: kernel offered live port ${PORT}; repicking."
        continue
    fi
    echo "Attempt ${ATTEMPT}/${MAX_ATTEMPTS}: starting candidate ${IMAGE} on 127.0.0.1:${PORT} ..."
    docker rm -f "$CANDIDATE" >/dev/null 2>&1 || true
    docker run -d --name "$CANDIDATE" --network host \
        -v "${CONFIG_SRC}:/etc/nimaproxy.toml:ro" \
        -v "${CANDIDATE_LOG_DIR}:/var/log/nimaproxy:rw" \
        -e "$PID_FILE_ENV" \
        --entrypoint nimaproxy \
        "$IMAGE" --config /etc/nimaproxy.toml --port "$PORT" >/dev/null

    DEADLINE="$((SECONDS + STARTUP_TIMEOUT_SECS))"
    STATS_OK=""
    while [ "$SECONDS" -lt "$DEADLINE" ]; do
        if [ "$(docker inspect -f '{{.State.Running}}' "$CANDIDATE" 2>/dev/null || echo false)" != "true" ]; then
            echo "Attempt ${ATTEMPT}/${MAX_ATTEMPTS}: candidate exited before answering. Logs:"
            docker logs "$CANDIDATE" 2>&1 || true
            break
        fi
        if [ "$(http_code "$PORT" "/stats")" = "200" ]; then
            STATS_OK="1"
            break
        fi
        sleep 1
    done
    if [ -z "$STATS_OK" ]; then
        echo "Attempt ${ATTEMPT}/${MAX_ATTEMPTS}: /stats never returned 200 on port ${PORT}."
        docker rm -f "$CANDIDATE" >/dev/null 2>&1 || true
        continue
    fi

    LOOPS_CODE="$(http_code "$PORT" "/loops")"
    PROPS_CODE="$(http_code "$PORT" "/props")"
    RUNNING="$(docker inspect -f '{{.State.Running}}' "$CANDIDATE" 2>/dev/null || echo false)"
    if [ "$LOOPS_CODE" = "200" ] && [ "$PROPS_CODE" = "200" ] && [ "$RUNNING" = "true" ]; then
        VERIFIED_PORT="$PORT"
        break
    fi
    echo "Attempt ${ATTEMPT}/${MAX_ATTEMPTS}: /loops=${LOOPS_CODE} /props=${PROPS_CODE} candidate-running=${RUNNING}; want 200/200/true."
    docker rm -f "$CANDIDATE" >/dev/null 2>&1 || true
done

# (d) ANY verification failure: live container untouched, non-zero exit.
if [ -z "$VERIFIED_PORT" ]; then
    echo "ERROR: candidate ${IMAGE} failed verification after ${MAX_ATTEMPTS} attempt(s)." >&2
    echo "Live container '${CONTAINER}' untouched; no cutover performed. Rollback not needed." >&2
    exit 1
fi

echo "Candidate verified on 127.0.0.1:${VERIFIED_PORT} (/stats, /loops, /props all 200); removing candidate."
docker rm -f "$CANDIDATE" >/dev/null 2>&1 || true

# (c) Cut over: stop/remove the old container, start the new one with the
# captured spec. The previous image is deliberately left in place.
if [ -n "$OLD_IMAGE" ]; then
    echo "Stopping and removing ${CONTAINER} (was ${OLD_IMAGE}) ..."
    docker stop "$CONTAINER" >/dev/null
    docker rm "$CONTAINER" >/dev/null
fi
docker run -d --name "$CONTAINER" \
    -p "127.0.0.1:${LIVE_PORT}:8080" \
    -v "${CONFIG_SRC}:/etc/nimaproxy.toml:ro" \
    -v "${LOG_SRC}:/var/log/nimaproxy:rw" \
    -e "$PID_FILE_ENV" \
    --restart unless-stopped \
    --entrypoint nimaproxy \
    "$IMAGE" --config /etc/nimaproxy.toml >/dev/null

# (e) Exact rollback command. No image was deleted, so the old tag is present.
if [ -n "$OLD_IMAGE" ]; then
    echo "Deploy complete: ${CONTAINER} now runs ${IMAGE}."
    echo "Previous image ${OLD_IMAGE} kept for rollback."
    echo "Rollback: docker stop ${CONTAINER} && docker rm ${CONTAINER} && docker run -d --name ${CONTAINER} -p 127.0.0.1:${LIVE_PORT}:8080 -v ${CONFIG_SRC}:/etc/nimaproxy.toml:ro -v ${LOG_SRC}:/var/log/nimaproxy:rw -e ${PID_FILE_ENV} --restart unless-stopped --entrypoint nimaproxy ${OLD_IMAGE} --config /etc/nimaproxy.toml"
else
    echo "Deploy complete: fresh install of ${IMAGE} in ${CONTAINER} (no previous container)."
    echo "Rollback: docker stop ${CONTAINER} && docker rm ${CONTAINER}"
fi
echo "Remember to bump the image tag in the infra repo's nimaproxy stack (dirmacs/infra, stacks/nimaproxy/) to ${NEW_TAG} so the spec stays current."
