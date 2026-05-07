# Manual smoke test: `freshdock recreate`

This walks through the Phase 2 recreate cycle (issue #9) on a real Docker
daemon. Phase 2 deliberately stops short of health gating, rollback, and
removal of the archived container — those land in Phase 3.

## Prerequisites

- A working Docker daemon reachable on the standard socket.
- `freshdock` built locally: `just build`.
- Your shell on `phase-2-single-recreate` (or any branch where `freshdock
  recreate` is wired in).

## Steps

```bash
# 1. Launch the test container with freshdock labels so the recreate
#    command is willing to act on it (the orchestrator refuses to touch
#    containers that did not opt in).
docker run -d --name fd-smoke \
  --label freshdock.enable=true \
  --label freshdock.mode=watch \
  -p 8081:80 nginx:alpine

# 2. Confirm it's serving.
curl -fsS http://localhost:8081/ > /dev/null && echo "nginx OK"

# 3. Run the recreate.
./target/release/freshdock recreate fd-smoke
```

## Expected observations

- The CLI prints `recreated fd-smoke: archived old container as fd-smoke-old-<unix-ts>, new id <id>`.
- `docker ps -a` shows two containers:
  - `fd-smoke` — running, with a fresh container id.
  - `fd-smoke-old-<unix-ts>` — stopped, kept around (Phase 3 removes it on success).
- The new container has the same port mapping (`0.0.0.0:8081->80/tcp`),
  the same `freshdock.enable=true` / `freshdock.mode=watch` labels, and
  the same nginx image.
- `curl -fsS http://localhost:8081/` still returns the default nginx
  page from the new container.

## Cleanup

```bash
docker rm -f fd-smoke fd-smoke-old-*
```

## Known Phase-2 limitations (do not file as bugs)

- **No health gating.** The new container is started but the CLI returns
  before it's verified healthy. If the new image is broken, the CLI
  declares success anyway — Phase 3 fixes this.
- **The `-old-` container is not removed.** It is preserved as the
  rollback target for Phase 3. Until then the operator can `docker rm`
  it manually after confirming the new instance is healthy.
- **No live integration test in CI.** The "weird config" round-trip test
  described in [docs/PLAN.md] §6.3 is Phase 3 (P3-3); it depends on
  `testcontainers` which is currently incompatible with bollard 0.21
  (see the comment in [Cargo.toml]).
- **Registry auth is out of scope.** Pull works for Docker Hub
  anonymously and for any image already present in the local cache.
  GHCR / Quay / lscr.io land in Phase 5.
