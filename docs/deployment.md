# Deployment

freshdock is a single static binary that talks to the Docker socket. Run it however
suits you: as a container alongside the ones it manages, or directly on the host
(under systemd, for example). For configuration, see the
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

Or with compose. The runnable stacks live in
[`examples/compose/`](https://github.com/Turbootzz/freshdock/tree/main/examples/compose):

- [`minimal-watch.yml`](https://github.com/Turbootzz/freshdock/blob/main/examples/compose/minimal-watch.yml): watch-only, read-only socket.
- [`mixed-modes.yml`](https://github.com/Turbootzz/freshdock/blob/main/examples/compose/mixed-modes.yml): live + nightly + watch on one daemon.
- [`notifications-enabled.yml`](https://github.com/Turbootzz/freshdock/blob/main/examples/compose/notifications-enabled.yml): mounts a `freshdock.toml`.
- [`registry-authenticated.yml`](https://github.com/Turbootzz/freshdock/blob/main/examples/compose/registry-authenticated.yml): private registry via env.
- [`watch-all.yml`](https://github.com/Turbootzz/freshdock/blob/main/examples/compose/watch-all.yml): opt-out mode, every container included unless it opts out.
- [`compose-project.yml`](https://github.com/Turbootzz/freshdock/blob/main/examples/compose/compose-project.yml): a stack with a one-shot migration, rolled out as one unit. See [Compose projects](compose.md).

### Socket: read-only vs writable

| Workload | Socket mount |
|---|---|
| `watch` / `check` only (never recreates) | `:ro` is enough: `-v /var/run/docker.sock:/var/run/docker.sock:ro` |
| Any updating mode (`live`/`nightly`/`weekly`/`monthly`, or `recreate`) | writable: `-v /var/run/docker.sock:/var/run/docker.sock` |

### Configuration: environment first

Most deployments need no config file. Fleet-wide settings, registry credentials,
notification targets, and the `run` flags are all environment variables. Pass them under `environment:` in
compose, or `Environment=` in a systemd unit:

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

Add `FRESHDOCK_WATCH_ALL: "true"` to that block if you would rather not label each
container: every running container is then included unless it opts out, on the mode
set by `FRESHDOCK_DEFAULT_MODE`. See
[watching every container](configuration.md#watching-every-container).

The full list is the [env-var table](configuration.md#environment-variables).

### Notifications

A notification target can be declared from the environment with a
[shoutrrr-style URL](notifications.md#declaring-targets-from-the-environment), with
no file to mount:

```yaml
services:
  freshdock:
    image: ghcr.io/turbootzz/freshdock:latest
    command: ["run"]
    environment:
      FRESHDOCK_NOTIFY_OPS_URL: "discord://${DISCORD_TOKEN}@${DISCORD_ID}"
      FRESHDOCK_NOTIFY_OPS_TRIGGERS: "succeeded,failed"
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock
    restart: unless-stopped
```

To use a file instead, declare the target in a `[notifications.<name>]` table, mount
it read-only, and keep its secret in the environment:

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
then create a unit. The host needs a CA bundle (the `ca-certificates` package) or
`SSL_CERT_FILE` pointing at one, or freshdock refuses to start; see
[Troubleshooting](troubleshooting.md#no-ca-certificates-were-found).

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

- On the host: run as a user in the `docker` group.
- In a container: the socket's group GID inside the container must match the host
  socket's owner. On some hosts you must pass `--group-add <gid>` (find it with
  `stat -c '%g' /var/run/docker.sock`).

Access to the Docker socket is effectively root on the host, so grant it with care.

## Compatibility

| Platform | Status |
|---|---|
| Plain Docker | Primary target. The API version is negotiated with your daemon at connect time; CI exercises the Docker engine shipped on GitHub's `ubuntu-latest` runners. |
| Docker Desktop (Linux, macOS, Windows) | Supported. |
| Portainer (CE and Business) | Supported via the same Docker socket. |
| Podman 4+ | Supported via the Docker-compatible socket (discovery below). |
| Docker Compose | A multi-service project is updated as one unit, in `depends_on` order. See [Compose projects](compose.md). |
| Compose-based UIs (Dockge, Komodo, and similar) | Supported; compose files are never read or edited. |
| Kubernetes / Swarm | Out of scope; use platform-native mechanisms. |

One daemon-version floor applies: re-creating a container attached to more than one
network requires Docker API 1.44 (Docker 25.0), because older daemons cannot attach
several networks in a single create call. freshdock checks the negotiated API
version before it stops anything, so on an older daemon such a container is refused
with an explanatory error rather than being taken down and left unable to come back.
Detach the extra networks (re-attaching them after the update) or upgrade the daemon.

### Which socket freshdock uses

freshdock connects in this order at startup and logs which family answered:

1. `DOCKER_HOST`, if set. `unix://`, `tcp://`, `http://`, `https://` and `ssh://`
   are all honoured. Only the scheme is logged; the value may carry credentials.
2. The local Docker socket: `/var/run/docker.sock` (named pipe on Windows).
3. Podman's sockets: `$XDG_RUNTIME_DIR/podman/podman.sock`,
   `/run/user/$UID/podman/podman.sock`, then `/run/podman/podman.sock`.

So a rootless or rootful Podman host in the standard location works with no
configuration. For anything else, set `DOCKER_HOST=unix:///path/to/podman.sock`.

A recreate replaces the container with a new ID, so a UI that pinned the old ID may
briefly show it "out of sync" until its next refresh. freshdock never edits your
compose/stack files, only the running container.
