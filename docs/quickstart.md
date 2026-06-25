# Quickstart

Get freshdock watching a container in about a minute. No config file required —
freshdock is driven by container labels and environment variables, notifications
[included](notifications.md#declaring-targets-from-the-environment).

## 1. Install

Pick one (full options in the [README](https://github.com/Turbootzz/freshdock/blob/main/README.md#install)):

```bash
cargo install freshdock                          # from crates.io
# or
docker pull ghcr.io/turbootzz/freshdock:latest   # container image
```

## 2. See what freshdock would do (read-only)

Label a container to opt it in, then run `check` — it never changes anything:

```yaml
services:
  web:
    image: nginx:1.27
    labels:
      - "freshdock.enable=true"   # opt in; mode defaults to watch
```

```bash
freshdock check
```

You'll get a table of opted-in containers and whether each has an update available.
(See the [CLI reference](cli-reference.md#freshdock-check) for what the status cells
mean.)

## 3. Run the daemon

freshdock is **opt-in** and defaults enabled containers to `watch` (notice updates,
never restart). Start the scheduler:

```bash
docker run -d \
  --name freshdock \
  --restart unless-stopped \
  -v /var/run/docker.sock:/var/run/docker.sock:ro \
  ghcr.io/turbootzz/freshdock:latest run
```

This is the [`minimal-watch.yml`](https://github.com/Turbootzz/freshdock/blob/main/examples/compose/minimal-watch.yml) example. A
read-only socket is enough for `watch`.

## 4. Let it actually update something

Switch a container to an updating mode and give the daemon a writable socket:

```yaml
    labels:
      - "freshdock.enable=true"
      - "freshdock.mode=nightly"   # check at 04:00 daily; recreate if newer
      - "freshdock.notify=true"    # if you've configured a notifier
```

Updates are [health-gated with automatic rollback](health-and-rollback.md) — a broken
new image is reverted, not left running.

## Next steps

- [Configuration reference](configuration.md) — every label, setting, and env var.
- [Scheduling](scheduling.md) — modes and cron syntax.
- [Notifications](notifications.md) — webhook, Discord, Telegram, SMTP.
- [Registry authentication](registry-auth.md) — private images.
- [Deployment](deployment.md) — systemd, socket permissions, compatibility.
- [Troubleshooting](troubleshooting.md) — something not behaving? Start here.
- [Coming from Watchtower?](migrating-from-watchtower.md)
