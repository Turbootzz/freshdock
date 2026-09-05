# Manual smoke test: `freshdock recreate`

This walks through the recreate cycle on a real Docker daemon. As of Phase 3
the cycle is health-gated: the new container must reach `healthy` (or stay
running for a grace period if it has no healthcheck) before the run is declared
a success and the archived `-old-` container is removed; a new image that fails
health is **rolled back** to the previous container.

> **Canonical automated gate.** The authoritative "is the tool safe to ship"
> check is the live round-trip test
> [tests/recreate_roundtrip_live.rs](https://github.com/Turbootzz/freshdock/blob/main/tests/recreate_roundtrip_live.rs)
> (PLAN §6.3, P3-3). It creates a kitchen-sink container, recreates it, and
> asserts the inspected config round-trips byte-identical. Run it with a daemon
> available:
>
> ```bash
> cargo test --test recreate_roundtrip_live -- --ignored
> ```
>
> A failure of that test is a **release blocker**. This manual procedure
> remains for quick human verification on first install.

## Prerequisites

- A working Docker daemon reachable on the standard socket.
- `freshdock` built locally: `just build`.
- A checkout where `freshdock recreate` is wired in (i.e. anything from
  `main` post-Phase-2 onward).

## Steps

`freshdock recreate` is a *manual* admin tool, not the automatic update
loop. It refuses two opt-out signals (`freshdock.enable` not `true`, or
`freshdock.mode=off`) and otherwise honours the operator's explicit
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
#    round-trip below. This must read `nginx:alpine`, not the resolved
#    `Image` digest, which is a separate field.
docker inspect fd-smoke --format '{{.Config.Image}}'   # -> nginx:alpine

# 4. Run the recreate.
./target/release/freshdock recreate fd-smoke
```

## Expected observations

- The CLI prints a line starting `recreated fd-smoke: healthy` that names the
  removed archive `fd-smoke-old-<unix-ts>` and the new id.
- `docker ps -a` shows a **single** `fd-smoke` container, running with a fresh
  id; the archived `fd-smoke-old-<unix-ts>` was removed once the new instance
  passed health gating. (nginx:alpine has no healthcheck, so success is decided
  by the grace period; add a healthcheck to exercise the `healthy` path.)
- The new container has the same port mapping (`0.0.0.0:8081->80/tcp`),
  the same `freshdock.enable=true` / `freshdock.mode=watch` labels, and
  the same nginx image.
- **`Config.Image` round-trip (regression #25).** After the recreate,
  `docker inspect fd-smoke --format '{{.Config.Image}}'` must read
  byte-identical to the pre-recreate value (`nginx:alpine`). Drift to
  `library/nginx:alpine` is the original bug. It must not return.
- `curl -fsS http://localhost:8081/` still returns the default nginx
  page from the new container.

To exercise **rollback**: recreate a container whose tag has been re-pointed to
an image with a failing healthcheck; the CLI prints
`recreate failed for <name>: ... rolled back to the previous container` and the
original container is restored under its original name.

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
its cache dirs replaced by tmpfs, orthogonal to recreate). The point of
this smoke is to verify *spec preservation*, not application behaviour.

```bash
# Two networks the container will attach to.
docker network create fd-front >/dev/null
docker network create fd-back  >/dev/null

# Bind-mount sources. /tmp keeps these cleanup-friendly and avoids needing
# root for /host/* paths; the container-side destinations match the
# fixture so `weird_spec_preserves_binds_and_tmpfs` exercises the same
# `Binds` shape that this manual procedure does.
mkdir -p /tmp/fd-state /tmp/fd-secrets

# Launch with the kitchen-sink dimensions: non-default user, multi-port
# binding (one with explicit HostIp), bind mounts + tmpfs (HostConfig.Tmpfs
# dict), multiple cap_add / cap_drop, sysctls, restart policy with retry
# count, memory + nano-cpus + pids limit, custom stop signal + timeout,
# multiple ulimits, extra_hosts, and freshdock.* labels alongside user
# labels.
docker run -d --name fd-smoke-weird \
  --label freshdock.enable=true \
  --label freshdock.mode=watch \
  --label freshdock.notify=true \
  --label app=weird --label team=platform --label owner=owner@example.invalid \
  --user 1000:1000 \
  --network fd-front --network-alias weird-front \
  -p 127.0.0.1:18443:8443 -p 19090:9090 \
  -v /tmp/fd-state:/var/lib/state:rw -v /tmp/fd-secrets:/run/secrets:ro \
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

# Attach the second network with its own alias. Mirrors the multi-network
# attachment that `weird_spec_preserves_multi_network_endpoints_with_aliases`
# pins on the fixture side.
docker network connect --alias weird-back fd-back fd-smoke-weird

# Capture every dimension we care about *before* the recreate.
before=$(docker inspect fd-smoke-weird)

# Run the recreate.
./target/release/freshdock recreate fd-smoke-weird

# Spot-check the headline #25 assertion: Config.Image must round-trip
# byte-identical (alpine, NOT library/alpine).
docker inspect fd-smoke-weird --format '{{.Config.Image}}'   # -> alpine

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

# Bind mounts must round-trip: same source/destination/RW. .Mounts entries
# for binds carry only durable fields (the daemon-assigned propagation is
# also stable across recreate).
diff \
  <(echo "$before" | jq '[.[0].Mounts[] | select(.Type == "bind") | {Source, Destination, Mode, RW}] | sort_by(.Destination)') \
  <(echo "$after"  | jq '[.[0].Mounts[] | select(.Type == "bind") | {Source, Destination, Mode, RW}] | sort_by(.Destination)')
                                              # expected: empty diff

# NetworkSettings.Networks: a strict diff would fail because the new
# container gets a fresh IP / MAC / EndpointID. Those are network-driver
# assignments, not part of the spec. Docker also auto-adds the new
# container's own short id as an alias on every attached network, which
# differs per instance. Project to the durable invariants: the set of
# attached networks + the *user-assigned* aliases (filtering out the
# auto-added short id). Then diff those.
sid_before=$(echo "$before" | jq -r '.[0].Id[0:12]')
sid_after=$(echo "$after"  | jq -r '.[0].Id[0:12]')
diff \
  <(echo "$before" | jq --arg sid "$sid_before" '.[0].NetworkSettings.Networks | to_entries | map({net: .key, aliases: ((.value.Aliases // []) | map(select(. != $sid)) | sort)}) | sort_by(.net)') \
  <(echo "$after"  | jq --arg sid "$sid_after"  '.[0].NetworkSettings.Networks | to_entries | map({net: .key, aliases: ((.value.Aliases // []) | map(select(. != $sid)) | sort)}) | sort_by(.net)')
                                              # expected: empty diff
```

Cleanup:

```bash
docker rm -f fd-smoke-weird $(docker ps -a --filter name=fd-smoke-weird-old- -q)
docker network rm fd-front fd-back
rm -rf /tmp/fd-state /tmp/fd-secrets
```

## Current limitations (do not file as bugs)

- **Registry auth is out of scope.** Pull works for Docker Hub
  anonymously and for any image already present in the local cache.
  GHCR / Quay / lscr.io land in Phase 5.
- **Health timing is not yet configurable from the CLI/config.** Phase 3 uses
  built-in defaults (`HealthConfig::default()`); sourcing it from
  `freshdock.toml`/labels is Phase 4/5.
