<!--
  This file is the body of the next GitHub Release (read by .github/workflows/
  release.yml via `body_path`). Update it to match the tag you are about to cut —
  in particular swap the `blob/<tag>` links to the tag being released. See
  RELEASE.md for the full runbook.
-->
# freshdock v1.0.0

freshdock began as a `0.0.1` name reservation on crates.io and is now a complete,
health-gated Docker container auto-updater — a maintained successor to the
archived Watchtower, in a single static binary. This is the first stable release,
closing Phases 0–7 of the roadmap after a homelab beta.

## Highlights

- **Health-gated rollback** — a broken new image is detected and the previous
  container is restored automatically, instead of leaving a dead service.
- **Per-container update modes** via Docker labels: `live`, `nightly`, `weekly`,
  `monthly`, `watch`, `off`. Opt-in by design.
- **Five registries** — Docker Hub, GHCR, Quay.io, lscr.io, and any OCI
  bearer-token registry.
- **Four notification backends** — webhook, Discord, Telegram, SMTP.
- **Single static binary** (≤ 10 MB), no runtime dependencies.

## Install

```bash
# crates.io:
cargo install freshdock

# Container image (multi-arch: amd64, arm64, armv7):
docker pull ghcr.io/turbootzz/freshdock:1.0.0    # or :latest
```

Prebuilt static-musl binaries for amd64 / arm64 / armv7 are attached below.
Verify them against `SHA256SUMS`:

```bash
sha256sum -c SHA256SUMS
```

## Read more

- Architecture & roadmap: [docs/PLAN.md](https://github.com/Turbootzz/freshdock/blob/v1.0.0/docs/PLAN.md)
- Coming from Watchtower: [migration guide](https://github.com/Turbootzz/freshdock/blob/v1.0.0/docs/migrating-from-watchtower.md)
- Full configuration reference: [docs/configuration.md](https://github.com/Turbootzz/freshdock/blob/v1.0.0/docs/configuration.md)

Full changelog: [CHANGELOG.md](https://github.com/Turbootzz/freshdock/blob/v1.0.0/CHANGELOG.md).
