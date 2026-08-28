# Manual smoke test: Compose project rollouts

Verifies that a Docker Compose project is updated as one unit (issue #78): the
one-shots the project waits on are re-run first, services follow in `depends_on`
order, `restart: true` dependents are bumped afterwards, and a failed one-shot
aborts the rollout without touching anything downstream.

The unit tests in
[src/rollout.rs](https://github.com/Turbootzz/freshdock/blob/main/src/rollout.rs)
and [src/compose.rs](https://github.com/Turbootzz/freshdock/blob/main/src/compose.rs)
are the authoritative checks; every rule below is pinned there. This procedure
is for human verification against a real daemon, and it is what to re-run after
touching the rollout planner.

## Prerequisites

- A working Docker daemon on the standard socket.
- `freshdock` built locally: `just build` (or `cargo build` for a debug binary).
- Outbound access to Docker Hub (the stack uses `busybox` and `alpine`).

## The stack

One project exercising every rule at once. Note which containers carry labels
and which deliberately do not.

```yaml
# docker-compose.yml
services:
  # Unlabelled one-shot the project waits on. MUST be re-run.
  migrate:
    image: busybox:latest
    command: ["sh", "-c", "if [ -f /data/fail ]; then echo failed >> /data/migrations.log; exit 1; fi; echo ran >> /data/migrations.log"]
    volumes: ["./data:/data"]
    restart: "no"

  # Unlabelled one-shot that explicitly opts out. MUST NOT be re-run.
  seed:
    image: busybox:latest
    command: ["sh", "-c", "echo ran >> /data/seed.log; exit 0"]
    volumes: ["./data:/data"]
    restart: "no"
    labels:
      freshdock.enable: "false"

  # The labelled service that triggers the rollout.
  web:
    image: busybox:latest
    command: ["sh", "-c", "while true; do sleep 5; done"]
    labels:
      freshdock.enable: "true"
      freshdock.mode: "live"
    depends_on:
      migrate:
        condition: service_completed_successfully
      seed:
        condition: service_completed_successfully

  # Unlabelled, restart: true on web. MUST be restarted, not recreated.
  sidecar:
    image: alpine:latest
    command: ["sh", "-c", "while true; do sleep 5; done"]
    depends_on:
      web:
        condition: service_started
        restart: true

  # Unlabelled and long-running on the SAME image as web.
  # MUST NOT be updated: sharing an image is not consent.
  bystander:
    image: busybox:latest
    command: ["sh", "-c", "while true; do sleep 5; done"]

  # Labelled but deliberately stopped. MUST NOT be started.
  paused:
    image: busybox:latest
    command: ["sh", "-c", "while true; do sleep 5; done"]
    labels:
      freshdock.enable: "true"
      freshdock.mode: "live"
```

```bash
mkdir -p data
docker compose -p fdsmoke up -d
docker stop fdsmoke-paused-1

# Record the baseline: container ids are what the assertions turn on.
docker ps -a --filter label=com.docker.compose.project=fdsmoke \
  --format '{{.Names}}\t{{.Status}}\t{{.ID}}' | sort
wc -l data/migrations.log data/seed.log
```

## 1. The happy path

`freshdock recreate` always recreates its target, so it drives a rollout without
waiting for an upstream image to move.

```bash
freshdock recreate fdsmoke-web-1
```

Expected output, in this order:

```text
rollout of compose project fdsmoke:
  fdsmoke-migrate-1: re-ran to a successful exit
  fdsmoke-web-1: updated and healthy (new id ...)
  fdsmoke-sidecar-1: restarted (depends_on restart: true)
rollout complete: 3 step(s)
```

Then check the daemon, not just the log:

| Check | Expected |
|---|---|
| `wc -l data/migrations.log` | **2**, the unlabelled one-shot was re-run. |
| `wc -l data/seed.log` | **1**, `freshdock.enable=false` was honoured. |
| `fdsmoke-migrate-1` id | **changed**, recreated on the new image. |
| `fdsmoke-web-1` id | **changed**. |
| `fdsmoke-sidecar-1` id | **unchanged**, uptime reset: restarted, not recreated. |
| `fdsmoke-bystander-1` id | **unchanged**: an unlabelled sibling on the same image is not swept in. |
| `fdsmoke-paused-1` | still `Exited`, id unchanged: a stopped container is not started. |
| `docker ps -a \| grep -- -old-` | nothing, archives cleaned up. |

Ordering matters as much as the outcome: `migrate` must appear **before** `web`.

## 2. A failed migration aborts the rollout

This is the case the feature exists for. Trip the marker file and run again:

```bash
touch data/fail
freshdock recreate fdsmoke-web-1
```

```text
rollout ABORTED: fdsmoke-migrate-1 exited with code 1. The services after this
point were not touched and are still running their previous image.
```

| Check | Expected |
|---|---|
| `fdsmoke-web-1` id | **unchanged**: new code never came up against a failed migration. |
| `fdsmoke-sidecar-1` | not restarted. |
| `fdsmoke-migrate-1` | present, `Exited (1)`, kept for its logs. |
| `fdsmoke-migrate-1-old-<ts>` | present, the archive is kept too. |

Neither the failed container nor its archive is removed, and there is no
rollback: the command already ran, so its logs are the only evidence left.

Clean up before continuing: `rm data/fail`, then
`docker compose -p fdsmoke down -v && docker compose -p fdsmoke up -d`.

## 3. The opt-out switch

```bash
FRESHDOCK_COMPOSE_AWARE=0 freshdock recreate fdsmoke-web-1
```

Expected: the pre-#78 single-container output (`recreated fdsmoke-web-1:
healthy ...`), **no** rollout block, and
`data/migrations.log` still at one line.

## 4. The scheduler path

The rollout has to fire from the daemon, not just from `recreate`. Make the
local image genuinely stale so the digest probe reports an update:

```bash
docker pull busybox:1.36
docker pull busybox:latest

# The whole step only tests anything if the two really are different images.
# busybox:1.36 may already be what :latest points at, and then the scheduler
# finds the project up to date and never rolls anything out.
old=$(docker image inspect busybox:1.36 --format '{{.Id}}')
new=$(docker image inspect busybox:latest --format '{{.Id}}')
[ "$old" != "$new" ] || { echo "busybox:1.36 IS :latest; pick an older tag"; exit 1; }

docker tag busybox:1.36 busybox:latest   # local digest now differs from upstream
docker compose -p fdsmoke down -v && rm -f data/*.log && docker compose -p fdsmoke up -d
docker stop fdsmoke-paused-1

freshdock run --interval 5 --tick 5
```

Expected on the first tick: one `registry rate limit` line (the image is probed
**once**, for `web`), then the same three rollout steps. Every later tick must
be quiet: the project is up to date, and nothing re-rolls.

The single probe is the assertion that matters: it shows the rollout deduplicated
its own members instead of processing each one separately.

Restore the tag afterwards with `docker pull busybox:latest` (the rollout's own
pull normally does this for you).

## 5. A single-service project is unchanged

```bash
mkdir -p solo && cat > solo/docker-compose.yml <<'YAML'
services:
  only:
    image: busybox:latest
    command: ["sh", "-c", "while true; do sleep 5; done"]
    labels:
      freshdock.enable: "true"
      freshdock.mode: "live"
YAML
docker compose --project-directory solo -p fdsolo up -d
freshdock recreate fdsolo-only-1
```

Expected: the plain `recreated fdsolo-only-1: healthy ...` line and no rollout
block at all. When a plan comes down to the container that triggered it, the
ordinary path is used, so a one-service project behaves exactly as it did before
this feature existed.

## Cleanup

```bash
docker compose -p fdsmoke down -v
docker compose --project-directory solo -p fdsolo down -v
```
