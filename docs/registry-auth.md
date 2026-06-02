# Registry authentication (Phase 5)

freshdock checks digests against any OCI-compliant registry that uses the Docker
registry v2 bearer-token flow — Docker Hub, GHCR, Quay.io, lscr.io, and others.
Public images resolve anonymously; private images need credentials.

## Configuring credentials

Credentials live in `freshdock.toml` (default: `./freshdock.toml`, or set
`--config <path>` / `FRESHDOCK_CONFIG`). One table per registry:

```toml
[registry.dockerhub]
username = "myuser"      # required for Docker Hub
token    = "dckr_pat_…"  # password or personal access token

[registry.ghcr]
username = "myuser"      # any non-empty value works for a GHCR PAT
token    = "ghp_…"

[registry.quay]
token    = "…"           # username optional

# A literal host also works as the table key:
[registry."registry.example.com"]
username = "svc"
token    = "…"
```

The table key may be a friendly alias (`dockerhub`, `ghcr`, `quay`, `lscr`) or a
registry host; both fold onto the same registry as the matching image reference.

## Environment overrides

Environment variables override the file **per field** (a lone `…_TOKEN` replaces
the file token while keeping the file username):

```text
FRESHDOCK_REGISTRY_<NAME>_USERNAME
FRESHDOCK_REGISTRY_<NAME>_TOKEN
```

`<NAME>` is `DOCKERHUB`, `GHCR`, `QUAY`, `LSCR` (the friendly aliases). Hosts
containing dots can't be expressed as an env name unambiguously — configure
those in the file. Tokens never appear in logs, even at `RUST_LOG=trace`.

## Manual PAT smoke test

Private-registry auth can't run in CI (no secrets). To verify a real PAT end to
end:

```bash
export FRESHDOCK_REGISTRY_GHCR_USERNAME=<your-gh-user>
export FRESHDOCK_REGISTRY_GHCR_TOKEN=<a-PAT-with-read:packages>
# A container whose image is a private ghcr.io/<owner>/<repo> must show a
# digest (not "auth required") in the table:
RUST_LOG=trace cargo run -- check
# Confirm the token never appears in the trace output.
```

Redaction is also enforced by automated tests
(`config::tests::token_is_redacted_in_tracing_output` and
`registry::auth::tests::cached_token_debug_redacts_the_token`); this manual run
is just an extra end-to-end check.

## Out of scope (v1)

ECR / GCR / ACR / Harbor custom auth schemes; insecure (plain-HTTP) and
`localhost:port` registries; reusing `~/.docker/config.json`. Rate-limit headers
are logged but freshdock does not yet throttle proactively.
