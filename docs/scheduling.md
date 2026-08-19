# Scheduling & update modes

Every opted-in container picks an update **mode** with the `freshdock.mode` label.
The mode decides *whether* freshdock acts and *when*. The scheduler
([`freshdock run`](cli-reference.md#freshdock-run)) drives all of this.

## Modes

| Mode | When it acts | What it does |
|---|---|---|
| `live` | every `--interval` seconds (default 300) | Pull and recreate on every new digest. |
| `nightly` | cron `0 4 * * *` (04:00 daily) | Recreate if a newer image exists. |
| `weekly` | cron `0 4 * * 0` (04:00 Sunday) | Recreate if a newer image exists. |
| `monthly` | cron `0 4 1 * *` (04:00 on the 1st) | Recreate if a newer image exists. |
| `watch` | every `--interval` seconds | **Report only** — emit an `available` notification, never pull or restart. |
| `off` | never | Ignored by the scheduler. |

A single daemon mixes modes freely: container A `live`, container B `nightly`,
container C `watch`. When `freshdock.enable=true` but no mode is set, the default is
`watch` (or `[settings] default_mode` — see
[configuration](configuration.md#settings)).

`watch` de-duplicates: it alerts once per distinct new digest, not every poll, so
you aren't re-notified until you act (or the upstream digest changes again).

## Polling cadence (`live` / `watch`)

`live` and `watch` containers are checked on a fixed interval set by
`freshdock run --interval <seconds>` (default 300). A container is due when it has
never been checked, or when at least `--interval` seconds have passed since its last
check.

## Calendar modes (`nightly` / `weekly` / `monthly`)

These fire on a cron schedule. Override any of their default schedules with a
`freshdock.schedule` label (ignored for `live` / `watch` / `off`):

```yaml
labels:
  - "freshdock.enable=true"
  - "freshdock.mode=weekly"
  - "freshdock.schedule=0 2 * * 1"   # 02:00 every Monday
```

The schedule is evaluated once per scheduler **tick** (`--tick`, default 60 s), so a
calendar fire can be at most one tick late.

### Cron syntax

Standard 5 fields: `minute hour day-of-month month day-of-week`.

| Field | Range |
|---|---|
| minute | `0–59` |
| hour | `0–23` |
| day-of-month | `1–31` |
| month | `1–12` |
| day-of-week | `0–6` (**Sunday = 0**; names not supported) |

Each field accepts:

- `*` — any value
- `N` — an exact value
- `A-B` — an inclusive range
- `*/n` or `A-B/n` or `N/n` — a step
- `N,M,O` — a comma-separated list

Example: `*/15 9-17 * * 1-5` = every 15 minutes, 09:00–17:00, Monday–Friday.

**Day-of-month vs day-of-week.** When *both* are restricted (neither is `*`), a tick
matches if **either** matches — the classic Vixie-cron union. `0 4 13 * 5` fires at
04:00 on the 13th *and* every Friday.

## Timezone, DST, and missed windows

- **Timezone.** Schedules are evaluated in the host's **system local time**, not
  UTC. (Set the container's `TZ`/timezone if you want a specific zone.)
- **DST.** Across a spring-forward, a schedule landing in the skipped hour (e.g.
  `30 2 * * *`) does not fire that day; the 04:00 defaults steer clear of the
  transition hour.
- **No backfill.** Schedule state is in memory only. A window missed while the
  daemon was down is **not** caught up — it simply fires at the next occurrence.

## What happens when a container is due

1. The image digest is probed against its registry.
2. For `watch`: if a newer digest appeared, an `available`
   [notification](notifications.md) is dispatched (once per distinct digest).
3. For the updating modes: if a newer digest exists, the container is recreated and
   [health-gated, with rollback on failure](health-and-rollback.md).

The digest is compared for **membership**: Docker can record one local image
under several manifest digests, and the container counts as up to date when
upstream's digest is any of them — otherwise a republished multi-arch index
would recreate a healthy container on every run.
