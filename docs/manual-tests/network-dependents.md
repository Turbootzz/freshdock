# Manual smoke test: network-namespace dependents

Verifies that containers sharing another container's network namespace
(`--network container:X`, or compose's `network_mode: service:X`) are put back
together after freshdock recreates X (issue #68). Recreating X destroys the
namespace its dependents were attached to: without repair they keep running but
have no network at all, and any `container:<old id>` reference is now dangling.

freshdock captures the dependents *before* stopping X and re-creates each one
afterwards — on the success path with its reference repointed at the new
container id, and on the rollback path with the reference left as-is (the
restored container owns its original id again, but the namespace behind it is
new either way).

The unit tests in [src/docker/recreate.rs](https://github.com/Turbootzz/freshdock/blob/main/src/docker/recreate.rs)
(`healthy_update_reattaches_id_based_dependent`,
`healthy_update_keeps_name_based_dependent_ref`,
`rollback_reattaches_dependents_without_rewrite`,
`dependent_failure_does_not_fail_update`) are the authoritative checks; this
procedure is for human verification against a real daemon.

## Prerequisites

- A working Docker daemon on the standard socket.
- `freshdock` built locally: `just build`.

## Steps

The dependent needs **no freshdock labels**: it is not being updated, only
repaired, so the policy gate that guards `recreate` does not apply to it.

```bash
# 1. The namespace owner — the container freshdock will update.
docker run -d --name fd-base \
  --label freshdock.enable=true \
  --label freshdock.mode=live \
  nginx:alpine

# 2. A dependent joined to fd-base's network namespace. `--network
#    container:fd-base` is the name-based form; the id-based form compose
#    produces is covered further down.
docker run -d --name fd-peer --network container:fd-base \
  alpine:3.20 sleep 1d

# 3. Prove the namespace is shared: nginx is reachable on the peer's loopback.
docker exec fd-peer wget -qO- 127.0.0.1 | head -1   # → <!DOCTYPE html>

# 4. Capture the "before" facts.
docker inspect fd-base --format '{{.Id}}'                       # old owner id
docker inspect fd-peer --format '{{.HostConfig.NetworkMode}}'   # container:fd-base
docker inspect fd-peer --format '{{.State.StartedAt}}'

# 5. Recreate the owner.
./target/release/freshdock recreate fd-base
```

## Expected observations

- The CLI prints `recreated fd-base: healthy — ...`, and the log carries an
  `re-attaching network-namespace dependent` line naming `fd-peer`.
- `docker inspect fd-peer --format '{{.State.StartedAt}}'` is **newer** than the
  value from step 4 — the dependent was re-created, not left behind.
- `docker inspect fd-peer --format '{{.HostConfig.NetworkMode}}'` still reads
  `container:fd-base` byte-identical: a name-based reference resolves to
  whatever owns the name, which is already the replacement, so freshdock leaves
  it alone.
- `docker exec fd-peer wget -qO- 127.0.0.1 | head -1` reaches the **new** nginx.
  This is the headline assertion — before #68 it failed with no route to host.
- `docker ps -a` shows no `fd-peer-old-<ts>` left over: the dependent's own
  archive is removed once it is running again.

### Id-based reference (what compose actually writes)

```bash
docker rm -f fd-peer
base_id=$(docker inspect fd-base --format '{{.Id}}')
docker run -d --name fd-peer --network "container:$base_id" alpine:3.20 sleep 1d

./target/release/freshdock recreate fd-base

# The reference must now point at the NEW owner id, not the dead one.
docker inspect fd-peer --format '{{.HostConfig.NetworkMode}}'
docker inspect fd-base --format '{{.Id}}'   # → the id inside NetworkMode above
docker exec fd-peer wget -qO- 127.0.0.1 | head -1
```

Pass criteria: `NetworkMode` holds `container:<new fd-base id>` and the `wget`
still succeeds. A `container:<old id>` here means the rewrite did not happen —
the container would refuse to start at all on the next daemon restart.

### Rollback path

Re-point `fd-base`'s tag at an image with a failing healthcheck so the update
rolls back. The CLI prints `recreate failed for fd-base: ... rolled back`, and
`fd-peer` is still re-created afterwards (its `StartedAt` moves) with its
`NetworkMode` unchanged — the restart gave the restored `fd-base` a fresh
namespace, so the dependent has to re-attach even though nothing was updated.

### Best-effort contract

If a dependent's re-creation fails — it was removed by hand mid-run, its name
collides, the daemon refuses the create — the run still reports
`recreated fd-base: healthy — ...` and logs a warning naming the dependent.
Re-attachment repairs collateral damage; it never turns a good update into a
failure, and one broken dependent never stops the next one from being fixed.

## Cleanup

```bash
docker rm -f fd-peer fd-base $(docker ps -a --filter name=-old- -q) 2>/dev/null
```

## Current limitations (do not file as bugs)

- Only `HostConfig.NetworkMode = container:<ref>` is treated as a dependency.
  Containers attached to a *user-defined network* keep working across a
  recreate on their own and need no repair.
- Id references shorter than Docker's 12-character short id are ignored — they
  are not unique enough to attribute to the container being updated.
- Dependents of dependents are not followed transitively: a chain
  `C → B → A` re-creates `B` when `A` is updated, which in turn breaks `C`.
  Rare enough to leave to the next update cycle.
