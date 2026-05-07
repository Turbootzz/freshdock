# freshdock — A Modern Rust-based Watchtower Successor

> **Project name:** `freshdock` — verified available on crates.io, GitHub, Docker Hub, npm, PyPI.
> **Status:** Planning.
> **Author:** Thijs (Turboot).
> **Date:** May 2026.

> *A fresh dock where containers come to renew themselves.* Modern Docker, health-gated rollback, single static binary.

---

## 1. Context

Watchtower (`containrrr/watchtower`) was archived on **17 December 2025** and is no longer maintained. Beyond being abandoned, the codebase ships an outdated embedded Docker SDK (API 1.25), which makes it incompatible with Docker Engine 29+ (which requires API ≥ 1.44). The original maintainers themselves discourage the use of the active forks. There is a clear, current gap in the ecosystem for a maintained, modern, full-cycle (check → pull → restart) container auto-updater.

This project fills that gap, while serving a secondary goal: a substantial real-world project to deepen Rust skills (async, Tokio, error modelling, traits, state machines, packaging).

---

## 2. Competitive Landscape

### Go-based tools (the majority)

| Tool | What it does | Notes |
|---|---|---|
| What's Up Docker (WUD) | Check + optional update + web UI + many notifications | Closest "smart drop-in" replacement; heavy. |
| Diun | Notifications only | Deliberately read-only. |
| Tugtainer | Web UI, multi-host agents, dependency-aware updates, manual approval | Modern, growing user base. |
| Dockwatch (Notifiarr) | Dashboard, fits *arr stacks | Niche audience. |
| dockcheck | CLI shell script | Minimal. |
| `nicholas-fedor/watchtower` | Active fork of the original | Stop-gap, not a rewrite. |

### Rust-based tools

| Tool | What it does | Gap |
|---|---|---|
| Cup (`sergi0g/cup`) | Very fast checker (5.4 MB binary, 58 images in ~3.7s on a Pi 5), CLI + web | **Deliberately does not pull or restart** — checker only. |

### The gap this project fills

There is **no full-cycle (check → pull → recreate → restart → cleanup) Rust-based container auto-updater** with active maintenance. Users who want automation today fall back on heavier Go tools that pull 100+ MB images. A Rust tool that combines Cup-level footprint with Watchtower-level capability does not exist yet.

---

## 3. Goals and Non-Goals

### Goals

1. Be a true drop-in replacement for Watchtower's "set and forget" use case in homelabs.
2. Support modern Docker (API ≥ 1.44) and Podman without hacks.
3. Multiple update strategies on a per-container basis (live, scheduled, watch-only).
4. Healthy-by-default: never leave the user with a broken container if a rollback is possible.
5. Single static binary, small footprint, fast cold start.
6. Be a sustainable open source project with clear scope, good docs, and tests.

### Non-Goals (explicitly out of scope, at least for v1)

- Kubernetes — Kubernetes has its own update mechanisms; do not compete with them.
- Docker Swarm orchestration logic.
- Approval workflows / web UI in v1 (consider for v2).
- Multi-host agent architecture (consider for v2).
- Updating compose-managed stacks via the compose file directly (rely on label-based per-container updates instead).
- Image vulnerability scanning.

---

## 4. Differentiators

What this project will do better than the existing landscape:

1. **Modern Docker API.** Tested against Docker 24.x through 29+, auto-negotiated.
2. **Health-gated updates.** A container is only considered "successfully updated" when the new instance reaches its `healthcheck` healthy state (or stays running for a configurable grace period if no healthcheck exists). Failed updates trigger automatic rollback to the previous image.
3. **Per-container schedule mixing.** A single deployment can have container A on live updates, container B on nightly, container C on weekly, container D in watch-only mode — driven by Docker labels, no global compromise.
4. **Dependency-aware ordering.** Containers with `depends_on` are stopped/started in the correct order (inspired by Tugtainer).
5. **Smaller and faster than Go alternatives** while retaining the full update cycle (target: ≤ 10 MB binary, ≤ 30 MB resident memory at idle).
6. **OCI-correct.** Works with Podman's API socket without modification.
7. **Honest defaults.** Watch-only is the default — opt-in to auto-update per container, not opt-out. This protects users from "Watchtower broke my server overnight" stories.

