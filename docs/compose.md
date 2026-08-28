# Docker Compose projects

freshdock's normal unit of work is one container. Inside a Compose project that
is not enough, and the gap is quiet rather than loud.

Take a stack that runs its migrations as a one-shot service:

```yaml
services:
  migrate:
    image: app:latest
    command: ./bin/run-migrate.sh
    restart: "no"

  web:
    image: app:latest
    labels:
      freshdock.enable: "true"
      freshdock.mode: "live"
    depends_on:
      migrate:
        condition: service_completed_successfully
```

After a successful run, `migrate` sits in `exited (0)`. It is not a *running*
container, so a per-container updater never sees it. A new `app:latest` gets
pulled, `web` is recreated, and `migrate` keeps the old code. The result is new
application code against an old database schema: nothing crashes, the health
gate is satisfied, and the breakage shows up later as a missing column or a
malformed query.

freshdock treats the whole project as one update unit instead. When an update
applies to a container carrying `com.docker.compose.project`, it:

1. lists **every** container in that project, stopped ones included,
2. picks the ones the moved image applies to,
3. orders them by `depends_on`,
4. re-runs the one-shots the project waits on and blocks until they exit `0`,
5. updates the rest in dependency order, and
6. restarts the dependents that asked for it.

If a one-shot fails, the rollout **stops**. Nothing downstream is touched, so
`web` stays up on the image it was already running.

This is on by default. Turn it off with `[settings] compose_aware = false` or
`FRESHDOCK_COMPOSE_AWARE=false`, and every container is updated on its own again.

## No configuration needed

There is nothing to point freshdock at. Compose writes the entire dependency
graph into the containers' own labels, so the project is reconstructed from the
Docker socket alone: no compose file to locate, no bind mount of the project
directory, and no `docker compose` binary in the freshdock image.

| Label | What freshdock reads from it |
|---|---|
| `com.docker.compose.project` | Which project a container belongs to. |
| `com.docker.compose.service` | Its service name, the node in the graph. |
| `com.docker.compose.depends_on` | Its edges: `service:condition:restart`, comma-separated. |
| `com.docker.compose.oneoff` | `True` marks a `docker compose run` leftover, which is never part of a rollout. |

## What gets updated

