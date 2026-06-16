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
> settings, registry credentials, and `run` flags are all environment variables —
> a container deployment needs no config file. The only thing that still wants a
> `freshdock.toml` is *declaring* a notification target (its secret can stay in the
> environment). See the [configuration reference](configuration.md).

## Label translation

| Watchtower label | freshdock label | Notes |
|---|---|---|
| `com.centurylinklabs.watchtower.enable=true` | `freshdock.enable=true` | Opt **in**. |
| `com.centurylinklabs.watchtower.enable=false` (with global watch) | *omit the labels*, or `freshdock.mode=off` | freshdock ignores unlabelled containers, so there's usually nothing to disable. |
| `com.centurylinklabs.watchtower.monitor-only=true` | `freshdock.mode=watch` | Detect + notify, never pull/recreate. |
| *(no per-container schedule)* | `freshdock.mode=nightly`/`weekly`/`monthly` + `freshdock.schedule=<cron>` | Scheduling is **per container** in freshdock, not a single global cron. |
| `com.centurylinklabs.watchtower.no-pull=true` | *(no equivalent)* | freshdock always pulls before recreate; there is no "recreate without pull". |
| `com.centurylinklabs.watchtower.depends-on` | *(no equivalent in v1)* | Dependency ordering is out of v1 scope; containers are processed independently. |

## Flag / environment translation

| Watchtower flag / env | freshdock equivalent | Notes |
|---|---|---|
| `--interval` / `WATCHTOWER_POLL_INTERVAL` | `freshdock run --interval <seconds>` or `FRESHDOCK_INTERVAL` | Cadence for `live`/`watch` containers. |
| `--schedule` / `WATCHTOWER_SCHEDULE` (global cron) | per-container `freshdock.mode` + `freshdock.schedule` | freshdock schedules each container on its own mode. |
| `--monitor-only` / `WATCHTOWER_MONITOR_ONLY` | `freshdock.mode=watch` | Per container, not global. |
| `--label-enable` / `WATCHTOWER_LABEL_ENABLE` | *(always on)* | freshdock is always label-gated; `freshdock.enable=true` is required. |
| *(no global default mode)* | `[settings] default_mode` or `FRESHDOCK_DEFAULT_MODE` | Sets the fallback mode for enabled containers with no `freshdock.mode` label. A `freshdock.mode` label still wins per container. |
| `--cleanup` / `WATCHTOWER_CLEANUP` | `[settings] cleanup = true`, `FRESHDOCK_CLEANUP=true`, or `freshdock.cleanup=true` per container | Off by default. Removes the *replaced image* after a healthy update; add `[settings] prune_dangling = true` (or `FRESHDOCK_PRUNE_DANGLING=true`) for a daemon-wide dangling prune. The replaced container archive is always removed regardless. |
| `--remove-volumes` / `WATCHTOWER_REMOVE_VOLUMES` | *(no equivalent)* | freshdock never removes volumes; recreate preserves all mounts. |
| `--rolling-restart` / `WATCHTOWER_ROLLING_RESTART` | *(not applicable)* | freshdock recreates one container at a time and health-gates each. |
| `--notifications` + `WATCHTOWER_NOTIFICATION_URL` (shoutrrr) | a `[notifications.<name>]` table in `freshdock.toml` | The one thing that needs a file — env vars can carry the *secret* (`FRESHDOCK_NOTIFY_<NAME>_*`) but can't declare the target. Webhook / Discord / Telegram / SMTP. See [notifications](notifications.md). |
| `WATCHTOWER_NOTIFICATIONS_LEVEL` / per-event config | per-target `triggers = ["available","succeeded","failed"]` | Subscribe each target to the events it cares about. |
| `REPO_USER` / `REPO_PASS` (registry auth) | `FRESHDOCK_REGISTRY_*` env (or a `[registry.<name>]` table) | Per-registry credentials. An env token alone is enough; no file needed. |
| `DOCKER_HOST` | `DOCKER_HOST` | Same — bollard honours the standard Docker env. |

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
labels. The only file you need is a `freshdock.toml` to *declare* the Discord
target (everything else is labels and environment):

```yaml
services:
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
```toml
# freshdock.toml
[notifications.discord]
type        = "discord"
webhook_url = "https://discord.com/api/webhooks/<id>/<token>"
triggers    = ["succeeded", "failed"]
```
```bash
freshdock run        # foreground daemon; or run it as the freshdock container
```

See [`examples/compose/`](https://github.com/Turbootzz/freshdock/tree/main/examples/compose) for complete, `docker compose
config`-valid stacks.
