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

## With nerdctl (containerd)

freshdock can manage containers you run with [nerdctl](https://github.com/containerd/nerdctl)
on containerd. It does every operation by running the `nerdctl` command, so nerdctl
keeps doing the networking, naming and port setup it does for `nerdctl run`.

Requirements:

- nerdctl 2.3 or newer, **rootful** (one containerd running as root). Rootless
  nerdctl is not supported yet.
- freshdock runs **as root on the same host**, as a host binary or systemd service.
  It needs the containerd socket, nerdctl's state under `/var/lib/nerdctl`, and the
  CNI setup, so running it in a container is not supported.
- `runtime = "nerdctl"` in `[settings]`, or `FRESHDOCK_RUNTIME=nerdctl`.

```ini
# /etc/systemd/system/freshdock.service
[Unit]
Description=freshdock container auto-updater (nerdctl)
After=containerd.service
Requires=containerd.service

[Service]
ExecStart=/usr/local/bin/freshdock run
Restart=on-failure
Environment=RUST_LOG=info
Environment=FRESHDOCK_RUNTIME=nerdctl
# Only if nerdctl is not on the service's PATH:
# Environment=FRESHDOCK_NERDCTL_PATH=/usr/local/bin/nerdctl
# Only if your containers are not in nerdctl's default namespace:
# Environment=FRESHDOCK_CONTAINERD_NAMESPACE=default
# Do not add PrivatePIDs=: the cleanup of an interrupted update tells a live
# freshdock from a dead one by process id, so `run` and `recreate` must share
# the host's pid namespace.

[Install]
WantedBy=multi-user.target
```

Labels, modes, schedules, hooks and notifications work as on Docker. At startup
freshdock checks the nerdctl version and that nerdctl can reach containerd, and
logs both.

### What is different on nerdctl

- **Compose projects are updated one container at a time.** nerdctl compose does
  not record the `depends_on` graph on its containers, so `compose_aware` is
  turned off. Keep services you don't want updated on their own (a database, a
  migration) in `watch` mode.
- **Settings are rebuilt, then checked, before anything stops.** nerdctl cannot
  recreate a container from its inspect output, so freshdock rebuilds the
  `nerdctl create` flags from what nerdctl reports. Before stopping the container,
  it creates a throwaway copy from those flags (never started), compares its
  runtime settings with the original's, and removes it. If they differ, the update
  is refused and the log names the differing fields. The container keeps running
  untouched.
- **Some settings are always refused**: a static IP (`--ip`), an explicit
  `--group-add` (nerdctl reports it together with the groups it derives from the
  image), `--userns-remap`, an environment variable whose value spans several
  lines or ends in whitespace, and anything nerdctl does not report. A container
  created with different nerdctl global flags (for example another
  `--snapshotter`) is refused too. Recreate such a container by hand, or keep it in
  `watch` mode.
- **Secrets stay off the command line.** Anything in nerdctl's arguments is
  readable by every local user, so a container's environment variables are passed
  through an owner-only `--env-file` that exists only for the `nerdctl create`.
  nerdctl is also started with only the environment it needs (`PATH`, `HOME`,
  containerd, nerdctl and CNI variables), because it copies its caller's
  environment into every container it creates; freshdock's own
  `FRESHDOCK_REGISTRY_*` and `FRESHDOCK_NOTIFY_*` secrets never reach it. Proxy
  settings (`HTTP_PROXY` and the like, which can carry a password) go to
  `nerdctl pull` alone, the one call that needs the network.
- **An interrupted update is cleaned up by the next `run` or `recreate`.** To
  remove a renamed archive, freshdock briefly renames the updated container to
  `<name>-freshdock-aside-...` (a nerdctl limitation), and preflight creates a
  `<name>-freshdock-check-...` trial container. If freshdock is killed in between,
  the next `run` or `recreate` gives the container its name back, removes the
  trial and any leftover private temp directory, and logs it. Only what an exited
  freshdock left is touched, never what a still-running one (another daemon, a
  `recreate` in progress) is using, and `check` never cleans up at all.
- **A misspelled `runtime`** (say `nerdcl`) stops freshdock at startup rather than
  falling back to Docker.
- **The running image is remembered from first sight.** containerd records a
  container's image by name only, so freshdock notes which image each container
  runs when it first lists it, before pulling anything. When containers share a
  tag, updating one moves the tag, and the others are still recognised as behind
  it and updated in turn. What freshdock cannot know is a tag that moved before it
  first saw the container: a `nerdctl pull` by hand while freshdock was not
  running, or freshdock restarted after updating one container on a tag but
  before the others. `check` then reports such a container as up to date until
  upstream publishes again.
- **Containers are only ever created from local images** (`--pull=never`), so
  only `nerdctl pull` reaches the network, with freshdock's credentials and
  proxy settings. To keep the image a not-yet-updated sibling still runs usable
  after the pull moves the tag, freshdock first names it by its digest
  (`<repository>@sha256:...`) and removes that name again once no container runs
  the image. If freshdock is stopped in between, such a name can remain; remove it
  with `nerdctl rmi <repository>@sha256:...`.
- **Registry credentials** from freshdock's configuration are passed to
  `nerdctl pull` through a temporary, owner-only `DOCKER_CONFIG`. Without them,
  nerdctl's own login (`nerdctl login`) is used. See
  [Registry authentication](registry-auth.md).

## Compatibility

| Platform | Status |
|---|---|
| Plain Docker | Primary target. The API version is negotiated with your daemon at connect time; CI exercises the Docker engine shipped on GitHub's `ubuntu-latest` runners. |
| Docker Desktop (Linux, macOS, Windows) | Supported. |
| Portainer (CE and Business) | Supported via the same Docker socket. |
| Podman 4+ | Supported via the Docker-compatible socket (discovery below). |
| containerd with nerdctl 2.3+ (rootful) | Supported with `runtime = "nerdctl"`, as a host binary running as root. Compose projects are updated one container at a time. See [below](#with-nerdctl-containerd). Rootless nerdctl is not supported yet. |
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
