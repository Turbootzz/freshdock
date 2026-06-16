# Manual smoke test: image cleanup

Verifies the opt-in image cleanup that runs after a successful, health-passed
update (PLAN §5.2 step 8). Cleanup is **off by default**; it removes the image
the *replaced* container was running, and is best-effort — a shared image still
referenced by another container is kept, and a cleanup failure never fails the
update.

There are two knobs:

- `[settings] cleanup = true` (or per container `freshdock.cleanup=true`) —
  remove the replaced image after a healthy update.
- `[settings] prune_dangling = true` — additionally run a daemon-wide
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

`freshdock recreate` recreates against the *current* tag, so to see the old
image actually become superseded we re-point a local tag at a different image
between launch and recreate. Here `:demo` first points at one image, then at
another, so the originally-running image is no longer referenced after the
recreate.

```bash
# 1. Create a local moving tag pointing at an older image, and run a container
#    on it with cleanup opted in.
docker pull nginx:1.27-alpine
docker tag  nginx:1.27-alpine fd-cleanup:demo
old_id=$(docker image inspect fd-cleanup:demo --format '{{.Id}}')

docker run -d --name fd-cleanup \
  --label freshdock.enable=true \
  --label freshdock.mode=watch \
  --label freshdock.cleanup=true \
  fd-cleanup:demo

# 2. Re-point the tag at a newer image so the recreate pulls a different id.
docker pull nginx:1.28-alpine
docker tag  nginx:1.28-alpine fd-cleanup:demo

# 3. Recreate. After the new container is healthy, the superseded image is
#    removed (best-effort).
./target/release/freshdock recreate fd-cleanup

# 4. The old image id must be gone (no container references it any more).
docker image inspect "$old_id" >/dev/null 2>&1 \
  && echo "FAIL: old image still present" \
  || echo "OK: superseded image removed"
```

## Expected observations

- The CLI prints `recreated fd-cleanup: healthy — ...`.
- Step 4 prints `OK: superseded image removed`.
- With `freshdock.cleanup=false` (or the label omitted and `[settings] cleanup`
  unset), step 4 instead prints `FAIL` — i.e. the old image is **kept**. That is
  the correct default-off behaviour; re-run the procedure without the cleanup
  label to confirm.
- **Shared-image guard.** If a second container is still running on `$old_id`
  when you recreate, the removal is refused by the daemon (HTTP 409), logged as a
  warning, and the update still reports success. The image stays until the last
  referencing container is gone.

## Cleanup

```bash
docker rm -f fd-cleanup $(docker ps -a --filter name=fd-cleanup-old- -q) 2>/dev/null
docker rmi fd-cleanup:demo nginx:1.27-alpine nginx:1.28-alpine 2>/dev/null || true
```
