<div align="center">

<img src="assets/logo.svg" alt="freshdock logo" width="160">

# freshdock

**A modern, health-gated Docker container auto-updater — a maintained successor to Watchtower, in a single Rust binary.**

[![CI](https://github.com/Turbootzz/freshdock/actions/workflows/ci.yml/badge.svg)](https://github.com/Turbootzz/freshdock/actions/workflows/ci.yml)
[![Crate](https://img.shields.io/crates/v/freshdock.svg)](https://crates.io/crates/freshdock)
[![docs](https://img.shields.io/badge/docs-guide-blue.svg)](https://turbootzz.github.io/freshdock/)
[![GHCR image](https://img.shields.io/badge/ghcr.io-turbootzz%2Ffreshdock-2496ED.svg)](https://github.com/Turbootzz/freshdock/pkgs/container/freshdock)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache--2.0-blue.svg)](LICENSE)
[![Sponsor](https://img.shields.io/badge/Sponsor-%E2%9D%A4-db61a2.svg?logo=githubsponsors&logoColor=white)](https://github.com/sponsors/Turbootzz)

[Quickstart](docs/quickstart.md) · [Docs](https://turbootzz.github.io/freshdock/) · [Configuration](docs/configuration.md) · [From Watchtower](docs/migrating-from-watchtower.md)

</div>

---

## What it does

freshdock watches your running containers, notices when a newer image is published,
and updates them **safely** — a broken new image rolls back automatically instead of
leaving you with a dead service.

| Capability | What you get |
|---|---|
| **Health-gated rollback** | A container counts as updated only after its healthcheck passes (or a grace period for those without one). If the new image fails to come up, the previous container is restored — and you're notified. |
| **Per-container modes** | Drive each container with Docker labels: `live`, `nightly`, `weekly`, `monthly`, `watch`, `off`. Mix them freely on one daemon. |
| **Five registries** | Docker Hub, GHCR, Quay.io, lscr.io, and any OCI bearer-token registry — anonymous or authenticated. |
| **Four notifiers** | Webhook, Discord, Telegram, and SMTP, each subscribable to the events it cares about. |
| **Optional cleanup** | Remove superseded images after a healthy update; optionally prune dangling images. |
| **Single static binary** | ≤ 10 MB, no runtime dependencies. No JVM, no Go runtime, no 100 MB image to manage your homelab. |

> **Opt-in by design.** freshdock ignores every container until you set
> `freshdock.enable=true`, and an enabled container with no explicit mode defaults to
> `watch` (detect-and-notify, never restart). Nothing is touched until you ask.

## Quickstart

Label a container to opt it in, then watch it with a read-only socket:

```yaml
# docker-compose.yml
services:
  web:
    image: nginx:1.27
    labels:
      - "freshdock.enable=true"      # opt in; mode defaults to watch

  freshdock:
    image: ghcr.io/turbootzz/freshdock:latest
    command: ["run"]
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock:ro
    restart: unless-stopped
```

```bash
freshdock check    # read-only: which containers have updates available?
```

Ready to let it update something? Switch to `freshdock.mode=nightly` (and give the
daemon a writable socket). Full walkthrough → [**Quickstart**](docs/quickstart.md).

## Install

```bash
# crates.io
cargo install freshdock

# container image (multi-arch: amd64, arm64, armv7)
docker run -d --name freshdock --restart unless-stopped \
  -v /var/run/docker.sock:/var/run/docker.sock \
  ghcr.io/turbootzz/freshdock:latest run
```

Prebuilt static-musl binaries (amd64 / arm64 / armv7) are attached to each
[release](https://github.com/Turbootzz/freshdock/releases). From source:
`git clone … && just build` (binary at `target/release/freshdock`).

## Update modes

| Mode | Behaviour |
|---|---|
| `live` | Poll frequently; pull and recreate on every new digest. |
| `nightly` / `weekly` / `monthly` | Check on a cron schedule (default 04:00); recreate if newer. |
| `watch` | Detect updates and **notify only** — never pull or restart. |
| `off` | Ignore the container entirely. |

Set the mode (and an optional cron override) with labels:

```yaml
    labels:
      - "freshdock.enable=true"
      - "freshdock.mode=weekly"
      - "freshdock.schedule=0 2 * * 1"   # 02:00 every Monday (overrides the default)
```

Full label vocabulary and cron syntax → [**Configuration**](docs/configuration.md#labels)
and [**Scheduling**](docs/scheduling.md).

## Documentation

| | |
|---|---|
| [Quickstart](docs/quickstart.md) | Up and running in a minute. |
| [Configuration](docs/configuration.md) | Every label, `freshdock.toml` table, and env var (the single source of truth). |
| [CLI reference](docs/cli-reference.md) | `check`, `recreate`, `run`, and all flags. |
| [Scheduling & modes](docs/scheduling.md) | Update modes and cron syntax. |
| [Notifications](docs/notifications.md) | Webhook, Discord, Telegram, SMTP. |
| [Health & rollback](docs/health-and-rollback.md) | The recreate lifecycle and image cleanup. |
| [Registry auth](docs/registry-auth.md) | Private registries and credentials. |
| [Deployment](docs/deployment.md) | Container, systemd, socket permissions, compatibility. |
| [Troubleshooting](docs/troubleshooting.md) | Symptom-first fixes for common first-run issues. |
| [Migrating from Watchtower](docs/migrating-from-watchtower.md) | Label/flag translation. |
| [Architecture & roadmap](docs/PLAN.md) | Design, phases, goals, risks. |

Runnable example stacks: [`examples/compose/`](examples/compose/). A commented
config template: [`freshdock.toml.example`](freshdock.toml.example).

## Why freshdock

[Watchtower](https://github.com/containrrr/watchtower) — the de-facto Docker
auto-updater for years — was archived by its maintainers on 17 December 2025, and
shipped an embedded Docker SDK (API 1.25) incompatible with Docker Engine 29+. The
community has forks, but none combine what matters for a small homelab:

1. **Modern Docker API** — tested against Docker 24.x through 29+, auto-negotiated via [bollard](https://github.com/fussybeaver/bollard).
2. **Safe updates** — a broken new image rolls back automatically instead of leaving a dead container.
3. **Small footprint** — a single static binary, not a 100+ MB image to manage your other containers.

## Compatibility

| Platform | Status |
|---|---|
| Plain Docker (24.x – 29+) | Primary target. |
| Docker Desktop (Linux, macOS, Windows) | Supported. |
| Portainer (CE and Business) | Supported via the same Docker socket. |
| Podman 4+ | Supported via the Docker-compatible socket. |
| Compose-based UIs (Dockge, Komodo, …) | Containers updated individually; compose files untouched. |
| Kubernetes / Swarm | Out of scope — use platform-native mechanisms. |

## Status & roadmap

Phases 0–7 are complete and freshdock is at its first stable release, `1.0.0`.
The full plan and architecture live in [docs/PLAN.md](docs/PLAN.md); release
mechanics in [RELEASE.md](RELEASE.md); per-version notes in
[CHANGELOG.md](CHANGELOG.md).

## Contributing

Contributions are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md) for local setup,
the quality gates, and dependency hygiene. Security reports: [SECURITY.md](SECURITY.md).

## Support

freshdock is free and open source. If it keeps your homelab up to date, you can
support its development through [**GitHub Sponsors**](https://github.com/sponsors/Turbootzz).
Sponsoring is entirely optional — stars, bug reports, and pull requests help just as much.

## License

freshdock is licensed under the [Apache License 2.0](LICENSE).

## Acknowledgements

- [Watchtower](https://github.com/containrrr/watchtower) for being the standard for ten years and shaping the design space.
- [bollard](https://github.com/fussybeaver/bollard) for the Rust Docker SDK that makes any of this possible.
