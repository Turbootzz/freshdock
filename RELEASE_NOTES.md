<!--
  This file is the body of the next GitHub Release (read by .github/workflows/
  release.yml via `body_path`). Update it to match the tag you are about to cut —
  in particular swap the `blob/<tag>` links to the tag being released. See
  RELEASE.md for the full runbook.
-->
# freshdock v1.0.0-rc.1 — first release candidate

freshdock began as a `0.0.1` name reservation on crates.io and is now a complete,
health-gated Docker container auto-updater — a maintained successor to the
archived Watchtower, in a single static binary. This release candidate closes
Phases 0–7 of the roadmap ahead of the `1.0.0` tag.

> **Release candidate.** `cargo install freshdock` will **not** pick this up
> (crates.io treats `-rc` versions as pre-releases) and the GHCR `:latest` tag is
> **not** moved. Install it explicitly to help with the beta — see below.

## Highlights

- **Health-gated rollback** — a broken new image is detected and the previous
  container is restored automatically, instead of leaving a dead service.
- **Per-container update modes** via Docker labels: `live`, `nightly`, `weekly`,
  `monthly`, `watch`, `off`. Opt-in by default.
- **Five registries** — Docker Hub, GHCR, Quay.io, lscr.io, and any OCI
  bearer-token registry.
- **Four notification backends** — webhook, Discord, Telegram, SMTP.
- **Single static binary** (≤ 10 MB), no runtime dependencies.

## Install

```bash
# Release candidate from crates.io (explicit version required):
cargo install freshdock --version 1.0.0-rc.1

# Container image:
docker pull ghcr.io/turbootzz/freshdock:1.0.0-rc.1
```

Prebuilt static-musl binaries for amd64 / arm64 / armv7 are attached below.
Verify them against `SHA256SUMS`:

```bash
sha256sum -c SHA256SUMS
```

## Read more

- Architecture & roadmap: [docs/PLAN.md](https://github.com/Turbootzz/freshdock/blob/v1.0.0-rc.1/docs/PLAN.md)
- Coming from Watchtower: [migration guide](https://github.com/Turbootzz/freshdock/blob/v1.0.0-rc.1/docs/migrating-from-watchtower.md)
- Full configuration reference: [docs/configuration.md](https://github.com/Turbootzz/freshdock/blob/v1.0.0-rc.1/docs/configuration.md)

Full changelog: [CHANGELOG.md](https://github.com/Turbootzz/freshdock/blob/v1.0.0-rc.1/CHANGELOG.md).
