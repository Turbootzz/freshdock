# Configuration reference

This is the single source of truth for everything freshdock reads. Configuration
comes from three places:

- **Labels** on each container — what to update and when (per-container behaviour).
- **Environment variables** — fleet-wide settings, registry credentials, the `run`
  flags, and notification secrets. This is the primary way to configure a
  deployment: nothing to mount.
- An optional **`freshdock.toml`** file — needed only to hold credentials for a
  registry whose host can't be spelled as an env-var name. Everything else,
  notification targets included, can come from the environment. Environment
  variables override the file, per field.

> **You don't need a file.** Registry credentials, the `[settings]` defaults, the
> `run` flags, and now notification targets (via a
> [shoutrrr-style URL](notifications.md#declaring-targets-from-the-environment)) all
> have environment variables — a container deployment never has to mount a
> `freshdock.toml`. The only thing env can't express is a registry whose host has
> dots in it (its name can't be spelled as an env-var name).

- New here? Start with the [quickstart](quickstart.md).
- Just need the command flags? See the [CLI reference](cli-reference.md).

## Contents

- [Labels](#labels) — per-container behaviour
- [Watching every container](#watching-every-container) (the optional opt-out mode)
- [Environment variables](#environment-variables) — the primary fleet-wide config
- [The optional `freshdock.toml` file](#the-optional-freshdocktoml-file) — when you need one
- [`[settings]`](#settings) — fleet-wide defaults
- [`[registry.<name>]`](#registryname) — registry credentials
- [`[notifications.<name>]`](#notificationsname) — notification targets
- [A complete example](#a-complete-example)

---

## Labels

freshdock is **opt-in**: a container with no `freshdock.enable=true` is ignored
entirely. (That default can be inverted fleet-wide, see
[watching every container](#watching-every-container).) All behaviour is driven by
these Docker labels (set them in compose under `labels:` or with
`docker run --label`).

| Label | Values | Default | Meaning |
|---|---|---|---|
| `freshdock.enable` | `true` / `false` | `false` (`true` under [`watch_all`](#watching-every-container)) | Master switch. Without `true`, the container is invisible to freshdock and every other label is ignored. Under [`watch_all`](#watching-every-container) an absent label counts as enabled (the other labels then apply as usual) and `false` is the explicit opt-out. |
| `freshdock.mode` | `live` / `nightly` / `weekly` / `monthly` / `watch` / `off` | `watch` (or `[settings] default_mode`) | How and when this container updates. See [scheduling](scheduling.md). |
| `freshdock.schedule` | 5-field cron | the mode's default | Override the cron for a calendar mode. Ignored for `live` / `watch` / `off`. See [cron syntax](scheduling.md#cron-syntax). |
| `freshdock.notify` | `true` / `false` | `false` | Emit notifications for this container's update events. Requires a configured `[notifications.*]` target. See [notifications](notifications.md). |
| `freshdock.cleanup` | `true` / `false` | `[settings] cleanup` (else `false`) | After a healthy update, remove the image the *old* container ran. Overrides the global `[settings] cleanup`. See [health & cleanup](health-and-rollback.md#image-cleanup). |
| `freshdock.lifecycle.pre-update` | shell command | *(none)* | Exec'd (`sh -c`) in the old container before it is stopped; a non-zero exit, timeout, or exec failure **skips the update**. See [lifecycle hooks](lifecycle-hooks.md). |
| `freshdock.lifecycle.pre-update-timeout` | seconds | `60` | Time budget for the pre-update hook; `0` disables the timeout. |
| `freshdock.lifecycle.post-update` | shell command | *(none)* | Exec'd (`sh -c`) in the new container after it passes the health gate. Best-effort; a failure never fails the update. See [lifecycle hooks](lifecycle-hooks.md). |
| `freshdock.lifecycle.post-update-timeout` | seconds | `60` | Time budget for the post-update hook; `0` disables the timeout. |

Values are case-insensitive and tolerate surrounding whitespace. An invalid value
is reported with the offending label named.

> **Watchtower labels are read too.** `com.centurylinklabs.watchtower.enable`,
> `…monitor-only`, and the `…lifecycle.pre-update`/`post-update` hook labels
> (timeouts in Watchtower's **minutes**) are honoured as fallbacks, so a
> migrated fleet needs no relabelling. A `freshdock.*` label always wins over
> its watchtower counterpart. See
> [coming from Watchtower](migrating-from-watchtower.md#keep-your-labels--theyre-read-directly).

When `freshdock.enable=true` but `freshdock.mode` is absent, the mode is `watch`
(detect-and-notify, never mutate) — a non-destructive default. Change this
fleet-wide fallback with [`[settings] default_mode`](#settings) (or
`FRESHDOCK_DEFAULT_MODE`); an explicit `freshdock.mode` label always wins.

> **Mode vs. schedule.** `freshdock.schedule` only refines the *calendar* modes
> (`nightly` / `weekly` / `monthly`). `live` and `watch` are polled on the
> daemon's `run --interval` instead and ignore the label. See
> [scheduling](scheduling.md).

> **Health-gate timings.** The post-update health timeout and the grace period
> for containers without a healthcheck are currently hardcoded — not
> label/config/env-configurable. See
> [health & rollback: timings](health-and-rollback.md#timings).

> **Pinned images.** A container whose image is pinned to a digest
> (`repo@sha256:…`) has no moving tag to follow. freshdock reports it as
> `pinned (no check)` and never updates it.

---

## Watching every container

[`[settings] watch_all`](#settings) (or `FRESHDOCK_WATCH_ALL=true`) inverts the
opt-in gate, the way Watchtower worked: every running container counts as enabled
unless it opts out. An absent `freshdock.enable` label then means enabled, not
ignored. The default is `false`.

Three labels opt a container back out:

| Label | Value | Effect |
|---|---|---|
| `freshdock.enable` | `false` | Invisible again, exactly as without `watch_all`. |
| `com.centurylinklabs.watchtower.enable` | `false` | Same, so exclusions from a Watchtower fleet keep working. |
| `freshdock.mode` | `off` | Never updated. It still appears in `freshdock check` with mode `off`. |

A container enabled this way takes its mode from [`default_mode`](#settings)
(`FRESHDOCK_DEFAULT_MODE`), or `watch` when that is unset. Turning `watch_all` on
therefore never restarts anything by itself; it starts updating once
`default_mode` names an updating mode. Explicit labels always win: a container
with `freshdock.mode=live` keeps `live`, and `freshdock.enable=true` behaves as it
always did.

> **freshdock skips its own container** when it enables containers this way, so
> the daemon never tries to update itself. It recognises itself by the Docker
> default hostname (the short container id). If you give the freshdock
> container a custom `hostname`, add `freshdock.mode=off` to it (or label it
> deliberately if you do want it updated). The same applies when freshdock runs
> with `network_mode: container:<name>`: it then carries the namespace owner's
> hostname, so label both containers explicitly in that setup. The skip is
> evaluated per invocation, so a `freshdock check` run outside the daemon's
> container still shows a row for it.

`freshdock recreate <name>` follows the same rules: with `watch_all` on an
unlabelled container passes the gate, while `freshdock.enable=false` and
`freshdock.mode=off` are still refused.

A runnable stack is in
[`watch-all.yml`](https://github.com/Turbootzz/freshdock/blob/main/examples/compose/watch-all.yml).

---

## Environment variables

Environment variables are the primary way to configure freshdock — a container
deployment can run entirely from them, no file mounted. They also **override the
file per field** (a lone `…_TOKEN` replaces the file token while keeping the file
username). For registry and notification targets, the `<NAME>` is the table name,
upper-cased, with `-` → `_`.

| Variable | Sets / overrides | Notes |
|---|---|---|
| `FRESHDOCK_CONFIG` | config file path | The `--config` flag wins over it. |
| `FRESHDOCK_REGISTRY_<NAME>_USERNAME` | `[registry.<name>] username` | `<NAME>` = alias (`DOCKERHUB`, `GHCR`, `QUAY`, `LSCR`). Hosts with dots can't be expressed unambiguously — configure those in the file. |
| `FRESHDOCK_REGISTRY_<NAME>_TOKEN` | `[registry.<name>] token` | A token (with or without a username) is enough to create a registry entry from the environment alone — no file needed. |
| `FRESHDOCK_NOTIFY_<NAME>_URL` | declares a notification target | A [shoutrrr-style URL](notifications.md#declaring-targets-from-the-environment) — `discord://…`, `telegram://…`, `smtp://…`, or `https://…` — creates the target from the environment alone, no file needed. A bad URL warns and is skipped. |
| `FRESHDOCK_NOTIFY_<NAME>_TRIGGERS` | an env-declared target's `triggers` | Optional comma list of `available,succeeded,failed`; omit for all three. Pairs with `…_URL`. |
| `FRESHDOCK_NOTIFY_<NAME>_BOT_TOKEN` | a Telegram target's `bot_token` | Overrides the secret on an already-declared target (file or `…_URL`). |
| `FRESHDOCK_NOTIFY_<NAME>_PASSWORD` | an SMTP target's `password` | Same: overrides the secret on an already-declared target. |
| `FRESHDOCK_DEFAULT_MODE` | `[settings] default_mode` | One of `live`/`nightly`/`weekly`/`monthly`/`watch`/`off`. An invalid value warns and the file value (else `watch`) applies. |
| `FRESHDOCK_WATCH_ALL` | `[settings] watch_all` | `true`/`false`/`1`/`0`, case-insensitive. Treats every running container as enabled unless it opts out. See [watching every container](#watching-every-container). |
| `FRESHDOCK_CLEANUP` | `[settings] cleanup` | `true`/`false`/`1`/`0`, case-insensitive. An invalid value warns and the file value applies. |
| `FRESHDOCK_PRUNE_DANGLING` | `[settings] prune_dangling` | Same boolean forms as `FRESHDOCK_CLEANUP`. |
| `FRESHDOCK_INTERVAL`, `FRESHDOCK_TICK`, `FRESHDOCK_STOP_TIMEOUT` | the `run` flags of the same name | The flag wins over the env var. An invalid value is a startup error (it *is* the flag). See the [CLI reference](cli-reference.md#freshdock-run). |
| `NO_COLOR` | `--no-color` | Any non-empty value disables colored output. |
| `RUST_LOG` | log verbosity | e.g. `info`, `freshdock=debug`, `trace`. Default `info`. |
| `DOCKER_HOST` | Docker daemon endpoint | `unix://`, `tcp://`, `http://`, `https://` or `ssh://`. Unset: `/var/run/docker.sock`, then Podman's sockets. See [deployment](deployment.md#which-socket-freshdock-uses). |

`freshdock --help` prints the same override list (`after_long_help`).

---

## The optional `freshdock.toml` file

The file is **optional** — freshdock runs without it. Reach for it only when you
need something environment variables can't express:

1. **Registry credentials for a custom host with dots** (e.g.
   `registry.example.com`), whose name can't be spelled as an env-var name.
2. **A notification target you'd rather keep in a file** — though these can now be
   declared from the environment too (see
   [`FRESHDOCK_NOTIFY_<NAME>_URL`](#environment-variables) /
   [Declaring targets from the environment](notifications.md#declaring-targets-from-the-environment)).

Everything else — the four registry aliases, the `[settings]` defaults, the `run`
flags — has an environment variable, so most deployments need no file at all.

When present, it is resolved in this order:

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

Fleet-wide defaults. Every key is optional — and each has an environment variable
(shown below) that overrides it.

```toml
[settings]
default_mode   = "watch"   # fallback mode for an enabled container with no
                           # freshdock.mode label. Invalid → warn + fall back to watch.
watch_all      = false     # treat every running container as enabled unless it
                           # opts out (freshdock.enable=false / mode=off).
cleanup        = false     # remove the replaced image after a healthy update;
                           # overridable per container with freshdock.cleanup.
prune_dangling = false     # additionally run a daemon-wide dangling-image prune
                           # after each successful update (no per-container override).
```

| Key | Env var | Type | Default | Notes |
|---|---|---|---|---|
| `default_mode` | `FRESHDOCK_DEFAULT_MODE` | string (a mode name) | unset → `watch` | Applied to enabled containers without a `freshdock.mode` label. A `freshdock.mode` label always overrides it. |
| `watch_all` | `FRESHDOCK_WATCH_ALL` | bool | `false` | Enables every running container unless it opts out, and gives those containers `default_mode` (else `watch`). See [watching every container](#watching-every-container). |
| `cleanup` | `FRESHDOCK_CLEANUP` | bool | `false` | Default for `freshdock.cleanup`. Best-effort; a shared image in use elsewhere is kept, and a cleanup failure never fails the update. |
| `prune_dangling` | `FRESHDOCK_PRUNE_DANGLING` | bool | `false` | Daemon-wide; prunes untagged images after a success. Best-effort. |

### `[registry.<name>]`

One table per registry. `<name>` may be a friendly alias (`dockerhub`, `ghcr`,
`quay`, `lscr`) or a literal host (`"registry.example.com"`); both fold onto the
same registry as the matching image reference. For the four aliases you can skip
the file entirely and set `FRESHDOCK_REGISTRY_<NAME>_TOKEN` (and optionally
`_USERNAME`) instead.

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

One table per target, selected by `type`. A target can equally be declared from the
environment with [`FRESHDOCK_NOTIFY_<NAME>_URL`](notifications.md#declaring-targets-from-the-environment) —
the file form below is for when you'd rather keep it in config. Every target may set
an optional `triggers` list to subscribe to a subset of events; omit it (or use `[]`)
to receive all three (`available`, `succeeded`, `failed`). Payload formats and the
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
port     = 587                        # optional; defaults per tls: 587/465/25
username = "freshdock@example.com"    # username + password together, or neither
password = "s3cr3t"                   # or FRESHDOCK_NOTIFY_EMAIL_PASSWORD
from     = "freshdock@example.com"
to       = ["admin@example.com"]      # non-empty list
tls      = "starttls"                 # starttls (default) | implicit (465) | none
triggers = ["succeeded", "failed"]
```

Per-type keys:

| `type` | Keys | Notes |
|---|---|---|
| `webhook` | `url` (secret) | Generic JSON POST. |
| `discord` | `webhook_url` (secret) | Posts a coloured embed. |
| `telegram` | `bot_token` (secret), `chat_id` | Plain-text message via the Bot API. |
| `smtp` | `host`, `port`?, `username`?, `password`? (secret), `from`, `to` (list), `tls` (=`"starttls"`) | `tls` is `"starttls"` \| `"implicit"` \| `"none"` (plaintext, dev only — logged as a warning). An omitted `port` defaults **from the mode**: 587 (starttls), 465 (implicit), 25 (none) — set it explicitly for a relay on another port (e.g. mailpit on 1025). The legacy `starttls = true\|false` maps to starttls/implicit; keeping both keys is fine when they agree, a contradictory pair is an error. `username`+`password` must be set together or both omitted (anonymous relay). |

All targets also accept `triggers = ["available", "succeeded", "failed"]` (subset
allowed).

---

## A complete example

### File-free (environment only)

A deployment with a private GHCR image, `cleanup` on, and no notifications needs no
file at all — just environment:

```bash
FRESHDOCK_DEFAULT_MODE=nightly
FRESHDOCK_CLEANUP=true
FRESHDOCK_REGISTRY_GHCR_USERNAME=octocat
FRESHDOCK_REGISTRY_GHCR_TOKEN=ghp_xxx
```

(Set these under `environment:` in compose, `Environment=` in a systemd unit, or
`export` in a shell.)

### With a file (for notifications)

Once you want notifications, declare the target in a `freshdock.toml` and keep the
secret in the environment:

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

A copy-paste starting point with every section commented out lives at
[`freshdock.toml.example`](https://github.com/Turbootzz/freshdock/blob/main/freshdock.toml.example) in the repository root.
Runnable compose stacks live in [`examples/compose/`](https://github.com/Turbootzz/freshdock/tree/main/examples/compose).
