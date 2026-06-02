# freshdock

A modern Docker container auto-updater, built in Rust as a successor to Watchtower.

[![CI](https://github.com/Turbootzz/freshdock/actions/workflows/ci.yml/badge.svg)](https://github.com/Turbootzz/freshdock/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache--2.0-blue.svg)](LICENSE)
[![Crate](https://img.shields.io/crates/v/freshdock.svg)](https://crates.io/crates/freshdock)
[![Status: pre-alpha](https://img.shields.io/badge/status-pre--alpha-orange.svg)](#status)

---

## Status

**Pre-alpha — Phases 1–6 implemented; polishing toward v1.0 (Phase 7).**

The `0.0.1` crates.io release was a name reservation; a versioned release lands with Phase 7 (#21). The working surface today:

**`freshdock check`** — read-only. Lists running containers, parses freshdock labels into a per-container policy, resolves the latest digest from the registry, and prints a table of which containers have updates available. Never touches container state.

```bash
freshdock check             # render the update-status table
freshdock --no-color check  # ANSI-free output, suitable for log files
RUST_LOG=info freshdock check  # see registry rate-limit info etc.
```

**`freshdock recreate <name>`** — recreate one container against its current tag, health-gated with automatic rollback on failure.

**`freshdock run`** — the scheduler daemon. Polls opted-in containers on their per-mode cadence and applies updates with the same health-gated rollback, emitting notifications on the configured triggers:

```bash
freshdock run                  # poll live/watch every 5 min; cron modes on schedule
freshdock run --interval 600   # poll live/watch every 10 min instead
RUST_LOG=info freshdock run    # see per-container scheduler events
```

It runs in the foreground until SIGINT/SIGTERM, then finishes the in-flight container and exits. `watch` mode emits an *update available* notification but never pulls or recreates. See [Scheduling](#scheduling) for cron syntax and [Notifications](#notifications) for backends.

**Registries.** Docker Hub (anonymous or authenticated), GHCR, Quay, `lscr.io`, and any OCI bearer-token registry are supported; configure credentials in [`freshdock.toml`](#configuration-file-freshdocktoml). Digest-pinned containers (`image@sha256:…`) show as `pinned (no check)` — there is no moving tag to follow.

---

## Why freshdock exists

[Watchtower](https://github.com/containrrr/watchtower) — the de-facto Docker container auto-updater for years — was archived by its maintainers on 17 December 2025. Beyond being abandoned, the codebase ships an outdated embedded Docker SDK (API 1.25) that is incompatible with Docker Engine 29+ (which requires API ≥ 1.44).

The community has produced several forks and alternatives, but none combine the things that matter for a small homelab:

- Modern Docker API support that survives the next few engine releases.
- Updates that are *safe* — meaning a broken new image rolls back automatically instead of leaving you with a dead container.
- A footprint that does not require pulling a 100+ MB image to manage your other containers.

freshdock targets exactly that gap.

---

## Three things freshdock will do differently

1. **Modern Docker API.** Tested against Docker 24.x through 29+, with auto-negotiated API versions via [bollard](https://github.com/fussybeaver/bollard).
2. **Health-gated rollback.** A container is only considered successfully updated when its healthcheck returns healthy (or a configurable grace period passes for containers without one). If the new container fails to come up, freshdock stops it, restores the previous container, and notifies you. No more waking up to a silent breakage.
3. **Single static binary.** Target footprint: ≤ 10 MB binary, ≤ 30 MB resident memory at idle. No JVM, no Go runtime, no 100 MB image to manage your homelab.

---

## Features

### Update modes (per container, via Docker labels)

| Mode | Behaviour |
|---|---|
| `live` | Poll registry frequently; pull and recreate on every new digest. |
| `nightly` | Check once per day at a configurable time (default 04:00 local). |
| `weekly` | Check once per week. |
| `monthly` | Check on a configured day of the month. |
| `watch` | Detect updates and notify only — never pull or restart. |
| `off` | Ignore the container entirely. |

A single deployment can mix modes freely. Container A can live-update, container B can be nightly, container C can be watch-only.

Example:

```yaml
services:
  myapp:
    image: ghcr.io/example/myapp:latest
    labels:
      - "freshdock.enable=true"
      - "freshdock.mode=nightly"
      - "freshdock.notify=true"
```

### Scheduling

`live` and `watch` containers are polled on a fixed interval (`freshdock run --interval`, default 300 s). The calendar modes fire on a cron schedule:

| Mode | Default schedule | When |
|---|---|---|
| `nightly` | `0 4 * * *` | 04:00 every day |
| `weekly` | `0 4 * * 0` | 04:00 every Sunday |
| `monthly` | `0 4 1 * *` | 04:00 on the 1st |

Override any calendar mode's schedule with a `freshdock.schedule` label (ignored for `live`/`watch`/`off`):

```yaml
    labels:
      - "freshdock.enable=true"
      - "freshdock.mode=weekly"
      - "freshdock.schedule=0 2 * * 1"   # 02:00 every Monday
```

**Cron syntax.** Standard 5 fields: `minute hour day-of-month month day-of-week`. Each field accepts `*`, a value `N`, a range `A-B`, a step `*/n` or `A-B/n`, and comma-separated lists (e.g. `0,30`). Ranges: minute `0-59`, hour `0-23`, day-of-month `1-31`, month `1-12`, day-of-week `0-6` (**Sunday = 0**; names are not supported). When both day-of-month and day-of-week are restricted, a tick matches if **either** does (Vixie-cron behaviour).

**Timezone.** Schedules are evaluated in the host's **system local time**. Across a DST spring-forward, a schedule that lands in the skipped hour (e.g. `30 2 * * *`) does not fire that day; behaviour inside a transition hour is timezone-dependent, so the 04:00 defaults steer clear of it. Schedule state is in memory only — a window missed while the daemon was down is **not** backfilled; it fires at the next occurrence.

### Notifications

The scheduler (`freshdock run`) sends a notification when an opted-in container
(`freshdock.notify=true`) hits one of three triggers:

| Trigger | When | Modes |
|---|---|---|
| `available` | a newer image exists but was **not** applied | `watch` |
| `succeeded` | a recreate passed its health gate | `live`/`nightly`/`weekly`/`monthly` |
| `failed` | a recreate failed health and was **rolled back** | `live`/`nightly`/`weekly`/`monthly` |

Backends are configured as `[notifications.<name>]` tables in
[`freshdock.toml`](#configuration-file-freshdocktoml): **webhook** (generic JSON
POST), **Discord** (embed via webhook URL), **Telegram** (bot `sendMessage`), and
**SMTP** email. A target may subscribe to a subset of triggers with
`triggers = [...]`; omitting it subscribes to all three. A send that fails is
logged and skipped — notifications never block or abort an update.

### Registry support

Docker Hub, GitHub Container Registry (GHCR), Quay.io, `lscr.io`, and any
OCI-compliant registry with bearer-token auth. Credentials live in
`[registry.<name>]` tables (or `FRESHDOCK_REGISTRY_*` env vars). ECR/GCR/ACR
coming post-v1.

### Compatibility targets

| Platform | Status |
|---|---|
| Plain Docker (24.x – 29+) | Primary target. |
| Docker Desktop (Linux, macOS, Windows) | Supported. |
| Portainer (CE and Business) | Supported via the same Docker socket. |
| Podman 4+ | Supported via Bollard's auto-discovery. |
| Compose-based UIs (Dockge, Komodo, etc.) | Containers are updated individually; compose files are not edited. |
| Kubernetes / Swarm | Out of scope — use platform-native mechanisms. |

---

## Label vocabulary

Per-container behaviour is driven entirely by Docker labels. freshdock is
**opt-in**: a container with no `freshdock.enable=true` is ignored.

| Label | Values | Default | Meaning |
|---|---|---|---|
| `freshdock.enable` | `true` / `false` | `false` | Master switch. Without `true`, the container is ignored. |
| `freshdock.mode` | `live` / `nightly` / `weekly` / `monthly` / `watch` / `off` | `watch` | Update mode (see [Update modes](#update-modes-per-container-via-docker-labels)). `watch` notifies only — it never pulls. |
| `freshdock.schedule` | 5-field cron | mode default | Override the cron for a calendar mode. Ignored for `live`/`watch`/`off`. |
| `freshdock.notify` | `true` / `false` | `false` | Send notifications for this container's update events. Requires a configured `[notifications.*]` target. |

When `freshdock.enable=true` but no `freshdock.mode` is set, the mode is
`watch` (detect-and-notify, never mutate) — an honest, non-destructive default.

## Configuration file (`freshdock.toml`)

Credentials and notification targets live in a `freshdock.toml`, resolved from
`--config <path>`, then `$FRESHDOCK_CONFIG`, then `./freshdock.toml`. Secrets are
redacted in all log output and can be supplied via the environment instead of the
file. Runnable stacks are in [`examples/compose/`](examples/compose/).

```toml
# Registry credentials — keyed by registry alias or host. Env overrides:
# FRESHDOCK_REGISTRY_<NAME>_USERNAME / _TOKEN.
[registry.ghcr]
username = "octocat"
token    = "ghp_xxx"            # personal access token with read:packages

# --- Notification targets ---------------------------------------------------

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
bot_token = "123456:ABC-DEF"   # or FRESHDOCK_NOTIFY_TG_BOT_TOKEN
chat_id   = "987654321"
triggers  = ["failed"]

[notifications.email]
type     = "smtp"
host     = "smtp.example.com"
port     = 587                 # default 587
username = "freshdock@example.com"
password = "s3cr3t"            # or FRESHDOCK_NOTIFY_EMAIL_PASSWORD
from     = "freshdock@example.com"
to       = ["admin@example.com"]
starttls = true                # default true; set false for implicit TLS (465)
triggers = ["succeeded", "failed"]
```

Notification secrets may also be supplied via the environment (the `<NAME>` is
the table name, upper-cased, `-` → `_`):

- `FRESHDOCK_NOTIFY_<NAME>_BOT_TOKEN` — overrides a Telegram target's `bot_token`.
- `FRESHDOCK_NOTIFY_<NAME>_PASSWORD` — overrides an SMTP target's `password`.

## Troubleshooting

**`permission denied` on the Docker socket.** freshdock talks to
`/var/run/docker.sock`. Run it as a user in the `docker` group, or mount the
socket into the container (`-v /var/run/docker.sock:/var/run/docker.sock`). The
socket's group must match; on some hosts you must pass `--group-add` with the
socket's GID.

**Portainer (or another UI) shows a container as "out of sync" after an update.**
A recreate replaces the container with a new ID, so a UI that pinned the old ID
briefly shows a desync. It resolves on the UI's next refresh — freshdock does not
edit your compose/stack files, only the running container.

**`registry requires credentials` / `auth required`.** The image's registry
needs auth that isn't configured. Add a `[registry.<name>]` table (or
`FRESHDOCK_REGISTRY_*` env) for that host — see
[Configuration](#configuration-file-freshdocktoml).

**A notification logs `notification failed; continuing`.** Delivery is
best-effort and **non-fatal** by design: a failed send never blocks or rolls back
an update. Check the target's URL/credentials; tokens are redacted in logs.

**Coming from Watchtower?** See the
[migration guide](docs/migrating-from-watchtower.md) for a label/flag
translation table.

---

## Roadmap

The full plan, including phased milestones and architecture, lives in [PLAN.md](PLAN.md).

Short version:

- **Phase 0** — Name reservation, repo scaffolding, CI. ✅
- **Phase 1** — Read-only spike: list containers, check digests, print update status. ✅
- **Phase 2** — Single container recreate cycle. ✅
- **Phase 3** — Health-gating and rollback (the quality bar for v1.0). ✅
- **Phase 4** — Scheduling and per-container modes. ✅
- **Phase 5** — Multi-registry auth. ✅
- **Phase 6** — Notifications. ✅
- **Phase 7** *(current)* — Polish, documentation, v1.0 release.

Estimated total time to v1.0: roughly 12 weeks of part-time work.

---

## Installation

**From source** (works today):

```bash
git clone https://github.com/Turbootzz/freshdock
cd freshdock
just build           # release binary at target/release/freshdock
```

**Cargo** — installs the latest published crate (currently the `0.0.1` name
reservation; the first feature release lands with v1.0, #21):

```bash
cargo install freshdock
```

**Docker / prebuilt binaries** — the multi-arch image and release binaries ship
with the v1.0 release (#21). Once published:

```bash
docker run -d \
  --name freshdock \
  -v /var/run/docker.sock:/var/run/docker.sock \
  ghcr.io/turbootzz/freshdock
```

---

## Contributing

Contributions are welcome once Phase 1 lands.

If you want to help shape the project before code exists:

- Open an issue with feedback on [PLAN.md](docs/PLAN.md).
- Suggest registries, notification targets, or label conventions you'd want supported.
- Share what broke for you in Watchtower or its forks — pain points are the best feature requests.

### Local development

The repo ships a `justfile` that mirrors the CI gates and a tracked pre-push hook under `.githooks/`. One-time setup after cloning:

```bash
cargo install just cargo-deny  # if you don't already have them
just install-hooks             # enables .githooks/pre-push
```

`just ci` runs the full local CI suite (fmt-check, clippy, test, deny). The pre-push hook delegates to it, so anything that would fail on GitHub fails locally first. Bypass with `git push --no-verify` if you really need to (WIP branches, etc.). Run `just` with no arguments to list every recipe.

**Release-blocker quality gate.** The live "weird config" recreate round-trip ([tests/recreate_roundtrip_live.rs](tests/recreate_roundtrip_live.rs), PLAN §6.3) creates a kitchen-sink container, recreates it against a real daemon, and asserts the inspected config round-trips byte-identical. It is `#[ignore]`d (needs Docker) so default `cargo test` stays green; CI runs it in a dedicated job, and a failure blocks release. Run it locally with:

```bash
cargo test --test recreate_roundtrip_live -- --ignored
```

---

## License

freshdock is licensed under the [Apache License 2.0](LICENSE).

---

## Acknowledgements

- [Watchtower](https://github.com/containrrr/watchtower) for being the standard for ten years and shaping the design space.
- [bollard](https://github.com/fussybeaver/bollard) for the Rust Docker SDK that makes any of this possible.