# Coming from Watchtower?

[Watchtower](https://github.com/containrrr/watchtower) was archived in December
2025. freshdock is a from-scratch successor, so the concepts map closely but the
spelling differs. This page translates the labels and flags you already know.

The single biggest difference: **freshdock is opt-in.** Watchtower updates every
container unless you exclude it; freshdock ignores every container unless you set
`freshdock.enable=true`. And an enabled container with no explicit mode defaults
to `watch` (detect-and-notify, never restart) — nothing is recreated until you
ask for it with a mode like `live` or `nightly`.

> **Config is environment-first, like Watchtower.** freshdock's fleet-wide
> settings, registry credentials, `run` flags, and notification targets are all
> environment variables — a container deployment needs no config file. Notification
> targets even take a [shoutrrr-style URL](notifications.md#declaring-targets-from-the-environment),
> so `WATCHTOWER_NOTIFICATION_URL=discord://token@id` becomes
> `FRESHDOCK_NOTIFY_OPS_URL=discord://token@id` almost verbatim. See the
> [configuration reference](configuration.md).

## Keep your labels — they're read directly

freshdock reads the `com.centurylinklabs.watchtower.*` labels themselves, so an
existing fleet usually needs **no relabelling**: swap the updater container and
go. A `freshdock.*` label always wins when both are present, so you can migrate
label-by-label at your own pace.

What's honoured directly:

| Watchtower label | Effect in freshdock |
|---|---|
| `com.centurylinklabs.watchtower.enable=true` | Same as `freshdock.enable=true`. **Note:** the container lands on freshdock's safe default mode (`watch`, or `[settings] default_mode`) — it will not auto-update like Watchtower did until you give it an active mode. |
| `com.centurylinklabs.watchtower.enable=false` | Not opted in (same as having no labels — freshdock is opt-in anyway). |
| `com.centurylinklabs.watchtower.monitor-only=true` | Same as `freshdock.mode=watch`; beats `[settings] default_mode`. |
| `com.centurylinklabs.watchtower.lifecycle.pre-update` / `post-update` | Same as the [`freshdock.lifecycle.*` hooks](lifecycle-hooks.md). |
| `…lifecycle.pre-update-timeout` / `post-update-timeout` | Honoured in Watchtower's unit (**minutes**, converted; `0` = unlimited). The `freshdock.lifecycle.*-timeout` labels count seconds. |

Not supported — logged once and ignored: `no-pull`, `depends-on`, `scope`,
`lifecycle.pre-check` / `post-check`. Dependency ordering is out of v1 scope and
freshdock always pulls before recreate; there are no per-cycle check hooks.

## Label translation

Prefer clean labels (or need the finer-grained knobs)? The native spelling:

| Watchtower label | freshdock label | Notes |
|---|---|---|
| `com.centurylinklabs.watchtower.enable=true` | `freshdock.enable=true` | Opt **in**. |
| `com.centurylinklabs.watchtower.enable=false` (with global watch) | *omit the labels*, or `freshdock.mode=off` | freshdock ignores unlabelled containers, so there's usually nothing to disable. |
| `com.centurylinklabs.watchtower.monitor-only=true` | `freshdock.mode=watch` | Detect + notify, never pull/recreate. |
| *(no per-container schedule)* | `freshdock.mode=nightly`/`weekly`/`monthly` + `freshdock.schedule=<cron>` | Scheduling is **per container** in freshdock, not a single global cron. |
| `com.centurylinklabs.watchtower.lifecycle.pre-update` | `freshdock.lifecycle.pre-update` | Exec in the old container, but **stricter**: any non-zero exit (not just `75`), a timeout, or a failed exec skips the update. Timeout labels count **seconds** in freshdock, minutes in Watchtower. See [lifecycle hooks](lifecycle-hooks.md). |
| `com.centurylinklabs.watchtower.lifecycle.post-update` | `freshdock.lifecycle.post-update` | Best-effort exec in the new container after the health gate. |
| `com.centurylinklabs.watchtower.lifecycle.pre-check` / `post-check` | *(no equivalent)* | freshdock has no per-cycle check hooks. |
| `com.centurylinklabs.watchtower.no-pull=true` | *(no equivalent)* | freshdock always pulls before recreate; there is no "recreate without pull". |
| `com.centurylinklabs.watchtower.depends-on` | *(no equivalent in v1)* | Dependency ordering is out of v1 scope; containers are processed independently. |

## Flag / environment translation

| Watchtower flag / env | freshdock equivalent | Notes |
|---|---|---|
| `--interval` / `WATCHTOWER_POLL_INTERVAL` | `freshdock run --interval <seconds>` or `FRESHDOCK_INTERVAL` | Cadence for `live`/`watch` containers. |
| `--schedule` / `WATCHTOWER_SCHEDULE` (global cron) | per-container `freshdock.mode` + `freshdock.schedule` | freshdock schedules each container on its own mode. |
| `--monitor-only` / `WATCHTOWER_MONITOR_ONLY` | `freshdock.mode=watch` | Per container, not global. |
| `--label-enable` / `WATCHTOWER_LABEL_ENABLE` | *(always on)* | freshdock is always label-gated; `freshdock.enable=true` is required. |
| `--enable-lifecycle-hooks` / `WATCHTOWER_LIFECYCLE_HOOKS` | *(not needed)* | Setting a `freshdock.lifecycle.*` label is the opt-in; there is no global switch. |
| *(no global default mode)* | `[settings] default_mode` or `FRESHDOCK_DEFAULT_MODE` | Sets the fallback mode for enabled containers with no `freshdock.mode` label. A `freshdock.mode` label still wins per container. |
| `--cleanup` / `WATCHTOWER_CLEANUP` | `[settings] cleanup = true`, `FRESHDOCK_CLEANUP=true`, or `freshdock.cleanup=true` per container | Off by default. Removes the *replaced image* after a healthy update; add `[settings] prune_dangling = true` (or `FRESHDOCK_PRUNE_DANGLING=true`) for a daemon-wide dangling prune. The replaced container archive is always removed regardless. |
| `--remove-volumes` / `WATCHTOWER_REMOVE_VOLUMES` | *(no equivalent)* | freshdock never removes volumes; recreate preserves all mounts. |
| `--rolling-restart` / `WATCHTOWER_ROLLING_RESTART` | *(not applicable)* | freshdock recreates one container at a time and health-gates each. |
| `--notifications` + `WATCHTOWER_NOTIFICATION_URL` (shoutrrr) | `FRESHDOCK_NOTIFY_<NAME>_URL` (shoutrrr-style URL) or a `[notifications.<name>]` table | Near drop-in: `discord://token@id`, `telegram://token@telegram?chats=id`, `smtp://…`, or `https://…`. No file required. Add `FRESHDOCK_NOTIFY_<NAME>_TRIGGERS` to filter events. See [notifications](notifications.md#declaring-targets-from-the-environment). |
| `WATCHTOWER_NOTIFICATIONS_LEVEL` / per-event config | per-target `triggers = ["available","succeeded","failed"]` | Subscribe each target to the events it cares about. |
| `REPO_USER` / `REPO_PASS` (registry auth) | `FRESHDOCK_REGISTRY_*` env (or a `[registry.<name>]` table) | Per-registry credentials. An env token alone is enough; no file needed. |
| `DOCKER_HOST` | `DOCKER_HOST` | Same — the standard Docker env var, with the same schemes: `unix://`, `tcp://`, `http://`, `https://`, `ssh://`. Unset, freshdock falls back to `/var/run/docker.sock` and then to Podman's sockets. |

## A worked example

Watchtower (global daily updates, one excluded container, Discord notifications):

```bash
docker run -d --name watchtower \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -e WATCHTOWER_SCHEDULE="0 0 4 * * *" \
  -e WATCHTOWER_NOTIFICATION_URL="discord://token@id" \
  containrrr/watchtower
```
```yaml
services:
  db:
    labels:
      - "com.centurylinklabs.watchtower.enable=false"
```

The freshdock equivalent — schedule and notify-opt-in move onto the containers as
labels, and the Discord target is a single env var (no file at all):

```yaml
services:
  freshdock:
    image: ghcr.io/turbootzz/freshdock:latest
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock
    environment:
      # near-verbatim from WATCHTOWER_NOTIFICATION_URL=discord://token@id
      - "FRESHDOCK_NOTIFY_OPS_URL=discord://<token>@<id>"
      - "FRESHDOCK_NOTIFY_OPS_TRIGGERS=succeeded,failed"
    command: ["run"]
  app:
    image: ghcr.io/example/app:latest
    labels:
      - "freshdock.enable=true"
      - "freshdock.mode=nightly"      # 04:00 daily by default
      - "freshdock.notify=true"
  db:
    image: postgres:16
    # no freshdock.* labels → ignored entirely (no need to "disable" it)
```

See [`examples/compose/`](https://github.com/Turbootzz/freshdock/tree/main/examples/compose) for complete, `docker compose
config`-valid stacks.
