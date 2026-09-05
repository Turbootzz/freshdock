# Coming from Watchtower?

[Watchtower](https://github.com/containrrr/watchtower) was archived in December 2025.
freshdock is a from-scratch successor, so the concepts map closely but the spelling
differs. This page translates the labels and flags you already know. For a feature
scorecard against Watchtower, see
[freshdock.dev/watchtower-alternative](https://freshdock.dev/watchtower-alternative).

The biggest difference: freshdock is opt-in. Watchtower updates every container unless you
exclude it; freshdock ignores every container unless you set `freshdock.enable=true`. An
enabled container with no explicit mode defaults to `watch` (detect and notify, never
restart), so nothing is recreated until you ask with a mode like `live` or `nightly`. One
env var flips this back, see
[keeping Watchtower's opt-out model](#keeping-watchtowers-opt-out-model).

Config is environment-first, like Watchtower: fleet-wide settings, registry credentials,
`run` flags, and notification targets are all environment variables, so a container
deployment needs no config file. Notification targets take a
[shoutrrr-style URL](notifications.md#declaring-targets-from-the-environment), so
`WATCHTOWER_NOTIFICATION_URL=discord://token@id` becomes
`FRESHDOCK_NOTIFY_OPS_URL=discord://token@id` almost verbatim. See the
[configuration reference](configuration.md).

## Keep your labels

freshdock reads the `com.centurylinklabs.watchtower.*` labels themselves, so an existing
fleet usually needs no relabelling: swap the updater container and go. A `freshdock.*`
label always wins when both are present, so you can migrate label-by-label at your own
pace.

What's honoured directly:

| Watchtower label | Effect in freshdock |
|---|---|
| `com.centurylinklabs.watchtower.enable=true` | Same as `freshdock.enable=true`. The container lands on freshdock's safe default mode (`watch`, or `[settings] default_mode`), so it will not auto-update like Watchtower did until you give it an active mode. |
| `com.centurylinklabs.watchtower.enable=false` | An explicit opt-out: never enabled, never re-run as a compose one-shot, never repaired as a network-namespace sidecar (an absent label allows both of those). Under [`FRESHDOCK_WATCH_ALL`](#keeping-watchtowers-opt-out-model) it is the exclusion label. |
| `com.centurylinklabs.watchtower.monitor-only=true` | Same as `freshdock.mode=watch`; beats `[settings] default_mode`. |
| `com.centurylinklabs.watchtower.lifecycle.pre-update` / `post-update` | Same as the [`freshdock.lifecycle.*` hooks](lifecycle-hooks.md). |
| `...lifecycle.pre-update-timeout` / `post-update-timeout` | Honoured in Watchtower's unit (minutes, converted; `0` = unlimited). The `freshdock.lifecycle.*-timeout` labels count seconds. |

Not supported, logged once and ignored: `no-pull`, `depends-on`, `scope`,
`lifecycle.pre-check` / `post-check`. freshdock always pulls before recreate and has no
per-cycle check hooks. Dependency ordering is not read from the Watchtower label: inside a
Docker Compose project freshdock reads Compose's own `depends_on` graph instead, and rolls
the project out as one unit. See [Compose projects](compose.md).

## Keeping Watchtower's opt-out model

Set `FRESHDOCK_WATCH_ALL=true` (or `[settings] watch_all = true`) and freshdock treats
every running container as enabled unless it opts out, the way Watchtower did. Pair it with
`FRESHDOCK_DEFAULT_MODE` to say how those containers update; without it they land on
`watch` and nothing is recreated.

```yaml
services:
  freshdock:
    image: ghcr.io/turbootzz/freshdock:latest
    command: ["run"]
    environment:
      FRESHDOCK_WATCH_ALL: "true"         # every container, unless it opts out
      FRESHDOCK_DEFAULT_MODE: "nightly"   # 04:00 daily, in place of a global WATCHTOWER_SCHEDULE
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock
    restart: unless-stopped

  db:
    image: postgres:16
    labels:
      - "com.centurylinklabs.watchtower.enable=false"   # still an exclusion
```

The exclusion labels you already have keep working:
`com.centurylinklabs.watchtower.enable=false`, `freshdock.enable=false`, and
`freshdock.mode=off` all opt a container back out. Explicit `freshdock.*` labels are
unaffected: a `freshdock.mode=weekly` label still wins over `FRESHDOCK_DEFAULT_MODE`.

freshdock excludes its own container from this, so it never tries to update itself.
Detection and the custom-`hostname` caveat are covered in
[watching every container](configuration.md#watching-every-container).

## Label translation

If you prefer clean labels, or need the finer-grained knobs, this is the native spelling:

| Watchtower label | freshdock label | Notes |
|---|---|---|
| `com.centurylinklabs.watchtower.enable=true` | `freshdock.enable=true` | Opt in. |
| `com.centurylinklabs.watchtower.enable=false` (with global watch) | *omit the labels*, or `freshdock.mode=off` | freshdock ignores unlabelled containers, so there is usually nothing to disable. Keep the label on a compose one-shot or a network-namespace sidecar you never want touched. With [`FRESHDOCK_WATCH_ALL`](#keeping-watchtowers-opt-out-model) on, keep the exclusion label (either spelling works). |
| `com.centurylinklabs.watchtower.monitor-only=true` | `freshdock.mode=watch` | Detect and notify, never pull or recreate. |
| *(no per-container schedule)* | `freshdock.mode=nightly`/`weekly`/`monthly` + `freshdock.schedule=<cron>` | Scheduling is per container in freshdock, not a single global cron. |
| `com.centurylinklabs.watchtower.lifecycle.pre-update` | `freshdock.lifecycle.pre-update` | Exec in the old container, but stricter: any non-zero exit (not only `75`), a timeout, or a failed exec skips the update. Timeout labels count seconds in freshdock, minutes in Watchtower. See [lifecycle hooks](lifecycle-hooks.md). |
| `com.centurylinklabs.watchtower.lifecycle.post-update` | `freshdock.lifecycle.post-update` | Best-effort exec in the new container after the health gate. |
| `com.centurylinklabs.watchtower.lifecycle.pre-check` / `post-check` | *(no equivalent)* | freshdock has no per-cycle check hooks. |
| `com.centurylinklabs.watchtower.no-pull=true` | *(no equivalent)* | freshdock always pulls before recreate; there is no "recreate without pull". |
| `com.centurylinklabs.watchtower.depends-on` | *(no equivalent)* | The label itself is ignored. Inside a Compose project freshdock uses Compose's native `depends_on` graph, which needs no extra labels, see [Compose projects](compose.md). Outside one, containers are processed independently. |

## Flag / environment translation

| Watchtower flag / env | freshdock equivalent | Notes |
|---|---|---|
| `--interval` / `WATCHTOWER_POLL_INTERVAL` | `freshdock run --interval <seconds>` or `FRESHDOCK_INTERVAL` | Cadence for `live`/`watch` containers. |
| `--schedule` / `WATCHTOWER_SCHEDULE` (global cron) | per-container `freshdock.mode` + `freshdock.schedule` | freshdock schedules each container on its own mode. |
| `--monitor-only` / `WATCHTOWER_MONITOR_ONLY` | `freshdock.mode=watch` | Per container, not global. |
| `--label-enable` / `WATCHTOWER_LABEL_ENABLE` | *(the default)* | freshdock is label-gated out of the box; `freshdock.enable=true` is required. Set `FRESHDOCK_WATCH_ALL=true` for the opt-out behaviour you get without this flag in Watchtower. |
| `--enable-lifecycle-hooks` / `WATCHTOWER_LIFECYCLE_HOOKS` | *(not needed)* | Setting a `freshdock.lifecycle.*` label is the opt-in; there is no global switch. |
| *(no global default mode)* | `[settings] default_mode` or `FRESHDOCK_DEFAULT_MODE` | Sets the fallback mode for enabled containers with no `freshdock.mode` label. A `freshdock.mode` label still wins per container. |
| `--cleanup` / `WATCHTOWER_CLEANUP` | `[settings] cleanup = true`, `FRESHDOCK_CLEANUP=true`, or `freshdock.cleanup=true` per container | Off by default. Removes the *replaced image* after a healthy update; add `[settings] prune_dangling = true` (or `FRESHDOCK_PRUNE_DANGLING=true`) for a daemon-wide dangling prune. The replaced container archive is always removed regardless. |
| `--remove-volumes` / `WATCHTOWER_REMOVE_VOLUMES` | *(no equivalent)* | freshdock never removes volumes; recreate preserves all mounts. |
| `--rolling-restart` / `WATCHTOWER_ROLLING_RESTART` | *(not applicable)* | freshdock recreates one container at a time and health-gates each. |
| `--notifications` + `WATCHTOWER_NOTIFICATION_URL` (shoutrrr) | `FRESHDOCK_NOTIFY_<NAME>_URL` (shoutrrr-style URL) or a `[notifications.<name>]` table | Near drop-in: `discord://token@id`, `telegram://token@telegram?chats=id`, `smtp://...`, or `https://...`. No file required. Add `FRESHDOCK_NOTIFY_<NAME>_TRIGGERS` to filter events. See [notifications](notifications.md#declaring-targets-from-the-environment). |
| `WATCHTOWER_NOTIFICATIONS_LEVEL` / per-event config | per-target `triggers = ["available","succeeded","failed"]` | Subscribe each target to the events it cares about. |
| `REPO_USER` / `REPO_PASS` (registry auth) | `FRESHDOCK_REGISTRY_*` env (or a `[registry.<name>]` table) | Per-registry credentials. An env token alone is enough; no file needed. |
| `DOCKER_HOST` | `DOCKER_HOST` | The same standard Docker env var, with the same schemes: `unix://`, `tcp://`, `http://`, `https://`, `ssh://`. Unset, freshdock falls back to `/var/run/docker.sock` and then to Podman's sockets. |

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

The freshdock equivalent: schedule and notify opt-in move onto the containers as labels,
and the Discord target is a single env var, with no file at all.

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
    # no freshdock.* labels, so it is ignored entirely (no need to "disable" it)
```

See [`examples/compose/`](https://github.com/Turbootzz/freshdock/tree/main/examples/compose)
for complete stacks that pass `docker compose config`.
