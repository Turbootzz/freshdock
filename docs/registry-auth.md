# Registry authentication

freshdock checks digests against any OCI-compliant registry that uses the Docker
registry v2 bearer-token flow: Docker Hub, GHCR, Quay.io, lscr.io, and others.
Public images resolve anonymously; private images need credentials, supplied either
as `FRESHDOCK_REGISTRY_<NAME>_TOKEN` environment variables (plus `_USERNAME` where
the registry needs it) or as a `[registry.<name>]` table in `freshdock.toml`. The
file is the only option for a custom host whose name contains dots.

The exact variables and table keys live in the
[configuration reference](configuration.md#registryname). This page covers the
registry-specific guidance: what credentials each registry wants, the alias list, a
smoke test, and what is out of scope.

## Per-registry notes

| Registry | `username` | `token` |
|---|---|---|
| Docker Hub | the real account name (required) | password or access token |
| GHCR (`ghcr.io`) | any non-empty value | a PAT with `read:packages` |
| Quay.io | optional | robot-account token / password |
| lscr.io | optional | as the registry requires |
| Other OCI + bearer | as the registry requires | as the registry requires |

GHCR, Quay, and lscr authenticate with a token alone; Docker Hub also needs its
account name (`FRESHDOCK_REGISTRY_DOCKERHUB_USERNAME`).

Anonymous Docker Hub is rate-limited (about 100 requests per 6 h); adding
credentials raises the budget. freshdock dedupes to one request per unique image to
stay well under it.

## Aliases

A `[registry.<name>]` table key, and the `<NAME>` in the env-var form, may be a
friendly alias or a literal host; both fold onto the same registry as the matching
image reference:

| Alias | Registry |
|---|---|
| `dockerhub`, `docker`, `docker.io`, `registry-1.docker.io`, `index.docker.io` | `docker.io` |
| `ghcr` | `ghcr.io` |
| `quay` | `quay.io` |
| `lscr` | `lscr.io` |

A literal host (`"registry.example.com"`) works as a table key too. Hosts containing
dots can't be expressed unambiguously as an environment variable name, so configure
those in the file. Tokens never appear in logs, even at `RUST_LOG=trace`.

## Manual PAT smoke test

Private-registry auth can't run in CI (no secrets). To verify a real PAT end to end:

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
`registry::auth::tests::cached_token_debug_redacts_the_token`); this manual run is
an extra end-to-end check.

## Out of scope (v1)

ECR / GCR / ACR / Harbor custom auth schemes; insecure (plain-HTTP) registries,
since freshdock always talks HTTPS and a registry reachable only over plain HTTP
(the common case for a `localhost:5000` dev registry) won't work; and reusing
`~/.docker/config.json`. Rate-limit headers are logged but freshdock does not yet
throttle proactively.
