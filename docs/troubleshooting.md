# Troubleshooting

Symptom-first fixes for the most common first-run issues. Each section links to the
reference page with the full story.

## Contents

- [freshdock can't reach the daemon / wrong socket](#freshdock-cant-reach-the-daemon--wrong-socket)
- [`permission denied` on the Docker socket](#permission-denied-on-the-docker-socket)
- [`no CA certificates were found`](#no-ca-certificates-were-found)
- [freshdock sees my container but never updates it](#freshdock-sees-my-container-but-never-updates-it)
- [My container doesn't appear in `check` at all](#my-container-doesnt-appear-in-check-at-all)
- [`check` reports an update that never goes away](#check-reports-an-update-that-never-goes-away)
- [A container is reported as `pinned (no check)`](#a-container-is-reported-as-pinned-no-check)
- [Updates fail with a read-only socket](#updates-fail-with-a-read-only-socket)
- [A sidecar on `network_mode: container:X` lost its network](#a-sidecar-on-network_mode-containerx-lost-its-network)
- [My app updated but its database migration didn't run](#my-app-updated-but-its-database-migration-didnt-run)
- [A compose rollout aborted](#a-compose-rollout-aborted)
- [Where are the logs?](#where-are-the-logs)

---

## freshdock can't reach the daemon / wrong socket

freshdock probes, in order: `DOCKER_HOST` (any scheme: `unix://`, `tcp://`,
`http://`, `https://`, `ssh://`), then `/var/run/docker.sock`, then Podman's
sockets (`$XDG_RUNTIME_DIR/podman/podman.sock`,
`/run/user/$UID/podman/podman.sock`, `/run/podman/podman.sock`). The startup log
says which family answered and which API version was negotiated:

```
INFO freshdock::docker: connected to the local Docker socket
INFO freshdock::docker: negotiated Docker API version api_version=1.53
```

If those lines are missing, freshdock never reached a daemon; it fails at connect
time rather than part-way through a cycle. Point it at the right socket with
`DOCKER_HOST=unix:///path/to/podman.sock` (or a `tcp://` endpoint). `DOCKER_HOST`,
once set, is authoritative: there is no fallback to the local socket if that
endpoint is dead.

In CI, the live test suites skip themselves when no daemon answers. Set
`FRESHDOCK_LIVE_REQUIRED=1` (as the repo's own live gate job does) to turn that skip
into a failure, so a missing daemon can't pass as a green run.

See [Deployment: which socket freshdock uses](deployment.md#which-socket-freshdock-uses).

## `permission denied` on the Docker socket

freshdock can't reach `/var/run/docker.sock`. As a container, make sure the socket
is mounted; as a host binary, the user needs to be in the `docker` group (or run via
systemd with `SupplementaryGroups=docker`).

See [Deployment: Docker socket permissions](deployment.md#docker-socket-permissions).

## `no CA certificates were found`

The TLS root store is empty, so no HTTPS call to a registry or a notification
target could be verified. `check` and `run` refuse to start; `recreate` builds no
HTTP client, so it is unaffected. The official image ships a bundle. For a custom
image, install `ca-certificates` (Debian/Ubuntu/Alpine package name) or mount the
host bundle:

```bash
--mount type=bind,src=<host bundle>,dst=/etc/ssl/certs/ca-certificates.crt,readonly
```

The host path differs per distribution: Debian/Ubuntu/Alpine
`/etc/ssl/certs/ca-certificates.crt`, Fedora/RHEL `/etc/pki/tls/certs/ca-bundle.crt`,
openSUSE `/etc/ssl/ca-bundle.pem`.

If `SSL_CERT_FILE` or `SSL_CERT_DIR` is set, only that location is read and the
usual system paths are skipped, so a stale value in either produces this error on
a host that does have a bundle. Unset them, or point them at a real one.

## freshdock sees my container but never updates it

Almost certainly the container is in `watch` mode, the default for an enabled
container with no `freshdock.mode` label. `watch` detects and notifies, but never
pulls or restarts; that is the opt-in-by-design safety net, not a bug. To update,
set an updating mode:

```yaml
    labels:
      - "freshdock.enable=true"
      - "freshdock.mode=nightly"   # or live / weekly / monthly
```

Or change the fleet-wide fallback with `[settings] default_mode` /
`FRESHDOCK_DEFAULT_MODE`.

See [Scheduling & update modes](scheduling.md#modes).

## My container doesn't appear in `check` at all

The `freshdock.enable=true` label is missing (or typo'd). freshdock is opt-in: an
unlabelled container is invisible by design, and a misspelled label name is
indistinguishable from no label, so there is no error for it. Verify with:

```bash
docker inspect --format '{{json .Config.Labels}}' <container> | grep freshdock
```

See [Configuration: labels](configuration.md#labels).

## `check` reports an update that never goes away

Older versions compared upstream against a single entry of the local image's
`RepoDigests`. Docker records one image under several manifest digests once a
multi-arch index is republished without a change to your platform's manifest, so the
comparison could never match again: `update? = yes` survived `docker pull`,
`freshdock recreate`, and every scheduled run.

Upgrade past [#74](https://github.com/Turbootzz/freshdock/issues/74). On an older
build, drop the stale digest references:

```bash
docker image inspect <image> --format '{{json .RepoDigests}}'
docker rmi <repo>@sha256:<stale digest>
```

See [`freshdock check`](cli-reference.md#freshdock-check).

## A container is reported as `pinned (no check)`

Its image is pinned to a digest (`repo@sha256:<digest>`), so there is no moving tag
to follow and freshdock will never update it. Switch to a tag (`repo:1.27`) if you
want updates.

See [Configuration: pinned images](configuration.md#labels).

## Updates fail with a read-only socket

A socket mounted `:ro` is enough for `check` and `watch`, but an updating mode
(`live` / `nightly` / `weekly` / `monthly`) has to stop, create, and start
containers, which needs a writable socket mount (drop the `:ro`).

See [Deployment: socket read-only vs writable](deployment.md#socket-read-only-vs-writable).

## A sidecar on `network_mode: container:X` lost its network

It shouldn't any more. Recreating X necessarily destroys the network namespace its
sidecars share, so freshdock repairs them. The rules:

- Before stopping X, freshdock finds every running container whose
  `HostConfig.NetworkMode` is `container:X`, and re-creates each one afterwards.
  This also happens after a rollback, since restarting the restored container gives
  it a fresh namespace too.
- The repair runs before X's `-old-` archive is removed, because Docker refuses to
  remove a container whose namespace a running container still holds.
- Compose's `network_mode: service:X` becomes `container:<id of X>` on disk. That
  dead id is rewritten to the new container's id on the way through. A literal name
  reference is left alone, and after a rollback nothing is rewritten, because the
  restored container owns its original id again.
- The sidecar needs no freshdock labels: it is not being updated, only repaired, so
  the `freshdock.enable` gate does not apply to it.
- It is re-created from the exact image ID it was already running, so a moved tag
  can never sneak an upgrade in through a repair. Only when the daemon reports no
  image id does the existing image reference stand in. No health gate and no
  lifecycle hooks run.
- A sidecar that explicitly opts out with `freshdock.enable=false` or
  `freshdock.mode=off` is skipped, with a warning naming it. It keeps a dead
  namespace until you restart it yourself. An *absent* label is not an opt-out,
  since repairing unlabelled bystanders is the point.
- freshdock itself is skipped, with a warning, when deployed with
  `network_mode: container:<vpn>` and pointed at that same container. Stopping it
  would kill the daemon mid-cycle, so restart the freshdock container by hand
  afterwards.
- Re-attachment is best-effort. A failure logs
  `failed to re-attach network-namespace dependent`, naming the container; the
  update itself still stands.
- When the failure lands after the sidecar was renamed away, freshdock renames it
  back and starts it again, so a failed repair leaves a running container rather
  than a stopped `-old-` one. If even that fails, the warning names the archive you
  have to recover by hand.
- Known limits: sidecars referencing X by an id prefix shorter than 12 characters
  are not recognised, and dependency chains are not followed transitively.

See [Manual test: network-namespace dependents](manual-tests/network-dependents.md).

## My app updated but its database migration didn't run

A one-shot `migrate` service sits in `exited (0)` after its last run, so a
per-container updater never sees it. The app image moves, the app is recreated, and
the migration stays on the old code: new application code against an old schema.
Nothing crashes, so no health gate catches it.

freshdock handles this natively as long as the migration is wired up the normal
Compose way:

```yaml
  web:
    depends_on:
      migrate:
        condition: service_completed_successfully
```

That condition is what marks `migrate` as a one-shot the project waits on. freshdock
then re-runs it before recreating `web`, even though `migrate` has no freshdock
labels of its own.

If it still doesn't run, check in this order:

- **Is the condition actually `service_completed_successfully`?** A bare
  `depends_on: [migrate]` (or `condition: service_started`) does not mark it as a
  one-shot, and freshdock will not touch an unlabelled container without it.
- **Is `compose_aware` on?** It is by default; `FRESHDOCK_COMPOSE_AWARE=0` or
  `[settings] compose_aware = false` disables it.
- **Does the migration opt out?** `freshdock.enable=false`,
  `com.centurylinklabs.watchtower.enable=false`, or `freshdock.mode=off` on the
  one-shot are honoured, one-shot or not.
- **Do the two share an image?** The rollout is triggered by a container freshdock
  watches, and only members on that same image are updated. In the usual pattern
  `migrate` and `web` both run `app:latest`, which is what makes the link.
- **Was it still running?** A one-shot that is mid-run is left alone rather than
  stomped; it is picked up on the next cycle.

Full rules: [Compose projects](compose.md#what-gets-updated).

## A compose rollout aborted

```text
rollout ABORTED: shop-migrate-1 exited with code 1. The services after this
point were not touched and are still running their previous image.
```

This is the safety behaviour, not a bug: a one-shot the project waits on did not
exit `0`, so freshdock stopped rather than starting new code against a migration
that did not complete. Your stack is still running its previous image.

The failed container is kept on purpose, and so is the archive of its previous
instance, since the logs are the only record of what went wrong:

```bash
docker logs shop-migrate-1
```

Fix the cause, then re-run it with `freshdock recreate <the labelled service>`, or
wait for the next scheduled cycle. Nothing is rolled back automatically: the command
already ran and may have applied part of its work, so restoring the previous
container object would change nothing about your database while throwing the
evidence away. See [when a one-shot fails](compose.md#when-a-one-shot-fails).

## Where are the logs?

- Container: `docker logs freshdock`
- systemd: `journalctl -u freshdock`

Verbosity is controlled by `RUST_LOG` (default `info`); `RUST_LOG=freshdock=debug`
is the useful next step. Secrets (tokens, passwords, webhook URLs) are redacted at
every level, including `trace`.

See [Configuration: environment variables](configuration.md#environment-variables).

---

Still stuck? [Open an issue](https://github.com/Turbootzz/freshdock/issues) with the
`freshdock check` output and a `RUST_LOG=freshdock=debug` log excerpt.
