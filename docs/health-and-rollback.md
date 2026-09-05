# Health gating & rollback

A container counts as updated only once the **new** container proves healthy. If it
does not, the **previous** container is restored automatically. That is what makes
unattended updates safe.

## The recreate lifecycle

Whether triggered by [`freshdock recreate`](cli-reference.md#freshdock-recreate-name)
or by the [scheduler](scheduling.md), an update runs the same cycle:

1. **inspect** the running container (capture its full config).
2. **pull** the new image.
3. **stop** the old container.
4. **rename** it to an archive name `<name>-old-<timestamp>` (kept as the rollback source).
5. **create** the new container from the same config plus the new image.
6. **start** it.
7. **health-gate** the new container (below).
8. On success: remove the archive (and optionally [clean up the image](#image-cleanup)).
   On failure: **roll back**.

The recreated container preserves the original config (networks, volumes, env, caps,
user, and so on); a dedicated round-trip test asserts the inspected config comes back
byte-identical.

## Health verdicts

After start, freshdock polls the container until it reaches one of three verdicts:

| Verdict | Meaning | Outcome |
|---|---|---|
| **Healthy** | A declared healthcheck reported `healthy`, **or** (no healthcheck declared) the container stayed up for the grace period. | Remove the archive; success. |
| **Timeout** | A healthcheck was declared but never went `healthy` within the timeout. | Roll back; failure. |
| **Crashed** | The container exited before becoming healthy, or before the grace period elapsed. | Roll back; failure. |

Transient probe errors are tolerated (logged and retried); a persistent probe
failure past the timeout resolves to the safe `Timeout` verdict.

### Timings

These are currently **hardcoded** (not yet label/config/env-configurable):

| Setting | Value | Meaning |
|---|---|---|
| health timeout | 120 s | Max wait for a declared healthcheck to report `healthy`. |
| grace period | 10 s | How long a container with **no** healthcheck must stay running to count as healthy. |
| poll interval | 1 s | How often the new container's state is inspected. |

A container without a `HEALTHCHECK` can only be judged by whether it stayed up, so
the grace period is the best signal available. Declare a healthcheck for stronger
gating.

## Rollback

On `Timeout` or `Crashed`, freshdock restores the previous container atomically:

1. Stop the new (failed) container (best-effort; it may already be dead).
2. Force-remove the new container.
3. Rename the archive `<name>-old-<timestamp>` back to the original name.
4. Start the restored container.

You are left running what you had before the update. If
[`freshdock.notify=true`](notifications.md), a `failed` notification is sent with the
reason and the archive it was restored from.

## Image cleanup

A successful, health-passed update always removes the replaced **container** archive.
The superseded **image** is kept by default; opt into removing it:

- per container: `freshdock.cleanup=true`
- fleet-wide default: `[settings] cleanup = true` (or `FRESHDOCK_CLEANUP=true`)
- plus a daemon-wide dangling-image prune: `[settings] prune_dangling = true` (or `FRESHDOCK_PRUNE_DANGLING=true`)

Cleanup is best-effort:

- The old image is removed by its **ID**. If another container still references it,
  the daemon refuses (HTTP 409) and freshdock keeps it. That is the guard against
  deleting a shared base image, and is **not** treated as a failure.
- If no image ID can be resolved (a locally-built image, for example), cleanup is
  skipped.
- Any cleanup or prune failure is logged but **never** fails the update or triggers a
  rollback.

See the [configuration reference](configuration.md#settings) for the keys and the
[cleanup smoke-test playbook](manual-tests/cleanup.md) to verify it end to end.
