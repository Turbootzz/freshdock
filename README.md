<div align="center">

<img src="assets/logo.svg" alt="freshdock logo" width="160">

# freshdock

**A modern, health-gated Docker container auto-updater. A maintained successor to Watchtower, in a single Rust binary.**

[![CI](https://github.com/Turbootzz/freshdock/actions/workflows/ci.yml/badge.svg)](https://github.com/Turbootzz/freshdock/actions/workflows/ci.yml)
[![Crate](https://img.shields.io/crates/v/freshdock)](https://crates.io/crates/freshdock)
[![docs](https://img.shields.io/badge/docs-guide-blue.svg)](https://turbootzz.github.io/freshdock/)
[![GHCR image](https://img.shields.io/badge/ghcr.io-turbootzz%2Ffreshdock-2496ED.svg)](https://github.com/Turbootzz/freshdock/pkgs/container/freshdock)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache--2.0-blue.svg)](LICENSE)
[![Sponsor](https://img.shields.io/badge/Sponsor-%E2%9D%A4-db61a2.svg?logo=githubsponsors&logoColor=white)](https://github.com/sponsors/Turbootzz)

[Website](https://freshdock.dev) · [Quickstart](https://turbootzz.github.io/freshdock/quickstart.html) · [Docs](https://turbootzz.github.io/freshdock/) · [Configuration](https://turbootzz.github.io/freshdock/configuration.html) · [From Watchtower](https://turbootzz.github.io/freshdock/migrating-from-watchtower.html)

</div>

---

Why freshdock instead of Watchtower? See the [side-by-side comparison](https://freshdock.dev/watchtower-alternative).

## What it does

freshdock watches your running containers, notices when a newer image is published, and
updates them safely: a broken new image rolls back automatically instead of leaving you
with a dead service.

| Capability | What you get |
|---|---|
| **Health-gated rollback** | A container counts as updated only after its healthcheck passes, or after a grace period if it has none. If the new image fails to come up, the previous container is restored and you are notified. |
| **Per-container modes** | Drive each container with Docker labels: `live`, `nightly`, `weekly`, `monthly`, `watch`, `off`. Mix them freely on one daemon. |
| **Five registries** | Docker Hub, GHCR, Quay.io, lscr.io, and any OCI bearer-token registry, anonymous or authenticated. |
| **Four notifiers** | Webhook, Discord, Telegram, and SMTP, each subscribable to the events it cares about. |
| **Lifecycle hooks** | Run commands inside the container around an update. A pre-update hook can veto or defer (exit 75); a post-update hook handles maintenance like cache clears. |
| **Compose-aware rollouts** | A Compose project updates as one unit: the one-shot `migrate` service your app waits on is re-run before the new code starts, and a failed migration aborts the rollout. Read from Compose's own labels, no compose file needed. |
| **VPN stacks survive updates** | Containers routed through another container's network (`network_mode: container:X`, the gluetun pattern) are re-attached after that container updates instead of being left offline. |
| **Watchtower drop-in** | Reads `com.centurylinklabs.watchtower.*` labels (enable, monitor-only, lifecycle hooks) directly, so a fleet migrates without relabelling. `FRESHDOCK_WATCH_ALL=true` restores Watchtower's opt-out model. |
| **Optional cleanup** | Remove superseded images after a healthy update; optionally prune dangling images. |
| **Single static binary** | Under 10 MB, no runtime dependencies. No JVM, no Go runtime, no 100 MB image to manage your homelab. |

freshdock is opt-in: it ignores every container until you set `freshdock.enable=true`, and
an enabled container with no explicit mode defaults to `watch` (detect and notify, never
restart). One narrow exception applies inside Compose projects, described in
[Compose projects](https://turbootzz.github.io/freshdock/compose.html). To include every
container unless it opts out, see
[watching every container](https://turbootzz.github.io/freshdock/configuration.html#watching-every-container).

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

To let it update something, switch to `freshdock.mode=nightly` and give the daemon a
writable socket. Full walkthrough in the
[Quickstart](https://turbootzz.github.io/freshdock/quickstart.html).

## Install

```bash
# crates.io
cargo install freshdock

# container image (multi-arch: amd64, arm64, armv7)
docker run -d --name freshdock --restart unless-stopped \
  -v /var/run/docker.sock:/var/run/docker.sock \
  ghcr.io/turbootzz/freshdock:latest run
```

Prebuilt static-musl binaries (amd64, arm64, armv7) are attached to each
[release](https://github.com/Turbootzz/freshdock/releases), with a `SHA256SUMS` file to
verify them (`sha256sum -c SHA256SUMS`). From source: clone the repo and run `just build`
(binary at `target/release/freshdock`).

## Update modes

| Mode | Behaviour |
|---|---|
| `live` | Poll frequently; pull and recreate on every new digest. |
| `nightly` / `weekly` / `monthly` | Check on a cron schedule (default 04:00); recreate if newer. |
| `watch` | Detect updates and notify only, never pull or restart. |
| `off` | Ignore the container entirely. |

Set the mode (and an optional cron override) with labels:

```yaml
    labels:
      - "freshdock.enable=true"
      - "freshdock.mode=weekly"
      - "freshdock.schedule=0 2 * * 1"   # 02:00 every Monday (overrides the default)
```

Full label vocabulary and cron syntax:
[Configuration](https://turbootzz.github.io/freshdock/configuration.html#labels) and
[Scheduling](https://turbootzz.github.io/freshdock/scheduling.html).

## Documentation

| | |
|---|---|
| [Quickstart](https://turbootzz.github.io/freshdock/quickstart.html) | Up and running in a minute. |
| [Configuration](https://turbootzz.github.io/freshdock/configuration.html) | Every label, env var, and `freshdock.toml` table (the single source of truth). |
| [CLI reference](https://turbootzz.github.io/freshdock/cli-reference.html) | `check`, `recreate`, `run`, and all flags. |
| [Scheduling & modes](https://turbootzz.github.io/freshdock/scheduling.html) | Update modes and cron syntax. |
| [Compose projects](https://turbootzz.github.io/freshdock/compose.html) | A stack updated as one unit: one-shot migrations, `depends_on` ordering. |
| [Notifications](https://turbootzz.github.io/freshdock/notifications.html) | Webhook, Discord, Telegram, SMTP. |
| [Health & rollback](https://turbootzz.github.io/freshdock/health-and-rollback.html) | The recreate lifecycle and image cleanup. |
| [Lifecycle hooks](https://turbootzz.github.io/freshdock/lifecycle-hooks.html) | Pre-update and post-update commands. |
| [Registry auth](https://turbootzz.github.io/freshdock/registry-auth.html) | Private registries and credentials. |
| [Deployment](https://turbootzz.github.io/freshdock/deployment.html) | Container, systemd, socket permissions, compatibility. |
| [Troubleshooting](https://turbootzz.github.io/freshdock/troubleshooting.html) | Symptom-first fixes for common first-run issues. |
| [Migrating from Watchtower](https://turbootzz.github.io/freshdock/migrating-from-watchtower.html) | Label/flag translation. |
| [Architecture & roadmap](https://turbootzz.github.io/freshdock/PLAN.html) | Design, phases, goals, risks. |

Runnable example stacks: [`examples/compose/`](examples/compose/). A commented config
template: [`freshdock.toml.example`](freshdock.toml.example).

## Why freshdock

[Watchtower](https://github.com/containrrr/watchtower) was the de-facto Docker
auto-updater for years. Its maintainers archived it on 17 December 2025, and it shipped an
embedded Docker SDK (API 1.25) incompatible with Docker Engine 29+. The community has
forks, but none combine what matters for a small homelab:

1. Modern Docker API: the API version is negotiated with your daemon at connect time via [bollard](https://github.com/fussybeaver/bollard), so freshdock speaks the newest API that daemon supports.
2. Safe updates: a broken new image rolls back automatically instead of leaving a dead container.
3. Small footprint: a single static binary, not a 100+ MB image to manage your other containers.

A fuller comparison lives at [freshdock.dev/watchtower-alternative](https://freshdock.dev/watchtower-alternative).

## Compatibility

| Platform | Status |
|---|---|
| Plain Docker | Primary target. The API version is negotiated per daemon; CI exercises the Docker engine shipped on GitHub's `ubuntu-latest` runners. |
| Docker Desktop (Linux, macOS, Windows) | Supported. |
| Portainer (CE and Business) | Supported via the same Docker socket. |
| Podman 4+ | Supported via the Docker-compatible socket. |
| Docker Compose | A project is updated as one unit: one-shot `service_completed_successfully` services are re-run first, the rest follow in `depends_on` order. Compose files are never read or written. |
| Compose-based UIs (Dockge, Komodo, and similar) | Supported; the compose files themselves are untouched. |
| Kubernetes / Swarm | Out of scope. Use platform-native mechanisms. |

Socket discovery order, Podman paths, and the Docker API 1.44 floor for containers on more
than one network are covered in
[Deployment](https://turbootzz.github.io/freshdock/deployment.html).

## Status & roadmap

Phases 0-7 are complete and freshdock has been stable since `1.0.0`. The full plan and
architecture live in [the roadmap](https://turbootzz.github.io/freshdock/PLAN.html);
release mechanics in [RELEASE.md](RELEASE.md); per-version notes in
[CHANGELOG.md](CHANGELOG.md).

## Contributing

Contributions are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for local setup, the
quality gates, and dependency hygiene. Security reports: [SECURITY.md](SECURITY.md).

## Support

freshdock is free and open source. If it keeps your homelab up to date, you can support its
development through [GitHub Sponsors](https://github.com/sponsors/Turbootzz). Sponsoring is
optional; stars, bug reports, and pull requests help just as much.

## License

freshdock is licensed under the [Apache License 2.0](LICENSE).

## Acknowledgements

- [Watchtower](https://github.com/containrrr/watchtower) for being the standard for ten years and shaping the design space.
- [bollard](https://github.com/fussybeaver/bollard) for the Rust Docker SDK that makes any of this possible.