---

## 5. Core Features (MVP)

### 5.1 Update modes (per container, via labels)

| Mode | Behaviour |
|---|---|
| `live` | Poll registry frequently (default 5 min); pull and recreate immediately on new digest. |
| `nightly` | Check at a fixed daily window (default 04:00 local time). |
| `weekly` | Check once per week (configurable day + time). |
| `monthly` | Check on the Nth day of the month (configurable). |
| `watch` | Detect updates and notify only — never pull or restart. |
| `off` | Ignore the container entirely. |

Global default mode is configurable; per-container labels override.

Example:
```yaml
labels:
  - "freshdock.enable=true"
  - "freshdock.mode=nightly"
  - "freshdock.notify=true"
```

### 5.2 Update lifecycle

For each eligible container, on each scheduled tick:

1. Resolve current image reference (name + tag, or digest).
2. Query registry for the digest of that tag.
3. If digest unchanged → skip.
4. If digest changed:
   1. Pull new image.
   2. Inspect old container; capture full config (env, mounts, networks, restart policy, healthcheck, labels, command, etc.).
   3. Stop old container gracefully (respect stop signal + timeout).
   4. Rename old container `<name>-old-<timestamp>` (kept for rollback).
   5. Create new container with captured config + new image.
   6. Start new container.
   7. Wait for healthcheck to become `healthy` (or grace period if no check).
   8. On success: remove `-old-` container, optionally prune old image (configurable, off by default).
   9. On failure: stop and remove new container, rename `-old-` back, restart it. Send failure notification.

### 5.3 Scheduling

- Single async runtime (Tokio).
- One scheduler task per mode that picks up containers tagged for that mode.
- Cron-like expressions accepted for `nightly`/`weekly`/`monthly` (start with `0 4 * * *`-style strings; document fields explicitly).

### 5.4 Notifications (v1 scope)

- Webhook (generic POST with JSON body).
- Discord webhook (formatted embed).
- Telegram bot (broadly used in the homelab community; keeps deployment scope small).
- Email (SMTP) — basic.

Notification triggers: update available (watch mode), update succeeded, update failed (with rollback status).

### 5.5 Registry support (v1 scope)

- Docker Hub (anonymous + authenticated).
- GHCR (PAT or anonymous public).
- Quay.io.
- `lscr.io` (LinuxServer).
- Generic OCI-compliant registries with bearer-token auth.

ECR, GCR, ACR, Harbor with custom auth → v2.

### 5.6 Configuration

Two configuration paths:

1. **Container labels** (preferred — Watchtower-compatible style).
2. **Single config file** (`freshdock.toml`) for global defaults: poll intervals, notification endpoints, registry credentials, default schedule.

Environment variables override config file for credentials.

### 5.7 Compatibility targets

| Platform | How it works | Notes |
|---|---|---|
| Plain Docker (24.x, 25.x, 27.x, 28.x, 29+) | Talks to `/var/run/docker.sock` | Primary target. |
| Docker Desktop (Linux/macOS/Windows) | Same socket | Tested manually. |
| Portainer (CE + BE) | Talks to the same Docker socket Portainer uses | Document the "Portainer's stack view may briefly show out-of-sync state after a recreate" caveat. |
| Podman 4+ | Talks to Podman's socket via Bollard's automatic discovery | Rootless and rootful. |
| Dockge / Komodo / other compose-based UIs | Updates individual containers via the daemon socket | Compose stack files are not edited; users see the new image once they re-run their compose. |

---

## 6. Technical Architecture

### 6.1 Stack

