# Stage 1: Clean-Tree Image for nimaproxy

> **Paths in this document are environment variables.** This is a public repository, so the
> procedure is written against placeholders rather than one operator's host layout. Export
> these before running anything:
>
> ```bash
> export REPO_ROOT="$(git rev-parse --show-toplevel)"   # the nimakai checkout
> export CRATE_DIR="$REPO_ROOT/nimaproxy"                # the nimaproxy crate inside it
> export EXPORT_DIR="$(mktemp -d)"                      # scratch dir for the git-archive export
> export CONFIG_FILE=/etc/nimaproxy/nimaproxy.toml       # the deployed config (adjust to taste)
> ```

## 1. Goal

Produce a `dirmacs/nimaproxy:stage1` image that is built **only from committed content**, so that any binary, config, or behavioral difference against a baseline is attributable to a commit, never to uncommitted working-tree edits on this host.

The working tree at `$REPO_ROOT` is dirty (`git status --porcelain` lists modified `nimaproxy/Cargo.lock`, `Cargo.toml`, and many `src/`/`tests/` files), so a plain `docker build` of the crate directory would silently bake that uncommitted state into the image. The procedure below exports the crate from the `chore/rust-ci` tip with `git archive`, copies it to a scratch directory, and builds from that.

Sources for this document (all read, nothing invented):

| Path | What it establishes |
|---|---|
| `$CRATE_DIR/Dockerfile` | the crate-local Dockerfile actually built here |
| `$CRATE_DIR/.dockerignore` | context exclusions for that Dockerfile |
| `$REPO_ROOT/.dockerignore` | context exclusions for the **root** Dockerfile (see R-3) |
| `$REPO_ROOT/Dockerfile` | the stale root Dockerfile (see R-2) |
| `$CRATE_DIR/Cargo.toml` | bins, lib, test targets, deps, release profile |
| `$CRATE_DIR/nimaproxy.toml.example` | example-derived config, no secrets |
| `$REPO_ROOT/.github/workflows/ci.yml` | the toolchain and the test/lint/fmt gates CI runs |

## 2. Build commands

Run from `$REPO_ROOT`. All steps are verbatim-runnable.

```bash
# 0. Confirm the tip you intend to build (must be chore/rust-ci, NOT origin/main).
#    The payload commit is bf2d219 (C1/C2/R1-R3/OptFixIdle + docs); eeb70e1 is
#    its parent and does NOT contain the change set — building it bakes an
#    image without the payload.
cd $REPO_ROOT
git rev-parse chore/rust-ci          # bf2d219f688646492ad423c05d424d659bf39302
git log --oneline -2 chore/rust-ci   # bf2d219 (payload), eeb70e1 (parent)

# 1. Export the crate from the tip into a clean scratch tree.
mkdir -p $EXPORT_DIR
git archive chore/rust-ci nimaproxy | tar -x -C $EXPORT_DIR --strip-components=1

# 2. Verify the export contains the committed content only.
ls $EXPORT_DIR              # Cargo.toml Cargo.lock Dockerfile ... src tests
git status --porcelain > /dev/null   # (sanity: still dirty upstream; the export above is not)
diff -r $EXPORT_DIR/src <(git show chore/rust-ci:nimaproxy/src 2>/dev/null; true) >/dev/null 2>&1 || true
find $EXPORT_DIR -name '*.rs' | wc -l   # non-zero; export is populated

# 3. Build the image with the crate-local Dockerfile.
docker build -f $EXPORT_DIR/Dockerfile \
  -t dirmacs/nimaproxy:stage1 \
  $EXPORT_DIR
```

Context notes:

