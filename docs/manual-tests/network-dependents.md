# Manual smoke test: network-namespace dependents

Verifies that containers sharing another container's network namespace
(`--network container:X`, or compose's `network_mode: service:X`) are put back
together after freshdock recreates X (issue #68). Recreating X destroys the
namespace its dependents were attached to: without repair they keep running but
have no network at all, and any `container:<old id>` reference is now dangling.

freshdock captures the dependents *before* stopping X and re-creates each one
afterwards — on the success path with an **id-based** reference repointed at
the new container id (a literal name reference is left name-based), and on the
rollback path with the reference left as-is (the restored container owns its
original id again, but the namespace behind it is new either way). The repair runs **before** X's `-old-` archive is removed:
Docker refuses to remove a container whose namespace a running container still
holds.

Each dependent is re-created from the exact **image ID** it was running, not
from its (mutable) tag — a repair must never double as an unrequested upgrade.
(When the daemon reports no image id for it — rare — the existing image
reference is used instead.) Its `Config.Image` therefore reads as an image id
afterwards; that is deliberate, and the opposite of the owner's own cycle
(issue #25).

The unit tests in [src/docker/recreate.rs](https://github.com/Turbootzz/freshdock/blob/main/src/docker/recreate.rs)
(`healthy_update_reattaches_id_based_dependent`,
`healthy_update_keeps_name_based_dependent_ref`,
`rollback_reattaches_dependents_without_rewrite`,
`dependent_create_body_drops_what_a_shared_namespace_forbids`,
`dependent_create_failure_restores_the_dependent`,
`dependent_failure_does_not_fail_update`) are the authoritative checks; this
procedure is for human verification against a real daemon.

## Prerequisites

- A working Docker daemon on the standard socket.
- `freshdock` built locally: `just build`.

## Steps

The dependent needs **no freshdock labels**: it is not being updated, only
repaired, so the policy gate that guards `recreate` does not apply to it. It is
skipped only if it *explicitly* opts out (`freshdock.enable=false` or
`freshdock.mode=off`), or if it turns out to be the freshdock container itself.

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

# 4. Capture the "before" facts. NOTE: a modern daemon NORMALISES the reference
#    at create time, so this prints `container:<full 64-char id of fd-base>`,
#    not `container:fd-base`.
docker inspect fd-base --format '{{.Id}}'                       # old owner id
docker inspect fd-peer --format '{{.HostConfig.NetworkMode}}'   # container:<old id>
docker inspect fd-peer --format '{{.State.StartedAt}}'
docker inspect fd-peer --format '{{.Image}}'                    # image ID

# 5. Recreate the owner.
./target/release/freshdock recreate fd-base
```

## Expected observations

- The CLI prints `recreated fd-base: healthy — ...`, and the log carries an
  `re-attaching network-namespace dependent` line naming `fd-peer`.
- `docker inspect fd-peer --format '{{.State.StartedAt}}'` is **newer** than the
  value from step 4 — the dependent was re-created, not left behind.
- `docker inspect fd-peer --format '{{.HostConfig.NetworkMode}}'` reads
  `container:<new fd-base id>`. Because the daemon normalised `container:fd-base`
  to an id at create time, this is the same rewrite as the id-based case below.
  (freshdock still has a branch that leaves a *literal* name reference untouched;
  it only fires on daemons that store the reference verbatim.)
- `docker inspect fd-peer --format '{{.Image}}'` is **unchanged** from step 4:
  the dependent is re-created from the exact image ID it was running (the
  image-ref fallback only applies when the daemon reported no image id).
- `docker exec fd-peer wget -qO- 127.0.0.1 | head -1` reaches the **new** nginx.
  This is the headline assertion — before #68 it failed with no route to host.
- `docker ps -a` shows no `fd-peer-old-<ts>` left over: the dependent's own
  archive is removed once it is running again, and `fd-base-old-<ts>` is gone
  too — it can only be removed after the dependent let go of its namespace.

### Id-based reference (what compose, and any modern daemon, actually stores)

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

The failure has to live in the **container spec**, not in the image: the cycle
always pulls the tag first, which undoes any local re-tagging trick. Give the
owner a healthcheck that can never pass — it round-trips through
`ContainerSpec`, so the replacement fails the gate no matter what the pull
brings in:

```bash
docker rm -f fd-base fd-peer 2>/dev/null
docker run -d --name fd-base \
  --label freshdock.enable=true --label freshdock.mode=live \
  --health-cmd 'exit 1' --health-interval 2s \
  nginx:alpine
docker run -d --name fd-peer --network container:fd-base alpine:3.20 sleep 1d

./target/release/freshdock recreate fd-base
```

The CLI prints `recreate failed for fd-base: ... rolled back`, and `fd-peer` is
still re-created afterwards (its `StartedAt` moves) with its `NetworkMode`
unchanged — the restart gave the restored `fd-base` a fresh namespace, so the
dependent has to re-attach even though nothing was updated.

### Best-effort contract

If a dependent's re-creation fails — it was removed by hand mid-run, its name
collides, the daemon refuses the create — the run still reports
`recreated fd-base: healthy — ...` and logs a warning naming the dependent.
Re-attachment repairs collateral damage; it never turns a good update into a
failure, and one broken dependent never stops the next one from being fixed.

When the failure lands *after* the dependent was renamed to its `-old-` form,
freshdock renames it back and starts it again, so you are left with a running
container on a dead namespace rather than a stopped archive no scheduler will
ever look at. If that restore also fails, the warning names the archive to
recover by hand.

### Opt-out and self-exclusion

```bash
docker run -d --name fd-peer-off --network container:fd-base \
  --label freshdock.enable=false alpine:3.20 sleep 1d
```

`freshdock recreate fd-base` must leave `fd-peer-off` alone (its `StartedAt`
does not move) and log a warning that it keeps a dead network namespace until
restarted manually. The same applies to `freshdock.mode=off`. A dependent that
turns out to be the freshdock container itself (its id starts with freshdock's
own hostname) is skipped the same way — stopping it would kill the daemon
mid-cycle.

## Cleanup

```bash
# Scoped to THIS test's archives — a bare `name=-old-` filter would catch
# unrelated containers on the host.
docker rm -f fd-peer fd-base 2>/dev/null
docker ps -aq --filter name=fd-base-old- --filter name=fd-peer-old- | xargs -r docker rm -f
```

## Current limitations (do not file as bugs)

- Only `HostConfig.NetworkMode = container:<ref>` is treated as a dependency.
  Containers attached to a *user-defined network* keep working across a
  recreate on their own and need no repair.
- Id references shorter than Docker's 12-character short id are ignored — they
  are not unique enough to attribute to the container being updated.
- Dependents of dependents are not followed transitively: a chain
  `C → B → A` re-creates `B` when `A` is updated, which in turn breaks `C`.
  Nothing repairs `C` on its own — `B` is only re-created again when `B`'s own
  image updates, and `C`'s own update cannot fix a dangling
  `container:<old B id>` reference. Restart `C` by hand after such a run.