The label gate still decides. A container is updated when it is enabled the
usual way: its own `freshdock.enable=true`, or fleet-wide
[`watch_all`](configuration.md#watching-every-container).

There is exactly one exception, and it is deliberately narrow.

> **An unlabelled one-shot is re-run anyway** when another service waits on it
> with `service_completed_successfully`. That condition is the compose file
> itself saying the service must complete before its dependents start, which is
> precisely the case above. Requiring a label there would leave the sharp edge
> in place, since nobody labels their migration service.

The exception covers an *absent* label, not a quiet one. A service in
[`watch` mode](scheduling.md#modes) is never touched, one-shot or not, because
watch means "tell me, never restart me" and re-running a one-shot is a restart.

> **This matters under [`watch_all`](configuration.md#watching-every-container).**
> `watch_all` enables every container but leaves its mode at `watch` until
> `[settings] default_mode` says otherwise, so on that configuration a rollout
> has nothing it is allowed to do. Set `default_mode` to an updating mode, or
> label the services you want rolled out.

Everything else follows the ordinary rules:

| Project member | Rolled out? |
|---|---|
| Labelled `freshdock.enable=true` (or enabled by `watch_all`), on the moved image | **Yes.** |
| Unlabelled, and another service waits on it with `service_completed_successfully` | **Yes**, re-run as a one-shot. |
| Unlabelled and long-running, even on the same image | No. Sharing an image is not consent. |
| In `watch` mode | No. Watch means detect and report, never restart, and re-running a one-shot is a restart. Such a container is not bumped by a `restart: true` edge either. |
| `freshdock.enable=false`, `com.centurylinklabs.watchtower.enable=false`, or `freshdock.mode=off` | No. An explicit opt-out always wins, one-shots included. |
| Stopped, and not a one-shot | No. If you stopped it, freshdock will not start it. |
| A one-shot that is currently running | No. It is mid-run, and stomping it would kill a migration in flight. This holds even for an explicit `freshdock recreate` of that container. |
| A `<name>-old-<ts>` archive | No. Archives keep the original's compose labels, so a kept one would otherwise be re-run as a one-shot. |
| On a different image than the one that moved | No. Members are matched on the image reference, or on the image id when the tag has since moved and the daemon reports a bare id. |
| freshdock's own container | No, same self-guard as [`watch_all`](configuration.md#watching-every-container). |
| A `docker compose run` one-off | No. |

## Ordering and conditions

Services are ordered so every dependency is handled before its dependents
(a topological sort of `depends_on`). Independent services are ordered by name,
so a rollout is reproducible.

Each `depends_on` condition maps to what freshdock waits for:

| Condition | Behaviour |
|---|---|
| `service_completed_successfully` | The dependency is re-run and the rollout blocks until it exits. A non-zero exit or a timeout **aborts** the rollout. |
| `service_healthy` | When the dependency is part of the rollout, its own [health gate](health-and-rollback.md) is what the dependents wait on: the next service is not started until it reports healthy. A dependency the rollout does not touch is already up, so there is nothing to wait for. |
| `service_started` | Ordering alone, nothing extra to wait for. |

A one-shot has 10 minutes to finish before the rollout gives up on it. That is
deliberately generous: a schema migration on a large database is the case this
exists for, and abandoning one halfway is worse than waiting.

> **Cyclic `depends_on`** cannot happen through Compose, which rejects it. If a
> hand-written label produces one anyway, freshdock logs it and rolls the
> affected services out in name order rather than hanging.

### `restart: true` dependents

Compose's `depends_on.<service>.restart` means "restart me when this dependency
is recreated". freshdock honours it, after every update in the rollout has
landed:

```yaml
  web:
    depends_on:
      config-sidecar:
        condition: service_started
        restart: true
```

This is a **restart**, not an update: the dependent's own image never moves, so
there is no pull and no rollback surface. The stop honours the container's own
`stop_signal` and `stop_grace_period`, exactly as a recreate would.

A dependent is left alone when it explicitly opts out, when its labels cannot be
read, or when it is in `watch` mode: `restart: true` is written for Compose,
while `freshdock.mode=watch` is written for freshdock and says not to restart
this container at all. A stopped dependent is not started, and a restart that
fails is logged without failing the rollout.

## When a one-shot fails

This is the case the feature exists for, so it is worth being precise about.

The rollout stops immediately. Concretely:

- The **failed container is kept**, exited non-zero, and so is the archive of
  its previous instance. Its logs are the only record of why the migration
  failed, so neither is cleaned up. `docker logs <project>-<service>-1` is the
  first thing to read.
- It is **not rolled back.** The command already ran and may have applied part
  of its work; restoring the previous *container object* would change nothing
  about the database and would throw the evidence away.
- Every service after it in the order is **untouched** and still serving its
  previous image. Your stack keeps working on the old version. It stays that way
  for the rest of the cycle too: the rollout claims the members it never
  reached, so nothing re-triggers it and re-runs the failed step per member.
- Services updated *before* the failure stay updated. There is no safe way to
  un-apply a migration, so freshdock does not pretend there is.
- With [notifications](notifications.md) configured, one `failed`-trigger event
  is emitted for the **project**, naming what was updated and what was not.

Fix the cause, then let the next cycle run, or re-run it by hand:

```bash
freshdock recreate web
```

## Reading the logs

A rollout logs per project rather than per container, so the sequence reads as
one operation:

```text
INFO rollout: starting compose project rollout project=shop targets=2
INFO rollout: re-running one-shot project=shop container=shop-migrate-1 service=migrate
INFO one-shot: finished container=6a8e89bf0c1a exit_code=0
INFO rollout: updating service project=shop container=shop-web-1 service=web
INFO rollout: restarting dependent (depends_on restart: true) project=shop container=shop-sidecar-1
INFO rollout: complete project=shop steps=3
```

`freshdock recreate` prints the same sequence on stdout:

```console
$ freshdock recreate shop-web-1
rollout of compose project shop:
  shop-migrate-1: re-ran to a successful exit
  shop-web-1: updated and healthy (new id 16d07544c3e8)
  shop-sidecar-1: restarted (depends_on restart: true)
rollout complete: 3 step(s)
```

## Limits

- **A rollout is triggered by a container freshdock already watches.** The
  project is examined because an update was found for one of its labelled
  containers. A one-shot on an image that no watched container shares is never
  reached; give the one-shot the same image as the service that depends on it
  (the normal Compose pattern), or label a service that does run that image.
- **A single-service project behaves exactly as before.** When the plan comes
  down to the one container that triggered it, with nothing to order and
  nothing to restart, freshdock uses the ordinary single-container path.
- **`docker compose up` is not shelled out to.** It would need the compose
  binary and a bind mount of the project directory, neither of which exists in
  a socket-only deployment. The graph in the labels is enough.
- **Scaled services** (`--scale`) are handled: every replica of a target
  service is updated, in name order.

## See also

- [Configuration](configuration.md#settings) — the `compose_aware` setting.
- [Health gating & rollback](health-and-rollback.md) — what happens per container.
- [Scheduling & modes](scheduling.md) — when a rollout is triggered at all.
- [Compose rollouts playbook](manual-tests/compose-rollout.md) — verifying it against a real daemon.