- The build context is `$EXPORT_DIR`, i.e. the **crate root**, because the Dockerfile copies `Cargo.toml Cargo.lock`, `src/`, and `tests/` relative to the context root. Passing `$REPO_ROOT` instead is R-2/R-3 (see §6).
- `$EXPORT_DIR/.dockerignore` comes from the committed `nimaproxy/.dockerignore` (exported by `git archive`), which excludes `target/`, `Dockerfile`, `.git/`, `*.md` and keeps `README.md`. The `Dockerfile` self-exclusion is harmless here: the Dockerfile is read via `-f`, never from the context, so it is not needed inside the context.
- `Cargo.lock` is part of the export, so the build's `cargo build --release --locked` resolves the same dependency set the tip committed. A dirty-tree build (R-7) is the only way this breaks.
- The image has no `target/` in it: the release build happens in the `rust:1.99-bookworm` builder stage and only `/out/nimaproxy` and `/out/nimaproxy-query` are copied into the runtime stage. Toolchain parity: CI pins `RUSTUP_TOOLCHAIN: '1.99.0'` and the Dockerfile pins `rust:1.99-bookworm`.

## 3. Run command (port 18080, example-derived config, no secrets)

The service binary reads a TOML config. The file below is derived from `nimaproxy.toml.example` (committed) reduced to the minimum needed to start: one placeholder key, an explicit `listen` on all interfaces, and a log path that is a bind-mount.

Write it to `/tmp/nimaproxy-stage1.toml` (never into the repo; `nimaproxy.toml` is gitignored and holds a live NVIDIA key):

```toml
# /tmp/nimaproxy-stage1.toml — example-derived, secret-free, placeholder key.
listen = "0.0.0.0:18080"

[[keys]]
key = "nvapi-PLACEHOLDER-NOT-A-REAL-KEY"
label = "key1"

[logging]
enabled = true
path = "/var/log/nimaproxy/turns.jsonl"
```

Run it, publishing host port 18080, mounting the config from §3 and
bind-mounting the host log directory. Both `-v` flags are required: without
the config mount the container falls back to the baked-in example (see R-5),
and without the log mount the turn log is lost with the container.

```bash
mkdir -p /tmp/nimaproxy-stage1-logs
chown 10001:999 /tmp/nimaproxy-stage1-logs   # R-4: the pinned uid must own it

docker run -d --name nimaproxy-stage1 \
  -p 127.0.0.1:18080:18080 \
  -v /tmp/nimaproxy-stage1.toml:/etc/nimaproxy.toml:ro \
  -v /tmp/nimaproxy-stage1-logs:/var/log/nimaproxy \
  dirmacs/nimaproxy:stage1 \
  --config /etc/nimaproxy.toml --port 18080
```

Notes:

- **The service binds container-loopback even with `listen = "0.0.0.0:18080"` in the mounted config.** Measured 2026-10-20: the container logs `nimaproxy listening on http://127.0.0.1:18080` and host-side `curl http://127.0.0.1:18080/health` gets `Connection reset by peer`, while `docker exec nimaproxy-stage1 curl -fsS http://127.0.0.1:18080/health` returns 200 with the key-pool JSON. The `-p` publish is therefore dead for this run: probe from inside the container (`docker exec ... curl`), not from the host.
- `--port 18080` after the image name overrides the image default; the image's own `CMD` is `--config /etc/nimaproxy.toml`.
- The baked-in `/etc/nimaproxy.toml` is the **example file**, whose keys are `nvapi-YOUR_FIRST_KEY_HERE` placeholders. The `-v ...:/etc/nimaproxy.toml:ro` mount above replaces it with the §3 config; without that mount upstream calls fail (401/403-class) — see R-5. For a stage-1 image that is acceptable: the readiness probes below do not require upstream success.
- `/var/log/nimaproxy` inside the container is owned by the pinned `nimaproxy` user (uid 10001, gid 999); the host dir `/tmp/nimaproxy-stage1-logs` must be `chown 10001:999` or turn logging disables itself at startup — see R-4.

## 4. Readiness probes

The router registers `/health` and `/stats` (see `src/main.rs:313-314`), both
`GET`. Because the service binds container-loopback (§3 note), probe from
inside the container — host-side curl gets `Connection reset by peer`:

