# Troubleshooting

Symptom-first fixes for the most common first-run issues. Each section links to
the reference page with the full story.

## Contents

- [`permission denied` on the Docker socket](#permission-denied-on-the-docker-socket)
- [freshdock sees my container but never updates it](#freshdock-sees-my-container-but-never-updates-it)
- [My container doesn't appear in `check` at all](#my-container-doesnt-appear-in-check-at-all)
- [A container is reported as `pinned (no check)`](#a-container-is-reported-as-pinned-no-check)
- [Updates fail with a read-only socket](#updates-fail-with-a-read-only-socket)
- [A sidecar on `network_mode: container:X` lost its network](#a-sidecar-on-network_mode-containerx-lost-its-network)
- [Where are the logs?](#where-are-the-logs)

---

## `permission denied` on the Docker socket

freshdock can't reach `/var/run/docker.sock`. As a container, make sure the
socket is mounted; as a host binary, the user needs to be in the `docker` group
(or run via systemd with `SupplementaryGroups=docker`).

→ [Deployment: Docker socket permissions](deployment.md#docker-socket-permissions)

## freshdock sees my container but never updates it

Almost certainly the container is in `watch` mode — the **default** for an
enabled container with no `freshdock.mode` label. `watch` detects and notifies,
but **never pulls or restarts**; that's the opt-in-by-design safety net, not a
bug. To actually update, set an updating mode:

```yaml
    labels:
      - "freshdock.enable=true"
      - "freshdock.mode=nightly"   # or live / weekly / monthly
```

…or change the fleet-wide fallback with `[settings] default_mode` /
`FRESHDOCK_DEFAULT_MODE`.

→ [Scheduling & update modes](scheduling.md#modes)

## My container doesn't appear in `check` at all

The `freshdock.enable=true` label is missing (or typo'd). freshdock is opt-in:
an unlabelled container is invisible by design, and a misspelled label name is
indistinguishable from no label — there's no error for it. Verify with:

```bash
docker inspect --format '{{json .Config.Labels}}' <container> | grep freshdock
```

→ [Configuration: labels](configuration.md#labels)

## A container is reported as `pinned (no check)`

Its image is pinned to a digest (`repo@sha256:…`), so there is no moving tag to
follow — freshdock will never update it. Switch to a tag (`repo:1.27`) if you
want updates.

→ [Configuration: pinned images](configuration.md#labels)

## Updates fail with a read-only socket

A socket mounted `:ro` is enough for `check` and `watch`, but an updating mode
(`live` / `nightly` / `weekly` / `monthly`) has to stop, create, and start
containers — that needs a writable socket mount (drop the `:ro`).

→ [Deployment: socket read-only vs writable](deployment.md#socket-read-only-vs-writable)

## A sidecar on `network_mode: container:X` lost its network

It shouldn't any more. Recreating X necessarily destroys the network namespace
its sidecars share, so freshdock finds every running container whose
`HostConfig.NetworkMode` is `container:X` **before** stopping X, and re-creates
each one afterwards — including after a rollback, since restarting the restored
container also gives it a fresh namespace. Compose's `network_mode: service:X`
becomes `container:<id of X>` on disk; that dead id is rewritten to the new
container's id on the way through.

The sidecar needs **no freshdock labels** for this: it is not being updated,
only repaired, so the `freshdock.enable` gate does not apply to it. Nothing else
about it changes — same image, no health gate, no lifecycle hooks.

Re-attachment is best-effort. If it fails you'll see
`failed to re-attach network-namespace dependent` in the log, naming the
container; the update itself still stands and a manual
`docker rm -f <sidecar> && docker run …` puts it back. Sidecars referencing X by
an id prefix shorter than 12 characters are not recognised, and dependency
chains are not followed transitively.

→ [Manual test: network-namespace dependents](manual-tests/network-dependents.md)

## Where are the logs?

- Container: `docker logs freshdock`
- systemd: `journalctl -u freshdock`

Verbosity is controlled by `RUST_LOG` (default `info`); `RUST_LOG=freshdock=debug`
is the useful next step. Secrets (tokens, passwords, webhook URLs) are redacted
at every level, including `trace`.

→ [Configuration: environment variables](configuration.md#environment-variables)

---

Still stuck? [Open an issue](https://github.com/Turbootzz/freshdock/issues) with
the `freshdock check` output and a `RUST_LOG=freshdock=debug` log excerpt.
