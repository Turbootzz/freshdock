# CLI reference

freshdock has three subcommands: [`check`](#freshdock-check) (read-only),
[`recreate`](#freshdock-recreate-name) (one container, manual), and
[`run`](#freshdock-run) (the scheduler daemon). `freshdock --help` and
`freshdock <command> --help` print the same information at the terminal.

## Global options

Available on every subcommand.

| Option | Default | Meaning |
|---|---|---|
| `--no-color` | colour on a TTY | Disable ANSI colour, for log files and non-interactive output. `NO_COLOR` set to any non-empty value does the same. |
| `--config <PATH>` | see below | Path to an optional `freshdock.toml`. |

The config file is optional: you need one only for a registry host whose name has
dots in it, since everything else, notification targets included, has an
[environment variable](configuration.md#environment-variables). Resolution order is
`--config <PATH>`, then `$FRESHDOCK_CONFIG`, then `./freshdock.toml`; an explicit
path that is missing is an error, a missing default file is fine. See the
[configuration reference](configuration.md#the-optional-freshdocktoml-file).

`RUST_LOG` controls log verbosity (default `info`; try `freshdock=debug` or
`trace`). Secrets are always redacted.

---

## `freshdock check`

```bash
freshdock check
```

Read-only. Lists every opted-in container (`freshdock.enable=true`), resolves the
latest digest once per unique image, and prints a status table. It never pulls,
stops, or recreates anything. The dedupe conserves Docker Hub's anonymous budget
of 100 pulls / 6 h; manifest fetches count as pulls.

The table has six columns: `container`, `image`, `mode`, `current digest`,
`latest digest`, and `update?`. The `update?` column is `yes` (a newer digest
exists), `no` (up to date), or `-`/`?` when no comparison was possible. When a
digest can't be resolved, `latest digest` shows the reason instead:

| `latest digest` value | Meaning |
|---|---|
| a short digest (e.g. `sha256:ab12cd...`) | The resolved upstream digest; compare with `update?`. |
| `pinned (no check)` | Image is pinned to a digest, so there is no moving tag to follow. |
| `auth required (set credentials)` | The registry needs credentials that aren't configured. See [registry-auth](registry-auth.md). |
| `network unavailable` | The registry couldn't be reached; nothing is assumed. |
| `error: ...` | The probe failed for another reason (the message follows). |

Docker can record one local image under several manifest digests: pulling a tag
whose multi-arch index was republished without a change to your platform's
manifest adds the new index digest beside the old one. freshdock reports `no`
when the upstream digest is any of them, and shows that digest as
`current digest`.

Examples:

```bash
freshdock check                 # render the table
freshdock --no-color check      # ANSI-free, for logs
RUST_LOG=info freshdock check   # include registry rate-limit info
```

---

## `freshdock recreate <NAME>`

```bash
freshdock recreate <NAME>
```

Manually update one container by name or ID: inspect, pull, stop, rename, create,
start, then [health-gate](health-and-rollback.md) the new container and roll back
to the previous one if it fails. A configured
[pre-update lifecycle hook](lifecycle-hooks.md) runs between pull and stop and can
skip the update (the command reports the skip and exits `0`).

| Argument | Meaning |
|---|---|
| `<NAME>` | Name or ID of the running container to recreate. |

It ignores `freshdock.mode`, since modes drive the scheduler and not manual
intent, but it refuses a container that is `freshdock.enable=false` or
`freshdock.mode=off`: a graceful no-op, so an explicit opt-out can't be recreated
by accident. Any other mode, `watch` included, is allowed because you typed the
command yourself.

---

## `freshdock run`

```bash
freshdock run [--interval <SECS>] [--tick <SECS>] [--stop-timeout <SECS>]
```

The scheduler daemon. Each tick it lists running containers, parses their labels,
and acts on the ones that are due: `live`/`nightly`/`weekly`/`monthly` are updated
(health-gated, with rollback), `watch` is report-only. Runs in the foreground
until `SIGINT`/`SIGTERM`, then finishes the in-flight container and exits. See
[scheduling](scheduling.md) for the timing model.

| Option | Env var | Default | Meaning |
|---|---|---|---|
| `--interval <SECS>` | `FRESHDOCK_INTERVAL` | `300` | Poll cadence for `live` and `watch` containers. |
| `--tick <SECS>` | `FRESHDOCK_TICK` | `60` | Scheduler loop granularity. Calendar (cron) modes are evaluated once per tick, so this bounds how late a fire can be. |
| `--stop-timeout <SECS>` | `FRESHDOCK_STOP_TIMEOUT` | `30` | Max seconds to drain in-flight work after a shutdown signal before force-exit. |

The flag wins over its env var; an invalid env value is a startup error.

Examples:

```bash
freshdock run                            # poll live/watch every 5 min; cron modes on schedule
freshdock run --interval 600             # poll every 10 min instead
freshdock run --config /etc/freshdock.toml
RUST_LOG=info freshdock run              # per-container scheduler events
```
