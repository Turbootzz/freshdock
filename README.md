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

Authenticated registries (GHCR, Quay, lscr.io, generic OCI bearer-token) are reported as "skipped: not yet supported (Phase 5)" and lift in Phase 5. The daemon entry (`freshdock run`) lands in Phase 4.

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

---

## License

freshdock is licensed under the [Apache License 2.0](LICENSE).

---

## Acknowledgements

- [Watchtower](https://github.com/containrrr/watchtower) for being the standard for ten years and shaping the design space.
- [bollard](https://github.com/fussybeaver/bollard) for the Rust Docker SDK that makes any of this possible.