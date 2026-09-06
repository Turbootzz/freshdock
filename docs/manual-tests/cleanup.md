# Manual smoke test: image cleanup

Verifies the opt-in image cleanup that runs after a successful, health-passed
update (PLAN §5.2 step 8). Cleanup is **off by default**; it removes the image
the *replaced* container was running, and is best-effort (a shared image still
referenced by another container is kept, and a cleanup failure never fails the
update).

There are two knobs:

- `[settings] cleanup = true` (or per container `freshdock.cleanup=true`):
  remove the replaced image after a healthy update.
- `[settings] prune_dangling = true`: additionally run a daemon-wide
  dangling-image prune after each successful update.

The unit tests in [src/docker/recreate.rs](https://github.com/Turbootzz/freshdock/blob/main/src/docker/recreate.rs)
(`recreate_with_health_removes_old_image_when_cleanup_enabled`,
`recreate_with_health_prunes_dangling_when_enabled`,
`cleanup_failure_does_not_fail_the_update`) are the authoritative checks; this
procedure is for human verification against a real daemon.

## Prerequisites

- A working Docker daemon on the standard socket.
- `freshdock` built locally: `just build`.

## Steps

`freshdock recreate` always pulls the container's tag before it recreates, so
the tag has to exist upstream: a local-only tag such as `fd-cleanup:demo` fails
the pull with a 404 before cleanup ever runs. The procedure therefore points the
real `nginx:alpine` tag at an older image for the duration of the test. The
container keeps that older image id, and the recreate's own pull moves the tag
back to the current upstream image, so the originally-running image is no
longer referenced afterwards.

```bash
# 1. Point `nginx:alpine` at an older image and run a container on it with
#    cleanup opted in.
docker pull nginx:1.27-alpine
docker tag  nginx:1.27-alpine nginx:alpine

docker run -d --name fd-cleanup \
  --label freshdock.enable=true \
  --label freshdock.mode=watch \
  --label freshdock.cleanup=true \
  nginx:alpine
old_id=$(docker inspect fd-cleanup --format '{{.Image}}')

# 2. Drop the extra tag so nothing but the container references the old image.
#    Skip this step to exercise the shared-image guard instead (see below).
docker rmi nginx:1.27-alpine

# 3. Recreate. The pull restores the upstream `nginx:alpine`; after the new
#    container is healthy the superseded image is removed (best-effort).
./target/release/freshdock recreate fd-cleanup

# 4. The old image id must be gone (no container references it any more).
docker image inspect "$old_id" >/dev/null 2>&1 \
  && echo "FAIL: old image still present" \
  || echo "OK: superseded image removed"
```

## Expected observations

- The CLI prints a line starting `recreated fd-cleanup: healthy` that names the
  removed archive and the new id.
- Step 4 prints `OK: superseded image removed`.
- With `freshdock.cleanup=false` (or the label omitted and `[settings] cleanup`
  unset), step 4 instead prints `FAIL`, i.e. the old image is **kept**. That is
  the correct default-off behaviour; re-run the procedure from step 1 without
  the cleanup label to confirm.
- **Shared-image guard.** Skip step 2 (or keep a second container running on
  `$old_id`) and the removal is refused by the daemon (HTTP 409) because the
  image is still referenced, logged as a warning, and the update still reports
  success. The image stays until the last reference is gone.

## Cleanup

```bash
docker rm -f fd-cleanup $(docker ps -a --filter name=fd-cleanup-old- -q) 2>/dev/null
docker rmi nginx:1.27-alpine 2>/dev/null || true
docker pull nginx:alpine   # only needed if you skipped step 3
```