```bash
docker exec nimaproxy-stage1 curl -fsS -o /dev/null -w '%{http_code}\n' http://127.0.0.1:18080/health   # 200
docker exec nimaproxy-stage1 curl -fsS http://127.0.0.1:18080/health | head -c 300
# {"admission_wait_ms":1500,"gateway_in_flight":0,...} (key-pool JSON, 1 key)

docker exec nimaproxy-stage1 curl -fsS -o /dev/null -w '%{http_code}\n' http://127.0.0.1:18080/stats    # 200
```

Expected: both **200**. Measured 2026-10-20 against `dirmacs/nimaproxy:stage1`
(bf2d219 payload): `/health` 200 with the 1-key pool JSON, `/stats` 200 with
`racing_wins` present.

**The built-in `HEALTHCHECK` will report unhealthy — do not use it as the oracle.** The image's `HEALTHCHECK CMD` is hardcoded to `curl -fsS http://127.0.0.1:8080/health` (`Dockerfile:73-74`), while the service in this run listens on 18080. `docker inspect --format '{{.State.Health.Status}}' nimaproxy-stage1` will read `unhealthy` (after `--start-period=15s` and 3 retries at 30s intervals, i.e. roughly 105s) even though the service is up and answering on 18080. `docker ps` will show `(unhealthy)`.

Therefore: **`docker exec ... curl http://127.0.0.1:18080/health` is the readiness oracle**, and `docker inspect` health is only meaningful when the service actually listens on 8080.

## 5. Two-image comparison vs the origin/main baseline

Purpose: prove that any binary or behavioral difference between `stage1` and the `origin/main` baseline comes from the commit range, not from the dirty tree or from build nondeterminism in the build inputs.

```bash
cd $REPO_ROOT

# A. Build the baseline image from origin/main, the same clean-export way.
git rev-parse origin/main           # b5413266e3d8266d0dec2fc3c39e83d00597d05c
rm -rf /tmp/nimaproxy-baseline
mkdir -p /tmp/nimaproxy-baseline
git archive origin/main nimaproxy | tar -x -C /tmp/nimaproxy-baseline --strip-components=1
docker build -f /tmp/nimaproxy-baseline/Dockerfile \
  -t dirmacs/nimaproxy:baseline \
  /tmp/nimaproxy-baseline

# B. Extract both binaries from the two images.
rm -rf /tmp/img-stage1 /tmp/img-baseline
mkdir -p /tmp/img-stage1 /tmp/img-baseline
cid1=$(docker create dirmacs/nimaproxy:stage1)
cid2=$(docker create dirmacs/nimaproxy:baseline)
docker cp "$cid1:/usr/local/bin/nimaproxy"     /tmp/img-stage1/nimaproxy
docker cp "$cid1:/usr/local/bin/nimaproxy-query" /tmp/img-stage1/nimaproxy-query
docker cp "$cid2:/usr/local/bin/nimaproxy"     /tmp/img-baseline/nimaproxy
docker cp "$cid2:/usr/local/bin/nimaproxy-query" /tmp/img-baseline/nimaproxy-query
docker rm "$cid1" "$cid2"

# C. Byte-compare. A clean pair of digests means the two trees compiled to the
#    same bytes; any mismatch is a real difference to explain, not noise.
sha256sum /tmp/img-stage1/nimaproxy /tmp/img-baseline/nimaproxy
sha256sum /tmp/img-stage1/nimaproxy-query /tmp/img-baseline/nimaproxy-query
cmp /tmp/img-stage1/nimaproxy /tmp/img-baseline/nimaproxy     # prints first differing byte
```

Interpretation rule: `cmp` exit 0 (identical) plus identical `sha256sum` means the exported trees are binary-equivalent — no behavioral claim can be made from a binary comparison alone, so the discriminators below carry the behavioral evidence. A `cmp` mismatch must be reconciled with the commit range before any behavioral conclusion; the crate builds with `lto = true`, `codegen-units = 1`, `strip = true` (`Cargo.toml:49-53`), so timestamps should not leak in, but a mismatch is still a prompt to re-verify the export refs, not an immediate defect claim.

### Payload discriminators: proving bf2d219 is in the image

