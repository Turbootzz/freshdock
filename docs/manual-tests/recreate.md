# Manual smoke test: `freshdock recreate`

This walks through the Phase 2 recreate cycle (issue #9) on a real Docker
daemon. Phase 2 deliberately stops short of health gating, rollback, and
removal of the archived container — those land in Phase 3.

## Prerequisites

- A working Docker daemon reachable on the standard socket.
- `freshdock` built locally: `just build`.
- A checkout where `freshdock recreate` is wired in (i.e. anything from
  `main` post-Phase-2 onward).

## Steps

`freshdock recreate` is a *manual* admin tool, not the automatic update
loop. It refuses two opt-out signals — `freshdock.enable` not `true`, or
`freshdock.mode=off` — and otherwise honours the operator's explicit
invocation regardless of the scheduler mode (`live`, `nightly`,
`weekly`, `monthly`, `watch`). This is why the test container below
uses `mode=watch`: a watch-mode container is *never* touched by the
automatic loop, but is a fine target for a manual `recreate`.

```bash
# 1. Launch the test container with freshdock labels so the recreate
#    command is willing to act on it.
docker run -d --name fd-smoke \
  --label freshdock.enable=true \
  --label freshdock.mode=watch \
  -p 8081:80 nginx:alpine

# 2. Confirm it's serving.
curl -fsS http://localhost:8081/ > /dev/null && echo "nginx OK"

# 3. Capture Config.Image *before* the recreate so we can assert the
#    round-trip below. This must read `nginx:alpine` — not the resolved
#    `Image` digest, which is a separate field.
docker inspect fd-smoke --format '{{.Config.Image}}'   # → nginx:alpine

# 4. Run the recreate.
./target/release/freshdock recreate fd-smoke
```

## Expected observations

- The CLI prints `recreated fd-smoke: archived old container as fd-smoke-old-<unix-ts>, new id <id>`.
- `docker ps -a` shows two containers:
  - `fd-smoke` — running, with a fresh container id.
  - `fd-smoke-old-<unix-ts>` — stopped, kept around (Phase 3 removes it on success).
- The new container has the same port mapping (`0.0.0.0:8081->80/tcp`),
  the same `freshdock.enable=true` / `freshdock.mode=watch` labels, and
  the same nginx image.
- **`Config.Image` round-trip (regression #25).** After the recreate,
  `docker inspect fd-smoke --format '{{.Config.Image}}'` must read
  byte-identical to the pre-recreate value (`nginx:alpine`). Drift to
  `library/nginx:alpine` is the original bug — it must not return.
- `curl -fsS http://localhost:8081/` still returns the default nginx
  page from the new container.

## Cleanup

```bash
docker rm -f fd-smoke fd-smoke-old-*
```

## Weird-config round-trip smoke

Mirrors the dimensions covered by [tests/fixtures/container_inspect_weird.json]
so the manual procedure stays in lockstep with the automated round-trip suite
in [tests/spec_roundtrip.rs] (the `weird_spec_*` tests). Run this whenever the
recreate orchestrator or `ContainerSpec` projection changes.

This uses `alpine` running `sleep` rather than `nginx:alpine`, so the
container doesn't fight the config (nginx fails when run as uid 1000 with
its cache dirs replaced by tmpfs — orthogonal to recreate). The point of
this smoke is to verify *spec preservation*, not application behaviour.

```bash
# Two networks the container will attach to.
docker network create fd-front >/dev/null
docker network create fd-back  >/dev/null

# Launch with the kitchen-sink dimensions: non-default user, multi-port
# binding (one with explicit HostIp), bind + tmpfs (HostConfig.Tmpfs dict),
# multiple cap_add / cap_drop, sysctls, restart policy with retry count,
# memory + nano-cpus + pids limit, custom stop signal + timeout, multiple
# ulimits, extra_hosts, and freshdock.* labels alongside user labels.
docker run -d --name fd-smoke-weird \
  --label freshdock.enable=true \
  --label freshdock.mode=watch \
  --label freshdock.notify=true \
  --label app=weird --label team=platform --label owner=thijs@bendy.nl \
  --user 1000:1000 \
  --network fd-front --network-alias weird-front \
  -p 127.0.0.1:18443:8443 -p 19090:9090 \
  --tmpfs /run:rw,size=32m --tmpfs /var/cache:rw,size=64m \
  --cap-add NET_BIND_SERVICE --cap-add SYS_TIME \
  --cap-drop MKNOD --cap-drop AUDIT_WRITE \
  --sysctl net.ipv4.ip_unprivileged_port_start=0 \
  --sysctl net.core.somaxconn=4096 \
  --restart on-failure:5 \
  --memory 128m --memory-reservation 64m --cpus 0.5 --pids-limit 256 \
  --stop-signal SIGUSR1 --stop-timeout 45 \
  --ulimit nofile=8192:16384 --ulimit nproc=512:1024 \
  --add-host db.internal:10.0.0.5 \
  -e APP_MODE=production -e 'APP_TOKEN=base64=padded==' -e EMPTY_VAR= \
  alpine sleep 600

# Attach the second network with its own alias.
docker network connect --alias weird-back fd-back fd-smoke-weird

# Capture every dimension we care about *before* the recreate.
before=$(docker inspect fd-smoke-weird)

# Run the recreate.
./target/release/freshdock recreate fd-smoke-weird

# Spot-check the headline #25 assertion: Config.Image must round-trip
# byte-identical (alpine, NOT library/alpine).
docker inspect fd-smoke-weird --format '{{.Config.Image}}'   # → alpine

# Diff the recreate-relevant slices of the inspect. Container ID and
# Image (resolved digest) are expected to differ; everything else under
# Config.* and HostConfig.* should match `before`.
after=$(docker inspect fd-smoke-weird)
diff \
  <(echo "$before" | jq '.[0].Config'    ) \
  <(echo "$after"  | jq '.[0].Config'    )    # only Config.MacAddress may
                                              # appear in AFTER (daemon
                                              # quirk: API-created containers
                                              # surface the auto-assigned MAC
                                              # on Config.MacAddress, while
                                              # `docker run` originals expose
                                              # it only on NetworkSettings).
                                              # Not a recreate bug.
diff \
  <(echo "$before" | jq '.[0].HostConfig') \
  <(echo "$after"  | jq '.[0].HostConfig')    # expected: empty diff
```

Cleanup:

```bash
docker rm -f fd-smoke-weird $(docker ps -a --filter name=fd-smoke-weird-old- -q)
docker network rm fd-front fd-back
```

## Known Phase-2 limitations (do not file as bugs)

- **No health gating.** The new container is started but the CLI returns
  before it's verified healthy. If the new image is broken, the CLI
  declares success anyway — Phase 3 fixes this.
- **The `-old-` container is not removed.** It is preserved as the
  rollback target for Phase 3. Until then the operator can `docker rm`
  it manually after confirming the new instance is healthy.
- **No live integration test in CI.** The "weird config" round-trip test
  described in [docs/PLAN.md] §6.3 is Phase 3 (P3-3); it depends on
  `testcontainers` which is currently incompatible with bollard 0.21
  (see the comment in [Cargo.toml]).
- **Registry auth is out of scope.** Pull works for Docker Hub
  anonymously and for any image already present in the local cache.
  GHCR / Quay / lscr.io land in Phase 5.
