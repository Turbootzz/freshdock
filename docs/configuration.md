# Configuration reference

This is the single source of truth for everything freshdock reads: the per-container
**labels**, the **`freshdock.toml`** file (settings, registries, notifications), and
the **environment variables** that override the file.

- New here? Start with the [quickstart](quickstart.md).
- Just need the command flags? See the [CLI reference](cli-reference.md).

## Contents

- [Labels](#labels) — per-container behaviour
- [The `freshdock.toml` file](#the-freshdocktoml-file) — location and precedence
- [`[settings]`](#settings) — fleet-wide defaults
- [`[registry.<name>]`](#registryname) — registry credentials
- [`[notifications.<name>]`](#notificationsname) — notification targets
- [Environment variables](#environment-variables)
- [A complete example](#a-complete-example)

---

## Labels

freshdock is **opt-in**: a container with no `freshdock.enable=true` is ignored
entirely. All behaviour is driven by these Docker labels (set them in compose under
`labels:` or with `docker run --label`).

| Label | Values | Default | Meaning |
|---|---|---|---|
| `freshdock.enable` | `true` / `false` | `false` | Master switch. Without `true`, the container is invisible to freshdock and every other label is ignored. |
| `freshdock.mode` | `live` / `nightly` / `weekly` / `monthly` / `watch` / `off` | `watch` (or `[settings] default_mode`) | How and when this container updates. See [scheduling](scheduling.md). |
| `freshdock.schedule` | 5-field cron | the mode's default | Override the cron for a calendar mode. Ignored for `live` / `watch` / `off`. See [cron syntax](scheduling.md#cron-syntax). |
| `freshdock.notify` | `true` / `false` | `false` | Emit notifications for this container's update events. Requires a configured `[notifications.*]` target. See [notifications](notifications.md). |
| `freshdock.cleanup` | `true` / `false` | `[settings] cleanup` (else `false`) | After a healthy update, remove the image the *old* container ran. Overrides the global `[settings] cleanup`. See [health & cleanup](health-and-rollback.md#image-cleanup). |

Values are case-insensitive and tolerate surrounding whitespace. An invalid value
is reported with the offending label named.

When `freshdock.enable=true` but `freshdock.mode` is absent, the mode is `watch`
(detect-and-notify, never mutate) — a non-destructive default. Change this
fleet-wide fallback with [`[settings] default_mode`](#settings); an explicit
`freshdock.mode` label always wins.

> **Pinned images.** A container whose image is pinned to a digest
> (`repo@sha256:…`) has no moving tag to follow. freshdock reports it as
> `pinned (no check)` and never updates it.

---

## The `freshdock.toml` file

The file is **optional** — freshdock runs without it (public images need no
credentials, and notifications are simply off). It is resolved in this order:

1. `--config <path>` flag
2. `$FRESHDOCK_CONFIG`
3. `./freshdock.toml` in the working directory

An explicit path (flag or env) that doesn't exist is an error; a missing default
`./freshdock.toml` is fine (you get an empty config). Secrets in the file are
redacted in all log output, even at `RUST_LOG=trace`, and can be supplied via
[environment variables](#environment-variables) instead.

The file has three top-level tables, all optional: `[settings]`, `[registry.*]`,
and `[notifications.*]`.

### `[settings]`

Fleet-wide defaults. Every key is optional.

```toml
[settings]
default_mode   = "watch"   # fallback mode for an enabled container with no
                           # freshdock.mode label. Invalid → warn + fall back to watch.
cleanup        = false     # remove the replaced image after a healthy update;
                           # overridable per container with freshdock.cleanup.
prune_dangling = false     # additionally run a daemon-wide dangling-image prune
                           # after each successful update (no per-container override).
```

| Key | Type | Default | Notes |
|---|---|---|---|
| `default_mode` | string (a mode name) | unset → `watch` | Applied to enabled containers without a `freshdock.mode` label. A `freshdock.mode` label always overrides it. |
| `cleanup` | bool | `false` | Default for `freshdock.cleanup`. Best-effort; a shared image in use elsewhere is kept, and a cleanup failure never fails the update. |
| `prune_dangling` | bool | `false` | Daemon-wide; prunes untagged images after a success. Best-effort. |

### `[registry.<name>]`

One table per registry. `<name>` may be a friendly alias (`dockerhub`, `ghcr`,
`quay`, `lscr`) or a literal host (`"registry.example.com"`); both fold onto the
same registry as the matching image reference.

```toml
[registry.ghcr]
username = "octocat"          # any non-empty value works for a GHCR PAT
token    = "ghp_xxx"          # personal access token (read:packages for GHCR)

[registry.dockerhub]
username = "myuser"           # required for Docker Hub
token    = "dckr_pat_xxx"

[registry."registry.example.com"]
token    = "…"                # username optional
```

| Key | Type | Required | Notes |
|---|---|---|---|
| `username` | string | depends on registry | Docker Hub needs the real account name; GHCR and most others accept any non-empty value with a PAT. |
| `token` | string (secret) | yes | Password or personal access token. Redacted in logs. |

For per-registry guidance (PAT scopes, the alias list, a smoke test, and what's
out of scope), see [registry-auth.md](registry-auth.md).

### `[notifications.<name>]`

One table per target, selected by `type`. Every target may set an optional
`triggers` list to subscribe to a subset of events; omit it (or use `[]`) to
receive all three (`available`, `succeeded`, `failed`). Payload formats and the
event/mode matrix are documented in [notifications.md](notifications.md).

```toml
[notifications.ops-webhook]
type = "webhook"
url  = "https://example.com/hooks/freshdock"
# triggers omitted → all of available, succeeded, failed

[notifications.discord]
type        = "discord"
webhook_url = "https://discord.com/api/webhooks/123/abc"
triggers    = ["succeeded", "failed"]

[notifications.tg]
type      = "telegram"
bot_token = "123456:ABC-DEF"          # or FRESHDOCK_NOTIFY_TG_BOT_TOKEN
chat_id   = "987654321"
triggers  = ["failed"]

[notifications.email]
type     = "smtp"
host     = "smtp.example.com"
port     = 587                        # default 587
username = "freshdock@example.com"    # username + password together, or neither
password = "s3cr3t"                   # or FRESHDOCK_NOTIFY_EMAIL_PASSWORD
from     = "freshdock@example.com"
to       = ["admin@example.com"]      # non-empty list
starttls = true                       # default true; false → implicit TLS (465)
triggers = ["succeeded", "failed"]
```

Per-type keys:

| `type` | Keys | Notes |
|---|---|---|
| `webhook` | `url` (secret) | Generic JSON POST. |
| `discord` | `webhook_url` (secret) | Posts a coloured embed. |
| `telegram` | `bot_token` (secret), `chat_id` | Plain-text message via the Bot API. |
| `smtp` | `host`, `port` (=587), `username`?, `password`? (secret), `from`, `to` (list), `starttls` (=true) | `username`+`password` must be set together or both omitted (anonymous relay). |

All targets also accept `triggers = ["available", "succeeded", "failed"]` (subset
allowed).

---

## Environment variables

Environment variables override the file **per field** (a lone `…_TOKEN` replaces
the file token while keeping the file username). The `<NAME>` is the table name,
upper-cased, with `-` → `_`.

| Variable | Overrides | Notes |
|---|---|---|
| `FRESHDOCK_CONFIG` | config file path | The `--config` flag wins over it. |
| `FRESHDOCK_REGISTRY_<NAME>_USERNAME` | `[registry.<name>] username` | `<NAME>` = alias (`DOCKERHUB`, `GHCR`, `QUAY`, `LSCR`). Hosts with dots can't be expressed unambiguously — configure those in the file. |
| `FRESHDOCK_REGISTRY_<NAME>_TOKEN` | `[registry.<name>] token` | |
| `FRESHDOCK_NOTIFY_<NAME>_BOT_TOKEN` | a Telegram target's `bot_token` | |
| `FRESHDOCK_NOTIFY_<NAME>_PASSWORD` | an SMTP target's `password` | |
| `RUST_LOG` | log verbosity | e.g. `info`, `freshdock=debug`, `trace`. Default `info`. |
| `DOCKER_HOST` | Docker daemon endpoint | Honoured by the underlying Docker client (bollard). |

`freshdock --help` prints the same override list (`after_long_help`).

---

## A complete example

```toml
# freshdock.toml
[settings]
default_mode   = "watch"
cleanup        = true
prune_dangling = false

[registry.ghcr]
username = "octocat"
token    = "ghp_xxx"

[notifications.discord]
type        = "discord"
webhook_url = "https://discord.com/api/webhooks/123/abc"
triggers    = ["succeeded", "failed"]
```

Runnable compose stacks live in [`examples/compose/`](../examples/compose/).