| Concern | Choice |
|---|---|
| Language | Rust (stable, edition 2024). |
| Async runtime | Tokio. |
| Docker client | `bollard` (mature, supports API 1.52, also handles Podman). |
| HTTP (registry) | `reqwest` with rustls. |
| Serialization | `serde` + `serde_json` + `toml`. |
| CLI | `clap` v4 with derive. |
| Logging | `tracing` + `tracing-subscriber`. |
| Errors | `thiserror` for libraries, `anyhow` for the binary entry point. |
| Scheduling | `tokio-cron-scheduler` or hand-rolled with `tokio::time` (decide during prototyping). |
| Config | `figment` or plain `serde` over TOML. |
| Tests | `tokio::test` + `testcontainers-rs` for integration. |

### 6.2 Crate layout

A workspace with separate crates is overkill for v1. Start as a single binary crate with internal modules; promote to a workspace only when a clear library boundary emerges.

```
src/
  main.rs              // entry, CLI parsing, daemon bootstrap
  config.rs            // TOML + env loading
  labels.rs            // label parsing → per-container policy
  docker/
    mod.rs             // bollard wrappers
    inspect.rs         // capture full container spec for recreation
    recreate.rs        // recreate-with-same-args logic
  registry/
    mod.rs
    auth.rs            // token negotiation per registry
    digest.rs          // HEAD /manifests/<tag> → digest
  scheduler.rs         // mode → tick → container set
  updater.rs           // the lifecycle state machine
  health.rs            // healthcheck waiting + grace period
  rollback.rs          // -old- container handling
  notify/
    mod.rs
    webhook.rs
    discord.rs
    telegram.rs
    smtp.rs
  errors.rs
```

### 6.3 The recreation problem (the hardest part)

Watchtower-style "restart with the same options" is the single most error-prone area. Plan:

1. Use `docker inspect`-equivalent (`bollard::Docker::inspect_container`) to get the full `ContainerInspectResponse`.
2. Map that structure into a fresh `CreateContainerOptions` + `Config` for the new container.
3. Re-attach all networks the old container was on (with the same aliases and IP if static).
4. Re-attach all mounts (binds, volumes, tmpfs).
5. Preserve restart policy, log driver, capabilities, security opts, sysctls, ulimits, devices, GPU options.
6. Preserve labels — but strip the lifecycle labels added by this tool itself, then re-add.

Write an integration test that creates a container with a "weird" config (custom network with alias, healthcheck, capabilities, GPU stub, restart policy) and verifies that after a recreate the inspected output is byte-identical except for the image digest and the container ID.

This test is the project's quality gate — if it passes, the tool is safe to ship. If it fails, the tool is dangerous.

---

## 7. Phased Roadmap

Estimates assume part-time evening/weekend work alongside the dual study programme.

### Phase 0 — Reserve the name & scaffolding (1 week)

- **Reserve `freshdock` everywhere before anything else.** Crate names on crates.io are permanent and first-come-first-served. Order: (a) publish a 0.0.1 placeholder crate with a minimal `Cargo.toml` and a stub `main.rs`; (b) create the GitHub repo under your account or a `freshdock` org; (c) claim the `freshdock` Docker Hub namespace; (d) optional — register `freshdock.dev` or `.io` if still available.
- Pick licence: AGPL-3.0 (like Cup, protects against commercial appropriation) or MIT/Apache-2.0 dual (maximum adoption). Decide before first real commit.
- Set up CI (GitHub Actions: fmt, clippy, test, cross-compile to musl for amd64 + arm64).
- Set up cargo-deny for license/dependency hygiene.
- Repo skeleton, README stub with the three differentiators (modern Docker, health-gated rollback, Rust footprint) front and centre.

### Phase 1 — Read-only spike (2 weeks)

Goal: prove the concept end-to-end without touching containers.

- List running containers via Bollard.
- Parse labels into a policy struct.
- Implement digest checking against Docker Hub (anonymous).
- Print a table of "container | current digest | latest digest | update?".

This becomes the watch-only mode for free.

### Phase 2 — Single recreate (2 weeks)

- Implement the `inspect → stop → rename → create → start` cycle for one container.
- Handle the basic config preservation (env, mounts, networks, restart policy).
- Manual testing only at this stage.

