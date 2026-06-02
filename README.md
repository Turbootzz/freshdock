# freshdock

A modern Docker container auto-updater, built in Rust as a successor to Watchtower.

[![CI](https://github.com/Turbootzz/freshdock/actions/workflows/ci.yml/badge.svg)](https://github.com/Turbootzz/freshdock/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache--2.0-blue.svg)](LICENSE)
[![Crate](https://img.shields.io/crates/v/freshdock.svg)](https://crates.io/crates/freshdock)
[![Status: pre-alpha](https://img.shields.io/badge/status-pre--alpha-orange.svg)](#status)

---

## Status

**Pre-alpha — Phase 1 (read-only spike) is landing.**

The `0.0.1` crates.io release was a name reservation. The first working iteration is `freshdock check`: a read-only command that lists running containers, parses freshdock labels into a per-container policy, fetches the latest digest from Docker Hub anonymously, and prints a table showing which containers have updates available. It never touches container state.

```bash
freshdock check             # render the update-status table
freshdock --no-color check  # ANSI-free output, suitable for log files
RUST_LOG=info freshdock check  # see registry rate-limit info etc.
```

The scheduler daemon `freshdock run` (Phase 4) polls opted-in containers on their per-mode cadence and applies updates with the same health-gated rollback as `freshdock recreate`:

```bash
freshdock run                  # poll live/watch every 5 min; cron modes on schedule
freshdock run --interval 600   # poll live/watch every 10 min instead
RUST_LOG=info freshdock run    # see per-container scheduler events
```

It runs in the foreground until SIGINT/SIGTERM, then finishes the in-flight container and exits. `watch` mode logs an `update_available` event but never pulls or recreates (notification backends land in Phase 6). See [Scheduling](#scheduling) for cron syntax.

Authenticated registries (GHCR, Quay, lscr.io, generic OCI bearer-token) are reported as "skipped: not yet supported (Phase 5)" and lift in Phase 5. Digest-pinned containers (`image@sha256:…`) are shown as `pinned (no check)` — there is no moving tag to follow.

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

## Planned features

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

### Notifications (v1 scope)

Webhook, Discord, Telegram, and SMTP email. Triggers for: update available (watch mode), update succeeded, update failed (with rollback status).

### Registry support (v1 scope)

Docker Hub, GitHub Container Registry (GHCR), Quay.io, `lscr.io`, and any OCI-compliant registry with bearer-token auth. ECR/GCR/ACR coming post-v1.

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

## Roadmap

The full plan, including phased milestones and architecture, lives in [PLAN.md](PLAN.md).

Short version:

- **Phase 0** — Name reservation, repo scaffolding, CI.
- **Phase 1** *(current)* — Read-only spike: list containers, check digests, print update status.
- **Phase 2** — Single container recreate cycle.
- **Phase 3** — Health-gating and rollback (the quality bar for v1.0).
- **Phase 4** — Scheduling and per-container modes.
- **Phase 5** — Multi-registry auth.
- **Phase 6** — Notifications.
- **Phase 7** — Polish, documentation, v1.0 release.

Estimated total time to v1.0: roughly 12 weeks of part-time work.

---

## Installation

> Not yet. Come back after Phase 1.

When freshdock is ready, installation will be the standard options for a Rust binary:

```bash
# Cargo (after first usable release)
cargo install freshdock

# Docker (after first usable release)
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