The byte comparison proves the images differ; these prove the difference is
the bf2d219 payload (C1/C2/R1-R3/OptFixIdle), not build noise. Run against
the extracted `/tmp/img-stage1/nimaproxy` from §5B (measured 2026-10-20:
5,155,248 bytes — multi-MB, so not the R-8 stub):

1. **C2 knobs in the config surface.** The release binary embeds the TOML key
   names it parses; bf2d219 added two:
   `strings /tmp/img-stage1/nimaproxy | grep -o 'first_byte_timeout_ms\|idle_timeout_ms' | sort -u`
   must print both names. The baseline (pre-C2 tree) contains neither.
2. **OptFixIdle stall reporting.** bf2d219 reports buffered mid-stream stalls
   against the idle window with the message shape `"upstream body stalled
   after {}ms"`:
   `strings /tmp/img-stage1/nimaproxy | grep -c 'upstream body stalled after'`
   must be ≥ 1 (measured: 2). Note the release profile strips symbols, so
   function names (`decide_for_window`, `can_retry_with_window`,
   `is_model_configured`) are NOT reliable discriminators — method-call
   strings and config keys are; unmangled identifiers must be absent-or-ignored
   rather than asserted.
3. **Export-tree source check (strongest).** The export in §2 step 1 is plain
   source; assert the payload's new APIs exist there before building:
   `grep -c 'decide_for_window' $EXPORT_DIR/src/proxy/retry.rs` (6),
   `grep -c 'can_retry_with_window' $EXPORT_DIR/src/proxy/request_state.rs` (7),
   `grep -c 'is_model_configured' $EXPORT_DIR/src/lib.rs` (4),
   `grep -n 'arc-swap' $EXPORT_DIR/Cargo.toml` (line 35),
   `ls $EXPORT_DIR/docs` (all four `.md` files incl. the three new docs).
   Any zero here means the export ref is wrong (e.g. eeb70e1 instead of bf2d219)
   — stop before building.
4. **C1 behavioral discriminator: aborted SSE streams.**
   The commit range includes `5d37430 fix(nimaproxy): finalize the SSE tap when the stream is dropped, not only when drained` — a change that is only observable when a client disconnects mid-stream. A plain request/response comparison cannot discriminate it:
   1. Start `stage1` on 18080 and `baseline` on a second port (e.g. 18081) with equivalent configs and a log bind-mount each.
   2. Send a streaming request to each **from inside the container** (§3 note: the service binds container-loopback, so host-side curl is reset; use `docker exec <name> curl ... http://127.0.0.1:1808N/v1/chat/completions`) and **abort the client mid-stream** (kill curl after the first SSE chunk).
   3. Read the `turns.jsonl` turn record for that request in each container's log mount: the fixed tree finalizes the SSE tap on drop (the turn record is written/completed); the unfixed tree finalizes only on drain.
   4. Discriminating prediction: the two images produce **different** turn records for the aborted request. If both behave identically, the fix is not present in `stage1` or the probe did not actually abort — re-verify step 2 before concluding anything about the fix.
   A positive control for step 3 is required before trusting a negative result: run the same abort probe against a tree you know is pre-`5d37430` (e.g. its parent `8215f3a`), and confirm the abort signature differs there. An empty or unread turn record is a probe failure (wrong log path, missing bind-mount, logging disabled), not a behavioral result.

## 6. Risks

