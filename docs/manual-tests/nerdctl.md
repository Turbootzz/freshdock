# Manual smoke test: the nerdctl runtime

Verifies the update cycle on containerd through rootful nerdctl: a rebuilt
container keeps its settings, a crashing replacement is rolled back, a
network-namespace dependent follows its owner, and a setting freshdock can't
reproduce is refused before anything stops.

The automated version of these checks is
[tests/nerdctl_live.rs](https://github.com/Turbootzz/freshdock/blob/main/tests/nerdctl_live.rs),
run by `just live-nerdctl` and by CI's "Live nerdctl gate" job. Use this playbook on
a real host, for example before a release or after upgrading nerdctl.

## Setup

Needs nerdctl 2.3+ with a rootful containerd, and a freshdock build. Everything
below runs in a throwaway namespace, so containers in your other namespaces are
never seen or touched:

```bash
export NS=freshdock-smoke
alias n="sudo nerdctl --namespace $NS"
alias fd="sudo FRESHDOCK_RUNTIME=nerdctl FRESHDOCK_CONTAINERD_NAMESPACE=$NS ./target/release/freshdock"
fd check    # logs "connected to containerd through nerdctl ... namespace=freshdock-smoke"
```

## 1. Round trip

```bash
n run -d --name web --label freshdock.enable=true \
  -p 18090:80 -v web-data:/data -e FOO=bar --restart unless-stopped \
  --health-cmd 'wget -qO- http://127.0.0.1/ >/dev/null || exit 1' --health-interval 2s \
  --memory 256m --security-opt no-new-privileges --init \
  docker.io/library/nginx:1.27-alpine
n inspect --mode=native web > /tmp/before.json
fd recreate web
```

Expect a `recreated web: healthy` line that names the removed `web-old-<ts>`
archive. Then:

- `n ps -a` shows `web` running with a new id, and no `web-old-*` or
  `*-freshdock-check-*` container.
- `curl -fsS http://127.0.0.1:18090/` answers.
- `n inspect --mode=native web` matches `/tmp/before.json` apart from ids, the
  hostname and state paths.

## 2. Rollback

```bash
n run -d --name crashy --label freshdock.enable=true docker.io/library/alpine:3.20 \
  sh -c 'sleep 2; while [ -f /tmp/ok ]; do sleep 1; done; exit 3'
n exec crashy touch /tmp/ok; sleep 3
n inspect crashy | jq -r '.[0].Id'
fd recreate crashy
```

The replacement has no `/tmp/ok`, so it exits. Expect
`recreate failed for crashy: new image was unhealthy (Crashed); rolled back`, and
`crashy` running again with the **same** id as before.

## 3. Network-namespace dependent

```bash
n run -d --name vpn --label freshdock.enable=true docker.io/library/alpine:3.20 sleep 1d
n run -d --name app --network container:vpn docker.io/library/alpine:3.20 sleep 1d
fd recreate vpn
n inspect app | jq -r '.[0].HostConfig.NetworkMode'
```

Expect `container:<new vpn id>` and `app` running.

## 4. Refused before the stop

```bash
n network create --subnet 10.199.77.0/24 smoke-net
n run -d --name pinned --label freshdock.enable=true --network smoke-net --ip 10.199.77.9 \
  docker.io/library/alpine:3.20 sleep 1d
fd recreate pinned
```

Expect `pinned has settings freshdock can't reproduce with nerdctl, so it was not
updated: label nerdctl/ip`, and `pinned` still running with its original id.

## Cleanup

Stop before removing. A plain `rm -f` on a container with a restart policy can race
containerd's restart monitor and leave an orphaned task holding its ports.

```bash
for c in $(n ps -a --format '{{.Names}}'); do n stop "$c"; n rm "$c"; done
n network rm smoke-net; n volume rm web-data
```