### Phase 3 — Health gating + rollback (1–2 weeks)

- Wait-for-healthy logic.
- Grace period for containers without healthchecks.
- Rollback path on failure.
- The "weird config" integration test mentioned in §6.3.

### Phase 4 — Scheduling (1 week)

- The five modes (`live`, `nightly`, `weekly`, `monthly`, `watch`).
- Cron expression parsing.
- Per-container override via labels.

### Phase 5 — Multi-registry + auth (2 weeks)

- GHCR, Quay, lscr.io, generic bearer-token registries.
- Credentials from config file + env.
- Rate-limit-aware checking (the Cup approach: HEAD requests, not pulls).

### Phase 6 — Notifications (1 week)

- Webhook, Discord, Telegram, SMTP.
- Trigger matrix: success / failure / available-only.

### Phase 7 — Polish and v1.0 release (2 weeks)

- Documentation site (mdBook or just a thorough README).
- Docker image (multi-arch: amd64, arm64, armv7).
- Sample compose snippets.
- Migration guide from Watchtower (label translation table).
- v1.0.0 tag.

**Total estimate: ~12 weeks of part-time work to v1.0.** Cut Phase 5 and Phase 6 in half if pacing slips — they extend cleanly post-v1.

### Post-v1 (no commitment, just ideas)

- Optional web UI (Leptos or Axum + a simple HTMX frontend).
- Multi-host agent architecture.
- Approval workflow (queue updates for manual confirmation).
- ECR/GCR/ACR registry support.
- Vulnerability data integration (Trivy / Grype output as a notification field).

---

## 8. Risks and Mitigations

| Risk | Mitigation |
|---|---|
| Recreation loses a non-obvious container setting and silently breaks something. | The §6.3 integration test as a hard quality gate; community beta period before tagging v1.0. |
| Registry auth zoo is bigger than expected. | Scope strictly: ship v1 with five well-known registries; document a clear extension point for others. |
| Burnout from a long parallel project. | Phase boundaries are checkpoints; v1 is not "every feature" — it is "watch-only + nightly auto-update reliably". Ship that. |
| Crowded space — "yet another Watchtower clone". | Differentiate honestly on three things only: modern Docker, health-gated rollback, Rust footprint. Lead with these in the README. |
| Portainer users hit confusing UI desync after recreations. | Document the behaviour in a dedicated README section; don't pretend it doesn't happen. |
| User expectations from the dead Watchtower carry over and don't match reality. | Provide a "Coming from Watchtower?" page that explicitly maps old labels and flags to the new ones. |

---

## 9. Success Criteria for v1.0

- Runs as a single static binary (≤ 10 MB) with no runtime dependencies.
- Successfully auto-updates a fleet of 20+ mixed containers across at least three different image registries on a real homelab for two weeks without intervention.
- Survives an intentionally bad image push (broken healthcheck) by rolling back cleanly and notifying.
- Documentation lets a Watchtower user migrate in under 15 minutes.
- At least one external user has it deployed and gives a thumbs-up.

---

## 10. Open Questions

These are intentionally left open until Phase 0 kickoff:

1. AGPL-3.0 (like Cup) vs MIT/Apache-2.0 dual licence?
2. Should v1 ship a CLI subcommand (`freshdock check`) like Cup, or stay daemon-only?
3. Scheduling: bring in `tokio-cron-scheduler` or hand-roll? (Likely hand-roll; less dependency surface.)
4. Label prefix shorthand: support `fd.*` as alias for `freshdock.*` to save typing in long compose files? (Decide late — easy to add, hard to remove.)

---

## 11. References

- containrrr/watchtower archive announcement (Dec 2025).
- `sergi0g/cup` — Rust container update checker.
- `crazy-max/diun` — notification-only Go tool.
- `quenary/tugtainer` — Go web UI auto-updater with dependency awareness.
- `fjall/bollard` — Rust Docker SDK (API 1.52).
- Watchtower's original recreation logic (Go) — for reference on edge cases worth replicating.