# Changelog

All notable changes to freshdock are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.4.2](https://github.com/Turbootzz/freshdock/compare/v1.4.1...v1.4.2) - 2026-08-19

### Fixed

- *(probe)* treat a republished multi-arch index as up to date ([#75](https://github.com/Turbootzz/freshdock/pull/75))

### Other

- *(readme)* refresh status line and mention network-mode re-attach ([#72](https://github.com/Turbootzz/freshdock/pull/72))

## [1.4.1](https://github.com/Turbootzz/freshdock/compare/v1.4.0...v1.4.1) - 2026-08-09

### Fixed

- *(docker)* negotiate the API version and honour every DOCKER_HOST scheme ([#71](https://github.com/Turbootzz/freshdock/pull/71))

### Other

- *(deps)* bump the cargo-minor-patch group across 1 directory with 8 updates ([#65](https://github.com/Turbootzz/freshdock/pull/65))
- SMTP tls modes incl. plaintext (#57, #58) + re-attach network_mode dependents ([#68](https://github.com/Turbootzz/freshdock/pull/68)) ([#69](https://github.com/Turbootzz/freshdock/pull/69))

## [1.4.0](https://github.com/Turbootzz/freshdock/compare/v1.3.0...v1.4.0) - 2026-07-28

### Added

- read watchtower labels as fallbacks ([#66](https://github.com/Turbootzz/freshdock/pull/66))
- lifecycle hooks — pre/post-update commands via labels ([#62](https://github.com/Turbootzz/freshdock/pull/62))

## [1.3.0](https://github.com/Turbootzz/freshdock/compare/v1.2.1...v1.3.0) - 2026-06-25

### Added

- *(notify)* declare targets via env URLs, add notification logging ([#56](https://github.com/Turbootzz/freshdock/pull/56))

### Other

- env-first config docs, Pages links, fix crates.io badge ([#53](https://github.com/Turbootzz/freshdock/pull/53))

## [1.2.1](https://github.com/Turbootzz/freshdock/compare/v1.2.0...v1.2.1) - 2026-06-16

### Other

- *(deps)* bump toml from 0.8.23 to 1.1.2+spec-1.1.0 ([#50](https://github.com/Turbootzz/freshdock/pull/50))

## [1.2.0](https://github.com/Turbootzz/freshdock/compare/v1.1.0...v1.2.0) - 2026-06-16

### Added

- credential observability and survive rejected tokens ([#45](https://github.com/Turbootzz/freshdock/pull/45))

### Other

- add monthly Dependabot for cargo and GitHub Actions ([#47](https://github.com/Turbootzz/freshdock/pull/47))
- publish documentation as an mdBook site on GitHub Pages ([#46](https://github.com/Turbootzz/freshdock/pull/46))
- Added funding option ([#44](https://github.com/Turbootzz/freshdock/pull/44))
- add project logo and embed in README header ([#41](https://github.com/Turbootzz/freshdock/pull/41))

## [1.1.0](https://github.com/Turbootzz/freshdock/compare/v1.0.0...v1.1.0) - 2026-06-10

### Added

- full env-var config coverage, onboarding docs, node24 release actions ([#39](https://github.com/Turbootzz/freshdock/pull/39))

## [1.0.0] - 2026-06-10

First stable release. Same surface as `1.0.0-rc.1`, promoted after a homelab beta
(watch, read-only check, and the recreate health-gate verified on Docker /
Portainer / TrueNAS). No functional changes since the candidate.

## [1.0.0-rc.1] - 2026-06-09

First release candidate. Closes Phases 0–7 of the [roadmap](docs/PLAN.md); the
final `1.0.0` tag follows a community beta (see [RELEASE.md](RELEASE.md)).

### Added
- `freshdock check` — read-only update-status table. Lists opted-in containers,
  resolves the latest registry digest per unique image (deduped to respect Docker
  Hub's anonymous rate budget), and reports which have updates. Never mutates state.
- `freshdock recreate <name>` — manual single-container update: inspect → pull →
  stop → rename → create → start, health-gated with automatic rollback on failure.
- `freshdock run` — scheduler daemon polling opted-in containers on their per-mode
  cadence (`--interval`, `--tick`, `--stop-timeout`); graceful SIGINT/SIGTERM drain.
- Per-container update modes via Docker labels: `live`, `nightly`, `weekly`,
  `monthly`, `watch`, `off`. Opt-in by default (`freshdock.enable=true`).
- Per-container cron override (`freshdock.schedule`) with a hand-rolled 5-field
  parser; calendar modes evaluated in system local time with DST-gap handling.
- Health-gated rollback: a new container must pass its healthcheck (or a grace
  period) before the old one is removed; otherwise it is restored.
- Multi-registry digest checks: Docker Hub (anonymous + authenticated), GHCR,
  Quay.io, lscr.io, and any OCI bearer-token registry. Per-registry credentials
  via `[registry.<name>]` tables or `FRESHDOCK_REGISTRY_*` env overrides.
- Notifications: webhook, Discord, Telegram, and SMTP backends, each subscribable
  to a subset of the `available` / `succeeded` / `failed` triggers.
- Image cleanup: remove the superseded image after a healthy update
  (`[settings] cleanup` / `freshdock.cleanup`); optional daemon-wide dangling
  prune (`[settings] prune_dangling`).
- Global default mode (`[settings] default_mode`) for enabled containers with no
  explicit `freshdock.mode` label.
- Multi-arch container image (amd64, arm64, armv7) on GHCR and static-musl release
  binaries for the same three architectures.

[Unreleased]: https://github.com/Turbootzz/freshdock/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/Turbootzz/freshdock/releases/tag/v1.0.0
[1.0.0-rc.1]: https://github.com/Turbootzz/freshdock/releases/tag/v1.0.0-rc.1
