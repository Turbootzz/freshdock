# Lifecycle hooks

Some apps need a hand around an update: flush a write buffer before going down,
clear a cache once the new version is up (Dolibarr, GLPI, Nextcloud, …).
Lifecycle hooks let a container declare those commands as labels; freshdock
runs them inside the container with `docker exec` at the right moments of the
[update lifecycle](health-and-rollback.md).

```yaml
services:
  glpi:
    image: glpi/glpi:latest
    labels:
      freshdock.enable: "true"
      freshdock.mode: "nightly"
      freshdock.lifecycle.pre-update: "/opt/glpi/bin/maintenance --enable"
      freshdock.lifecycle.post-update: "php bin/console cache:clear"
```

## Labels

| Label | Value | Default | Meaning |
|---|---|---|---|
| `freshdock.lifecycle.pre-update` | shell command | *(none)* | Runs in the **old** container, after the new image is pulled and before the container is stopped. If it does not succeed, the update is **skipped**. |
| `freshdock.lifecycle.pre-update-timeout` | seconds | `60` | Time budget for the pre-update hook. `0` disables the timeout. |
| `freshdock.lifecycle.post-update` | shell command | *(none)* | Runs in the **new** container once it has passed the health gate. Best-effort: a failure is logged, the update stands. |
| `freshdock.lifecycle.post-update-timeout` | seconds | `60` | Time budget for the post-update hook. `0` disables the timeout. |

Commands are executed as `sh -c "<command>"` inside the container, so the image
must ship a `sh` (distroless images cannot use hooks — same constraint as
Watchtower). Hook output is logged at `debug` level.

## Where the hooks sit in the update

```text
inspect → pull → PRE-UPDATE HOOK → stop → rename → create → start
        → health gate → remove old container → POST-UPDATE HOOK → image cleanup
```

The pre-update hook runs *after* the pull so the image is already local and the
downtime window stays short, and *before* the stop so the app is still up to
answer the exec.

## Pre-update: the veto

The pre-update hook is a gate. The update proceeds **only** when the hook exits
`0`. Anything else leaves the container untouched, running its old image:

- **Exit code `75`** (`EX_TEMPFAIL`, Watchtower-compatible) — the polite "not
  now": the app is mid-backup, mid-migration, has active sessions, … freshdock
  logs the deferral at `info` and simply tries again the next time the
  container is due.
- **Any other non-zero exit** — logged as a warning, update skipped.
- **Timeout** — logged as a warning, update skipped. (Docker has no way to kill
  an exec, so the command itself keeps running inside the container; only the
  verdict is decided.)
- **Exec failure** (no `sh` in the image, daemon error) — logged as a warning,
  update skipped.

The skip is not an error: `freshdock recreate` reports it and exits `0`, and
the scheduler retries on the container's normal cadence (`--interval` for
`live`, the cron for calendar modes).

A pre-update script that only allows updates outside business hours:

```sh
#!/bin/sh
hour=$(date +%H)
if [ "$hour" -ge 8 ] && [ "$hour" -lt 18 ]; then
  exit 75   # EX_TEMPFAIL: defer, try again next cycle
fi
exit 0
```

## Post-update: best-effort maintenance

The post-update hook runs in the **new** container, after the health gate has
passed and the archived old container has been removed — the update has already
succeeded at that point, so a failing or timing-out post-update hook is logged
as a warning and never fails (or rolls back) the update.

If the update rolls back, the post-update hook does **not** run: there was no
update to follow up on.

## Interaction with rollback

Hooks never weaken the safety contract:

- A pre-update veto happens *before* anything is stopped — the container is
  simply left alone.
- Rollback is still driven purely by the [health gate](health-and-rollback.md);
  hooks cannot cause or prevent a rollback.

## Watchtower equivalents

| Watchtower | freshdock |
|---|---|
| `com.centurylinklabs.watchtower.lifecycle.pre-update` | `freshdock.lifecycle.pre-update` |
| `com.centurylinklabs.watchtower.lifecycle.post-update` | `freshdock.lifecycle.post-update` |
| `…lifecycle.pre-update-timeout` (**minutes**) | `freshdock.lifecycle.pre-update-timeout` (**seconds**) |
| `…lifecycle.post-update-timeout` (**minutes**) | `freshdock.lifecycle.post-update-timeout` (**seconds**) |
| `--enable-lifecycle-hooks` / `WATCHTOWER_LIFECYCLE_HOOKS` | *(not needed)* — setting a hook label is the opt-in |
| `…lifecycle.pre-check` / `…lifecycle.post-check` | *(no equivalent)* — freshdock has no per-cycle check hooks |

Note the timeout unit difference: Watchtower counts minutes, freshdock counts
seconds. A Watchtower `pre-update-timeout: "5"` becomes
`freshdock.lifecycle.pre-update-timeout: "300"`.
