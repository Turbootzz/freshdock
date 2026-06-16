# Deployment

freshdock is a single static binary that talks to the Docker socket. Run it however
suits you: as a container alongside the ones it manages, or directly on the host
(e.g. under systemd). For configuration, see the
[configuration reference](configuration.md).

## As a container (recommended)

Mount the Docker socket and run the `run` subcommand:

```bash
docker run -d \
  --name freshdock \
  --restart unless-stopped \
  -v /var/run/docker.sock:/var/run/docker.sock \
  ghcr.io/turbootzz/freshdock:latest run
```

Or with compose — see the runnable stacks in
[`examples/compose/`](https://github.com/Turbootzz/freshdock/tree/main/examples/compose):

- [`minimal-watch.yml`](https://github.com/Turbootzz/freshdock/blob/main/examples/compose/minimal-watch.yml) — watch-only, read-only socket.
- [`mixed-modes.yml`](https://github.com/Turbootzz/freshdock/blob/main/examples/compose/mixed-modes.yml) — live + nightly + watch on one daemon.
- [`notifications-enabled.yml`](https://github.com/Turbootzz/freshdock/blob/main/examples/compose/notifications-enabled.yml) — mounts a `freshdock.toml`.
- [`registry-authenticated.yml`](https://github.com/Turbootzz/freshdock/blob/main/examples/compose/registry-authenticated.yml) — private registry via env.

### Socket: read-only vs writable

| Workload | Socket mount |
|---|---|
| `watch` / `check` only (never recreates) | `:ro` is enough — `-v /var/run/docker.sock:/var/run/docker.sock:ro` |
| Any updating mode (`live`/`nightly`/`weekly`/`monthly`, or `recreate`) | writable — `-v /var/run/docker.sock:/var/run/docker.sock` |

### Configuration: environment first

Most deployments need no config file. Fleet-wide settings, registry credentials,
and the `run` flags are all environment variables — pass them under `environment:`
in compose (or `Environment=` in a systemd unit):

```yaml
services:
  freshdock:
    image: ghcr.io/turbootzz/freshdock:latest
    command: ["run"]
    environment:
      FRESHDOCK_DEFAULT_MODE: "nightly"
      FRESHDOCK_REGISTRY_GHCR_USERNAME: "${GHCR_USER:-}"
      FRESHDOCK_REGISTRY_GHCR_TOKEN: "${GHCR_TOKEN:-}"
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock
    restart: unless-stopped
```

The full list is the [env-var table](configuration.md#environment-variables).

### Mounting a config file (for notifications)

You only need a `freshdock.toml` to *declare* a notification target (see
[notifications](notifications.md)). Mount it read-only and keep its secrets in the
environment:

```yaml
services:
  freshdock:
    image: ghcr.io/turbootzz/freshdock:latest
    command: ["run", "--config", "/config/freshdock.toml"]
    environment:
      FRESHDOCK_NOTIFY_EMAIL_PASSWORD: "${SMTP_PASSWORD:-}"
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock
      - ./freshdock.toml:/config/freshdock.toml:ro
    restart: unless-stopped
```

## As a host binary (systemd)

Install the binary (`cargo install freshdock`, a release binary, or `just build`),
then create a unit:

```ini
# /etc/systemd/system/freshdock.service
[Unit]
Description=freshdock container auto-updater
After=docker.service
Requires=docker.service

[Service]
ExecStart=/usr/local/bin/freshdock run
Restart=on-failure
Environment=RUST_LOG=info
# If freshdock.toml is not in the working directory:
# Environment=FRESHDOCK_CONFIG=/etc/freshdock/freshdock.toml
# Run as a user in the docker group rather than root where possible.
# SupplementaryGroups=docker

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now freshdock
journalctl -u freshdock -f
```

## Docker socket permissions

freshdock talks to `/var/run/docker.sock`. `permission denied` on the socket means
the process isn't allowed to use it:

- **On the host:** run as a user in the `docker` group.
- **In a container:** the socket's group GID inside the container must match the
  host socket's owner. On some hosts you must pass `--group-add <gid>` (find it with
  `stat -c '%g' /var/run/docker.sock`).

Note: access to the Docker socket is effectively root on the host — grant it
deliberately.

## Compatibility

| Platform | Status |
|---|---|
| Plain Docker (24.x – 29+) | Primary target. |
| Docker Desktop (Linux, macOS, Windows) | Supported. |
| Portainer (CE and Business) | Supported via the same Docker socket. |
| Podman 4+ | Supported via the Docker-compatible socket. |
| Compose-based UIs (Dockge, Komodo, …) | Containers are updated individually; compose files are not edited. |
| Kubernetes / Swarm | Out of scope — use platform-native mechanisms. |

A recreate replaces the container with a new ID, so a UI that pinned the old ID may
briefly show it "out of sync" until its next refresh. freshdock never edits your
compose/stack files — only the running container.