- **R-1 — Dirty-tree context.** Building with `$REPO_ROOT` or `$CRATE_DIR` as context bakes the 19+ modified files (`git status --porcelain`) into the image. Mitigation: always build from the `git archive` export in §2.
- **R-2 — Stale root Dockerfile.** `$REPO_ROOT/Dockerfile` builds only `--bin nimaproxy` (no `nimaproxy-query`, no `tests`), installs `libssl3` and uses `useradd --create-home --uid 10001` without a pinned gid, and its comments reference `phase2`. Using it produces a different image than the crate Dockerfile. Mitigation: always `-f <export>/Dockerfile` with the crate-root context.
- **R-3 — Root `.dockerignore` breaking the crate Dockerfile.** `$REPO_ROOT/.dockerignore` (root context) excludes `target/`, `**/target/`, `.git/`, `.omp/`, `*.swp`, `.DS_Store`, `.env`, `.env.*`, `nimaproxy.toml`, `nimaproxy_racing.toml`, `nimaproxy.toml.test`, `node_modules/`. If a build mistakenly uses the root context with `-f nimaproxy/Dockerfile`, the `COPY src ./src` / `COPY tests ./tests` paths resolve relative to the root and are **not** excluded, so the build can succeed while shipping unintended root-level content and skipping the crate's own `.dockerignore`; conversely `*.md` exclusions differ between the two ignore files. Mitigation: crate-root context only, as in §2.
- **R-4 — Pinned uid/gid vs host log ownership.** The Dockerfile pins `groupadd -g 999` / `useradd -r -u 10001 -g 999` because the deployment bind-mount is owned by `10001:999`; a uid allocated by `useradd -r` instead would silently lose write access and turn logging off at startup. The host dir in §3 is `/tmp/nimaproxy-stage1-logs` and must be `chown 10001:999` before `docker run` (the §3 command does this); `mkdir -p` alone creates it root-owned and the container's non-root user cannot write `turns.jsonl`. Confirm the file appears inside the mount.
- **R-5 — Example keys are non-functional.** The baked `/etc/nimaproxy.toml` is `nimaproxy.toml.example` with `nvapi-YOUR_*_KEY_HERE` placeholders; upstream calls fail (401/403-class). Readiness on `/health` and `/stats` does not depend on upstream, but any request that reaches the upstream gateway does. Mitigation: mount a real config (secret-free for stage-1 smoke; real keys only via mount, never baked) and treat upstream failures as expected, not as an image defect.
- **R-6 — Healthcheck port mismatch.** The image `HEALTHCHECK` hardcodes `127.0.0.1:8080/health` while this run listens on 18080, so the container reads `unhealthy` while healthy. Mitigation: use explicit `curl` on 18080 as the oracle (§4); optionally override at run time with `--health-cmd` / `--no-healthcheck`.
- **R-7 — `--locked` vs a dirty `Cargo.lock`.** The Dockerfile builds `--locked`, which requires the committed `Cargo.lock` to match `Cargo.toml`. The working tree has both modified; a clean export of `chore/rust-ci` keeps them consistent, but re-exporting a ref where they diverge fails the build with a lockfile error. Mitigation: build only from a `git archive` export of a ref whose tree builds under CI's `--locked` gates (`.github/workflows/ci.yml` runs `cargo build/test/clippy/fmt` with `--locked`).
- **R-8 — Dummy-main empty-binary trap.** The builder stage writes `fn main() {}` stubs into `src/main.rs` and `src/log_query.rs` and an empty `src/lib.rs` **before** copying the real `src/`. If a build is interrupted, reordered, or the `COPY src ./src` layer is cache-invalidated in an unexpected way, the release binaries can be built from the stubs: they compile, install, and ship, but contain no service. A binary that exists and exits 0 proves nothing. Mitigation: after the build, assert the shipped binary is non-trivial — e.g. `docker run --rm dirmacs/nimaproxy:stage1 sh -c 'stat -c%s /usr/local/bin/nimaproxy'` (a stub-sized binary, a few hundred KB or less, is the trap; the real LTO release binary is multi-MB) and confirm `/health` answers.
- **R-9 — No tests in the image.** `tests/` is shipped for manifest resolution only (`Cargo.toml` declares `[[test]]` targets `integration` and `stress_test`) and is never executed in the image; CI runs `coverage_gaps`, `integration`, `proxy_error_paths`, `stream_observability` with `RUSTUP_TOOLCHAIN=1.99.0`. A green image build therefore says nothing about test status. Mitigation: treat §2's build as an artifact step and run the CI gates separately on the same ref (`cargo test --locked --manifest-path nimaproxy/Cargo.toml --test <t>`, `cargo clippy --locked --all-targets ... -- -D warnings`, `cargo fmt --check`) before claiming the tree is verified.
